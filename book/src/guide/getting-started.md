# Getting Started

## Prerequisites

- Rust 1.85+ (via `rust-toolchain.toml`)
- `protoc` (Protocol Buffers compiler): `brew install protobuf`

## Build

```bash
cd path/to/foundry
cargo build --release
```

This produces two binaries:

- `target/release/foundryd` — the daemon
- `target/release/foundry` — the CLI controller

## Install Locally

Install both binaries to `~/.cargo/bin/` using the convenience script:

```bash
./install.sh
```

Or install each crate individually:

```bash
cargo install --path crates/foundryd
cargo install --path crates/foundry-cli
```

Re-run after making changes to pick up the latest version.

## Start the Daemon

```bash
foundryd
```

You'll see the registered task blocks and the listening address:

```text
INFO foundryd::engine: registered task block block="Compose Greeting" sinks=[GreetRequested]
INFO foundryd::engine: registered task block block="Deliver Greeting" sinks=[GreetingComposed]
INFO foundryd::engine: registered task block block="Audit Release Tag" sinks=[VulnerabilityDetected]
INFO foundryd::engine: registered task block block="Audit Main Branch" sinks=[ReleaseTagAudited]
INFO foundryd::engine: registered task block block="Remediate Vulnerability" sinks=[MainBranchAudited]
INFO foundryd::engine: registered task block block="Commit and Push" sinks=[RemediationCompleted]
INFO foundryd::engine: registered task block block="Cut Release" sinks=[MainBranchAudited]
INFO foundryd::engine: registered task block block="Watch Pipeline" sinks=[ReleaseCompleted]
INFO foundryd::engine: registered task block block="Install Locally" sinks=[ProjectChangesPushed, ReleasePipelineCompleted]
INFO foundryd: foundryd listening on 127.0.0.1:50051
```

## Send Your First Event

In another terminal:

```bash
foundry emit greet_requested \
  --project hello \
  --payload '{"name": "World"}'
```

In the daemon logs you'll see the event chain:

```text
INFO foundryd::service: processing event event_type=greet_requested project=hello throttle=full
INFO foundryd::engine: executing task block block="Compose Greeting" ...
INFO foundryd::blocks::greet: composed greeting greeting=Hello, World!
INFO foundryd::engine: executing task block block="Deliver Greeting" ...
INFO foundryd::blocks::greet: delivering greeting greeting=Hello, World!
INFO foundryd::service: event chain complete total_events=3
```

Three events in the chain: `greet_requested` → `greeting_composed` → `greeting_delivered`.

## Try Throttle Control

```bash
# Observers run, mutators suppress emission
foundry emit greet_requested \
  --project hello \
  --throttle audit_only \
  --payload '{"name": "World"}'
```

Now you'll see only 2 events — `greeting_delivered` is suppressed because
Deliver Greeting is a Mutator and the throttle is `audit_only`.

## Quality Gates

```bash
cargo test                                              # Run all tests
cargo clippy --all-targets --all-features -- -D warnings # Lint
cargo fmt --check                                       # Format check
cargo deny check                                        # License + advisory audit
```
