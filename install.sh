#!/usr/bin/env bash
set -euo pipefail

echo "Installing Foundry binaries..."
cargo install --path crates/foundryd
cargo install --path crates/foundry-cli
echo "Done. foundryd and foundry installed to ~/.cargo/bin/"
