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
| Command | `*Requested` | Intent — someone or something wants action taken | `IterationRequested`, `MaintenanceRequested`, `ReleaseRequested`, `PipelineCheckRequested` |
| Lifecycle start | `*Started` | A multi-step operation began | `RemediationStarted`, `MaintenanceRunStarted` |
| Lifecycle end | `*Completed` | An operation finished (check payload for success/failure) | `PreflightCompleted`, `GateResolutionCompleted`, `ProjectIterationCompleted` |
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
| `foundry iterate <project>` | AI-assisted quality improvement cycle |
| `foundry scout <project>` | Detect intent drift without changes |
| `foundry validate <project>` | Check quality gate health |
| `foundry run` | Full maintenance across registered projects |
| `foundry gates <project>` | Auto-discover quality gates |
| `foundry pipeline <project>` | Check GitHub Actions pipeline health and auto-remediate failures (CheckPipeline → RemediatePipeline) |
| `foundry release <project> [--bump patch\|minor\|major]` | Agent-driven release workflow (ExecuteRelease → WatchPipeline → InstallLocally) |
| `foundry emit <event>` | Raw event emission for advanced use |

## Payload Conventions

Task blocks in `foundryd` use typed `*Payload` structs from `foundry_core::payload` rather than untyped `serde_json` access.

**Reading a trigger payload:**

```rust
let p = trigger.parse_payload::<PreflightCompletedPayload>()?;
let all_passed = p.all_passed;
```

Use `.ok()` when parsing is best-effort (e.g., a block that sinks on multiple event types):

```rust
let strategic = trigger.parse_payload::<IterationRequestedPayload>().ok().and_then(|p| p.strategic).unwrap_or(false);
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

To cut a release:

```bash
# Update version in Cargo.toml [workspace.package], update CHANGELOG.md
git tag v0.X.0
git push origin main --tags
```

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

- `~/.foundry/registry.json` — project registry (managed via `foundry registry` commands)
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
