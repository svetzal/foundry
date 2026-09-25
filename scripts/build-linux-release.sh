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
