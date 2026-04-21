# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

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
