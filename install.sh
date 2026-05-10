#!/usr/bin/env bash
set -euo pipefail

echo "Installing Foundry binaries..."
cargo install --path crates/foundryd
cargo install --path crates/foundry-cli

# On macOS, cargo's default ad-hoc signature uses a hash-derived identifier
# (e.g. "foundryd-7c8b3941b75c7124") that changes on every rebuild. macOS TCC
# keys privacy grants (Full Disk Access, Documents, Desktop, etc.) by signing
# identifier, so a fresh hash means TCC sees a brand-new app and re-prompts
# the user the next time the daemon touches a protected location. Re-sign
# with a stable identifier so grants survive future rebuilds.
if [[ "$(uname -s)" == "Darwin" ]]; then
    echo "Re-signing with stable code-signing identifiers..."
    codesign --force --sign - --identifier com.mojility.foundryd "${CARGO_HOME:-$HOME/.cargo}/bin/foundryd"
    codesign --force --sign - --identifier com.mojility.foundry  "${CARGO_HOME:-$HOME/.cargo}/bin/foundry"
fi

echo "Done. foundryd and foundry installed to ~/.cargo/bin/"
