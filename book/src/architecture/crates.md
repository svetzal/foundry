# Crate Structure

Foundry is organised as a Cargo workspace with three crates:

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
- `trace.rs` — `TraceIndex`, `BlockExecution`, `ProcessResult` — the structured
  types used to persist and display execution traces. Moved here from `foundryd`
  so the CLI can deserialise on-disk traces without depending on the daemon crate.

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
  root event ID. Used for fast `Trace` RPC lookups of workflows still in
  progress or recently completed.
- `trace_writer.rs` — persists completed `ProcessResult` objects to disk as
  pretty-printed JSON files under `~/.foundry/traces/YYYY-MM-DD/{event_id}.json`.
  Traces written here survive daemon restarts indefinitely and are read by
  `foundry history` and `foundry trace` when the in-memory store has no match.
- `workflow_tracker.rs` — tracks workflows that are currently being processed
  by background tasks. Thread-safe via `RwLock`. Each `Emit` RPC inserts an
  `ActiveWorkflow` entry on start; a RAII `WorkflowGuard` removes it on
  completion or panic. The `Status` RPC reads this tracker to show live
  in-flight workflows.
- `shell.rs` — async shell runner used by block implementations. Runs an
  external command with configurable timeout (default 5 min), captures stdout
  and stderr, and returns a `CommandResult`.
- `scanner.rs` — vulnerability scanner abstraction. Dispatches to the
  stack-appropriate tool (`cargo audit`, `npm audit`, `pip-audit`,
  `mix deps.audit`) and normalizes output into a `Vec<Vulnerability>`.
- `gateway.rs` — I/O abstraction layer for task blocks. Defines `ShellGateway`
  and `ScannerGateway` traits with `ProcessShellGateway` and
  `ProcessScannerGateway` production implementations. Also provides
  `FakeShellGateway` and `FakeScannerGateway` test doubles (available under
  `#[cfg(test)]` only) that record invocations and return pre-configured
  results, enabling hermetic unit testing of every block without spawning
  real processes.
- `summary.rs` — renders a `MaintenanceRunSummary` as a Markdown report
  (project table with success/failure/skipped, a failures section, and timing
  statistics).

### Task block implementations (`blocks/`)

- `validate.rs` — `ValidateProject`: pre-flight checks before a maintenance run
- `resolve_gates.rs` — `ResolveGates`: reads `.hone-gates.json` and emits gate definitions
- `run_preflight_gates.rs` — `RunPreflightGates`: runs gates on unmodified codebase
- `run_verify_gates.rs` — `RunVerifyGates`: runs gates after code changes
- `route_gate_result.rs` — `RouteGateResult`: routes pass/fail to completion or retry
- `route_validation_result.rs` — `RouteValidationResult`: routes validation-only results
- `check_charter.rs` — `CheckCharter`: validates project charter before iteration
- `assess_project.rs` — `AssessProject`: AI-driven project assessment
- `triage_assessment.rs` — `TriageAssessment`: prioritises assessment findings
- `create_plan.rs` — `CreatePlan`: generates an execution plan from triaged findings
- `execute_plan.rs` — `ExecutePlan`: executes the generated plan
- `execute_maintain.rs` — `ExecuteMaintain`: runs maintenance tasks
- `retry_execution.rs` — `RetryExecution`: retries failed executions with context
- `summarize_result.rs` — `SummarizeResult`: generates workflow summary and traces
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
  `watch`, `trace`, `run`, `history`, `registry`
- `commands.rs` — async implementations of each subcommand via `tonic` gRPC
  client; also contains the `history` command which reads on-disk traces
  directly from `~/.foundry/traces/` without a daemon connection
- `registry_commands.rs` — pure I/O implementations of the `registry`
  subcommands (`init`, `list`, `show`, `add`, `remove`, `edit`); reads and
  writes `~/.foundry/registry.json` using `foundry_core::registry` types

## proto/foundry.proto

The gRPC contract between CLI and daemon:

- `Emit` — fire an event with type, project, throttle, and optional JSON payload
- `Status` — query active workflow states (all or by workflow ID)
- `Watch` — server-side streaming of live events, filterable by project
- `Trace` — retrieve the full event chain and block execution records for a
  completed workflow
