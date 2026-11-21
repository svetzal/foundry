#!/usr/bin/env bash
# Build, test and package a Foundry release on the Linux ops host, from a tag.
#
#   scripts/build-linux-release.sh v0.39.4
#
# Builds in a detached worktree at the tag with one reused target directory
# ($FOUNDRY_RELEASE_DIR/target), and removes both the worktree and the target
# directory when it exits, whether the build passed or not. Only the tarball
# ($FOUNDRY_RELEASE_DIR/foundry-<version>-linux-x64.tar.gz) is kept. Release
# build output is tens of gigabytes; left behind, it filled the host's disk.
set -euo pipefail

TAG=${1:?usage: build-linux-release.sh vX.Y.Z}
REPO=$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)
OUT=${FOUNDRY_RELEASE_DIR:-$HOME/.cache/foundry-release}
WORK=$(mktemp -d)
WT="$WORK/foundry-$TAG"
export CARGO_TARGET_DIR="$OUT/target"

cleanup() {
    git -C "$REPO" worktree remove --force "$WT" 2>/dev/null || true
    git -C "$REPO" worktree prune
    rm -rf "$CARGO_TARGET_DIR" "$WORK"
}
trap cleanup EXIT

mkdir -p "$OUT"
git -C "$REPO" fetch -q --tags origin
git -C "$REPO" worktree add -q --detach "$WT" "$TAG"
cd "$WT"

# Build with the toolchain the tag pins in rust-toolchain.toml, the one release
# CI uses. A non-interactive ssh shell may lack ~/.cargo/bin, and mise's cargo
# shim ignores both RUSTUP_TOOLCHAIN and rust-toolchain.toml, so put rustup's
# proxies first and drop any inherited toolchain override.
export PATH="${CARGO_HOME:-$HOME/.cargo}/bin:$PATH"
unset RUSTUP_TOOLCHAIN
command -v rustup >/dev/null || { echo "rustup not found in ${CARGO_HOME:-$HOME/.cargo}/bin" >&2; exit 1; }
CHANNEL=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)
[ -n "$CHANNEL" ] || { echo "no channel in rust-toolchain.toml" >&2; exit 1; }
rustup toolchain install "$CHANNEL" --profile minimal --component rustfmt,clippy
case "$(rustc --version)" in
    "rustc $CHANNEL "*) rustc --version ;;
    *) echo "expected rustc $CHANNEL, got: $(rustc --version) ($(command -v rustc))" >&2; exit 1 ;;
esac

# Gate fixtures must not wait for or read the production campaign/ledger stores,
# nor write evidence into the production home. FOUNDRY_HOME moves every default
# path; the explicit overrides below also beat a per-path override inherited
# from the operator's shell.
GATE_STATE="$WORK/gate-state"
mkdir -p "$GATE_STATE"
export FOUNDRY_HOME="$GATE_STATE/home"
export FOUNDRY_CAMPAIGNS_PATH="$GATE_STATE/campaigns.json"
export FOUNDRY_WORK_ITEMS_PATH="$GATE_STATE/work-items.json"
export FOUNDRY_WORKTREES_DIR="$GATE_STATE/worktrees"
export FOUNDRY_PRESERVED_DIR="$GATE_STATE/preserved"

cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo build --release -p foundryd -p foundry-cli

VERSION=${TAG#v}
TARBALL="$OUT/foundry-$VERSION-linux-x64.tar.gz"
mkdir -p "$WORK/pkg"
cp "$CARGO_TARGET_DIR/release/foundry" "$CARGO_TARGET_DIR/release/foundryd" "$WORK/pkg/"
tar -C "$WORK/pkg" -czf "$TARBALL" foundry foundryd
sha256sum "$TARBALL"
"$WORK/pkg/foundry" --version
