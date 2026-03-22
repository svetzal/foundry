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

## Documentation

mdBook documentation lives in `book/`. Build with:

```bash
mdbook build book/
```
