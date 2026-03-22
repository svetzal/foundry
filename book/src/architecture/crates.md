# Crate Structure

Foundry is organized as a Cargo workspace with three crates:

```text
foundry/
├── Cargo.toml              # Workspace root
├── proto/foundry.proto     # gRPC service definition
├── crates/
│   ├── foundry-core/       # Shared types (library)
│   ├── foundryd/           # Daemon (binary)
│   └── foundry-cli/        # CLI controller (binary)
└── book/                   # This documentation
```

## foundry-core

Shared types used by both the daemon and CLI:

- `event.rs` — `Event` struct, `EventType` enum, deterministic ID generation
- `throttle.rs` — `Throttle` enum (`Full`, `AuditOnly`, `DryRun`)
- `task_block.rs` — `TaskBlock` trait, `BlockKind`, `TaskBlockResult`

This crate has no async runtime dependency. It defines the vocabulary
that the rest of the system speaks.

Future: this crate (or a subset) will be shared with a Rust rewrite of
`evt-cli`, providing a single source of truth for event types.

## foundryd

The daemon process. Listens on gRPC (`[::1]:50051` by default) and runs
the workflow engine.

- `engine.rs` — event router: matches events to task blocks, executes
  them, propagates emitted events (checking throttle)
- `service.rs` — gRPC service implementation (`Emit`, `Status`, `Watch`)
- `blocks/` — task block implementations

## foundry-cli

The CLI controller. Connects to `foundryd` over gRPC.

- `commands.rs` — `emit`, `status`, `watch` subcommand implementations
- Parses arguments via `clap`, sends requests via `tonic` client

## proto/foundry.proto

The gRPC contract between CLI and daemon:

- `Emit` — fire an event with type, project, throttle, and payload
- `Status` — query active workflow states
- `Watch` — server-side stream of workflow status updates
