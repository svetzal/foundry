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
./install.sh
```

Or individually:

```bash
cargo install --path crates/foundryd
cargo install --path crates/foundry-cli
```

On macOS, `install.sh` re-signs both binaries with stable ad-hoc identifiers
(`com.mojility.foundryd`, `com.mojility.foundry`) after the cargo install
step. This is required because cargo's default ad-hoc signature uses a
hash-derived identifier that changes on every rebuild, causing macOS TCC to
treat each rebuild as a brand-new app and re-prompt for Full Disk Access,
Documents, Desktop, OneDrive, etc. With a stable identifier, TCC grants
survive future rebuilds. **Always use `./install.sh` rather than running
`cargo install` directly on macOS** — invoking cargo bare will reset the
identifier and re-trigger morning Allow dialogs.

After the first install on a new machine, grant Full Disk Access to the
foundryd binary path in System Settings → Privacy & Security → Full Disk
Access. The path depends on install method:

- `~/.cargo/bin/foundryd` for `./install.sh` installs
- `/opt/homebrew/bin/foundryd` (Apple Silicon) or `/usr/local/bin/foundryd`
  (Intel) for `brew install foundry` installs

This is a one-time grant per install path that persists across rebuilds
because the identifier is stable. The CI release workflow (`release.yml`)
applies the same re-sign step before tarballing, so Homebrew-distributed
binaries inherit the stable identifier as well.

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

## Event Naming Conventions

All event types follow a disciplined taxonomy with four suffix categories:

| Category | Suffix | Meaning | Examples |
|----------|--------|---------|----------|
| Command | `*Requested` | Intent — someone or something wants action taken | `ProjectIterationRequested`, `ProjectMaintenanceRequested`, `ReleaseRequested`, `PipelineCheckRequested`, `MaintenanceSummaryRequested` |
| Lifecycle start | `*Started` | A multi-step operation began | `MaintenanceCycleStarted`, `ProjectRunStarted`, `StrategicCycleStarted`, `InnerIterationStarted`, `RemediationStarted` |
| Lifecycle end | `*Completed` | An operation finished (check payload for success/failure) | `MaintenanceCycleCompleted`, `ProjectRunCompleted`, `ProjectIterationCompleted`, `PreflightCompleted`, `GateResolutionCompleted` |
| Domain fact | Specific past participle | A meaningful domain event where the verb adds clarity over `*Completed` | `VulnerabilityDetected`, `MainBranchAudited`, `ProjectChangesPushed`, `PipelineChecked` |

Rules:

- **Commands are always `*Requested`** — never `*Triggered` or other verbs for intent events.
- **`*Completed` is the default** for lifecycle endpoints. Use a specific past participle only when it adds domain meaning (e.g., `VulnerabilityDetected` says more than `ScanCompleted`).
- **`*Started`/`*Completed` must pair** — if you add a `*Started`, there must be a corresponding `*Completed`.
- **Noun form for compound prefixes** — use `ProjectIterationCompleted` (noun), not `ProjectIterateCompleted` (verb).
- **Payload boolean results use `success`** — not `passed`, `ok`, or other variants. The one exception is the `passed` field on individual gate results (where "passed" is domain-specific to gates).

## CLI Commands

| Command | Purpose |
|---------|---------|
| `foundry iterate <project>` | AI-assisted quality improvement cycle (legitimate no-op is a success when plan agent sets `correctionNeeded: false`) |
| `foundry scout <project>` | Detect intent drift without changes |
| `foundry validate <project>` | Check quality gate health |
| `foundry run` | Full maintenance across registered projects |
| `foundry gates <project>` | Auto-discover quality gates |
| `foundry pipeline <project>` | Check GitHub Actions pipeline health and auto-remediate failures (CheckPipeline → RemediatePipeline) |
| `foundry release <project> [--bump patch\|minor\|major]` | Agent-driven release workflow (ExecuteRelease → WatchPipeline → InstallLocally) |
| `foundry emit <event>` | Raw event emission for advanced use |

### Registry commands

Registry **mutations** (`add`, `edit`, `remove`) now go through `foundryd` via gRPC so the daemon's in-memory registry stays consistent with the file on disk.  Pass `--offline` to bypass the daemon and write the file directly (useful when bootstrapping before `foundryd` starts).

| Command | Daemon required? | Notes |
|---------|-----------------|-------|
| `foundry registry init` | No | Creates an empty `~/.foundry/registry.json` |
| `foundry registry list` | No | Reads the file directly |
| `foundry registry show <name>` | No | Reads the file directly |
| `foundry registry add …` | Yes (or `--offline`) | Adds via gRPC; falls back with a warning when daemon is unreachable |
| `foundry registry edit <name> …` | Yes (or `--offline`) | Edits via gRPC; falls back with a warning when daemon is unreachable |
| `foundry registry remove <name>` | Yes (or `--offline`) | Removes via gRPC; falls back with a warning when daemon is unreachable |

The gRPC RPCs added for registry mutations are `RegistryAdd`, `RegistryRemove`, and `RegistryEdit` (see `proto/foundry.proto`).

> **Note for scripts/automation**: If you run `foundry registry add/edit/remove` without `--offline` and `foundryd` is not listening, the command will warn and fall back to direct file editing.  A running daemon will not see that change until it is restarted.  Start `foundryd` before running mutations, or use `--offline` deliberately and restart the daemon afterward.

## Payload Conventions

Task blocks in `foundryd` use typed `*Payload` structs from `foundry_core::payload` rather than untyped `serde_json` access.

**Reading a trigger payload:**

```rust
let p = trigger.parse_payload::<PreflightCompletedPayload>()?;
let all_passed = p.all_passed;
```

Use `.ok()` when parsing is best-effort (e.g., a block that sinks on multiple event types):

```rust
let strategic = trigger.parse_payload::<ProjectIterationRequestedPayload>().ok().and_then(|p| p.strategic).unwrap_or(false);
```

**Writing an output payload:**

```rust
let event_payload = Event::serialize_payload(&MyPayload { ... })?;
Event::new(EventType::SomethingCompleted, project, throttle, event_payload)
```

Or use the convenience method when deriving from the trigger event:

```rust
trigger.with_payload(EventType::SomethingCompleted, &MyPayload { ... })?
```

**Rules:**

- Do NOT invent new `*Payload` structs without a clear typed consumer — if you only need one or two fields, use direct `.get().and_then().unwrap_or()` Value access.
- Do NOT use `PayloadExt` (`.str_or`, `.bool_or`, etc.) or `Event::payload_str_or` etc. in task block production code. Those helpers are reserved for `foundry-cli` display logic.
- Wire format must remain byte-for-byte identical — typed structs serialize to the same JSON shape as the untyped `json!({})` they replace.
- `dry_run_events` serialization failures must use `.expect("... is infallibly serializable")`, not `.unwrap_or_else(|_| json!({}))`.

## Tracing

Foundry uses OpenTelemetry-shaped nested spans. Every event carries `trace_id` (32-char hex), `span_id` (16-char hex), and `parent_span_id` (16-char hex). It also carries `causation_id` — the `id` of the event that triggered the block which emitted it — recording the domain causality edge independent of span structure (`None` for root events), and `gather_id` — the fan-out (scatter/gather) group the event belongs to (`None` outside any fan-out). The engine stamps span fields per two rules (default + span-opener registry), stamps `causation_id` unconditionally, and propagates `gather_id` verbatim like `trace_id`; all stamping is "set if unset". Subprocesses inherit `TRACEPARENT`.

See `book/src/architecture/tracing.md` for the full model. When adding a new workflow `*Requested` event, register it as a span opener in `foundry_core::event::EventType::is_span_opener`.

## Key Conventions

- Edition 2024, Rust 1.85+, `unsafe_code` is denied
- Clippy pedantic warnings enabled with selective exceptions (see any crate's `Cargo.toml`)
- gRPC via tonic/prost, proto definition in `proto/foundry.proto`
- Both `foundryd` and `foundry-cli` compile the proto in their `build.rs`
- Structured logging via `tracing` with `info_span!` for request correlation
- No external observability dependencies — tracing spans only
- All tasks must include tests and all relevant documentation updates

## Branching Workflow

This project follows trunk-based development. `main` is the only long-lived branch. All work lands on `main` via direct commit. Feature branches are not pushed to `origin` and pull requests are not used. Short-lived local working branches (e.g. from hopper worktrees) are merged to `main` and deleted locally before work is considered complete.

## CI / Release

- **CI** runs on push/PR to `main`: fmt, clippy, test (`.github/workflows/ci.yml`)
- **Release** runs on tag push (`v*`): builds macOS arm64, macOS x86_64, and Linux x86_64 binaries, creates a GitHub release with tarballs and checksums (`.github/workflows/release.yml`)

**Do NOT use `foundry release foundry`.** The foundry release-chain workflow hangs silently when applied to foundry itself, because `foundryd` cannot replace the running `foundryd` binary mid-release. All other registered projects release via `foundry release <project>` normally; foundry itself must be released manually.

To cut a release:

```bash
# 1. Update version in Cargo.toml [workspace.package] and skill/foundry/SKILL.md
#    metadata.version (must match Cargo.toml — see "Deployable Skill" below).
# 2. Update CHANGELOG.md — move [Unreleased] content to a new dated [vX.Y.Z] section.
# 3. cargo build to refresh Cargo.lock.
# 4. Commit the bump, then:
git tag v0.X.Y
git push origin main --tags

# 5. Wait for the Release workflow to publish tarballs to the GitHub release page.
# 6. Swap the running daemon onto the new binary:
foundry registry show foundry --json | jq -r '.install_command'   # locate install.command
# run the install.command, then reload the daemon so it picks up the new binary:
launchctl unload ~/Library/LaunchAgents/com.mojility.foundryd.plist
launchctl load   ~/Library/LaunchAgents/com.mojility.foundryd.plist
```

Steps 5–6 are required — without them the daemon keeps serving the old binary even after the GitHub release publishes, so the fix doesn't take effect. See `launchd/README.md` for the canonical load/unload commands.

The repo is public under `svetzal/foundry`. Homebrew distribution via `svetzal/homebrew-tap` — the release workflow auto-updates the formula.

Install via Homebrew:

```bash
brew tap svetzal/tap
brew install foundry
```

## Documentation

mdBook documentation lives in `book/`. Build with:

```bash
mdbook build book/
```

## Deployable Skill

The `skill/foundry/` directory contains the Claude Code skill that teaches agents how to use Foundry. It is deployed via `foundry init`:

- `foundry init` — installs to project-local `.claude/skills/foundry/`
- `foundry init --global` — installs to `~/.claude/skills/foundry/` (available across all projects)

When adding new CLI commands or workflows, update the in-repo skill files (`SKILL.md`, `references/workflows.md`, `references/event-model.md`) to match, then re-run `foundry init --global` to deploy.

The skill version in `skill/foundry/SKILL.md` (metadata `version` field) must always match the workspace version in `Cargo.toml`. When bumping the version for a release, update both locations.

## Key Directories

- `~/.foundry/registry.json` — project registry; mutations go through `foundryd` gRPC so the daemon's in-memory state stays consistent (use `--offline` to write the file directly when the daemon is not running)
- `~/.foundry/traces/YYYY-MM-DD/` — persistent trace files (survive daemon restarts)
- `~/.foundry/audits/{project}/` — centralized audit logs
- `~/.foundry/events/YYYY-MM.jsonl` — event persistence (configurable via `FOUNDRY_EVENTS_DIR`)

## Future Direction: Agent Efficacy Retrospectives

Foundry already captures rich event data about agent activity — iterations, maintenance runs, gate results, failures, retries. The next step is automated retrospectives on agent efficacy: analyzing patterns across runs to surface what's working, what's failing persistently, and where agent time is being wasted. This could feed back into the MBOS event stream as `ai_learning_detected` events, closing the loop between automated work and operational awareness. See the archived `Skills/_archived/LearningReview/` in Operations for the original concept.

## Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `FOUNDRY_REGISTRY_PATH` | `~/.foundry/registry.json` | Project registry file |
| `FOUNDRY_EVENTS_DIR` | `~/.foundry/events` | JSONL event output directory |
| `FOUNDRY_TRACES_DIR` | `~/.foundry/traces` | Persistent trace storage |
| `FOUNDRY_AUDITS_DIR` | `~/.foundry/audits` | Centralized audit logs |
