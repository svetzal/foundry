# Foundry Implementation Plan

Phased plan to complete the event-driven workflow engine, replacing the
imperative maintenance shell scripts with composable task blocks connected
by events.

## Current State

The engine skeleton is complete: event routing, throttle control, gRPC
transport, CLI, and trace store all work. Nine task blocks are registered
but all simulate their work — no real shell commands execute. The
vulnerability remediation workflow is structurally proven with integration
tests covering the dirty path, clean path, not-vulnerable early-stop,
and throttle suppression.

### What Works

- foundry-core: Event (deterministic IDs), EventType (18 variants),
  TaskBlock trait, Throttle (Full/AuditOnly/DryRun)
- foundryd: Engine (depth-first propagation), gRPC service
  (Emit/Status/Watch/Trace), TraceStore (in-memory, TTL)
- foundry-cli: emit, status, watch, trace commands
- Hello-world workflow: ComposeGreeting, DeliverGreeting
- Vulnerability workflow (stubs): AuditReleaseTag, AuditMainBranch,
  RemediateVulnerability, CommitAndPush, CutRelease, WatchPipeline,
  InstallLocally
- Integration tests proving event chain routing and throttle behavior

### What's Missing

| Gap | Description |
|-----|-------------|
| Shell execution | No block runs real commands (git, hone, cargo audit, etc.) |
| Registry | No project configuration loaded — blocks don't know paths, stacks, actions |
| Maintenance blocks | ValidateProject, RunHoneIterate, RunHoneMaintain not implemented |
| CommitAndPush sinks | Only sinks on RemediationCompleted — needs ProjectIterateCompleted, ProjectMaintainCompleted |
| AuditReleaseTag sinks | Only sinks on VulnerabilityDetected — needs ProjectChangesPushed |
| Event persistence | Events exist only in-memory during processing — not written to JSONL |
| Parallelization | Engine processes one chain at a time — no per-project fan-out/fan-in |
| Summary generation | No markdown summary output matching current maintain.sh format |
| launchd integration | No daemon startup or scheduled run trigger |

---

## Phase 0: Command Execution Foundation

*Prerequisite for all subsequent phases. Adds new modules only — no existing
block logic changes.*

### 0.1 Shell Command Runner

Create `crates/foundryd/src/shell.rs`:

- Async function wrapping `tokio::process::Command`
- Takes working directory, command, args, optional env vars
- Returns `CommandResult { stdout, stderr, exit_code, success }`
- Configurable timeout (default 5 minutes)
- Logs command and result via `tracing`

Tests: successful command, failed command, timeout behavior.

### 0.2 Registry Data Model

Create `crates/foundry-core/src/registry.rs`:

- `Registry` struct: version, projects vec
- `ProjectEntry`: name, path, stack (enum), agent, repo, branch,
  skip (Option), actions (`ActionFlags`), install (Option `InstallConfig`)
- `ActionFlags`: iterate, maintain, push, audit, release (all bool)
- `InstallConfig` enum: `Command { command }` or `Brew { formula }`
- `Registry::load(path)` — deserialize from JSON matching the v2 format
  in `~/Work/Operations/Automation/maintenance/registry.json`
- `Registry::active_projects()` — filter out skipped entries

Tests: deserialize real registry format, active project filtering.

### 0.3 Inject Registry into Blocks

Constructor injection — blocks that need registry take `Arc<Registry>` at
construction. The `TaskBlock` trait signature stays unchanged. Hello-world
blocks keep the unit struct pattern.

Update `foundryd/src/main.rs` to load registry at startup and pass it to
blocks that need it.

---

## Phase 1: Maintenance Workflow Skeleton

*Makes the nightly maintenance chain structurally complete. ValidateProject
does real filesystem checks. Hone blocks shell out to real commands. Other
blocks remain stubs but respond to the correct events.*

### 1.1 ValidateProject Block

Create `crates/foundryd/src/blocks/validate.rs`:

- **Kind**: Observer
- **Sinks on**: `MaintenanceRunStarted`
- **Work** (real):
  - Look up project in registry
  - Check directory exists
  - Check current git branch (`git rev-parse --abbrev-ref HEAD`)
  - Recover from detached HEAD (`git checkout <configured_branch>`)
  - Check `.hone-gates.json` exists
- **Emits**: `ProjectValidationCompleted` with
  `{ project, status: "ok"|"error", reason }`
- **Self-filters**: skip if project has `skip` reason in registry

### 1.2 RunHoneIterate Block

Create `crates/foundryd/src/blocks/hone_iterate.rs`:

- **Kind**: Mutator
- **Sinks on**: `ProjectValidationCompleted`
- **Self-filters**: only when `status == "ok"` and project's
  `actions.iterate == true`
- **Work**: `hone iterate <agent> <path> --audit-dir <dir> --json`
- **Emits**: `ProjectIterateCompleted` with hone JSON output in payload

### 1.3 RunHoneMaintain Block

Create `crates/foundryd/src/blocks/hone_maintain.rs`:

- **Kind**: Mutator
- **Sinks on**: `ProjectIterateCompleted` AND `ProjectValidationCompleted`
  (when iterate is disabled for the project)
- **Self-filters**: check trigger event type + project action flags to
  determine the correct entry path
- **Work**: `hone maintain <agent> <path> --audit-dir <dir> --json`
- **Emits**: `ProjectMaintainCompleted`

### 1.4 Wire CommitAndPush to Additional Sinks

Update `crates/foundryd/src/blocks/git_ops.rs`:

- Add `ProjectIterateCompleted` and `ProjectMaintainCompleted` to `sinks_on()`
- Self-filter: check payload for changes (or actively check git status)
- Differentiate commit message based on trigger event type

### 1.5 Wire AuditReleaseTag to Additional Sink

Update `crates/foundryd/src/blocks/audit.rs`:

- Add `ProjectChangesPushed` to `AuditReleaseTag::sinks_on()`
- When triggered by `ProjectChangesPushed`, look up project stack in
  registry for scanner selection

---

## Phase 2: Real Git Operations + Real Audit

*Blocks start doing real work. After this phase, foundry can maintain a
single project end-to-end.*

### 2.1 Stack-Specific Scanner Utility

Create `crates/foundryd/src/scanner.rs`:

- `async fn run_audit(path: &Path, stack: &Stack) -> Result<AuditResult>`
- `AuditResult { vulnerabilities: u32, raw_output: String, tool: String }`
- Stack-specific scanners matching `audit-releases.sh` `run_audit()`:
  - Python: `uv sync --extra dev && uv run pip-audit --format json`
  - TypeScript/Bun: `npm audit --json` / `bun audit --json`
  - Rust: `cargo audit --json`
  - Elixir: `mix deps.get --quiet && mix deps.audit`
- JSON output parsing for vulnerability counts

### 2.2 Real AuditReleaseTag

Replace stub in `crates/foundryd/src/blocks/audit.rs`:

- `git fetch --tags --quiet`
- Find latest tag: `git tag --sort=-v:refname | head -1`
- Save current ref, check for dirty working tree
- `git checkout <tag> --quiet`
- Run scanner via `scanner::run_audit()`
- Three-layer cleanup (matching audit-releases.sh paranoia):
  `git checkout -- .`, `git clean -fd`, force checkout back to branch
- Emit `ReleaseTagAudited` with `{ tag, vulnerabilities, stack, vulnerable }`

### 2.3 Real AuditMainBranch

Replace stub in `crates/foundryd/src/blocks/audit.rs`:

- Run scanner on current branch via `scanner::run_audit()`
- Clean up files dirtied by audit
- Emit `MainBranchAudited` with `{ branch, vulnerabilities, clean, dirty }`

### 2.4 Real CommitAndPush

Replace stub in `crates/foundryd/src/blocks/git_ops.rs`:

- Check for changes: `git diff --quiet && git diff --cached --quiet` +
  `git ls-files --others --exclude-standard`
- If no changes: emit nothing, success, "No changes to commit"
- `git add -A`
- `git commit -m <message>` (headline from trigger payload)
- Emit `ProjectChangesCommitted`
- If project's `actions.push == true`: `git push`
- Emit `ProjectChangesPushed`

---

## Phase 3: Real Release + Install + Pipeline Watching

*Completes the tail end of the workflow — everything after audit.*

### 3.1 Real CutRelease

Replace stub in `crates/foundryd/src/blocks/release.rs`:

- Verify `AGENTS.md` exists in project directory
- Invoke Claude CLI:
  `claude --model sonnet --print -p "<release prompt>" --dangerously-skip-permissions`
- Set `CLAUDECODE=` env var to prevent nested session error
- Capture output, check exit code
- Emit `AutoReleaseCompleted` with `{ success, new_tag }`

Reference: `Automation/maintenance/auto-release.sh`

### 3.2 Real WatchPipeline

Replace stub in `crates/foundryd/src/blocks/release.rs`:

- Poll GitHub Actions:
  `gh run list --workflow release --branch <branch> --limit 1 --json status,conclusion,url`
- Exponential backoff: start 30s, max 5 min, total timeout 30 min
- Use `tokio::time::sleep` between polls
- Emit `ReleasePipelineCompleted` with `{ status, conclusion, run_url }`
- Handle edge case: no release workflow exists

This is the async boundary identified in the original event model — the gap
between pushing a tag and distribution being ready.

### 3.3 Real InstallLocally

Replace stub in `crates/foundryd/src/blocks/install.rs`:

- Look up install config in registry
- `Command` method: run shell command in project directory
  (e.g., `uv tool install --reinstall .`, `cargo install --path .`)
- `Brew` method: `brew upgrade <formula>`
  (handle "already up to date" as success)
- Self-filter: skip if project has no install config
- Emit `LocalInstallCompleted` with `{ method, status, details }`

Reference: `Automation/maintenance/install-local.sh`

### 3.4 Real RemediateVulnerability

Replace stub in `crates/foundryd/src/blocks/remediate.rs`:

- Run `hone maintain` targeting the vulnerable dependency
- Similar to RunHoneMaintain but triggered from vulnerability path
- Emit `RemediationCompleted` with `{ cve, success }`

---

## Phase 4: Event Persistence (JSONL)

*Can proceed in parallel with Phases 2-3. Cross-cutting concern on the
engine output side.*

### 4.1 JSONL Event Writer

Create `crates/foundryd/src/event_writer.rs`:

- `EventWriter` holds intake directory path
  (default: `~/Work/Operations/Events/intake/`)
- `fn write(&self, event: &Event) -> Result<()>`:
  - Filename: `YYYY-MM.jsonl` from `occurred_at`
  - Serialize to JSON matching evt-cli common schema:
    `id`, `type`, `occurredAt`, `recordedAt`, `category`, `urgency`,
    `status`, `source`, `summary`, plus event-specific fields
  - Append line to file via `tokio::fs`
- Map EventType variants to evt-cli type strings and categories

### 4.2 Hook Writer into Engine

- Write each event as it is produced (inside processing loop) for
  real-time persistence
- Inject `EventWriter` into Engine or FoundryService via constructor
- Configuration: intake directory path, enabled/disabled

### 4.3 Event Schema Alignment

- For each EventType, define mapping to evt-cli schema format
- Create or update schemas in `Events/schemas/` for new event types
  (Operations repo, not foundry)
- Validate: events written by foundry can be queried by `evt query`

---

## Phase 5: Per-Project Fan-out / Fan-in

*Major engine addition. Makes the maintenance run process all projects
concurrently.*

### 5.1 Maintenance Orchestrator

Create `crates/foundryd/src/orchestrator.rs`:

- `MaintenanceOrchestrator` struct, separate from Engine
- `async fn run(registry, engine, throttle) -> MaintenanceRunResult`
- Reads registry for all active projects
- Emits `MaintenanceRunStarted` per project
- Spawns `tokio::task` per project via `JoinSet`
- Each task runs its project's chain via `Engine::process()`
- Depth-first within a project, parallel across projects

The engine stays simple (process a single event chain). The orchestrator
handles enumeration, spawning, and aggregation.

### 5.2 Fan-in Aggregation

- After all per-project tasks complete, collect `ProcessResult`s
- Emit `MaintenanceRunCompleted` with aggregate payload:
  `run_id`, `date`, `projects_processed`, `passed`, `failed`,
  per-project status summaries

### 5.3 CLI "run" Command

Add `foundry run` to foundry-cli:

- Options: `--project <name>` (single project), `--throttle <level>`,
  `--registry <path>`
- Calls a new gRPC `Run` endpoint or reuses `Emit` with
  `maintenance_run_started`
- Shows progress as projects complete

### 5.4 Concurrency Guards

- Prevent two runs from operating on the same project simultaneously
- `HashSet<String>` of active project names behind `tokio::sync::RwLock`
- Skip project with warning if already being processed

---

## Phase 6: Summary + launchd Integration

*Replaces the last shell script responsibilities.*

### 6.1 Summary Renderer

Create `crates/foundryd/src/summary.rs`:

- `fn render_summary(result: &MaintenanceRunResult) -> String`
- Markdown output matching current maintain.sh format:
  - Overview table (processed, passed, failed)
  - Per-project status (project, stack, status, iterate headline,
    maintain headline, gates)
  - Release audit section
  - Local installs section
  - Auto-release logs
- Write to run directory

### 6.2 launchd Plists

- `com.mojility.foundryd.plist`: starts daemon at boot (KeepAlive),
  sets PATH, logs to `logs/`
- `com.mojility.foundry-run.plist`: runs `foundry run` at 2:00 AM daily,
  replacing the current `com.mojility.maintenance.plist`

### 6.3 Deprecation Bridge

Update `maintain.sh` to check if `foundryd` is running:

- If yes: delegate to `foundry run` and exit
- If no: fall through to existing shell logic
- Allows gradual cutover while launchd still calls maintain.sh

---

## Phase 7: Resilience + Observability Polish

### 7.1 Retry Logic

- Git push failures, network errors during `gh run`, brew timeouts
- Configurable retry policy per block (max retries, backoff)
- Implement in shell runner or block-level

### 7.2 Trace Output Improvements

- Add duration tracking to `BlockExecution`
- Enhance `foundry trace` to show timing and failure details

### 7.3 Watch Stream Implementation

- Implement real event streaming on the `Watch` gRPC endpoint
- `tokio::sync::broadcast` channel for live event publishing
- Subscribers receive events in real-time during a run

---

## Sequencing

```text
Phase 0 (foundation) ───────────────┐
                                     │
                    ┌────────────────┴──────────────────┐
                    |                                    |
              Phase 1 (skeleton)              Phase 4 (JSONL persistence)
                    |                                    |
              Phase 2 (git + audit)                      |
                    |                                    |
              Phase 3 (release + install)                |
                    |                                    |
                    └────────────────┬───────────────────┘
                                     |
                               Phase 5 (fan-out / fan-in)
                                     |
                               Phase 6 (summary + launchd)
                                     |
                               Phase 7 (polish)
```

- Phase 0 is prerequisite for everything
- Phases 1 through 3 are sequential (each builds on the previous)
- Phase 4 is independent of 1-3, can run in parallel after Phase 0
- Phase 5 needs Phases 1-3 complete
- Phases 6-7 need Phase 5

## Design Decisions

**Constructor injection over trait changes.** Blocks that need registry
take `Arc<Registry>` at construction. The `TaskBlock::execute()` signature
stays unchanged. This avoids touching every block atomically.

**Shared scanner utility.** Stack-specific audit logic lives in
`scanner.rs` so AuditReleaseTag and AuditMainBranch share code, matching
how audit-releases.sh has a single `run_audit()` function.

**Orchestrator above the engine.** Fan-out/fan-in lives in
`MaintenanceOrchestrator`, not inside Engine. Engine stays simple
(process one chain). Orchestrator handles project enumeration, parallel
spawning, and aggregation. Keeps engine testable and block tests unchanged.

**Depth-first within project, parallel across projects.** Each project's
chain runs depth-first in a single async task. Projects run concurrently
via `JoinSet`.

**Dual-sink for RunHoneMaintain.** Sinks on both ProjectIterateCompleted
and ProjectValidationCompleted (when iterate is disabled). Self-filtering
checks trigger event type and project action flags.

## Risks

| Risk | Mitigation |
|------|------------|
| Claude CLI for auto-release is fragile (`--dangerously-skip-permissions`, `CLAUDECODE=` env workaround) | Robust error handling with timeout, capture full output for debugging |
| Git state corruption during tag checkout/restore in AuditReleaseTag | Port three-layer cleanup from audit-releases.sh (checkout --, clean -fd, force checkout) |
| Trait change needed later if blocks need per-invocation context (run_id, parent_event_id) | Accept as future cost — current 9 blocks are manageable to update |
