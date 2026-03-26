# Foundry — Agent Guidance

## Project Overview

Foundry is an event-driven workflow engine for engineering automation. It consists of a Rust workspace with three crates:

- **foundry-core** — Shared domain types (Event, TaskBlock trait, Throttle)
- **foundryd** — Daemon/service binary (gRPC server, engine, task blocks, trace store)
- **foundry-cli** — CLI controller binary (gRPC client)

## How to Build

```bash
cargo build --workspace
```

## How to Deploy Locally

Install both binaries to `~/.cargo/bin/`:

```bash
./install.sh
```

Or individually:

```bash
cargo install --path crates/foundryd
cargo install --path crates/foundry-cli
```

Re-run after making changes to pick up the latest version.

Start the daemon:

```bash
foundryd
```

## Quality Gates

Run all of these before considering work complete:

```bash
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
```

## Key Conventions

- Edition 2024, Rust 1.85+, `unsafe_code` is denied
- Clippy pedantic warnings enabled with selective exceptions (see any crate's `Cargo.toml`)
- gRPC via tonic/prost, proto definition in `proto/foundry.proto`
- Both `foundryd` and `foundry-cli` compile the proto in their `build.rs`
- Structured logging via `tracing` with `info_span!` for request correlation
- No external observability dependencies — tracing spans only
- All tasks must include tests and all relevant documentation updates

## CI / Release

- **CI** runs on push/PR to `main`: fmt, clippy, test (`.github/workflows/ci.yml`)
- **Release** runs on tag push (`v*`): builds macOS arm64, macOS x86_64, and Linux x86_64 binaries, creates a GitHub release with tarballs and checksums (`.github/workflows/release.yml`)

To cut a release:

```bash
# Update version in Cargo.toml [workspace.package], update CHANGELOG.md
git tag v0.X.0
git push origin main --tags
```

The repo is public under `svetzal/foundry`. Homebrew distribution via `svetzal/homebrew-tap` — the release workflow auto-updates the formula.

Install via Homebrew:

```bash
brew tap svetzal/tap
brew install foundry
```

## Documentation

mdBook documentation lives in `book/`. Build with:

```bash
mdbook build book/
```

## Key Directories

- `~/.foundry/registry.json` — project registry (managed via `foundry registry` commands)
- `~/.foundry/traces/YYYY-MM-DD/` — persistent trace files (survive daemon restarts)
- `~/.foundry/audits/{project}/` — centralized hone audit logs
- `~/.foundry/events/YYYY-MM.jsonl` — event persistence (configurable via `FOUNDRY_EVENTS_DIR`)

## Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `FOUNDRY_REGISTRY_PATH` | `~/.foundry/registry.json` | Project registry file |
| `FOUNDRY_EVENTS_DIR` | `~/.foundry/events` | JSONL event output directory |
| `FOUNDRY_TRACES_DIR` | `~/.foundry/traces` | Persistent trace storage |
| `FOUNDRY_AUDITS_DIR` | `~/.foundry/audits` | Centralized hone audit logs |
