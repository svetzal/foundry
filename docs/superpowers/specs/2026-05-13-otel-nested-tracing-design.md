# OpenTelemetry-shaped nested tracing for Foundry

Status: **Design approved, ready for implementation plan**
Date: 2026-05-13 (spec written), 2026-05-14 (last update)
Author: Stacey Vetzal (with Claude as collaborator)

## Resume / next step

Brainstorming is complete. All eight decisions in "Decisions locked"
have been confirmed by Stacey. The next step is to invoke the
`superpowers:writing-plans` skill to produce a step-by-step
implementation plan in `docs/superpowers/plans/`, then execute
phase-by-phase per the "Implementation phasing" section below.

When resuming:

1. Re-read this spec start-to-finish (≈30 min) — it's the source of
   truth.
2. Invoke `superpowers:writing-plans` with this spec as input.
3. The plan should preserve the 8-phase ordering: foundation in
   foundry-core → proto + service → engine span stamping → event
   taxonomy rename → historical backfill → subprocess propagation →
   `Span` RPC → CLI display → docs.

## Why

Today every per-project workflow chain owns a flat `trace_id`. The
nightly "scheduler kicked off N projects" relationship is implicit —
N sibling traces start within the same second with no parent link,
and the only structural connection between them is a payload-level
`project_trace_ids` map in `MaintenanceRunCompleted`. That's enough
to write a summary but not enough to drive a generic call-tree
visualizer or to answer "what actually happened" across a cycle
without bespoke parsing.

We want the relationship to be first-class so that consumers
(ops-visualizer, future analytics, ad-hoc queries) can:

- Group every event in a nightly batch under one root.
- Render a real call tree from cycle root down to block leaves.
- Distinguish concurrent or overlapping batches that happen to
  start near each other in time.
- Reuse the same model for manual `foundry iterate` / `foundry
  release` runs, just rooted at the workflow rather than a cycle.

We pick the OpenTelemetry data model as our north star. It's mature,
solves the call-tree problem, and matches the wire format of an
ecosystem we may want to interoperate with later.

## Decisions locked

These are settled and not up for re-debate in implementation:

1. **Full four-level call tree**: cycle → project_run → workflow →
   block. Block-level spans are required because the ops-visualizer
   needs to traverse the full hierarchy.
2. **OTel-wire-shaped identifiers**: 128-bit hex `trace_id` (32
   chars, no prefix), 64-bit hex `span_id` (16 chars, no prefix).
   The current `trc_<uuid-simple>` format is retired for new events.
3. **W3C Trace Context for subprocess propagation**: `TRACEPARENT`
   env var with value `00-<trace_id>-<span_id>-01`.
4. **No span kind / status / attributes yet**: defer until a
   downstream consumer demands them. Re-add as additive proto fields.
5. **Rename `MaintenanceRunStarted` / `MaintenanceRunCompleted`**:
   split into cycle-level and project-level events. Do the rename
   now, not later.
6. **Normalize workflow `*Requested` events to noun form**:
   `IterationRequested` → `ProjectIterationRequested`,
   `MaintenanceRequested` → `ProjectMaintenanceRequested`,
   `GreetRequested` → `GreetingRequested`. Each opener now pairs
   cleanly with its `*Completed` closer (per AGENTS.md's noun-form
   rule). No new event types — `*Requested` remains the workflow
   span opener.
7. **Hard cutover, self-identifying schemas**: old events keep their
   `trc_*` `trace_id` and no `span_id` — they're identifiable as
   pre-nested by ID format. No `schema_version` field, no `cycle_id`
   interim field.
8. **Backfill historical event-type names**: a one-shot jq script
   rewrites `maintenance_run_*` / `iteration_requested` /
   `maintenance_requested` / `greet_requested` in archived event
   logs and trace files. `span_id` / `parent_span_id` are *not*
   backfilled.

## Data model

### Core `Event` struct (`crates/foundry-core/src/event.rs`)

```rust
pub struct Event {
    pub id: String,
    pub event_type: EventType,
    pub project: String,
    pub occurred_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
    pub throttle: Throttle,

    /// 32-char lowercase hex. Identifies the whole tree (cycle root
    /// through leaf blocks). Inherited from parent span; minted only
    /// at root spans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,

    /// 16-char lowercase hex. Identifies the span this event
    /// belongs to. Every event participating in a span shares the
    /// same `span_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,

    /// 16-char lowercase hex. The span that caused this one to
    /// open. `None` for root spans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,

    pub payload: serde_json::Value,
}
```

`Event::compute_id` is unchanged — `id` remains content-derived
(event_type, project, occurred_at, payload). Span metadata is
topology, not content, and must not change the deterministic event
id.

### ID minting (`crates/foundry-core/src/event.rs`)

```rust
/// 128 random bits → 32 lowercase hex chars. No prefix.
pub fn mint_trace_id() -> String;

/// 64 random bits → 16 lowercase hex chars. No prefix.
pub fn mint_span_id() -> String;
```

Both use `rand::thread_rng` to fill the byte buffer. We do not seed
from time, to avoid leaking ordering across concurrent cycles.

The old `mint_trace_id` returning `trc_<uuid-simple>` is replaced
(not kept alongside). Callers that need to *recognize* old-format
IDs in archived data use a small helper:

```rust
/// True if `id` is a pre-nested-tracing trace ID (`trc_*`).
pub fn is_legacy_trace_id(id: &str) -> bool;
```

### Proto changes (`proto/foundry.proto`)

`Event` gains:

```protobuf
string span_id = 8;
string parent_span_id = 9;
```

`TraceEvent`, `WatchResponse` mirror those additions.

`EmitRequest` gains:

```protobuf
string span_id = 6;          // Optional. Auto-minted if empty.
string parent_span_id = 7;   // Optional. None ⇒ root span.
```

`TraceBlockExecution` gains:

```protobuf
string span_id = 13;
string parent_span_id = 14;
```

`TraceResponse` is unchanged in shape — the response is still "every
event whose `trace_id` matches the resolved trace_id, plus every
`BlockExecution` from those events." Clients build the tree from
`parent_span_id`. We add one new RPC:

```protobuf
// Retrieve every event and block execution within a single span.
rpc Span(SpanRequest) returns (SpanResponse);

message SpanRequest {
    string span_id = 1;
}

message SpanResponse {
    bool found = 1;
    repeated TraceEvent events = 2;
    repeated TraceBlockExecution block_executions = 3;
    uint64 total_duration_ms = 4;
}
```

`Span` lets ops-visualizer drill into one project's run within a
cycle without paging through siblings.

### `BlockExecution` struct (`crates/foundry-core/src/trace.rs`)

```rust
pub struct BlockExecution {
    pub block_name: String,
    pub trigger_event_id: String,
    pub success: bool,
    pub summary: String,
    pub emitted_event_ids: Vec<String>,
    pub duration_ms: u64,
    pub raw_output: Option<String>,
    pub exit_code: Option<i32>,
    pub trigger_payload: serde_json::Value,
    pub emitted_payloads: Vec<serde_json::Value>,
    pub audit_artifacts: Vec<String>,

    /// This block's own span_id. The block's span is a child of
    /// the workflow span it runs inside. Events emitted by the
    /// block do not carry this `span_id` — they carry the
    /// workflow's span_id under the default propagation rule. Use
    /// `emitted_event_ids` to find what this block produced.
    pub span_id: Option<String>,

    /// The workflow span this block executes inside (= the trigger
    /// event's `span_id`).
    pub parent_span_id: Option<String>,
}
```

## Span boundaries and lifecycle

A **span** is a logical unit of work with a beginning, end, and
identity. In Foundry, spans are minted at four levels:

| Level        | Span opens at                                       | Span closes at                              | `span_id` minted by                  |
|--------------|-----------------------------------------------------|---------------------------------------------|--------------------------------------|
| Cycle        | `MaintenanceCycleStarted` event                     | `MaintenanceCycleCompleted` event           | service layer at root emission       |
| Project run  | `ProjectRunStarted` event                           | `ProjectRunCompleted` event                 | engine (span-opener rule)            |
| Workflow     | A `*Requested` or `*Started` event (opener)         | The matching workflow completion event      | engine (span-opener rule)            |
| Block        | Engine dispatches a block                           | Block returns                               | engine (per-dispatch)                |

The cycle span is the only span minted outside the engine. Every
other span_id appears via the engine's span-opener stamping rule
(see "Span propagation rules" below) — block authors never mint
span_ids themselves.

### Span propagation rules

These two rules govern every event the engine stamps. They are
applied by the engine after a block returns, before persisting and
broadcasting. All stamping is "set if unset," so a block that has a
reason to emit an event with explicit span context can override.

**Default rule** (applies to any event whose `event_type` is not a
registered span opener):

```
emitted.trace_id       = trigger.trace_id
emitted.span_id        = trigger.span_id
emitted.parent_span_id = trigger.parent_span_id
```

Effect: the emitted event is a *peer* of its trigger, attached to
the same span. All events emitted within a workflow share the
workflow's `(span_id, parent_span_id)` pair, regardless of which
block emits them.

**Span-opener rule** (applies when `event_type` is registered as
opening a new span — see registry below):

```
emitted.trace_id       = trigger.trace_id
emitted.span_id        = mint_span_id()       // fresh
emitted.parent_span_id = current_block.span_id // the block that emitted it
```

Effect: a new span is opened whose parent is the block that emitted
the opener. The block's own span lives only in `BlockExecution` — it
is the bridge between the surrounding workflow span and the
newly-opened child workflow span.

**Span-opener registry**: a small, explicit list of `EventType`
values lives in `foundry-core`. Note that `*Requested` events serve
as workflow span openers because the request *is* the entry point
of the workflow — AGENTS.md classifies them as commands (intent),
which is fully consistent with their role as span openers here.

| Span level   | Opener event types                                                          |
|--------------|-----------------------------------------------------------------------------|
| Cycle        | `MaintenanceCycleStarted`                                                    |
| Project run  | `ProjectRunStarted`                                                          |
| Workflow     | `ProjectIterationRequested`, `ProjectMaintenanceRequested`,                  |
|              | `ValidationRequested`, `DriftAssessmentRequested`,                           |
|              | `ReleaseRequested`, `PipelineCheckRequested`, `GreetingRequested`,           |
|              | `RemediationStarted`                                                         |
| Nested       | `StrategicCycleStarted`, `InnerIterationStarted`                             |

Span closers don't need a registry — under the default rule, any
`*Completed` event emitted from within a span naturally inherits
that span's `(span_id, parent_span_id)`. Closers are a
*convention* for human/consumer interpretation, not an engine
mechanism.

### Block-level spans

The engine mints a fresh `block_span_id` per dispatch and records
it on `BlockExecution`, not on emitted events. Before invoking
`block.execute(trigger)`:

1. Mint `block_span_id = mint_span_id()`.
2. Set `current_block.span_id = block_span_id` (task-local — see
   "Subprocess context propagation"). This is what span openers see
   as their `parent_span_id`.
3. Record `BlockExecution.span_id = block_span_id` and
   `BlockExecution.parent_span_id = trigger.span_id` (the workflow
   span this block runs inside).

The block-level span has **no explicit started/completed events**.
Total event volume is identical to today; the metadata that lights
up the call tree lives on `BlockExecution` + on the events stamped
by the rules above.

### Why this structure

This rule pair produces the tree we want:

- Sequential blocks within a workflow are siblings under the
  workflow span (because every event they emit carries the workflow
  span's `(span_id, parent_span_id)`).
- Each block dispatch is a child node of the workflow span (via
  `BlockExecution.parent_span_id = workflow_span`).
- When a block emits a workflow opener, a new child workflow span
  is parented to the *block's* span — visible in the tree as
  "iterate workflow ran inside RouteProjectWorkflow block."

Example tree for a cycle-rooted iteration on project `alpha`:

```
cycle_span                                              [from MaintenanceCycleStarted]
└── bs1: FanOutMaintenance                              [BlockExecution]
    └── project_run_span_alpha                          [from ProjectRunStarted]
        └── bs2: RouteProjectWorkflow                   [BlockExecution]
            └── iteration_span                          [from ProjectIterationRequested]
                ├── bs3: RunPreflightGates              [BlockExecution]
                ├── bs4: CreatePlan                     [BlockExecution]
                ├── bs5: ExecutePlan                    [BlockExecution]
                └── ... (ProjectIterationCompleted closes iteration_span)
```

### Cycle, project_run, and workflow spans in practice

- **Cycle root**: `foundry run` (no `--project`) lands in the
  service's `Emit` handler with `event_type =
  MaintenanceCycleStarted`. The handler mints a fresh `trace_id`
  and `span_id` for the root, sets `parent_span_id = None`, and
  begins processing. This is the only place a span_id is minted
  outside the engine.
- **Per project**: `FanOutMaintenance` emits one
  `ProjectRunStarted` event per active project, carrying no span
  metadata of its own. The engine's stamping pass, recognizing
  `ProjectRunStarted` as a span opener, mints a fresh `span_id`
  per emitted event and sets `parent_span_id` to the
  `FanOutMaintenance` block's span_id. (Today `FanOutMaintenance`
  mints a *fresh trace_id* per project; that behavior is replaced
  — `trace_id` is now inherited from the cycle.)
- **Workflow**: when any block emits a workflow opener
  (`ProjectIterationRequested`, `ProjectMaintenanceRequested`,
  etc.), the engine's stamping pass mints a new workflow `span_id`
  and
  parents it to the emitting block's span_id. The workflow's
  completion event (`ProjectIterationCompleted`,
  `ProjectMaintenanceCompleted`, etc.) is emitted from a later
  block running *inside* that workflow span, so the default rule
  attaches it to the workflow span automatically.
- **Strategic loop**: each strategic cycle is opened by
  `StrategicCycleStarted` (a span opener, new — see Event taxonomy)
  emitted from within the iterate workflow span; each inner
  iteration is opened by `InnerIterationStarted` (also new) within
  the strategic-cycle span. `StrategicCycleCompleted` and
  `InnerIterationCompleted` close those spans naturally under the
  default rule.

### Retries

- Block-internal retries (`Engine::execute_with_retry` looping up to
  `policy.max_retries`): one span. `duration_ms` includes all
  attempts. Matches existing `BlockExecution.duration_ms` semantics.
- Higher-level retries that emit fresh `*Requested` or
  `*Completed` events (e.g., strategic loop iterations): each
  iteration is a sibling span under the parent. No `retry_of`
  attribute needed; the sibling-span pattern is OTel-native.

## Event taxonomy changes

### Renames

Two groups of renames. First, splitting the cycle-vs-project events:

| Today                                         | Tomorrow                            | Where it's emitted                              |
|-----------------------------------------------|-------------------------------------|-------------------------------------------------|
| `MaintenanceRunStarted` (project = `system`)  | `MaintenanceCycleStarted`            | Scheduler / `foundry run` entry path            |
| `MaintenanceRunStarted` (per-project)         | `ProjectRunStarted`                  | `FanOutMaintenance`                             |
| `MaintenanceRunCompleted` (system)            | `MaintenanceCycleCompleted`          | `finalise_system_maintenance` in `service.rs`   |
| `MaintenanceRunCompleted` (per-project)       | `ProjectRunCompleted`                | Per-project completion in `service.rs`          |

Second, normalizing workflow `*Requested` events to noun form so
that each opener pairs cleanly with its closer (per AGENTS.md's
"noun form for compound prefixes" rule):

| Today                  | Tomorrow                       | Pairs cleanly with               |
|------------------------|--------------------------------|----------------------------------|
| `IterationRequested`   | `ProjectIterationRequested`    | `ProjectIterationCompleted`      |
| `MaintenanceRequested` | `ProjectMaintenanceRequested`  | `ProjectMaintenanceCompleted`    |
| `GreetRequested`       | `GreetingRequested`            | `GreetingDelivered`              |

`ValidationRequested`, `DriftAssessmentRequested`,
`ReleaseRequested`, and `PipelineCheckRequested` already pair
cleanly with their closers and are not renamed.

The `MaintenanceRunStartedPayload` and `MaintenanceRunCompletedPayload`
structs in `payload.rs` are renamed to match
(`MaintenanceCycleStartedPayload`, `ProjectRunStartedPayload`, etc.)
and split where their semantics diverge — the cycle-level completion
payload still carries `project_trace_ids`, `skipped_projects`, and
`total_duration_ms`, while the project-level completion is the
simpler "success + root_event_id" shape used today.

The `project_trace_ids` field on `MaintenanceCycleCompletedPayload`
becomes redundant once consumers can derive per-project trace IDs
from `(trace_id == cycle_trace_id, event_type ==
ProjectRunStarted)`. We keep it during the transition for
`GenerateSummary` (which currently iterates that map) and remove it
in a follow-up once `GenerateSummary` is rewritten to query spans.

### New `*Started` events for span openings

Workflows whose first event today is a `*Requested` use that event
as the workflow span opener — no change. Strategic loop currently
has `StrategicCycleCompleted` and `InnerIterationCompleted` with no
matching openers; we add:

- `StrategicCycleStarted` — opens a strategic-cycle span. Parent =
  iterate workflow span.
- `InnerIterationStarted` — opens an inner-iteration span. Parent =
  strategic-cycle span.

Per AGENTS.md taxonomy: "Started/Completed must pair" — this brings
the strategic loop into compliance with the rule.

### `EventType` enum

The renamed and added variants land in
`crates/foundry-core/src/event.rs`. Snake-case serialization is
preserved for all existing variants; the renamed ones get their new
snake_case strings (`maintenance_cycle_started`,
`project_run_started`, etc.). The full-variant serialization and
round-trip tests are updated to match.

## Engine changes (`crates/foundryd/src/engine.rs`)

The heart of the implementation lands here. Two existing functions
change:

### `run_block`

Before invoking `block.execute(trigger)`:

```rust
let block_span_id = mint_span_id();
let trace_id = trigger.trace_id.clone();
let workflow_span_id = trigger.span_id.clone();         // the parent workflow span
let workflow_parent_span_id = trigger.parent_span_id.clone();
```

The result is recorded as a `BlockExecution` with
`span_id = Some(block_span_id)` and
`parent_span_id = workflow_span_id`.

`block_span_id` is also stored in `SPAN_CONTEXT` (task-local) for
the duration of `block.execute` so that:

- Subprocess spawners can read it for `TRACEPARENT` injection.
- The stamping pass below can use it as the parent for span-opener
  events.

### `persist_and_broadcast_events`

The current behavior — "if emitted has no `trace_id`, inherit from
trigger" — is replaced by the two stamping rules from "Span
propagation rules" above:

```rust
fn stamp(emitted: &mut Event, trigger: &Event, block_span_id: &str) {
    if emitted.trace_id.is_none() {
        emitted.trace_id = trigger.trace_id.clone();
    }
    if is_span_opener(&emitted.event_type) {
        if emitted.span_id.is_none() {
            emitted.span_id = Some(mint_span_id());
        }
        if emitted.parent_span_id.is_none() {
            emitted.parent_span_id = Some(block_span_id.to_string());
        }
    } else {
        if emitted.span_id.is_none() {
            emitted.span_id = trigger.span_id.clone();
        }
        if emitted.parent_span_id.is_none() {
            emitted.parent_span_id = trigger.parent_span_id.clone();
        }
    }
}
```

`is_span_opener` consults the registry described in "Span
propagation rules." All stamping is "set if unset," so a block that
needs to emit an event with explicit span context retains that
ability.

### Dry-run

`dry_run_events` returns synthetic events whose `span_id` and
`parent_span_id` are populated by the same stamping pass. The
existing test `trace_id_propagates_in_dry_run` is updated to also
assert span propagation.

### Service-layer changes (`service.rs`)

- `Emit` RPC: if request has `trace_id` empty, mint a new one. If
  `span_id` empty, mint a new one. If `parent_span_id` empty, treat
  as root. The synthesized `Event` is stamped accordingly.
- `extract_per_project_traces`: filters now by `event_type ==
  ProjectRunStarted` (the renamed event).
- `finalise_system_maintenance`: emits `MaintenanceCycleCompleted`
  (renamed) with the same payload shape it has today. Sets its
  `(trace_id, span_id, parent_span_id)` to the cycle's
  `(trace_id, cycle_span_id, None)`.
- Per-project `MaintenanceRunCompleted` synthesis (the non-system
  branch) emits `ProjectRunCompleted` instead, with the project
  run's span context.

## Subprocess context propagation

Wherever `foundryd` spawns a subprocess, it sets `TRACEPARENT` in
the child's environment. Concretely, this is centralized in two
places:

1. **`shell.rs::run_shell`** — the general shell wrapper used by
   gate runners and various blocks. Reads the *current span context*
   from a `tokio::task_local!` (see below) and sets `TRACEPARENT`
   on the spawned `Command`.
2. **`agent_stream.rs`** — the Claude Code agent spawn site. Same
   pattern.

Direct `std::process::Command::new` calls scattered through
`engine.rs`, `validate.rs`, `install.rs`, `release.rs`, and
`gates_commands.rs` are migrated to go through `shell.rs::run_shell`
(or, where that's not possible because of streaming requirements, a
thin helper that handles the env-var injection consistently). This
is partly a cleanup win independent of tracing.

### Task-local span context

The engine sets a `tokio::task_local!` before calling
`block.execute(trigger)`:

```rust
tokio::task_local! {
    pub static SPAN_CONTEXT: SpanContext;
}

pub struct SpanContext {
    pub trace_id: String,
    pub span_id: String,  // The block's span_id
}
```

Subprocess-spawning helpers (`shell.rs`, `agent_stream.rs`) read
`SPAN_CONTEXT` via `try_with` and inject `TRACEPARENT` if present.
If unset (e.g., a unit test calling a helper directly), no
`TRACEPARENT` is set — graceful degradation.

### CLI side (`foundry-cli`)

`foundry emit` reads `TRACEPARENT` from its own env. If present, it
parses out the trace_id and span_id and sends them in the
`EmitRequest` as `trace_id` and `parent_span_id` respectively. The
service mints a fresh `span_id` for the emitted event.

## Manual triggers

A user-invoked `foundry iterate alpha` (no cycle context) starts a
fresh trace:

- `trace_id` = newly minted
- `span_id` = newly minted (this is the iterate workflow span)
- `parent_span_id` = None

The root event is the workflow's `*Requested` event
(`ProjectIterationRequested` here). Cycle-rooted vs manual is trivially
distinguishable by the root event type: `MaintenanceCycleStarted`
for nightly, a workflow `*Requested` for manual.

## Storage and read-path

### JSONL event log (`~/.foundry/events/YYYY-MM.jsonl`)

Same format. New fields serialize when present, omit when `None`
(via `skip_serializing_if`). Old archived files keep working —
analysis tools that detect `trc_*` prefix on `trace_id` know to
treat them as pre-nested.

### Trace files (`~/.foundry/traces/YYYY-MM-DD/`)

Per-trace JSON files now include the span fields on each event and
on each block execution. Old trace files keep their existing fields
and lack the new ones; readers tolerate both via
`#[serde(default)]`.

### `Trace` RPC

Unchanged shape, richer content. Clients can still ask "give me
everything with the same `trace_id` as event X" and now receive a
forest. Building the tree from `parent_span_id` is the consumer's
job.

### `Span` RPC (new)

Returns all events and block executions whose `span_id` matches the
request. Implementation: in-memory map `span_id → Vec<EventId>`
maintained alongside the trace store, populated as events are
persisted. Behind a single async function so the storage backend
remains pluggable.

### Trace store internals (`trace_store.rs`)

Add a secondary index from `span_id → trace_id` so the new `Span`
RPC can resolve cheaply. Add a secondary index from `trace_id →
Vec<span_id>` for cycle-level queries. Both are derived from the
existing per-event records — no schema change to on-disk traces.

## CLI display

`foundry trace <event-id>` is updated to render an indented tree:

```
[span] cycle                                trace=a1b2…  span=0a1b…  parent=∅
├── maintenance_cycle_started               (root event of cycle span)
├── [block: FanOutMaintenance]              block_span=0b1c…  parent=0a1b…  duration=12ms
│   ├── project_run_started   project=alpha (opens span 1b2c…, parent=0b1c…)
│   └── project_run_started   project=beta  (opens span 5f6a…, parent=0b1c…)
├── [span] project_run alpha                span=1b2c…  parent=0b1c…
│   ├── project_run_started                 (opener event)
│   ├── [block: RouteProjectWorkflow]       block_span=2b3c…  parent=1b2c…  duration=3ms
│   │   └── project_iteration_requested             (opens span 2c3d…, parent=2b3c…)
│   ├── [span] iteration                    span=2c3d…  parent=2b3c…
│   │   ├── project_iteration_requested             (opener event)
│   │   ├── [block: RunPreflightGates]      block_span=3d4e…  parent=2c3d…  duration=423ms
│   │   │   └── preflight_completed         (carries span=2c3d…, the iteration span)
│   │   ├── [block: CreatePlan]             block_span=4e5f…  parent=2c3d…  duration=12.3s
│   │   │   └── plan_completed              (carries span=2c3d…)
│   │   ├── [block: ExecutePlan]            block_span=5e6f…  parent=2c3d…  duration=2m4s
│   │   └── project_iteration_completed     (closes iteration span)
│   └── project_run_completed               (closes project_run span)
├── [span] project_run beta                 span=5f6a…  parent=0b1c…  (collapsed)
└── maintenance_cycle_completed             (closes cycle span)
```

Two visual conventions in this tree:

- `[span] ...` nodes are spans — they group events and child
  blocks/spans. Their `span_id` and `parent_span_id` come from any
  event carrying that span.
- `[block: Name]` nodes are `BlockExecution` records — leaves that
  represent the engine running one block. Their `parent_span_id`
  is the surrounding workflow span. Events emitted by a block sit
  underneath the `[block: …]` line in the display, but their
  `span_id` is the workflow's, not the block's.

A `--flat` flag preserves today's chronological-list rendering for
scripts that grep over `foundry trace` output.

`foundry status` adds an optional `--span <id>` filter alongside the
existing `--workflow` filter.

## Migration

Hard cutover with self-identifying ID formats:

- **Old events on disk** (`trc_*` trace_ids, no `span_id`): keep
  working in `Trace`, `Span` (which returns "not found" for any
  query whose `span_id` doesn't exist in old data), CLI display
  (`--flat` mode is automatic if no `span_id` is present), and the
  summary code (until `GenerateSummary` is rewritten to query spans
  — see below).
- **Old `~/.foundry/traces/` files**: kept as-is. `serde(default)`
  on the new fields makes them deserialize.
- **`GenerateSummary` transition**: currently reads
  `project_trace_ids` from `MaintenanceRunCompletedPayload`. After
  the rename it reads `project_trace_ids` from the new
  `MaintenanceCycleCompletedPayload` (kept for one release).
  Follow-up: rewrite `GenerateSummary` to query `Span` for the
  cycle's children, then drop `project_trace_ids` from the payload.
- **Workspace version**: bump to a minor (e.g. 0.17.0). Skill
  metadata version follows.
- **No `schema_version` field**, **no `cycle_id` interim field**.

### Historical event backfill

A one-shot script rewrites historical event-type names in archived
data so older traces stay queryable under the new vocabulary.
Span fields (`span_id`, `parent_span_id`) are **not** backfilled —
we lack the tree information to reconstruct accurately, and legacy
events remain identifiable by their `trc_*`-prefixed `trace_id`.

Script: `scripts/migrate-event-names.sh`. Implemented in `jq`
because the cycle-vs-project split needs to inspect the `project`
field, which `sed` can't do reliably.

Files in scope:

1. `~/.foundry/events/*.jsonl` — each line is a JSON event.
2. `~/.foundry/traces/YYYY-MM-DD/*.json` — each file has an
   `events` array (and a `block_executions` array, but blocks don't
   carry event_type).

Conditional renames applied to each event:

| Match                                                        | Rewrite to                  |
|--------------------------------------------------------------|-----------------------------|
| `event_type == "maintenance_run_started"`, `project == "system"` | `"maintenance_cycle_started"` |
| `event_type == "maintenance_run_started"`, `project != "system"` | `"project_run_started"`        |
| `event_type == "maintenance_run_completed"`, `project == "system"` | `"maintenance_cycle_completed"` |
| `event_type == "maintenance_run_completed"`, `project != "system"` | `"project_run_completed"`       |
| `event_type == "iteration_requested"`                         | `"project_iteration_requested"` |
| `event_type == "maintenance_requested"`                       | `"project_maintenance_requested"` |
| `event_type == "greet_requested"`                             | `"greeting_requested"`         |

Concrete shape (JSONL files; trace files use the same expression
inside `(.events[] |=)`):

```bash
for f in ~/.foundry/events/*.jsonl; do
  jq -c '
    if .event_type == "maintenance_run_started" then
      .event_type = (if .project == "system"
                     then "maintenance_cycle_started"
                     else "project_run_started" end)
    elif .event_type == "maintenance_run_completed" then
      .event_type = (if .project == "system"
                     then "maintenance_cycle_completed"
                     else "project_run_completed" end)
    elif .event_type == "iteration_requested"   then .event_type = "project_iteration_requested"
    elif .event_type == "maintenance_requested" then .event_type = "project_maintenance_requested"
    elif .event_type == "greet_requested"       then .event_type = "greeting_requested"
    else . end
  ' "$f" > "$f.new" && mv "$f.new" "$f"
done
```

**Event id consistency**: `Event::id` is derived from
(event_type, project, occurred_at, payload). Renaming `event_type`
breaks the deterministic id relationship for those events. The
script recomputes `id` for every rewritten event so the new
event_type and id remain consistent. Any payload references to old
ids (notably `project_trace_ids` maps inside
`maintenance_run_completed` payloads) are rewritten in the same
pass to point at the new ids — done by building an old-id → new-id
table during the first pass and substituting in the second.

**Safety**: the script writes to `*.new` and atomically renames, so
a kill mid-run leaves the original intact. A `--dry-run` mode prints
counts and example rewrites without touching files. The script is
idempotent — running it twice after success is a no-op (no remaining
old-name events to match).

After the script lands, it's removed (or moved to `scripts/archive/`)
in a follow-up release to avoid implying it should be re-run.

## Implementation phasing

Done in this order, each step a working green-build commit on main:

1. **Foundation in `foundry-core`**:
   - Add `span_id`, `parent_span_id` fields to `Event` and
     `BlockExecution`.
   - Replace `mint_trace_id` with hex output; add `mint_span_id`.
   - Add `is_legacy_trace_id` helper.
   - Add `SpanContext` and `SPAN_CONTEXT` task-local (or in a small
     `foundry-core::span` module).
   - Update existing tests for new ID format. New tests for hex
     length, uniqueness, and legacy detection.

2. **Proto + service plumbing**:
   - Update `proto/foundry.proto` with the new fields and `Span`
     RPC.
   - Update service handlers (`Emit`, `Trace`, `Watch`) to
     populate/return the new fields.
   - Stub the `Span` RPC.
   - Build, regenerate clients.

3. **Engine span stamping**:
   - Rewrite `run_block` and `persist_and_broadcast_events` per
     "Engine changes" above.
   - Update `trace_id_propagates_through_chain` and
     `trace_id_propagates_in_dry_run` tests for span propagation.
   - Add new tests: block-level span_id matches across emitted
     events, parent_span_id chains correctly across nested block
     dispatches.

4. **Event taxonomy rename**:
   - Add new `EventType` variants: `MaintenanceCycleStarted`,
     `MaintenanceCycleCompleted`, `ProjectRunStarted`,
     `ProjectRunCompleted`, `StrategicCycleStarted`,
     `InnerIterationStarted`.
   - Rename existing variants: `IterationRequested` →
     `ProjectIterationRequested`, `MaintenanceRequested` →
     `ProjectMaintenanceRequested`, `GreetRequested` →
     `GreetingRequested`.
   - Remove old `MaintenanceRunStarted`,
     `MaintenanceRunCompleted` (hard rename, no deprecation alias).
   - Rename payload structs in `payload.rs` and split
     cycle-vs-project semantics where they differ. Payload structs
     for the workflow renames (`IterationRequestedPayload` →
     `ProjectIterationRequestedPayload`, etc.) follow.
   - Update `FanOutMaintenance`, `finalise_system_maintenance`,
     `extract_per_project_traces`, `GenerateSummary`, all blocks'
     `sinks_on` lists, the CLI's emit paths, and all associated
     tests.
   - Update AGENTS.md's event-naming taxonomy examples to match.

4.5. **Historical data backfill**:
   - Add `scripts/migrate-event-names.sh` per "Historical event
     backfill" above (jq-based, with `--dry-run` mode and id
     recomputation).
   - Document its one-time use in CHANGELOG and the 0.17.0 release
     notes.
   - The daemon refuses to start if it detects pre-0.17.0 event
     names on disk and the backfill hasn't been run — a clear
     error message points at the script. (Or: refuses to *write*
     summaries until backfill is complete. Final shape decided in
     plan.)

5. **Subprocess propagation**:
   - Wire `SPAN_CONTEXT` through the engine before block dispatch.
   - Update `shell.rs::run_shell` and `agent_stream.rs` to inject
     `TRACEPARENT`.
   - Migrate direct `std::process::Command::new` calls in
     `engine.rs`, `validate.rs`, `install.rs`, `release.rs`,
     `gates_commands.rs` to go through `run_shell` (or a thin
     helper) where reasonable.
   - Update `foundry emit` CLI to read `TRACEPARENT` from env.

6. **`Span` RPC implementation**:
   - Populate the in-memory `span_id → trace_id` and `trace_id →
     [span_id]` indexes in `trace_store.rs`.
   - Implement the `Span` RPC handler.
   - Tests: emit a small chain, assert `Span` returns only the
     events for that span.

7. **CLI display**:
   - `foundry trace`: default tree rendering, `--flat` fallback.
   - `foundry status`: `--span` filter.

8. **Docs**:
   - mdBook chapter on the trace model.
   - AGENTS.md updates (rename examples, mention `TRACEPARENT`,
     mention `Span` RPC).
   - Skill (`skill/foundry/`) updated where workflows reference
     the renamed events.
   - CHANGELOG entry.

## Testing strategy

Per project conventions (functional core / imperative shell):

- **foundry-core**: pure tests for `mint_trace_id` /
  `mint_span_id` (hex length, uniqueness), `Event` round-trip
  (`(trace_id, span_id, parent_span_id)` serialize and deserialize
  correctly, omit when `None`), `is_legacy_trace_id` discrimination.
- **engine**: existing `trace_id_propagates_through_chain` is
  extended; new tests verify `span_id` is per-block, not per-chain;
  `parent_span_id` matches the trigger's `span_id`; dry-run
  simulated events get the same stamps as real ones.
- **orchestrator**: per-project test confirms each `ProjectRunStarted`
  has the cycle's `trace_id`, distinct `span_id`s per project, and
  `parent_span_id` matching the cycle's `span_id`.
- **service**: `Emit` mints fresh trace_id / span_id when absent,
  honors them when present; `Trace` returns the full forest;
  `Span` returns just the requested span.
- **subprocess**: integration test invokes a block that runs
  `printenv TRACEPARENT` via `shell.rs` and asserts the value
  matches the active span context.
- **CLI**: snapshot tests for `foundry trace` tree rendering.

`cargo fmt --all -- --check`, `cargo clippy --workspace -- -D
warnings`, and `cargo test --workspace` must all pass at every
phase boundary.

## Open questions deferred

These are explicitly out of scope for this design but worth
recording so we know we considered them:

- **Span kind** (INTERNAL / PRODUCER / CONSUMER): defer. Almost
  everything in Foundry is INTERNAL today; the distinction adds
  noise without a consumer.
- **Span status** (OK / ERROR / UNSET): the `success` payload
  field on `*Completed` events already carries this. Re-express as
  OTel status only when emitting to an external collector.
- **Span attributes** (key/value map on each span): defer. Today's
  rich payloads serve the same purpose and are queryable.
- **Sampling**: skip. Event volume is low; we record everything.
- **Clock-skew normalization across subprocess boundaries**: skip.
  `recorded_at` is set by `foundryd` on ingest and is authoritative
  for ordering.
- **OTLP / OpenTelemetry Collector export**: future work. The data
  model is now translation-friendly: every Foundry span maps onto
  an OTel span; emitted events become OTel log records or events
  on the span.
- **`project_trace_ids` payload removal**: kept during the
  transition for `GenerateSummary` compatibility. Remove in a
  follow-up after `GenerateSummary` is rewritten to query `Span`.

## File-level impact (informational)

Approximate set of files that change in this work, for plan sizing:

- `proto/foundry.proto`
- `crates/foundry-core/src/event.rs`
- `crates/foundry-core/src/trace.rs`
- `crates/foundry-core/src/payload.rs`
- `crates/foundry-core/src/lib.rs` (re-exports)
- `crates/foundryd/src/engine.rs`
- `crates/foundryd/src/orchestrator.rs`
- `crates/foundryd/src/service.rs`
- `crates/foundryd/src/trace_store.rs`
- `crates/foundryd/src/trace_writer.rs`
- `crates/foundryd/src/shell.rs`
- `crates/foundryd/src/agent_stream.rs`
- `crates/foundryd/src/blocks/generate_summary.rs`
- `crates/foundryd/src/blocks/validate.rs`,
  `install.rs`, `release.rs` (subprocess migration)
- `crates/foundry-cli/src/commands.rs` (trace command UI)
- `crates/foundry-cli/src/gates_commands.rs` (subprocess migration)
- `AGENTS.md`, `CHANGELOG.md`, `book/`, `skill/foundry/`
