//! A library for the privileged tunnel process for a Linux Firezone Client
//!
//! This is built both standalone and as part of the GUI package. Building it
//! standalone is faster and skips all the GUI dependencies. We can use that build for
//! CLI use cases.
//!
//! Building it as a binary within the `gui-client` package allows the
//! Tauri deb bundler to pick it up easily.
//! Otherwise we would just make it a normal binary crate.

use anyhow::{Context, Result};
use clap::Parser;
use connlib_client_shared::{file_logger, keypair, Callbacks, LoginUrl, Session, Sockets};
use connlib_shared::callbacks;
use firezone_cli_utils::setup_global_subscriber;
use secrecy::SecretString;
use std::{future, net::IpAddr, path::PathBuf, task::Poll};
use tokio::sync::mpsc;

use imp::default_token_path;

pub mod known_dirs;

#[cfg(target_os = "linux")]
pub mod imp_linux;
#[cfg(target_os = "linux")]
pub use imp_linux as imp;

#[cfg(target_os = "windows")]
pub mod imp_windows;
#[cfg(target_os = "windows")]
pub use imp_windows as imp;

/// Only used on Linux
pub const FIREZONE_GROUP: &str = "firezone-client";

/// Output of `git describe` at compile time
/// e.g. `1.0.0-pre.4-20-ged5437c88-modified` where:
///
/// * `1.0.0-pre.4` is the most recent ancestor tag
/// * `20` is the number of commits since then
/// * `g` doesn't mean anything
/// * `ed5437c88` is the Git commit hash
/// * `-modified` is present if the working dir has any changes from that commit number
pub const GIT_VERSION: &str = git_version::git_version!(
    args = ["--always", "--dirty=-modified", "--tags"],
    fallback = "unknown"
);

const TOKEN_ENV_KEY: &str = "FIREZONE_TOKEN";

#[derive(clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    // Needed to preserve CLI arg compatibility
    // TODO: Remove
    #[command(subcommand)]
    _command: Option<Cmd>,

    #[arg(
        short = 'u',
        long,
        hide = true,
        env = "FIREZONE_API_URL",
        default_value = "wss://api.firezone.dev"
    )]
    pub api_url: url::Url,

    /// Check the configuration and return 0 before connecting to the API
    ///
    /// Returns 1 if the configuration is wrong. Mostly non-destructive but may
    /// write a device ID to disk if one is not found.
    #[arg(long)]
    check: bool,

    /// Token generated by the portal to authorize websocket connection.
    // systemd recommends against passing secrets through env vars:
    // <https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html#Environment=>
    #[arg(env = TOKEN_ENV_KEY, hide = true)]
    token: Option<String>,

    /// A filesystem path where the token can be found

    // Apparently passing secrets through stdin is the most secure method, but
    // until anyone asks for it, env vars are okay and files on disk are slightly better.
    // (Since we run as root and the env var on a headless system is probably stored
    // on disk somewhere anyway.)
    #[arg(default_value_t = default_token_path().display().to_string(), env = "FIREZONE_TOKEN_PATH", long)]
    token_path: String,

    /// Identifier used by the portal to identify and display the device.

    // AKA `device_id` in the Windows and Linux GUI clients
    // Generated automatically if not provided
    #[arg(short = 'i', long, env = "FIREZONE_ID")]
    pub firezone_id: Option<String>,

    /// File logging directory. Should be a path that's writeable by the current user.
    #[arg(short, long, env = "LOG_DIR")]
    log_dir: Option<PathBuf>,

    /// Maximum length of time to retry connecting to the portal if we're having internet issues or
    /// it's down. Accepts human times. e.g. "5m" or "1h" or "30d".
    #[arg(short, long, env = "MAX_PARTITION_TIME")]
    max_partition_time: Option<humantime::Duration>,
}

#[derive(clap::Subcommand, Clone, Copy)]
enum Cmd {
    #[command(hide = true)]
    IpcService,
    Standalone,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub enum IpcClientMsg {
    Connect { api_url: String, token: String },
    Disconnect,
    Reconnect,
    SetDns(Vec<IpAddr>),
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub enum IpcServerMsg {
    Ok,
    OnDisconnect,
    OnUpdateResources(Vec<callbacks::ResourceDescription>),
    TunnelReady,
}

pub fn run_only_headless_client() -> Result<()> {
    let mut cli = Cli::parse();

    // Modifying the environment of a running process is unsafe. If any other
    // thread is reading or writing the environment, something bad can happen.
    // So `run` must take over as early as possible during startup, and
    // take the token env var before any other threads spawn.

    let token_env_var = cli.token.take().map(SecretString::from);
    let cli = cli;

    // Docs indicate that `remove_var` should actually be marked unsafe
    // SAFETY: We haven't spawned any other threads, this code should be the first
    // thing to run after entering `main`. So nobody else is reading the environment.
    #[allow(unused_unsafe)]
    unsafe {
        // This removes the token from the environment per <https://security.stackexchange.com/a/271285>. We run as root so it may not do anything besides defense-in-depth.
        std::env::remove_var(TOKEN_ENV_KEY);
    }
    assert!(std::env::var(TOKEN_ENV_KEY).is_err());

    let (layer, _handle) = cli.log_dir.as_deref().map(file_logger::layer).unzip();
    setup_global_subscriber(layer);

    tracing::info!(git_version = crate::GIT_VERSION);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let token = get_token(token_env_var, &cli)?.with_context(|| {
        format!(
            "Can't find the Firezone token in ${TOKEN_ENV_KEY} or in `{}`",
            cli.token_path
        )
    })?;
    run_standalone(cli, rt, &token)
}

// Allow dead code because Windows doesn't have an obvious SIGHUP equivalent
#[allow(dead_code)]
enum SignalKind {
    Hangup,
    Interrupt,
}

fn run_standalone(cli: Cli, rt: tokio::runtime::Runtime, token: &SecretString) -> Result<()> {
    tracing::info!("Running in standalone mode");
    let _guard = rt.enter();
    // TODO: Should this default to 30 days?
    let max_partition_time = cli.max_partition_time.map(|d| d.into());

    // AKA "Device ID", not the Firezone slug
    let firezone_id = match cli.firezone_id {
        Some(id) => id,
        None => connlib_shared::device_id::get().context("Could not get `firezone_id` from CLI, could not read it from disk, could not generate it and save it to disk")?.id,
    };

    let (private_key, public_key) = keypair();
    let login = LoginUrl::client(cli.api_url, token, firezone_id, None, public_key.to_bytes())?;

    if cli.check {
        tracing::info!("Check passed");
        return Ok(());
    }

    let (on_disconnect_tx, mut on_disconnect_rx) = mpsc::channel(1);
    let callback_handler = CallbackHandler { on_disconnect_tx };

    let session = Session::connect(
        login,
        Sockets::new(),
        private_key,
        None,
        callback_handler,
        max_partition_time,
        rt.handle().clone(),
    );
    // TODO: this should be added dynamically
    session.set_dns(imp::system_resolvers().unwrap_or_default());

    let mut signals = imp::Signals::new()?;

    let result = rt.block_on(async {
        future::poll_fn(|cx| loop {
            match on_disconnect_rx.poll_recv(cx) {
                Poll::Ready(Some(error)) => return Poll::Ready(Err(anyhow::anyhow!(error))),
                Poll::Ready(None) => {
                    return Poll::Ready(Err(anyhow::anyhow!(
                        "on_disconnect_rx unexpectedly ran empty"
                    )))
                }
                Poll::Pending => {}
            }

            match signals.poll(cx) {
                Poll::Ready(SignalKind::Hangup) => {
                    session.reconnect();
                    continue;
                }
                Poll::Ready(SignalKind::Interrupt) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        })
        .await
    });

    session.disconnect();

    result
}

#[derive(Clone)]
struct CallbackHandler {
    /// Channel for an error message if connlib disconnects due to an error
    on_disconnect_tx: mpsc::Sender<String>,
}

impl Callbacks for CallbackHandler {
    fn on_disconnect(&self, error: &connlib_client_shared::Error) {
        // Convert the error to a String since we can't clone it
        self.on_disconnect_tx
            .try_send(error.to_string())
            .expect("should be able to tell the main thread that we disconnected");
    }

    fn on_update_resources(&self, resources: Vec<callbacks::ResourceDescription>) {
        // See easily with `export RUST_LOG=firezone_headless_client=debug`
        for resource in &resources {
            tracing::debug!(?resource);
        }
    }
}

/// Read the token from disk if it was not in the environment
///
/// # Returns
/// - `Ok(None)` if there is no token to be found
/// - `Ok(Some(_))` if we found the token
/// - `Err(_)` if we found the token on disk but failed to read it
fn get_token(token_env_var: Option<SecretString>, cli: &Cli) -> Result<Option<SecretString>> {
    // This is very simple but I don't want to write it twice
    if let Some(token) = token_env_var {
        return Ok(Some(token));
    }
    read_token_file(cli)
}

/// Try to retrieve the token from disk
///
/// Sync because we do blocking file I/O
fn read_token_file(cli: &Cli) -> Result<Option<SecretString>> {
    let path = PathBuf::from(&cli.token_path);

    if let Ok(token) = std::env::var(TOKEN_ENV_KEY) {
        std::env::remove_var(TOKEN_ENV_KEY);

        let token = SecretString::from(token);
        // Token was provided in env var
        tracing::info!(
            ?path,
            ?TOKEN_ENV_KEY,
            "Found token in env var, ignoring any token that may be on disk."
        );
        return Ok(Some(token));
    }

    if std::fs::metadata(&path).is_err() {
        return Ok(None);
    }
    imp::check_token_permissions(&path)?;

    let Ok(bytes) = std::fs::read(&path) else {
        // We got the metadata a second ago, but can't read the file itself.
        // Pretty strange, would have to be a disk fault or TOCTOU.
        tracing::info!(?path, "Token file existed but now is unreadable");
        return Ok(None);
    };
    let token = String::from_utf8(bytes)?.trim().to_string();
    let token = SecretString::from(token);

    tracing::info!(?path, "Loaded token from disk");
    Ok(Some(token))
}
