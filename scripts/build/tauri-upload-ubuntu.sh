#!/usr/bin/env bash

set -euox pipefail

# This artifact name is tied to the update checker in `gui-client/src-tauri/src/client/updates.rs`

gh release upload "$TAG_NAME" \
    "$BINARY_DEST_PATH".deb \
    "$BINARY_DEST_PATH".deb.sha256sum.txt \
    --clobber \
    --repo "$REPOSITORY"
