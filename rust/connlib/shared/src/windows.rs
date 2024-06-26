//! Windows-specific things like the well-known appdata path, bundle ID, etc.

use crate::Error;
use known_folders::{get_known_folder_path, KnownFolder};
use std::path::PathBuf;

/// Returns e.g. `C:/Users/User/AppData/Local/dev.firezone.client
///
/// This is where we can save config, logs, crash dumps, etc.
/// It's per-user and doesn't roam across different PCs in the same domain.
/// It's read-write for non-elevated processes.
pub fn app_local_data_dir() -> Result<PathBuf, Error> {
    let path = get_known_folder_path(KnownFolder::LocalAppData)
        .ok_or(Error::CantFindLocalAppDataFolder)?
        .join(crate::BUNDLE_ID);
    Ok(path)
}

pub mod dns {
    //! Gives Firezone DNS privilege over other DNS resolvers on the system
    //!
    //! This uses NRPT and claims all domains, similar to the `systemd-resolved` control method
    //! on Linux.
    //! This allows us to "shadow" DNS resolvers that are configured by the user or DHCP on
    //! physical interfaces, as long as they don't have any NRPT rules that outrank us.
    //!
    //! If Firezone crashes, restarting Firezone and closing it gracefully will resume
    //! normal DNS operation. The Powershell command to remove the NRPT rule can also be run
    //! by hand.
    //!
    //! The system default resolvers don't need to be reverted because they're never deleted.
    //!
    //! <https://superuser.com/a/1752670>

    use anyhow::Result;
    use std::{net::IpAddr, os::windows::process::CommandExt, process::Command};

    /// Hides Powershell's console on Windows
    ///
    /// <https://stackoverflow.com/questions/59692146/is-it-possible-to-use-the-standard-library-to-spawn-a-process-without-showing-th#60958956>
    /// Also used for self-elevation
    const CREATE_NO_WINDOW: u32 = 0x08000000;

    // Unique magic number that we can use to delete our well-known NRPT rule.
    // Copied from the deep link schema
    const FZ_MAGIC: &str = "firezone-fd0020211111";

    /// Tells Windows to send all DNS queries to our sentinels
    ///
    /// Parameters:
    /// - `dns_config_string`: Comma-separated IP addresses of DNS servers, e.g. "1.1.1.1,8.8.8.8"
    pub fn activate(dns_config: &[IpAddr], iface_idx: u32) -> Result<()> {
        let dns_config_string = dns_config
            .iter()
            .map(|ip| format!("\"{ip}\""))
            .collect::<Vec<_>>()
            .join(",");

        // Set our DNS IP as the DNS server for our interface
        // TODO: Known issue where web browsers will keep a connection open to a site,
        // using QUIC, HTTP/2, or even HTTP/1.1, and so they won't resolve the DNS
        // again unless you let that connection time out:
        // <https://github.com/firezone/firezone/issues/3113#issuecomment-1882096111>
        // TODO: If we have a Windows gateway, it shouldn't configure DNS, right?
        Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
            .arg("-Command")
            .arg(format!("Set-DnsClientServerAddress -InterfaceIndex {iface_idx} -ServerAddresses({dns_config_string})"))
            .status()?;

        tracing::info!("Activating DNS control");
        Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
            .args([
                "-Command",
                "Add-DnsClientNrptRule",
                "-Namespace",
                ".",
                "-Comment",
                FZ_MAGIC,
                "-NameServers",
                &dns_config_string,
            ])
            .status()?;
        Ok(())
    }

    /// Tells Windows to send all DNS queries to this new set of sentinels
    ///
    /// Currently implemented as just removing the rule and re-adding it, which
    /// creates a gap but doesn't require us to parse Powershell output to figure
    /// out the rule's UUID.
    ///
    /// Parameters:
    /// - `dns_config_string` - Passed verbatim to [`activate`]
    pub fn change(dns_config: &[IpAddr], iface_idx: u32) -> Result<()> {
        deactivate()?;
        activate(dns_config, iface_idx)?;
        Ok(())
    }

    pub fn deactivate() -> Result<()> {
        Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
            .args(["-Command", "Get-DnsClientNrptRule", "|"])
            .args(["where", "Comment", "-eq", FZ_MAGIC, "|"])
            .args(["foreach", "{"])
            .args(["Remove-DnsClientNrptRule", "-Name", "$_.Name", "-Force"])
            .args(["}"])
            .status()?;
        tracing::info!("Deactivated DNS control");
        Ok(())
    }

    /// Flush Windows' system-wide DNS cache
    pub fn flush() -> Result<()> {
        tracing::info!("Flushing Windows DNS cache");
        Command::new("powershell")
            .creation_flags(CREATE_NO_WINDOW)
            .args(["-Command", "Clear-DnsClientCache"])
            .status()?;
        Ok(())
    }
}

/// Returns the absolute path for installing and loading `wintun.dll`
///
/// e.g. `C:\Users\User\AppData\Local\dev.firezone.client\data\wintun.dll`
pub fn wintun_dll_path() -> Result<PathBuf, Error> {
    let path = app_local_data_dir()?.join("data").join("wintun.dll");
    Ok(path)
}
