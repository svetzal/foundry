# Foundry — Agent Guidance

## Project Overview

Foundry is an event-driven workflow engine for engineering automation. It consists of a Rust workspace with five crates:

- **foundry-sdk** — Stable SDK contract (Event, TaskBlock trait, Throttle, payloads)
- **foundry-engine** — Core event processing engine
- **foundry-blocks** — Task block implementations
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

### Linux daemon setup

For Linux infrastructure, install the release tarball binaries to
`/usr/local/bin` and run `foundryd` as the operating user via a user-level
`systemd` service. Do not run `foundryd` as root; it needs the same GitHub,
agent CLI, repo checkout, and `~/.foundry` state as the user who owns the
workspaces.

See `systemd/README.md` and `systemd/foundryd.service`.

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
| Lifecycle end | `*Completed` | An operation finished (check payload for success/failure) | `MaintenanceCycleCompleted`, `ProjectRunCompleted`, `ProjectIterationCompleted`, `PreflightCompleted`, `GateResolutionCompleted`, `MaintenanceTriageCompleted` |
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
| `foundry task <project> "<description>" [--agent <provider>]` | Run one isolated, evidence-reviewed coding task and return a typed verdict |
| `foundry campaign add\|list\|show\|advance\|pause\|resume\|decide\|complete` | Manage durable objectives that derive one task at a time from live state or close on owner-verified evidence |
| `foundry scout <project>` | Detect intent drift without changes |
| `foundry validate <project>` | Check quality gate health |
| `foundry run` | Full maintenance across registered projects (the nightly schedule is now driven by the `nightly-maintenance` sentinel inside `foundryd`) |
| `foundry gates <project>` | Auto-discover quality gates |
| `foundry pipeline <project>` | Check GitHub Actions pipeline health and auto-remediate failures (CheckPipeline → RemediatePipeline) |
| `foundry release <project> [--bump patch\|minor\|major]` | Agent-driven release workflow (ExecuteRelease → WatchPipeline → InstallLocally) |
| `foundry emit <event>` | Raw event emission for advanced use |

### Registry commands

Registry commands are daemon-authoritative in normal online use. `list`, `show`, `add`, `edit`, and `remove` go through `foundryd` via typed gRPC so reads and writes all observe the daemon-owned registry state. Pass `--offline` only for explicit recovery when the daemon is stopped and you intentionally need direct file access.

| Command | Daemon required? | Notes |
|---------|-----------------|-------|
| `foundry registry init` | No | Offline-only recovery command; requires `--offline`, creates an empty `~/.foundry/registry.json`, and never contacts `foundryd` |
| `foundry registry list` | Yes (or `--offline`) | Reads daemon-owned registry state via `RegistryList`; `--offline` reads the file directly |
| `foundry registry show <name>` | Yes (or `--offline`) | Reads daemon-owned registry state via `RegistryShow`; `--offline` reads the file directly |
| `foundry registry add …` | Yes (or `--offline`) | Adds via `RegistryAdd`; unreachable daemon is an error unless `--offline` is set |
| `foundry registry edit <name> …` | Yes (or `--offline`) | Edits via `RegistryEdit`; unreachable daemon is an error unless `--offline` is set |
| `foundry registry remove <name>` | Yes (or `--offline`) | Removes via `RegistryRemove`; unreachable daemon is an error unless `--offline` is set |

The gRPC RPCs are `RegistryList`, `RegistryShow`, `RegistryAdd`, `RegistryRemove`, and `RegistryEdit` (see `proto/foundry.proto`).

> **Note for scripts/automation**: `foundry registry init` is offline-only recovery and rejects runs without `--offline` before any daemon or filesystem mutation. Online `foundry registry list/show/add/edit/remove` commands do not silently fall back and surface stable typed gRPC status errors instead. `list` and `show` render the daemon response directly, never read the client-side registry file, and if `FOUNDRY_REGISTRY_PATH` is absent the online path leaves it absent. If `foundryd` is not listening, all five commands fail and leave any existing client-side registry file untouched byte-for-byte. Use `--offline` deliberately when you want direct file recovery semantics.
> Online `add`/`edit`/`remove` are also persistence-atomic: if the daemon cannot save the registry, the RPC returns `INTERNAL`, reports a stable `failed to persist registry state` message, and leaves the daemon-owned registry unchanged in memory and on disk.

### Sentinel commands

Sentinels are declarative, named, scheduled triggers that live inside `foundryd` and emit a configured event when their schedule fires. Three canonical sentinels ship in the default seed (see the Sentinel commands section below for full details).

The daemon auto-seeds `~/.foundry/sentinels.json` on first start and **additively merges** missing canonical seed entries on every restart, so new Foundry releases that add canonical sentinels reach existing installs automatically without manual JSON edits. User toggles, hand-edited cron, and user-added entries on the file are never overwritten. See `book/src/guide/sentinels.md` for the full model.

Three canonical sentinels ship in the default seed:

- `nightly-maintenance` (02:00 local) — emits `MaintenanceCycleStarted` for project `system`. Drives the maintenance run.
- `daily-commit-digest` (17:00 local) — emits `CommitDigestStarted` for project `system`. Drives the commit digest formation; output lands at `{FOUNDRY_DIGESTS_DIR}/{YYYY-MM-DD}.md`. See `book/src/guide/commit-digest.md`.
- `ops-digest` (every 3 hours, `0 */3 * * *`) — emits `OpsDigestStarted` for project `system`. Reads MBOS JSONL events, applies a pressure gate (≥25 new events or any anomaly), summarises via agent, and writes `{FOUNDRY_OPS_DIGESTS_DIR}/{YYYY-MM-DD}.md`. See `book/src/guide/ops-digest.md`.
- `nightly-supply-chain` (06:00 local, `0 6 * * *`) — emits `SupplyChainScanStarted` for project `system`. Scans every active project's working-tree lockfile for dependency advisories, classifies each against that repo's committed `.supply-chain-allow.json`, triages live findings by fix availability (auto-fixable vs policy-call), and writes a deterministic digest to `{FOUNDRY_SUPPLY_CHAIN_DIR}/{YYYY-MM-DD}.md`. Chain: `SupplyChainScanStarted → ScanSupplyChain → SupplyChainScanned → RemediateSupplyChain → SupplyChainRemediated → WriteSupplyChainDigest → SupplyChainScanCompleted`. Advisory — never fails a run. `RemediateSupplyChain` always classifies; its auto-fix engine (verified, commit-only Rust in-range bumps with gate-verify-and-rollback) is gated dark behind `FOUNDRY_SUPPLY_CHAIN_REMEDIATE` and inert by default. See `book/src/guide/supply-chain.md`.

| Command | Daemon required? | Notes |
|---------|-----------------|-------|
| `foundry sentinel list` | No | Reads the file directly |
| `foundry sentinel show <name>` | No | Reads the file directly |
| `foundry sentinel enable <name>` | Yes (or `--offline`) | Enables via gRPC; falls back with a warning when daemon is unreachable |
| `foundry sentinel disable <name>` | Yes (or `--offline`) | Disables via gRPC; falls back with a warning when daemon is unreachable |

The gRPC RPCs are `SentinelEnable` and `SentinelDisable`. Both wake the daemon's in-process scheduler via a `Notify` so the next firing is recomputed immediately. Adding non-canonical (machine-local) sentinels still means hand-editing `~/.foundry/sentinels.json` and restarting `foundryd` — `foundry sentinel add | remove | edit` is deferred to a later slice.

## Payload Conventions

Task blocks in `foundryd` use typed `*Payload` structs from `foundry_sdk::payload` rather than untyped `serde_json` access.

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

See `book/src/architecture/tracing.md` for the full model. `EventType::is_span_opener` is an exhaustive `match` with no wildcard arm — adding a new `EventType` variant will not compile until it is classified (opener or non-opener) in that function. The compiler prompts the decision; no separate manual step is required.

## Key Conventions

- Edition 2024, Rust 1.85+, `unsafe_code` is denied
- Clippy pedantic warnings enabled with selective exceptions (see any crate's `Cargo.toml`)
- gRPC via tonic/prost, proto definition in `proto/foundry.proto`
- Both `foundryd` and `foundry-cli` compile the proto in their `build.rs`
- Structured logging via `tracing` with `info_span!` for request correlation
- No external observability dependencies — tracing spans only
- All tasks must include tests and all relevant documentation updates

## Dispatch / Routing

The engine is the single routing authority. Blocks are matched by two sequential filters before `execute()` is called:

1. **`sinks_on()`** — coarse event-type filter (which `EventType` values this block handles)
2. **`accepts(&self, trigger: &Event) -> bool`** — fine-grained payload-level predicate

### accepts() convention

Move a guard into `accepts()` when it says **"this event isn't for me"** based on payload content:

- Payload flag or field that determines whether the block should run (e.g. `strategic: true`, `all_passed: true`, `vulnerable: true`)
- Workflow-type routing (e.g. "only handle `Validate` workflow preflight events")
- Result-state routing (e.g. "only handle passing pipeline checks" → `decides_passing`)

`accepts()` should **always** handle payload parse errors by returning `false` (unknown events are rejected, not panicked on).

The default implementation returns `true`, so blocks with no payload filter do not need to override it.

### Leave in execute()

Two distinct kinds of guard belong in `execute()` rather than `accepts()`. Use the appropriate comment to distinguish them:

**`// Domain skip:`** — the guard emits a meaningful domain event that downstream blocks consume. Must stay in `execute()` because `accepts()` cannot emit events. Examples:

- `OpsDigestCompleted { skipped: true }` — tells the write block to short-circuit
- `ProjectValidationCompleted { status: "skipped" }` — domain fact that validation was skipped
- `PreflightCompleted { skipped: true }` — maintenance-workflow preflight bypass; downstream plan block reads this
- `ProjectRunCompleted { success: false }` — cycle-gather terminal emitted when validation fails

**`// Defensive:`** — the guard is unreachable in production because an `accepts()` predicate already filters the event, but is kept as a safety net. Does **not** emit a domain event. Example:

```rust
let CommitDecision::Proceed { cve } = decide_commit(trigger) else {
    // Defensive: accepts() filters SkipNestedLoop and SkipNoChanges before dispatch.
    return skip!("Skipped: no commit needed");
};
```

A future code audit must not re-flag `// Defensive:` guards as missing `// Domain skip:` comments — they are intentionally different.

The "project not in registry" case (via `require_project!`) returns `TaskBlockResult::project_not_found` — a block-level failure, not a domain event — so it also stays in `execute()` but carries neither comment. See the `require_project!` macro doc for the rationale.

The test naming convention reflects this split:

- `accepts_returns_false_when_*` / `accepts_returns_true_when_*` — sync unit tests for the `accepts()` predicate
- `dry_run_and_accepts_agree_on_skip_for_*` — verifies that `dry_run_events` and `accepts()` agree, and also verifies the `simulate()` mirror (replaces old `dry_run_and_execute_agree_on_skip_for_*`)
- Domain-skip tests remain async and test `execute()` behavior directly

### Dry-run simulation convention

Mutator blocks implement `SimulatedSuccess` (from `foundry-blocks::blocks::dry_run`) and use `dry_run_via_simulation!()` to generate `dry_run_events`. Hand-written `dry_run_events` overrides are prohibited.

- `simulate(&self, trigger)` — produces a synthetic success outcome from the trigger (no I/O). Returns `Option<T>` where `None` means skip (same condition as `accepts()`).
- `success_events(&self, trigger, outcome)` — SINGLE source of truth for event construction; called by the generated `dry_run_events`.
- Skip conditions live in `accepts()` (routing) and are mirrored in `simulate()`'s `Option<T>` return; the `dry_run_and_accepts_agree_on_skip_for_*` tests are regression guards over this structural invariant.

This is now universal — every Mutator block in the workspace composes via `TaskBlock` + `SimulatedSuccess` + `dry_run_via_simulation!()`. There is no second composition mechanism.

## Branching Workflow

This project follows trunk-based development. `main` is the only long-lived branch. All work lands on `main` via direct commit. Feature branches are not pushed to `origin` and pull requests are not used. Short-lived local working branches (e.g. from hopper worktrees) are merged to `main` and deleted locally before work is considered complete.

## CI / Release

- **CI** runs on push/PR to `main`: fmt, clippy, test (`.github/workflows/ci.yml`)
- **Release** runs on tag push (`v*`): builds macOS arm64, macOS x86_64, and Linux x86_64 binaries, creates a GitHub release with tarballs and checksums (`.github/workflows/release.yml`)

**Do NOT use `foundry release foundry`.** The foundry release-chain workflow hangs silently when applied to foundry itself, because `foundryd` cannot replace the running `foundryd` binary mid-release. All other registered projects release via `foundry release <project>` normally; foundry itself must be released manually.

To cut a release:

```bash
# 1. Update version in Cargo.toml [workspace.package] and (as convention) skill/foundry/SKILL.md
#    metadata.version (see "Deployable Skill" below — runtime no longer stamps this, but keep
#    it in sync for human readers).
# 2. Update CHANGELOG.md — move [Unreleased] content to a new dated [vX.Y.Z] section.
# 3. cargo build to refresh Cargo.lock.
# 4. Commit the bump, then:
git tag v0.X.Y
git push origin main --tags

# 5. Wait for the Release workflow to publish tarballs to the GitHub release page.
# 6. Swap the running daemon onto the new binary. Use ./install.sh — it does
#    `cargo install` for both binary crates AND re-signs them on macOS with
#    stable code-signing identifiers. Do NOT run a bare `cargo install`:
#    cargo's ad-hoc signature is hash-derived and changes every rebuild, so
#    macOS TCC sees a brand-new app and re-prompts (or silently denies) the
#    daemon's privacy grants. (`foundry`'s registry install_command is null —
#    install.sh is the canonical installer.)
./install.sh
# then reload the daemon so the running process picks up the new, stable-signed binary:
launchctl unload ~/Library/LaunchAgents/com.mojility.foundryd.plist
launchctl load   ~/Library/LaunchAgents/com.mojility.foundryd.plist
```

Steps 5–6 are required — without them the daemon keeps serving the old binary even after the GitHub release publishes, so the fix doesn't take effect. The reload must come *after* `install.sh`, so the live process inherits the stable signature rather than the one it launched with. See `launchd/README.md` for the canonical load/unload commands.

The repo is public under `svetzal/foundry`. Homebrew distribution via `svetzal/homebrew-tap` — the release workflow auto-updates the formula.

Install via Homebrew:

```bash
brew tap svetzal/tap
brew install foundry
```

## CLI Rendering Convention

`foundry-cli` follows a **functional core, imperative shell** pattern for all display output.

- **`crates/foundry-cli/src/render/`** is the pure functional core.  Every function takes data (proto types, SDK types, parsed payloads) and returns a `String`.  No `println!`, no I/O, no gRPC, no filesystem.  The `#![deny(clippy::print_stdout, clippy::print_stderr)]` lint in `render/mod.rs` enforces this.
- **Command modules** (`event_commands.rs`, `workflow_commands.rs`, etc.) are the imperative shell: they fetch data, call render functions, and call `print!` or `println!` exactly once per logical output.
- The exemplar is `render/trace_tree.rs` (moved from `trace_tree.rs`): `build_forest()` + `render(&[SpanNode], out: &mut String)` — pure, zero `println!`, with tests.

**Submodules:**

| Module | Renders |
|--------|---------|
| `render::workflow` | Watch event lines, scout results, validation results |
| `render::event` | Flat traces, history tables, live watch lines, workflow status |
| `render::registry` | Project detail and table views |
| `render::sentinel` | Sentinel detail and table views |
| `render::trace_tree` | OTel-shaped span tree (moved from top-level) |

**Adding new display output:** write a pure function in the appropriate render submodule, test it with unit tests, then call it from the command module with a single `print!("{}", render::...)`.

## Documentation

mdBook documentation lives in `book/`. Build with:

```bash
mdbook build book/
```

## Deployable Skill

The `skill/foundry/` directory contains the Claude Code skill that teaches agents how to use Foundry. It is deployed via `foundry init`:

- `foundry init` — installs to `~/.claude/skills/foundry/` (globally, the default)
- `foundry init --local` — installs to `.claude/skills/foundry/` (project scope)
- `foundry init --global` — accepted no-op alias; global is the default (load-bearing: foundry's registry derives `{binary} init --global --force` as the skill-install command)
- `foundry init --remove` — uninstalls the skill and cleans the lock entry in `~/.config/context-mixer/`

Installation is managed by [cmx-core](https://github.com/svetzal/context-mixer2/tree/main/cmx-core). Installed files are byte-identical to the bundled content; the installed version is tracked via a lockfile under `~/.config/context-mixer/cmx-lock.json` rather than stamped into the file content.

When adding new CLI commands or workflows, update the in-repo skill files (`SKILL.md`, `references/workflows.md`, `references/event-model.md`) to match, then re-run `foundry init` to deploy globally.

The `metadata.version` field in `skill/foundry/SKILL.md` should be kept in sync with the workspace version in `Cargo.toml` as an authoring convention for human readers, but the runtime no longer stamps or reads it — the cmx-core lockfile is the sole source of truth for the installed version.

## Key Directories

- `~/.foundry/registry.json` — project registry; online `list/show/add/edit/remove` route through `foundryd` gRPC so both reads and mutations use daemon-owned state. Use `--offline` only for direct file recovery while the daemon is not running.
- `~/.foundry/campaigns.json` — durable campaign definitions and cycle state; written atomically by the CLI and campaign formation
- `~/.foundry/worktrees/` — disposable isolated worktrees used by one-shot task executions
- `~/.foundry/preserved/` — fallback Git bundles when a non-complete task branch cannot be pushed to its remote
- `~/.foundry/sentinels.json` — sentinel store; auto-seeded by the daemon on first start with the canonical entries (`nightly-maintenance`, `daily-commit-digest`, `ops-digest`) and additively merged with the canonical seed on every restart. Mutations (`enable`/`disable`) go through `foundryd` gRPC so the in-memory scheduler is kept in sync (use `--offline` to write the file directly when the daemon is not running)
- `~/.foundry/traces/YYYY-MM-DD/` — persistent trace files (survive daemon restarts)
- `~/.foundry/audits/{project}/` — centralized audit logs
- `~/.foundry/digests/YYYY-MM-DD.md` — daily commit digest output, one file per day. Override the parent dir via `FOUNDRY_DIGESTS_DIR`; Stacey's setup points it at `~/Work/Operations/Automation/commit-digests` via the launchd plist.
- `~/.foundry/ops-digests/YYYY-MM-DD.md` — ops digest output (periodic summary of MBOS events), one file per day. Override the parent dir via `FOUNDRY_OPS_DIGESTS_DIR`.
- `~/.foundry/ops-digest.watermark` — ISO 8601 timestamp of the newest MBOS event included in the last successfully written ops digest. Advances atomically after each write so subsequent runs only process newer events.
- `~/.foundry/triage/YYYY-MM-DD.md` — post-maintenance failure triage digest, one file per maintenance run. Override the parent dir via `FOUNDRY_TRIAGE_DIR`. See `book/src/guide/maintenance-triage.md`.
- `~/.foundry/supply-chain/YYYY-MM-DD.md` — nightly supply-chain advisory scan digest, one file per scan. Override the parent dir via `FOUNDRY_SUPPLY_CHAIN_DIR`. See `book/src/guide/supply-chain.md`. The per-repo allowlist `.supply-chain-allow.json` is a neutral artifact Foundry reads (never writes).
- `~/.foundry/events/YYYY-MM.jsonl` — event persistence (configurable via `FOUNDRY_EVENTS_DIR`)

## Future Direction: Agent Efficacy Retrospectives

Foundry already captures rich event data about agent activity — iterations, maintenance runs, gate results, failures, retries. The next step is automated retrospectives on agent efficacy: analyzing patterns across runs to surface what's working, what's failing persistently, and where agent time is being wasted. This could feed back into the MBOS event stream as `ai_learning_detected` events, closing the loop between automated work and operational awareness. See the archived `Skills/_archived/LearningReview/` in Operations for the original concept.

## Environment Variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `FOUNDRY_REGISTRY_PATH` | `~/.foundry/registry.json` | Project registry file |
| `FOUNDRY_CAMPAIGNS_PATH` | `~/.foundry/campaigns.json` | Durable campaign store |
| `FOUNDRY_WORKTREES_DIR` | `~/.foundry/worktrees` | Isolated task worktrees |
| `FOUNDRY_PRESERVED_DIR` | `~/.foundry/preserved` | Fallback preserved-work bundles |
| `FOUNDRY_SENTINELS_PATH` | `~/.foundry/sentinels.json` | Sentinel store file |
| `FOUNDRY_EVENTS_DIR` | `~/.foundry/events` | JSONL event output directory |
| `FOUNDRY_TRACES_DIR` | `~/.foundry/traces` | Persistent trace storage |
| `FOUNDRY_AUDITS_DIR` | `~/.foundry/audits` | Centralized audit logs |
| `FOUNDRY_DIGESTS_DIR` | `~/.foundry/digests` | Daily commit-digest output directory |
| `FOUNDRY_OPS_DIGESTS_DIR` | `~/.foundry/ops-digests` | Ops-digest output directory |
| `FOUNDRY_OPS_EVENTS_DIR` | `~/Work/Operations/Events/intake` | MBOS JSONL intake directory |
| `FOUNDRY_TRIAGE_DIR` | `~/.foundry/triage` | Post-maintenance triage digest output directory |
| `FOUNDRY_SUPPLY_CHAIN_DIR` | `~/.foundry/supply-chain` | Nightly supply-chain advisory digest output directory |
| `FOUNDRY_SUPPLY_CHAIN_REMEDIATE` | *(unset → off)* | Truthy (`1`/`true`/`yes`/`on`) enables the supply-chain auto-fix engine (verified, commit-only Rust in-range bumps). Off by default — the formation only classifies until this is set. |
