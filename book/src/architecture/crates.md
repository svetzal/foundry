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
- `task_block.rs` — `TaskBlock` trait, `BlockKind`, `TaskBlockResult`, `RetryPolicy`
- `registry.rs` — `Registry`, `ProjectEntry`, `ActionFlags`, `Stack`, `InstallConfig`

This crate has no async runtime dependency. It defines the vocabulary
that the rest of the system speaks.

## foundryd

The daemon process. Listens on gRPC (`[::1]:50051` by default) and runs
the workflow engine.

### Core engine

- `engine.rs` — event router: matches events to task blocks, executes them with
  retry logic, propagates emitted events respecting the throttle level. Exposes
  `BlockExecution` and `ProcessResult` for structured telemetry.
- `service.rs` — gRPC service implementation (`Emit`, `Status`, `Watch`, `Trace`)

### Daemon support modules

- `orchestrator.rs` — coordinates per-project maintenance runs with concurrency
  control. Dispatches `MaintenanceRunStarted` per project, enforces
  `max_concurrent` via a semaphore, and prevents double-running via an active
  project set with a drop-guard cleanup.
- `event_writer.rs` — appends every event to monthly JSONL files
  (`YYYY-MM.jsonl`) inside `~/.foundry/events/` (or `FOUNDRY_EVENTS_DIR`).
  Crash-safe: each write opens, flushes, and closes the file. A `Mutex`
  serializes concurrent writes.
- `trace_store.rs` — in-memory store of recent `ProcessResult` chains, keyed by
  root event ID. Traces expire after a configurable TTL (default 1 hour).
- `shell.rs` — async shell runner used by block implementations. Runs an
  external command with configurable timeout (default 5 min), captures stdout
  and stderr, and returns a `CommandResult`.
- `scanner.rs` — vulnerability scanner abstraction. Dispatches to the
  stack-appropriate tool (`cargo audit`, `npm audit`, `pip-audit`,
  `mix deps.audit`) and normalizes output into a `Vec<Vulnerability>`.
- `summary.rs` — renders a `MaintenanceRunSummary` as a Markdown report
  (project table with success/failure/skipped, a failures section, and timing
  statistics).

### Task block implementations (`blocks/`)

- `validate.rs` — `ValidateProject`: pre-flight checks before a maintenance run
- `hone_iterate.rs` — `RunHoneIterate`: runs `hone iterate <agent> --json`
- `hone_maintain.rs` — `RunHoneMaintain`: runs `hone maintain`
- `git_ops.rs` — `CommitAndPush`: stages, commits, and optionally pushes changes
- `audit.rs` — `AuditReleaseTag`, `AuditMainBranch`: vulnerability scanning
- `release.rs` — `CutRelease`, `WatchPipeline`: tagging and CI monitoring
- `install.rs` — `InstallLocally`: reinstalls the project locally after a fix
- `remediate.rs` — `RemediateVulnerability`: invokes the AI agent to fix a CVE
- `scan.rs` — `ScanDependencies`: scans for known vulnerabilities
- `greet.rs` — `ComposeGreeting`, `DeliverGreeting`: hello-world engine validation

## foundry-cli

The CLI controller. Connects to `foundryd` over gRPC.

- `main.rs` — `clap`-based argument parsing; subcommands: `emit`, `status`,
  `watch`, `trace`, `run`
- `commands.rs` — async implementations of each subcommand via `tonic` gRPC
  client

## proto/foundry.proto

The gRPC contract between CLI and daemon:

- `Emit` — fire an event with type, project, throttle, and optional JSON payload
- `Status` — query active workflow states (all or by workflow ID)
- `Watch` — server-side streaming of live events, filterable by project
- `Trace` — retrieve the full event chain and block execution records for a
  completed workflow
