# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/),
and this project adheres to [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

- `foundry init` now installs on cmx-core 0.2, which reconciles the skill's
  `metadata.version` to the foundry binary version at install time. foundry's
  `SKILL.md` previously carried no version, so `cmx doctor` saw the installed
  skill as unversioned; it now declares `metadata.version` (e.g. `"0.27.0"`)
  automatically, with no in-tool stamping.

### Added

- Synchronous workflow commands now stream transient block progress messages
  such as `running block Run Verify Gates` and `finished block Run Verify Gates`
  while long-running blocks execute.
- `foundry task <project> "<description>"` runs one user-provided coding task
  through the lightweight task formation, backed by `ExecutionRequested` and
  the existing plan, execute, verify, summarize, and commit chain.

### Fixed

- **Sentinels sharing a scheduled instant now all fire.** The scheduler
  previously selected only one sentinel at the earliest deadline; after it
  fired, recomputing from the boundary skipped every other sentinel due at the
  same time. This starved `nightly-supply-chain` because its 06:00 schedule
  coincides with `ops-digest`. The scheduler now claims and emits the complete
  same-time cohort.
- Claude agent invocations now pass `--agent <name>` instead of the resolved
  `~/.claude/agents/<name>.md` path, matching current Claude Code CLI
  behaviour and preventing maintenance retries from failing before execution.

## [0.27.0] - 2026-07-04

### Changed

- **`foundry init` now uses cmx-core for skill installation.** The hand-rolled install mechanism has been replaced with [cmx-core](https://github.com/svetzal/context-mixer2/tree/main/cmx-core) (`SkillInstaller`). Behavioural changes:
  - **Default scope is now global** (`~/.claude/skills/foundry/`). Use `foundry init --local` for project scope.
  - `--global` is retained as a no-op alias (load-bearing: registry's derived skill-install command is `{binary} init --global --force`).
  - New `--remove` flag uninstalls the skill and cleans the lock entry.
  - Installed files are **byte-identical to bundled content** — the version stamp previously injected into `SKILL.md` frontmatter (`foundry-version:` / `metadata.version`) is gone. The cmx-core lockfile (`~/.config/context-mixer/cmx-lock.json`) is the sole source of truth for the installed version.
  - `--json` output shape changed: `targets[]` replaces `files[]`; `foundry-version` stamp field removed.

### Fixed

- **Post-push auditor honours per-project accepted CVEs.** New optional `audit_exceptions` field on registry `ProjectEntry` lets a project declare formally-accepted CVE/advisory IDs (case-insensitive). Matching vulnerabilities are filtered from the post-push auditor's output and logged as suppressed. Absent/empty preserves prior behaviour. Set by editing `~/.foundry/registry.json` (CLI wiring deferred).
- **Release pipeline watch no longer times out on healthy pipelines.** `gh run view` in the pipeline watcher's run-by-id polling ran without `--repo` from the daemon's working directory (not a git repository), so every poll failed and the watcher burned its full 30-minute timeout before emitting a false `release_pipeline_completed` timed-out/failure — which also stalled `foundry release` clients waiting on `local_install_completed`. Regressed in the run-by-id polling change released in 0.18.0.

## [0.26.1] - 2026-06-26

### Fixed

- **Terminal provider/account failures now stop agent workflows immediately.**
  Claude account exhaustion, including monthly spend-limit failures surfaced in
  session JSONL records with `api_error_status=429`, is now classified as a
  terminal provider/account condition. Foundry propagates structured failure
  metadata through agent session, execution, gate verification, and project
  completion events; bypasses retry routing for those failures; and opens an
  in-memory provider circuit breaker so foundryd stops launching additional
  doomed agent sessions during the same daemon lifetime.
- **Nightly maintenance sentinel duplicate emissions are guarded.** The
  scheduler now rechecks wall-clock time after waking and re-arms if it woke
  before the cron boundary, preventing the maintenance sentinel from emitting
  two runs for the same scheduled instant.

## [0.26.0] - 2026-06-16

### Added

- **Supply-chain auto-fix engine (EXP-003 Phase 2, Slice 2b — first increment,
  shipped dark).** `RemediateSupplyChain` can now *apply* a fixable advisory's
  fix, not just classify it. It is **off by default** and acts only when
  `FOUNDRY_SUPPLY_CHAIN_REMEDIATE` is truthy *and* the throttle is `Full` (never
  `dry_run`); otherwise it is byte-for-byte the classifier. Every fix runs a
  mandatory verify-and-rollback rail and is reversible — **committed locally,
  never pushed**: (1) skip any project whose working tree is dirty; (2) apply the
  fix — this increment ships the in-range Rust bump,
  `cargo update -p <pkg> --precise <fix>`; (3) re-run the repo's own gates; (4)
  on passing required gates, commit just the lockfile, else `git checkout`-revert
  it. Each applied fix commits immediately so a later finding's rollback cannot
  clobber an earlier success. A version the manifest forbids → `apply_failed`
  (the override-pin rewrite case, a later increment); non-Rust stacks →
  `no_fixer`; a repo with no gates is skipped (an unverifiable fix is never
  applied).
- `SupplyChainRemediatedPayload` carries per-finding `outcomes`
  (`RemediationOutcome`); the digest gains a **Remediation** section
  (*Auto-fixed* / *Reverted* / *Not auto-fixed*) shown only when the engine ran.
- New env var `FOUNDRY_SUPPLY_CHAIN_REMEDIATE` (default off).

## [0.25.2] - 2026-06-16

### Fixed

- **pip-audit exit code 1 (vulnerabilities found) misread as a tool failure.**
  Like `npm audit`, `pip-audit` exits non-zero when it finds advisories — the
  JSON report still goes to stdout. The scanner treated that as "audit tool
  failed" and discarded the findings, so a Python project *with* a real
  advisory landed under "Not scanned". `is_audit_vuln_exit_code` now recognises
  Python exit 1 as "vulnerabilities found" (stderr warnings are ignored; the
  stdout JSON is parsed). Completes the Python supply-chain scanning fix begun
  in 0.25.1 — real advisories (e.g. `chromadb` `CVE-2026-45829`) now surface as
  live findings rather than scan errors.

## [0.25.1] - 2026-06-16

### Fixed

- **Python supply-chain scanning now works (venv-local tool + correct parser).**
  Two bugs kept every Python project reading as "not scanned". (1) `pip-audit`
  was invoked as a bare command expecting a global PATH; it is a *project*
  dependency that lives in the project's virtualenv, so it is now resolved from
  `{project}/.venv/bin/pip-audit`. A project without it reports a clean,
  informative "pip-audit not found in .venv" rather than a spawn error. Global
  PATH is never consulted for language/project tooling. (2) Even when it ran,
  the parser expected a top-level JSON array; real `pip-audit --format=json`
  emits `{"dependencies": [...], "fixes": [...]}` with each dependency carrying
  its own `vulns` list. A dedicated `parse_pip_audit` handles the real shape
  (advisory `id`, `fix_versions`); `mix deps.audit` keeps the generic parser.
  Validated live: 4 Python projects now scan, surfacing real advisories (e.g.
  `chromadb` `CVE-2026-45829`, no fix → policy call).

## [0.25.0] - 2026-06-16

### Added

- **Supply-chain remediation triage (EXP-003 Phase 2, Slice 2 — non-mutating
  first increment).** A new `RemediateSupplyChain` block is inserted into the
  supply-chain formation between the scan and the digest, with a new mid-chain
  event `SupplyChainRemediated`. It triages every live finding by *fix
  availability*: a populated fix version → mechanically **auto-fixable**; an
  empty one → a **policy call** (an exploitability judgement about our usage that
  stays human). The chain is now `SupplyChainScanStarted → ScanSupplyChain →
  SupplyChainScanned → RemediateSupplyChain → SupplyChainRemediated →
  WriteSupplyChainDigest → SupplyChainScanCompleted`.
- **Scanner fix-version enrichment.** `Vulnerability` and `SupplyChainFinding`
  now carry an optional `fix_version`, populated from each tool's output
  (`cargo audit` `versions.patched`, `npm audit` `fixAvailable.version`,
  `pip-audit` `fix_versions`). A `bare_version` helper reduces version
  requirements (`">= 0.2.5"`, `"^0.28.1"`) to bare versions. Unknown fix
  versions fail safe to `None` (classified as a policy call, surfaced — never
  silently auto-fixed).
- **Digest triage surface.** The digest now opens with a `N auto-fixable · M
  policy-call` triage line and the per-project findings table gains a **Fix**
  column (resolving version, or `policy call`).

### Notes

- This increment is **advisory and non-mutating** — `RemediateSupplyChain`
  classifies only. The auto-fix engine (in-range bump; override-pin manifest
  rewrite with gate-verify-and-rollback; no-fix policy surface) lands in a later
  increment behind an explicit env gate, inert until enabled even under `Full`
  throttle. `remediated_count` is always `0` today.

## [0.24.0] - 2026-06-16

### Fixed

- **Triage formation self-emit loop (event-log runaway).** `WriteTriageDigest`
  sank on `MaintenanceTriageCompleted` *and* re-emitted that same event type,
  re-triggering itself in an unbounded loop. Each iteration appended a large
  verdicts-bearing event, ballooning `~/.foundry/events/2026-06.jsonl` to ~56 GB
  (≈1000× a normal month) and eventually filling the disk. The block now emits a
  distinct terminal event, `MaintenanceTriageDigestWritten` (consumed by
  nothing), matching the terminal-event pattern of the ops/commit/supply-chain
  digest writers. A regression test asserts the terminal type differs from the
  sink type.
- **Startup hang on an oversized event file.** `detect_legacy_event_names` (the
  0.17.0 migration guard) read every `.jsonl` fully into memory via
  `read_to_string`, wedging daemon boot on the 56 GB file — any restart hung. It
  now streams line-by-line through a `BufReader`, capped at 64 MB per file, so
  memory stays bounded and an oversized file can never block startup again.

### Added

- New terminal event type `MaintenanceTriageDigestWritten`.

## [0.23.0] - 2026-06-16

### Added

- **Supply-chain scan formation (nightly, working-tree, advisory).** A new
  `nightly-supply-chain` canonical sentinel (06:00 local, enabled by default)
  emits `SupplyChainScanStarted`, driving a two-block formation that scans every
  active project's working-tree lockfile for dependency advisories. This is the
  *detection* lane the release-tag `ReleaseTagAudited` audit does not cover —
  it scans what is checked out now, on a schedule, independent of whether code
  changed. `ScanSupplyChain` runs each stack's audit tool (`cargo audit`,
  `npm audit`, `pip-audit`, `mix deps.audit`), classifies each advisory against
  that repo's committed `.supply-chain-allow.json`, and emits `SupplyChainScanned`.
  `WriteSupplyChainDigest` renders a *deterministic* (no-agent) markdown digest
  to `{FOUNDRY_SUPPLY_CHAIN_DIR}/{YYYY-MM-DD}.md` and emits
  `SupplyChainScanCompleted`. The formation is advisory and read-only: it never
  mutates a working tree and never fails a project run — a supply-chain advisory
  is an external, time-triggered fact, not a regression in the project's code.
- **Committed per-repo supply-chain allowlist (`.supply-chain-allow.json`).** A
  neutral artifact Foundry reads (never writes) giving the supply-chain function
  the memory a stateless gate lacks: an accepted-advisory record with an
  `expires` date. An active acceptance suppresses the advisory; a lapsed one
  resurfaces it as a live finding for re-decision. Acceptances are authored by a
  human and land through the repo's normal commit flow, so each decision lives
  in git history. Read via `foundry_sdk::supply_chain`.
- New event types `SupplyChainScanStarted`, `SupplyChainScanned`,
  `SupplyChainScanCompleted`; path helper `supply_chain_dir()` with
  `FOUNDRY_SUPPLY_CHAIN_DIR` override.

## [0.22.0] - 2026-06-12

### Added

- **Post-maintenance failure triage formation (propose-only).** After each
  nightly maintenance run (`MaintenanceSummaryRequested`), two new blocks
  classify every gate failure from that run and write a dated digest to
  `~/.foundry/triage/YYYY-MM-DD.md`. `TriageMaintenance` reads the Foundry
  JSONL event log, extracts `PreflightCompleted` failures from the run window,
  classifies them into one of 12 domain classes (format drift, vuln with fix,
  test breakage, infra flake, etc.), correlates N≥3 same-signature infra
  failures into a single suppressed `InfraIncident`, detects N≥3 consecutive
  failures on the same gate (escalate), and emits `MaintenanceTriageCompleted`
  with a typed `MaintenanceTriageCompletedPayload`. `WriteTriageDigest` renders
  a markdown triage digest (summary table, escalations, auto-fixable proposals,
  infra-suppressed incidents, policy calls, needs-investigation, benign) and
  writes it atomically; dry-run skips the write. The formation is strictly
  propose-only — no changes are applied to any project. Override the output
  directory with `FOUNDRY_TRIAGE_DIR`.

## [0.21.0] - 2026-06-12

### Added

- **Self-healing gates via optional `fix_command`.** A gate in
  `.hone-gates.json` may now declare a `fix_command` — an in-place command that
  mechanically repairs its failure (a formatter or lint autofixer). When the
  gate `command` fails and a `fix_command` is present, the runner runs the fix
  once and re-checks; a passing re-check resolves the gate and sets
  `fix_applied` on the result, leaving the repaired tree for `CommitAndPush`
  (`git add -A`) to commit. This is the structural fix for the preflight
  deadlock: a required format/lint gate previously aborted the run before the
  maintain step that would have reformatted the code, so a project with
  formatting drift failed every night with no path to self-heal. The field
  flows resolve → payload → preflight parse → runner, and `foundry gates`
  derivation now emits it for safely auto-fixable gates only (never tests,
  build, or security gates).

- **Abstract model tiers and reasoning effort, configurable per provider.**
  Blocks now request work in provider-neutral terms — a `ModelTier`
  (`deep`/`balanced`/`fast`, mirroring hopper) and a `ReasoningEffort`
  (`minimal`/`low`/`medium`/`high`/`max`) — replacing the conflated
  `AgentCapability`. Each provider resolves a tier to a concrete model id and an
  effort to its CLI token (clamping levels it does not support; e.g. claude
  `--effort` has no `minimal`/`max`). claude now receives an `--effort` flag it
  previously ignored.
- **`~/.foundry/agents.json`** — a new seed-merged config store (same discipline
  as sentinels) that overrides the baked-in per-provider tier→model and
  effort→token maps without rebuilding. Defaults ship in code so the daemon
  works with no file; on first start the full seed is written, and upgrades
  additively fill any missing provider/tier/effort key while preserving
  hand-edited values. Override the path with `FOUNDRY_AGENT_CONFIG_PATH`.
- **Codex agent backend** — `CodexAgentGateway` drives the `codex` CLI
  (`codex exec --json -o …`) as a third provider alongside `claude` and
  `opencode`, all behind the same `AgentGateway` trait. Capability maps to
  `OpenAI` models with `model_reasoning_effort`; `ReadOnly` access maps to
  codex's *enforced* `-s read-only` sandbox (a real guarantee opencode's
  advisory mode lacks), `Full` to `--dangerously-bypass-approvals-and-sandbox`.
  Validated against `codex-cli` 0.134.0.
- **Per-request agent provider override.** A run may select its backend via an
  `agent_provider` field on the request event (`claude` | `opencode` | `codex`),
  carried through the whole iterate/maintain chain in `ChainContext` and honored
  by every agent invocation. Surfaced on the CLI as `--agent` for `foundry
  iterate`, `foundry scout`, and `foundry pipeline` (e.g. `foundry iterate myproj
  --agent codex`). Unknown names are rejected at the CLI.
- `AgentProvider` enum and an `AgentRequest.provider` field in the SDK gateway
  contract; a new `RoutingAgentGateway` dispatches each request to the matching
  backend, falling back to a process default.

### Changed

- `FOUNDRY_AGENT_PROVIDER` is now the *default* provider rather than the sole
  selector. The daemon constructs all three backends up front and routes per
  request; absent an override, requests use this default (still defaulting to
  `claude`, with a warning on an unknown value).
- **Event taxonomy renames** (wire format changes — no aliases, hard cutover):
  - `PromptExecutionRequested` → `ExecutionRequested`
    (wire: `prompt_execution_requested` → `execution_requested`)
  - `CommitSummaryComposed` → `CommitSummaryCompleted`
    (wire: `commit_summary_composed` → `commit_summary_completed`)
  - `OpsSummaryComposed` → `OpsSummaryCompleted`
    (wire: `ops_summary_composed` → `ops_summary_completed`)
  - `GreetingComposed` is **retained** — it is a genuine domain fact in a
    two-event sequence (`GreetingComposed` → `GreetingDelivered`), not a
    lifecycle end.
  - **Caveat:** pre-rename events persisted in `~/.foundry/events/*.jsonl`
    carrying the old wire strings will deserialize as `Custom(...)` — this is
    acceptable for historical records and does not affect current workflow
    execution.

### Fixed

- **Benign triage declines no longer inflate the maintenance run status table.**
  `GenerateSummary` now treats a terminal `success: false` whose summary is a
  benign decline ("triage rejected", "no correction warranted", "unknown
  violation") as a `Success` rather than a `Failed` row — these are no-ops, not
  defects. Backed by a shared `is_benign_decline` predicate reused by the triage
  formation. Also corrected a stale comment in `create_plan` that described the
  rejection terminal as `success: false` (it has emitted `success: true` since
  triage rejections became benign no-ops).

## [0.20.0] - 2026-05-29

This release adds the **ops-digest** formation — a periodic summary of
business activity from the MBOS event stream, the operational counterpart
to v0.19.0's commit digest. A new `ops-digest` sentinel fires every three
hours as a cheap heartbeat; a pressure gate in the first block decides
whether each tick produces a digest, so the effective cadence tracks
activity rather than the clock.

### Added

- Ops-digest formation — a three-block chain (`ObserveEvents` →
  `SummarizeEvents` → `WriteOpsDigest`) that reads MBOS JSONL event files,
  applies a pressure gate (≥25 new events or any anomaly), asks the agent to
  summarise the operational period, and writes
  `{FOUNDRY_OPS_DIGESTS_DIR}/{YYYY-MM-DD}.md`. Anomaly classification covers
  P0 urgency, CI pipeline failures, unresolved maintenance interventions,
  high/critical vulnerability alerts, and maintenance runs with failed repos.
- `ops-digest` sentinel (`0 */3 * * *`) added to the canonical default seed,
  emitting `OpsDigestStarted` every three hours. Existing installs pick it up
  automatically on the next daemon restart via the additive seed-merge.
- Four new event types: `OpsDigestStarted` (span opener), `OpsObserved`,
  `OpsSummaryComposed`, `OpsDigestCompleted`, with matching typed payload
  structs (`OpsEventDigest`, `OpsObservedPayload`, `OpsSummaryComposedPayload`,
  `OpsDigestCompletedPayload`).
- Three new path helpers in `foundry_core::paths`: `ops_digests_dir()`
  (`FOUNDRY_OPS_DIGESTS_DIR`), `ops_events_intake_dir()`
  (`FOUNDRY_OPS_EVENTS_DIR`, defaults to
  `~/Work/Operations/Events/intake`), and `ops_watermark_path()`
  (`~/.foundry/ops-digest.watermark`). The watermark advances atomically after
  each successful digest write so subsequent runs only process newer events.
- Watermark-based incremental ingestion: first-run lookback is 24 hours when
  no watermark exists. Malformed JSONL lines are skipped silently.

## [0.19.0] - 2026-05-28

This release ships the **Sentinel** subsystem and its first two formations:
the nightly maintenance run (internalised from launchd) and the daily
commit digest. Sentinels are declarative, named, scheduled triggers that
live inside `foundryd` and emit configured events when their schedule
fires, replacing the per-machine launchd plist pattern that previously
drove proactive workflows.

### Added

- `foundry_core::sentinel` module — `SentinelStore`, `SentinelEntry`,
  `Schedule` (externally-tagged enum so future kinds extend additively),
  `EmitSpec` (strongly-typed `EventType` on the wire), and
  `SentinelMutationError`. `default_seed()` ships the canonical set;
  `merge_default_seed_into` adds missing-by-name entries to existing
  stores on every daemon start, so new Foundry releases that ship more
  canonical sentinels reach existing installs automatically without
  manual JSON edits.
- In-process scheduler (`foundryd::scheduler`) — a tokio task that
  watches `~/.foundry/sentinels.json`, computes the soonest enabled
  firing across all entries, and emits the configured event when due.
  `tokio::select!` between the deadline and a `Notify` reload signal so
  mutations take effect immediately. Standard 5-field cron expressions
  are auto-padded to the 6-field shape the `cron` crate expects.
  Catch-up policy is "skip missed firings".
- Two new gRPC RPCs — `SentinelEnable` and `SentinelDisable` — plus the
  proto `Sentinel` message. Both wake the scheduler's reload notify so
  the next firing is recomputed immediately.
- CLI subcommands: `foundry sentinel list | show | enable | disable`,
  mirroring the `foundry registry` shape exactly. `list` and `show`
  always read the file directly; `enable` and `disable` try gRPC and
  fall back to direct file mutation with a warning when the daemon is
  unreachable.
- `nightly-maintenance` sentinel (`0 2 * * *`) in the default seed,
  internalising the schedule previously held by
  `launchd/com.mojility.foundry-maintenance.plist`.
- `daily-commit-digest` sentinel (`0 17 * * *`) in the default seed,
  driving a new linear formation: `ObserveCommits` (Observer; runs
  `git log --since="24 hours ago" --no-merges --pretty=format:...` per
  active project, captures per-project errors inline so the chain
  continues), `SummarizeCommits` (Observer; agent-invoked Reasoning,
  ReadOnly access, with Canadian English / `##` per project / 7-char
  SHA / ⚠ Heads-up section / no-fabrication prompt rules;
  short-circuits the agent call on empty days), `WriteCommitDigest`
  (atomic tempfile-then-rename to
  `{FOUNDRY_DIGESTS_DIR}/{YYYY-MM-DD}.md`; dry-run firings run the full
  chain but skip the file write).
- Four new event types: `CommitDigestStarted` (span opener),
  `CommitsObserved`, `CommitSummaryComposed`, `CommitDigestCompleted`,
  with matching typed payload structs (`CommitInfo`, `ProjectCommits`,
  and the four `*Payload` types).
- New paths: `~/.foundry/sentinels.json` (overridable via
  `FOUNDRY_SENTINELS_PATH`) and `~/.foundry/digests/YYYY-MM-DD.md`
  (overridable via `FOUNDRY_DIGESTS_DIR`).
- mdBook chapters `book/src/guide/sentinels.md` and
  `book/src/guide/commit-digest.md`, plus AGENTS.md and skill updates.

### Changed

- `service::emit()` now delegates to a shared `service::spawn_workflow`
  helper that both the gRPC handler and the in-process scheduler use,
  so sentinel firings appear in `foundry status` and produce traces
  identically to user-triggered runs.

### Removed

- `launchd/com.mojility.foundry-maintenance.plist`. The nightly
  schedule now lives in `~/.foundry/sentinels.json`. Upgrade path is in
  `launchd/README.md` and `book/src/guide/sentinels.md`: unload the
  legacy plist and `rm` it; the canonical seed merge populates
  `nightly-maintenance` on the next daemon start.

### Dependencies

- Added `cron = "0.16"` to `foundryd` for parsing sentinel schedules.

## [0.18.1] - 2026-05-22

### Fixed

- A triage rejection is now always classified as a successful no-op. Previously
  `create_plan` split rejections by the assessor's severity number: below the
  triage threshold was `success: true`, but at-or-above threshold was reported
  as `success: false`, so a project whose only finding was triaged away as
  busy-work showed up as a failed maintenance run. Severity is already folded
  into the triage agent's accept/reject decision, so re-deriving a failure
  verdict from it in a later block was redundant and wrong. Both rejection
  paths now emit `ProjectIterationCompleted { success: true }`, carrying the
  triage agent's reason in the summary.

## [0.18.0] - 2026-05-21

### Removed

- The `audit_only` throttle level. It executed Mutator blocks for real but
  suppressed delivery of their events, so a chain halted at the first
  mutation — a confusing middle ground whose name implied the opposite of its
  behaviour. `Throttle` is now binary: `full` (run for real) and `dry_run`
  (simulate Mutators). The proto throttle encoding is now `0 = full`,
  `1 = dry_run`. No workflow depended on the halt-at-first-mutation
  behaviour.

### Changed

- `FanOutMaintenance` now uses the scatter/gather primitive. A system
  maintenance cycle scatters its per-project `ProjectRunStarted` events and
  gathers their `ProjectRunCompleted` terminals; the engine synthesizes the
  cycle's `MaintenanceCycleCompleted` as a genuine fan-in. The service layer
  no longer hand-aggregates per-project completions — it persists per-project
  traces and emits a new `MaintenanceSummaryRequested` command that triggers
  `GenerateSummary`. The `MaintenanceCycleStarted` payload struct
  `MaintenanceCycleCompletedPayload` is renamed `MaintenanceSummaryRequestedPayload`
  (same fields). `foundry run` detects cycle completion from
  `maintenance_summary_requested` rather than `maintenance_cycle_completed`.

### Added

- Events now carry an optional `causation_id` — the `id` of the event that
  triggered the block which emitted it. This records the domain causality
  edge directly in the event envelope, independent of the observability
  span structure (`trace_id`/`span_id`/`parent_span_id`). The engine stamps
  it unconditionally on every emitted event ("set if unset"); root events
  carry no `causation_id`. Legacy events without the field continue to
  parse. Groundwork for native fan-out/fan-in coordination.
- Events now carry an optional `gather_id` — the fan-out (scatter/gather)
  group the event belongs to. Unlike `causation_id`, it propagates verbatim
  to every descendant like `trace_id`, so a scattered child workflow's
  terminal `*Completed` event still identifies its group across span-opener
  boundaries. `None` outside any fan-out; legacy events continue to parse.
  Groundwork for the gather/reduce engine primitive.
- Native scatter/gather (map/reduce) coordination. A task block can return a
  `Scatter` in its `TaskBlockResult` (`TaskBlockResult::scattering`) declaring
  a set of child events and a `GatherSpec`. The engine mints a `gather_id`,
  stamps and dispatches the children, counts their completions, and
  synthesizes a reduce event (`GatherCompletedPayload`) once the
  `GatherPolicy` (`All` or `Count(n)`) is satisfied. Gathers nest — a reduce
  event rejoins its enclosing group — and an empty scatter reduces
  immediately. For now a gather completes within a single `process()`
  traversal; durable, cross-call gathers are a later step behind the same
  interface.

## [0.17.1] - 2026-05-20

### Fixed

- Below-threshold triage rejections are now classified as successful no-ops
  rather than failures. A project that has stabilized — where the iterate
  triage finds no correction warranted — no longer shows as `failed` in
  maintenance run summaries.
- Terminal events now emit with accurate success status on all trace paths.
- `span_context` no longer carries an inherited `TRACEPARENT` when the
  context is absent.
- `retry_execution` replaces an `expect` with a graceful fallback in
  `dry_run_events`.

### Changed

- Internal refactors: event serialization and agent JSON parsing consolidated
  in `blocks`; loose parameters encapsulated into `ProjectSpec`,
  `ProjectEdits`, and `ExecutionContext`.
- Upgraded `rand` 0.8 → 0.10.

## [0.17.0] - 2026-05-15

### Breaking changes

- **Event taxonomy renames** (no aliases — hard cutover):
  - `MaintenanceRunStarted` split into `MaintenanceCycleStarted` (cycle root)
    and `ProjectRunStarted` (per-project).
  - `MaintenanceRunCompleted` split similarly into `MaintenanceCycleCompleted`
    and `ProjectRunCompleted`.
  - `IterationRequested` → `ProjectIterationRequested`.
  - `MaintenanceRequested` → `ProjectMaintenanceRequested`.
  - `GreetRequested` → `GreetingRequested`.
  - Snake_case wire strings change accordingly (e.g. `iteration_requested`
    → `project_iteration_requested`).
- **`trc_*` trace IDs replaced** with OpenTelemetry-compatible 32-char
  lowercase hex. Legacy `trc_*` IDs remain readable (a helper
  `is_legacy_trace_id` discriminates), but newly-emitted events use the
  new format.
- **`FanOutMaintenance` no longer mints a fresh `trace_id` per project**.
  Per-project events inherit the cycle root's `trace_id` via the engine's
  span-stamping pass, so every event in a nightly batch shares one
  `trace_id`.

### Migration

Run once after upgrading and **before** restarting `foundryd`:

```bash
scripts/migrate-event-names.sh --dry-run   # review counts
scripts/migrate-event-names.sh             # apply
```

The script rewrites archived event names in `~/.foundry/events/*.jsonl`
and `~/.foundry/traces/**/*.json`, recomputes `Event::id` for renamed
events, and fixes up payload references (`project_trace_ids`,
`root_event_id`). `foundryd` refuses to start (exit code 2) if it
detects legacy event names on disk and prints the remediation command.

### Added

- **OpenTelemetry-shaped nested tracing**: every `Event` now carries
  `span_id` (16-char hex) and `parent_span_id` (16-char hex) alongside
  `trace_id` (32-char hex). The engine stamps these per two rules:
  emitted events default to the trigger's span; events registered as
  *span openers* (`*Requested` workflow events, `*Started` lifecycle
  events, `RemediationStarted`) get a fresh `span_id` parented to the
  emitting block's span. Block-level spans are recorded on
  `BlockExecution` (not on emitted events), so the call tree can be
  reconstructed without changing event volume on the wire.
- **New `Span` RPC** (`proto/foundry.proto`): retrieve every event and
  block execution within a single span. Phase 2 stub returns
  `found = false`; Phase 6 wires it to a real `span_id → trace_id`
  index in the trace store.
- **`mint_span_id` / `mint_trace_id`** in `foundry_core::event`: both
  hex-encoded random output (16-char and 32-char respectively).
- **`EventType::is_span_opener`**: predicate over the span-opener
  registry. Used by the engine's stamping pass to decide between the
  default rule and the span-opener rule.

### Changed

- **`EventType::IterationRequested` → `ProjectIterationRequested`** and
  the parallel renames listed above. Payload structs renamed in
  lockstep (`IterationRequestedPayload` → `ProjectIterationRequestedPayload`,
  etc.).
- **`MaintenanceRunStartedPayload`/`MaintenanceRunCompletedPayload`**
  removed and replaced by four new payload structs:
  `MaintenanceCycleStartedPayload`, `ProjectRunStartedPayload`,
  `MaintenanceCycleCompletedPayload`, `ProjectRunCompletedPayload`.
  The cycle-level completion still carries `project_trace_ids` /
  `skipped_projects` / `total_duration_ms` for `GenerateSummary`
  compatibility; the per-project completion is a leaner shape
  (`success` + optional `root_event_id`).
- **`AGENTS.md` event taxonomy examples** updated to new names.

## [0.16.1] - 2026-05-13

### Fixed

- **`ExecutePlan` short-circuits when `correction_needed=false`**
  (`crates/foundryd/src/blocks/execute_plan.rs`): when `CreatePlan` returns
  `correctionNeeded: false` (the Reasoning agent inspected the codebase and
  concluded the assessment was inaccurate), `ExecutePlan` now emits a synthetic
  `ExecutionCompleted` event immediately and skips the Coding-agent invocation.
  Previously the Coding agent was invoked unconditionally with an imperative
  "the plan MUST be applied" prompt, burning ~48s of Full-access tokens per
  occurrence on a plan that prescribed no changes. The new path preserves
  `correction_reason` in `execution_output` for trace visibility, sets
  `changes_detected: false`, and patterns on `CreatePlan`'s `accepted=false`
  short-circuit.

### Changed

- **`docs` gate added to `.hone-gates.json`**: foundry's own quality-gate suite now
  runs `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps` as a required gate,
  matching the CI "Check documentation compiles" step. `iterate`/`validate`/maintenance
  runs previously only checked fmt/clippy/test/coverage/deny, so doc-link breakage (e.g.
  the 0.16.0 private intra-doc link) could pass local gates and only fail in CI.

## [0.16.0] - 2026-05-13

### Added

- **gRPC registry mutations** (`proto/foundry.proto`, `foundry-core`, `foundryd`,
  `foundry-cli`): `foundry registry add|remove|edit` now route through the daemon's
  gRPC API (`RegistryAdd`, `RegistryRemove`, `RegistryEdit` RPCs) when the daemon is
  reachable, so the in-memory registry stays consistent with `registry.json` across
  concurrent CLI calls. Pass `--offline` to bypass gRPC and mutate the file directly
  (useful when the daemon is not running). The daemon holds `Arc<RwLock<Registry>>`
  and persists changes atomically via `Registry::save` inside the write-lock guard.
  `foundry-core` gains `ProjectSpec`, `ProjectEdits`, `RegistryMutationError`, and
  `parse_stack` to support typed add/edit operations.

### Changed

- **CI maintenance — Node 24 action bumps** (`.github/workflows/`): bumped pinned
  GitHub Actions ahead of the forced Node.js 24 runner cutover (2026-06-02) and the
  Node 20 removal (2026-09-16). `actions/checkout@v4` → `@v6`, `actions/upload-artifact@v4`
  → `@v7`, `actions/download-artifact@v4` → `@v8`, `actions/upload-pages-artifact@v3` → `@v5`,
  `actions/deploy-pages@v4` → `@v5`. `Swatinem/rust-cache@v2` already resolves to a Node 24
  release; `dtolnay/rust-toolchain` and `EmbarkStudios/cargo-deny-action` are composite/Docker
  actions and unaffected. No workflow logic changed.

### Fixed

- **`cargo doc` broke on a private intra-doc link** (`crates/foundryd/src/engine.rs`):
  `Engine::with_event_writer`'s doc comment linked `[`EventWriter`]`, which is
  crate-private, so `cargo doc --workspace -D warnings` (a CI gate) failed. Replaced
  with a plain code span.

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
