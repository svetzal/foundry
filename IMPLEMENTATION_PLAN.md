# Foundry Implementation Plan

Event-driven workflow engine replacing imperative maintenance shell scripts
with composable task blocks connected by events.

## Status: Complete

All infrastructure and tool integration phases are implemented:
shell execution, registry, JSONL persistence, per-project orchestration,
concurrency guards, launchd integration, CLI with live progress streaming,
real-time event streaming, and full summary rendering.

## Implemented Features

### Hone CLI Integration
- **RunHoneIterate** — Invokes `hone iterate <agent> <path> --json`, chains to maintenance when `actions.maintain=true`
- **RunHoneMaintain** — Invokes `hone maintain <agent> <path> --json`
- **RemediateVulnerability** — Invokes `hone maintain` for vulnerability remediation
- Shared `parse_hone_summary` utility for JSON output parsing

### Release Automation
- **CutRelease** — Claude CLI invocation with AGENTS.md verification, 15-minute timeout
- **WatchPipeline** — GitHub Actions polling via `gh` CLI with exponential backoff (30s–5min, 30min cap)
- **InstallLocally** — Shell command and Homebrew dispatch with graceful skip

### CLI
- **foundry run** — Emits maintenance event and streams live progress via Watch gRPC
- **foundry trace** — Renders event chain tree with per-block timing and chain overhead
- **foundry watch** — Real-time event stream subscription

### Summary Renderer
- Project status overview table with timing
- Failures section with per-project error details
- Release audit section (vulnerability status per tag)
- Auto-release log (new tags cut per run)
- Local installs section (method and status)

### Observability
- Retry policies on CommitAndPush (2 retries, 5s backoff) and InstallLocally (1 retry, 10s backoff)
- Per-block timing in trace output with chain overhead reporting

## Architecture Notes

**Event-delineated maintenance routing.** `RouteProjectWorkflow` (Observer)
sinks on `ProjectValidationCompleted` and emits either `IterationRequested`
(when `actions.iterate=true`) or `MaintenanceRequested` (when only
`actions.maintain=true`).

**Constructor injection over trait changes.** Blocks that need registry take
`Arc<Registry>` at construction. `TaskBlock::execute()` signature unchanged.

**Shared scanner utility.** Stack-specific audit logic lives in `scanner.rs`
so `AuditReleaseTag` and `AuditMainBranch` share one `run_audit()` function.

**Orchestrator above the engine.** Fan-out/fan-in lives in
`MaintenanceOrchestrator`, not inside `Engine`. Engine processes one chain;
orchestrator handles project enumeration, parallel spawning via `JoinSet`,
and aggregation.
