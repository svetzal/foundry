# Foundry Implementation Plan

Event-driven workflow engine replacing imperative maintenance shell scripts
with composable task blocks connected by events.

## Current State

The engine is fully operational. All infrastructure phases are complete:
shell execution, registry, JSONL persistence, per-project orchestration,
concurrency guards, launchd integration, CLI, and real-time event streaming.
The maintenance workflow is structurally sound with explicit routing via
`RouteProjectWorkflow` → `IterationRequested` / `MaintenanceRequested`.

What remains is wiring the actual tool invocations (`hone`, Claude CLI,
GitHub Actions polling) into blocks that currently succeed unconditionally,
and filling out the summary renderer and observability polish.

---

## Remaining Work

### Hone Command Invocations

**`RunHoneIterate`** (`crates/foundryd/src/blocks/hone_iterate.rs`)

Currently stubs with unconditional success. Replace with real invocation:

```
hone iterate <agent> <path> --json
```

Parse hone JSON output into `ProjectIterateCompleted` payload. Emit
`MaintenanceRequested` post-success when `payload.actions.maintain=true`.

**`RunHoneMaintain`** (`crates/foundryd/src/blocks/hone_maintain.rs`)

Currently stubs with unconditional success. Replace with real invocation:

```
hone maintain <agent> <path> --json
```

Parse hone JSON output into `ProjectMaintainCompleted` payload.

**`RemediateVulnerability`** (`crates/foundryd/src/blocks/remediate.rs`)

Currently stubs. Replace with real invocation:

```
hone maintain <agent> <path> --json
```

Target the vulnerable dependency. Emit `RemediationCompleted { cve, success }`.

---

### Real Release + Install

**`CutRelease`** (`crates/foundryd/src/blocks/release.rs`)

Claude CLI invocation is TODO. Implement:

- Verify `AGENTS.md` exists in project directory
- Invoke: `claude --model sonnet --print -p "<release prompt>" --dangerously-skip-permissions`
- Set `CLAUDECODE=` env var to prevent nested session error
- Parse output for new tag, emit `AutoReleaseCompleted { success, new_tag }`

Reference: `Automation/maintenance/auto-release.sh`

**`WatchPipeline`** (`crates/foundryd/src/blocks/release.rs`)

GitHub Actions polling is not implemented. Replace `WatchPipeline::stub()` with:

- Poll: `gh run list --workflow release --branch <branch> --limit 1 --json status,conclusion,url`
- Exponential backoff: 30 s initial, 5 min cap, 30 min total timeout
- `tokio::time::sleep` between polls
- Emit `ReleasePipelineCompleted { status, conclusion, run_url }`
- Handle: no release workflow exists

**`InstallLocally`** (`crates/foundryd/src/blocks/install.rs`)

Shell dispatch has TODO comments. Complete:

- `Command` variant: run shell command in project directory
  (e.g., `cargo install --path .`, `uv tool install --reinstall .`)
- `Brew` variant: `brew upgrade <formula>`, treat "already up to date" as success
- Skip gracefully if no install config defined

Reference: `Automation/maintenance/install-local.sh`

---

### CLI Progress Display

**`foundry run`** (`crates/foundry-cli/src/commands.rs`)

Currently emits `maintenance_run_started` and exits. The command should
stream per-project progress as chains complete — subscribe to the Watch
gRPC stream and print results as they arrive.

---

### Summary Renderer

**`crates/foundryd/src/summary.rs`**

Current renderer produces: overview table, per-project status, failures,
timing. Still missing sections from the original `maintain.sh` format:

- Release audit section (vulnerability status per project per tag)
- Local installs section (what was installed/upgraded)
- Auto-release log (new tags cut this run)

---

### Observability Polish

**Retry logic** — `RetryPolicy` trait method exists but no block overrides it.
Wire actual retry behaviour for transient failures: git push network errors,
`gh` API rate limits, brew timeouts. Block-level configuration via
`retry_policy()` override.

**Trace output** — `duration_ms` field exists on `BlockExecution` and
`ProcessResult`. Enhance `foundry trace` rendering to display per-block
timing and a total chain duration footer.

---

## Architecture Notes

**Event-delineated maintenance routing.** `RouteProjectWorkflow` (Observer)
sinks on `ProjectValidationCompleted` and emits either `IterationRequested`
(when `actions.iterate=true`) or `MaintenanceRequested` (when only
`actions.maintain=true`). `RunHoneIterate` sinks on `IterationRequested` only;
`RunHoneMaintain` sinks on `MaintenanceRequested` only. This supersedes the
earlier dual-sink design.

**Constructor injection over trait changes.** Blocks that need registry take
`Arc<Registry>` at construction. `TaskBlock::execute()` signature unchanged.

**Shared scanner utility.** Stack-specific audit logic lives in `scanner.rs`
so `AuditReleaseTag` and `AuditMainBranch` share one `run_audit()` function,
matching the structure of `audit-releases.sh`.

**Orchestrator above the engine.** Fan-out/fan-in lives in
`MaintenanceOrchestrator`, not inside `Engine`. Engine processes one chain;
orchestrator handles project enumeration, parallel spawning via `JoinSet`,
and aggregation. Concurrency guards prevent the same project processing twice.

---

## Risks

| Risk | Mitigation |
|------|------------|
| Claude CLI fragility (`--dangerously-skip-permissions`, `CLAUDECODE=` env) | Robust error handling with timeout; capture full output for debugging |
| Git state corruption during tag checkout/restore in AuditReleaseTag | Three-layer cleanup already ported from `audit-releases.sh`: checkout --, clean -fd, force checkout back to branch |
