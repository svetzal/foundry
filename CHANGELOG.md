# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.15.1] - 2026-05-12

### Fixed

- **Agent session events carried an empty `project`** (`crates/foundryd/src/gateway.rs`,
  `blocks/mod.rs`, `blocks/release.rs`): `ClaudeAgentGateway::invoke` hard-coded
  `project: String::new()` when emitting `AgentSessionStarted`/`AgentSessionEnded`
  events and the started payload, so ops-visualizer's `/agents` view showed every
  session with a blank project. `AgentRequest` (and `ReleaseInput`) now carry a
  `project` field; callers in `invoke_agent` and the release adapters thread the
  registry project name through, and the gateway emits it on both events. No proto /
  gRPC schema change — `WatchResponse.project` already carried the event's `project`.

## [0.15.0] - 2026-05-11

### Added

- **`correctionNeeded` flag in `PlanCompleted`** (`foundry-core`, `foundryd`): the plan
  agent now emits a machine-readable fenced JSON block at the end of its response
  declaring `{ "correctionNeeded": true|false, "reason": "..." }`.  When `false`, a
  clean working tree after `ExecutePlan` is treated as a **legitimate no-op** (success)
  rather than an agent flake (failure → retry).  This mirrors hone's busy-work-containment
  semantics: an agent that correctly concludes "nothing to do" is no longer penalised
  with up to 4 redundant retries.
  - `PlanCompletedPayload` gains two new serde-default fields: `correction_needed: bool`
    (defaults to `true`) and `correction_reason: String`.
  - `build_agent_execution_result` and `build_execution_outcome` in `mod.rs` accept a
    `correction_needed: bool` parameter; the clean-tree override branches on it.
  - `RetryExecution` and `ExecuteMaintain` always pass `true` (correction is required
    when retrying or maintaining).
  - Parse is fail-closed: any output without a valid boolean `correctionNeeded` field
    → `correction_needed = true`.

## [0.14.2] - 2026-05-10

### Fixed

- **Silent no-op guard misfiring on agents that commit their work** (`crates/foundryd/src/blocks/mod.rs`, `execute_plan.rs`, `execute_maintain.rs`, `retry_execution.rs`): `detect_post_execution_changes` previously ran only `git status --porcelain`, which sees uncommitted changes. Agents that commit and push their work (Claude Code's default behaviour) left a clean working tree, so the detector reported `changes_detected: false` and the 0.14.1 silent-no-op guard then incorrectly marked the run as a failure — causing 4× redundant retries each producing real, committed work. Reproduced in production on the 2026-05-10 maintenance run (10/20 projects falsely flagged). Fix: capture HEAD via `git rev-parse HEAD` immediately before agent invocation, then detect changes via `git diff --name-only <pre_sha>` which captures both committed and uncommitted changes since the snapshot. Falls back to porcelain on diff failure or when no pre-sha is available.

## [0.14.1] - 2026-05-09

### Fixed

- **Iterate workflow silent no-op** — closes the failure mode where the iterate agent could exit successfully without applying its plan (302 of 382 traces over the prior 30-day window). Two complementary changes:
  - **Execute prompt strengthened** (`crates/foundryd/src/blocks/execute_plan.rs`): `build_execution_prompt` now uses imperative requirements that explicitly invalidate "gates already pass" as a stopping condition and require the working tree to contain modifications when the plan is complete. Maintain prompt unchanged.
  - **Block-level no-op detection** (`crates/foundryd/src/blocks/mod.rs`, `run_verify_gates.rs`): when the iterate workflow's agent exits cleanly but `changes_detected == false` (or only auxiliary paths like `.claude/worktrees/` were touched), the execution result is overridden to `success: false` with summary `"agent did not modify any files (silent no-op)"`. The downstream chain treats this as a real failure: `RunVerifyGates` short-circuits with a synthetic `agent_execution` gate, which routes through retry up to `--max-retries`, eventually surfacing `ProjectIterationCompleted { success: false }` if retries don't recover. Maintain workflow is explicitly unaffected — `changes_detected: false` remains a successful maintenance run when deps are already current.

## [0.14.0] - 2026-05-09

### Added

- **Agent session visibility v1** — `foundryd` now emits `AgentSessionStarted` and `AgentSessionEnded` events around every Claude agent invocation, capturing session lifecycle in the event stream. New `AgentSessionStarted` / `AgentSessionEnded` payload structs in `foundry-core`, `agent_sessions_dir()` path helper, and `AgentStreamRunner` trait with `ProcessAgentStreamRunner` implementation that runs `claude` with `--print --output-format stream-json` so per-tool-call lifecycle events flow through the engine in real time. Wires `event_tx` and `ProcessAgentStreamRunner` into `ClaudeAgentGateway`.
- **Per-gate timing** — `GateRunResult` now carries `duration_ms`, and `RunVerifyGates` / `RunPreflightGates` populate it for every gate executed. Unblocks "agent thinking vs build/test time" decomposition in event analyses.
- **Working-tree change detection on execution** — `ExecutionCompletedPayload` carries `changes_detected: bool` and `files_changed: Vec<String>`, populated via `git status --porcelain` after agent execution. Surfaces silent no-op cases (agent claims success while leaving the tree clean) directly in events instead of requiring inference.
- **Coverage gate** — foundry's own gate suite now runs `cargo tarpaulin --fail-under 61` as a required gate.
- Unit tests for `greet`, `check_pipeline`, and `watch_pipeline` blocks in `foundryd`.

### Changed

- Internal refactor: extracted shared agent-remediation and gate-result builder helpers across `execute_plan`, `execute_maintain`, and `retry_execution` to remove duplication.

## [0.13.0] - 2026-04-21

### Added

- `WorkflowStatus.trace_id` on the `Status` gRPC response. The root event's `trace_id` is now tracked on `ActiveWorkflow` and surfaced through `Status`, letting dashboards and external tools group the live `Watch` stream events by the workflow they belong to. Additive proto field (tag 8), backward-compatible for existing clients.

## [0.12.0] - 2026-04-20

### Changed

- **Breaking**: `foundryd` now binds to `127.0.0.1:50051` (IPv4 loopback) instead of `[::1]:50051` (IPv6 loopback). The CLI default `--addr` tracks this change (`http://127.0.0.1:50051`). Motivation: several client HTTP/2 stacks (notably Elixir's `:grpc`/Gun adapter) can't pass the `:inet6` transport option needed to dial an IPv6-only listener through their `transport_opts` plumbing, so pure-IPv6 binding blocked off legitimate localhost clients. IPv4 loopback is still localhost-only and works for every stack. No change is needed for users who rely on the CLI default; anyone who passed `--addr http://[::1]:50051` explicitly must update to `http://127.0.0.1:50051`.

## [0.11.2] - 2026-04-20

### Added

- `foundry registry show <project>` now displays an `Installs skill:` line summarizing the `installs_skill` field — `yes (default -- runs <derived-command>)`, `command: <command>`, or `no (explicitly disabled)`. The displayed default command comes from the same derivation function `InstallLocally` uses, so what's shown is always what would actually run.
- `foundry registry list` adds a narrow `Skill` column showing `auto` / `cmd` / `off` / blank per project.

## [0.11.1] - 2026-04-20

### Added

- `foundry init` brought up to the canonical Mojility skill-install spec: new `--force` and `--json` flags, version-guard that refuses to overwrite when the installed skill version is newer than this binary (override with `--force`), version-stamping (`foundry-version: <X>` written into installed file frontmatter), and per-file action reporting (`Created` / `Updated` / `UpToDate` / `Skipped`). Exit code is non-zero when any file is skipped. With this change, `installs_skill: true` can safely be set on foundry's own registry entry.

## [0.11.0] - 2026-04-20

### Added

- `installs_skill` registry field — when set, the `InstallLocally` block automatically runs a per-tool skill installer after the binary install step of a release, so `~/.claude/skills/<name>/SKILL.md` is refreshed without a manual `<tool> init`. Accepts `true` (runs `<binary> init --global --force`) or `{ command: "..." }` for tools whose skill-install verb differs (e.g. `gilt skill-init --global --force`). Absent field preserves current behavior. New `LocalSkillInstallCompleted` event is emitted on success or failure; skill-install failure is a soft warning and does not fail the parent `InstallLocally` block.

## [0.10.0] - 2026-04-10

### Added

- `CleanupBranches` task block — automatically deletes merged local branches and removes stale git worktrees during project validation, preventing accumulation of leftover branches from hopper jobs and Claude Code agent sessions

## [0.8.0] - 2026-03-29

### Added

- `foundry init` command — installs the bundled Foundry skill for Claude agents
- `foundry init --global` — installs to `~/.claude/skills/foundry/` instead of local `.claude/skills/foundry/`
- Skill files embedded in the binary via `include_str!()`, updated on every release
- Event naming convention documentation in AGENTS.md

### Changed

- `AutoReleaseTriggered` renamed to `ReleaseRequested` (commands use `*Requested` suffix)
- `AutoReleaseCompleted` renamed to `ReleaseCompleted`
- `GatesResolved` renamed to `GateResolutionCompleted` (lifecycle endpoints use `*Completed` suffix)
- `ProjectIterateCompleted` renamed to `ProjectIterationCompleted` (noun form for compound prefixes)
- `ProjectMaintainCompleted` renamed to `ProjectMaintenanceCompleted` (noun form for compound prefixes)
- `CharterCheckCompleted` payload field `passed` renamed to `success` for consistency

## [0.3.0] - 2026-03-26

### Added

- Open-source release under MIT license (svetzal/foundry)
- GitHub Pages documentation site via mdBook
- Homebrew tap distribution (`brew tap svetzal/tap && brew install foundry`)
- Summary module for automated maintenance reports
- Orchestrator for automated maintenance workflows
- Exit condition for `foundry watch` stream

### Changed

- Repository transferred from Mojility org to svetzal personal account
- Registry action flags forwarded in validation payload
- CI pipeline now installs `protoc` for proto compilation

## [0.2.0] - 2026-03-22

### Added

- Async emit: `Emit` RPC now returns immediately, processing runs in the background
- `--wait` flag on `foundry emit` to block until processing completes and display the trace
- Workflow status tracking: `foundry status` shows active in-flight workflows
- `WorkflowTracker` module with RAII `WorkflowGuard` for robust cleanup
- `ShellGateway` trait for I/O abstraction in task blocks (functional core / imperative shell)
- `FakeShellGateway` for deterministic, fast unit tests
- Configurable per-project `timeout_secs` in the registry (defaults to 30 minutes)
- Project charter (`CHARTER.md`)

### Changed

- All task blocks refactored to use `ShellGateway` dependency injection instead of hard-wired shell calls
- Workspace lint configuration deduplicated into root `Cargo.toml`
- Block tests now use fakes instead of real shell commands

## [0.1.0] - 2026-03-15

### Added

- Event-driven workflow engine with queue-based event propagation
- Three-crate workspace: `foundry-core`, `foundryd`, `foundry-cli`
- gRPC service with `Emit`, `Status`, `Watch`, and `Trace` RPCs
- Task block library: `ValidateProject`, `ComposeGreeting`, `DeliverGreeting`,
  `ScanDependencies`, `AuditReleaseTag`, `AuditMainBranch`, `RemediateVulnerability`,
  `CommitAndPush`, `CutRelease`, `WatchPipeline`, `InstallLocally`,
  `RouteProjectWorkflow`, `RunHoneIterate`, `RunHoneMaintain`
- Throttle control: `full`, `audit_only`, `dry_run`
- Project registry (`~/.foundry/registry.json`) with per-project configuration
- JSONL event writer for persistent event logging
- Trace store with 1-hour TTL for completed event chains
- `foundry run` command for triggering maintenance workflows with live streaming
- Maintenance orchestrator with per-project concurrency guards
- Configurable retry policies per task block
- Stack-specific audit scanner (Rust, Python, TypeScript, Elixir)
- mdBook documentation
- launchd plist files for daemon and scheduled runs
