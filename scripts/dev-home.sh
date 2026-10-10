#!/usr/bin/env bash
# dev-home.sh — seed the development Foundry home with a synthetic project.
#
# Creates ~/.foundry/sandbox/ with a bare origin and a checkout of `sample-rs`
# (a zero-dependency Rust crate with a charter, gates and one test), then
# registers it with the development daemon. Idempotent: re-running changes
# nothing that already exists. Never touches a daemon other than FOUNDRY_ADDR.
set -euo pipefail

ADDR="${FOUNDRY_ADDR:-http://127.0.0.1:50051}"
HOME_DIR="${HOME}/.foundry"
SANDBOX="${HOME_DIR}/sandbox"
ORIGIN="${SANDBOX}/origin/sample-rs.git"
CHECKOUT="${SANDBOX}/sample-rs"

mkdir -p "${SANDBOX}/origin" "${SANDBOX}/events"

if [ ! -d "${ORIGIN}" ]; then
  git init -q --bare -b main "${ORIGIN}"
  echo "created origin ${ORIGIN}"
fi

if [ ! -d "${CHECKOUT}/.git" ]; then
  git init -q -b main "${CHECKOUT}"
  cd "${CHECKOUT}"
  cat > Cargo.toml <<'TOML'
[package]
name = "sample-rs"
version = "0.1.0"
edition = "2021"
description = "Synthetic project for exercising a development Foundry daemon."
license = "MIT"

[dependencies]
TOML
  mkdir -p src
  cat > src/lib.rs <<'RS'
//! A deliberately small library: one pure function and one test, so that
//! every Foundry gate runs in seconds and a change to it is easy to review.

/// Returns the greeting Foundry's synthetic project owes `name`.
pub fn greet(name: &str) -> String {
    format!("hello, {name}")
}

#[cfg(test)]
mod tests {
    use super::greet;

    #[test]
    fn greets_by_name() {
        assert_eq!(greet("foundry"), "hello, foundry");
    }
}
RS
  cat > CHARTER.md <<'MD'
# Charter

## Mission

Be the smallest project a development Foundry daemon can run every workflow
against: validate, iterate, scout, maintain, a task, a campaign cycle, a
release rehearsal. Changes here prove Foundry behaviour; they carry no value
of their own.

## Principles

1. Every gate finishes in seconds and needs no network.
2. The code stays small enough that a reviewer can read all of it.
3. Nothing here is ever promoted anywhere.
MD
  cat > AGENTS.md <<'MD'
# sample-rs agent guidance

Synthetic project owned by the development Foundry daemon. Trunk is `main`;
the origin is a local bare repository, so pushes need no credentials. Quality
gates: `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D
warnings`, `cargo test`. Keep the crate dependency-free so gates stay offline.
MD
  cat > .hone-gates.json <<'JSON'
{
  "gates": [
    { "name": "format", "command": "cargo fmt --all -- --check", "required": true },
    { "name": "lint", "command": "cargo clippy --all-targets -- -D warnings", "required": true },
    { "name": "test", "command": "cargo test", "required": true }
  ]
}
JSON
  printf 'target/\n' > .gitignore
  printf '# sample-rs\n\nSynthetic project for the development Foundry daemon. See CHARTER.md.\n' > README.md
  cargo generate-lockfile -q 2>/dev/null || true
  git add -A && git -c user.name=foundry-dev -c user.email=foundry-dev@localhost commit -q -m "Seed the synthetic sample project"
  git remote add origin "${ORIGIN}"
  git push -q -u origin main
  echo "created checkout ${CHECKOUT}"
fi

if foundry --addr "${ADDR}" registry show sample-rs >/dev/null 2>&1; then
  echo "sample-rs already registered at ${ADDR}"
else
  foundry --addr "${ADDR}" registry add \
    --name sample-rs --path "${CHECKOUT}" --stack rust --agent codex \
    --repo sandbox/sample-rs --branch main --iterate --maintain --push --audit
  foundry --addr "${ADDR}" registry edit sample-rs --update-policy major >/dev/null
  echo "registered sample-rs at ${ADDR}"
fi
