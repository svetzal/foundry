# OpenTelemetry-shaped Nested Tracing — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace Foundry's flat `trc_*` workflow trace IDs with OpenTelemetry-shaped nested spans (cycle → project_run → workflow → block), wire W3C `TRACEPARENT` propagation through subprocesses, add a `Span` RPC, and rename event taxonomy to reflect the new model — without changing on-disk event volume or deterministic event ids.

**Architecture:** Each `Event` gains two optional fields — `span_id` (16-hex) and `parent_span_id` (16-hex) — alongside an OTel-shaped `trace_id` (32-hex). The engine stamps these fields when persisting events using two rules: a **default rule** (peers inherit trigger's span) and a **span-opener rule** (registered event types mint a fresh `span_id` parented to the emitting block's span). Block-level spans live on `BlockExecution` only — no synthetic open/close events. A `tokio::task_local!` `SPAN_CONTEXT` exposes the current block's span to subprocess spawners, which inject `TRACEPARENT` env vars. Old `trc_*` ids remain on disk and stay queryable; a one-shot jq script renames historical event types (`maintenance_run_*`, `iteration_requested`, etc.) and recomputes their `Event::id`.

**Tech Stack:** Rust 2024 / 1.85+, tonic/prost gRPC, tokio task-local, serde with `skip_serializing_if = "Option::is_none"`, jq for historical backfill, mdBook for docs.

**Spec:** `docs/superpowers/specs/2026-05-13-otel-nested-tracing-design.md` is the source of truth. Read it before starting — every "Decisions locked" item there is settled and not up for re-debate.

---

## File Structure

Files created or modified across the plan, grouped by responsibility. Each phase touches a subset.

### Created

- `crates/foundryd/src/span_context.rs` — `SpanContext` struct + `SPAN_CONTEXT` `tokio::task_local!`. Lives in `foundryd` (not core) to keep `foundry-core` free of runtime deps.
- `scripts/migrate-event-names.sh` — one-shot jq script that rewrites historical `event_type` names in `~/.foundry/events/*.jsonl` and `~/.foundry/traces/YYYY-MM-DD/*.json`, recomputes `Event::id`, and rewrites payload id references. Has `--dry-run`.
- `book/src/architecture/tracing.md` — mdBook chapter on the trace model (cycle/project_run/workflow/block, stamping rules, `TRACEPARENT`, span-opener registry).

### Modified — `foundry-core`

- `crates/foundry-core/src/event.rs` — `Event` gains `span_id` / `parent_span_id`; replace `mint_trace_id` with 128-bit hex; add `mint_span_id` (64-bit hex), `is_legacy_trace_id`, `is_span_opener` predicate over `EventType`. Add new variants and rename existing ones (see Phase 4).
- `crates/foundry-core/src/trace.rs` — `BlockExecution` gains `span_id` and `parent_span_id`.
- `crates/foundry-core/src/payload.rs` — rename `MaintenanceRunStartedPayload` / `MaintenanceRunCompletedPayload` into a cycle/project pair; rename `IterationRequestedPayload` → `ProjectIterationRequestedPayload`, etc.

### Modified — `foundryd`

- `crates/foundryd/src/engine.rs` — rewrite `persist_and_broadcast_events` to use the stamping rules; mint per-block `block_span_id` in `run_block`; wrap `block.execute` in `SPAN_CONTEXT::scope`.
- `crates/foundryd/src/service.rs` — `Emit` handler mints `trace_id` / `span_id` defaults; renames `MaintenanceRunStarted`/`Completed` callsites to cycle/project pair; implements `Span` RPC.
- `crates/foundryd/src/orchestrator.rs` — `FanOutMaintenance` emits `ProjectRunStarted` (not `MaintenanceRunStarted`), no longer mints fresh `trace_id` per project.
- `crates/foundryd/src/trace_store.rs` — add secondary indexes `span_id → trace_id` and `trace_id → Vec<span_id>`.
- `crates/foundryd/src/trace_writer.rs` — serialize the new `BlockExecution` fields.
- `crates/foundryd/src/shell.rs` — read `SPAN_CONTEXT`, inject `TRACEPARENT` into spawned `Command`.
- `crates/foundryd/src/agent_stream.rs` — same `TRACEPARENT` injection at the Claude Code agent spawn site.
- `crates/foundryd/src/blocks/*.rs` — update all `sinks_on` lists and payload references to renamed event types.
- `crates/foundryd/src/main.rs` — on startup, refuse to start if legacy event-type names are present on disk and migration script hasn't been run.
- `crates/foundryd/src/event_writer.rs`, `crates/foundryd/src/workflow_tracker.rs` — touch wherever they reference renamed types.
- `crates/foundryd/src/blocks/validate.rs`, `crates/foundryd/src/blocks/release.rs`, `crates/foundryd/src/blocks/install.rs` — migrate direct `Command::new` calls to `shell.rs::run_shell` (or thin wrapper).

### Modified — `foundry-cli`

- `crates/foundry-cli/src/commands.rs` — `foundry trace` adds tree rendering with `--flat` fallback; `foundry status` adds `--span` filter; `foundry emit` reads `TRACEPARENT` from env.
- `crates/foundry-cli/src/gates_commands.rs` — subprocess migration like the foundryd blocks.

### Modified — `proto` and generated code

- `proto/foundry.proto` — `Event`, `TraceEvent`, `WatchResponse`, `EmitRequest`, `TraceBlockExecution` gain `span_id` and `parent_span_id`. New `Span` RPC + `SpanRequest` / `SpanResponse` messages. `build.rs` in both `foundryd` and `foundry-cli` regenerates.

### Modified — Docs & metadata

- `AGENTS.md` — update event-naming taxonomy examples to reference new names; document `TRACEPARENT` and `Span` RPC.
- `CHANGELOG.md` — `0.17.0` entry covering the cutover and migration script.
- `Cargo.toml` workspace — bump version to `0.17.0`.
- `skill/foundry/SKILL.md` — bump metadata `version`; update workflows that reference renamed events.

---

## Phase 1 — Foundation in `foundry-core`

**Goal:** Add the new ID minting, span fields, and span-opener predicate. No engine or service code changes yet — this phase produces a green workspace where the new types exist but aren't wired into stamping.

### Task 1.1: Replace `mint_trace_id` with 128-bit hex output

**Files:**

- Modify: `crates/foundry-core/src/event.rs:154-157` (current `mint_trace_id`)
- Modify: `crates/foundry-core/src/event.rs:504-518` (existing tests for old format)

- [ ] **Step 1: Update existing tests to assert hex output (failing)**

Replace `mint_trace_id_produces_trc_prefix` and `mint_trace_id_unique` in `crates/foundry-core/src/event.rs`:

```rust
#[test]
fn mint_trace_id_produces_32_hex_chars() {
    let id = super::mint_trace_id();
    assert_eq!(id.len(), 32, "trace_id must be exactly 32 hex chars");
    assert!(
        id.chars().all(|c| c.is_ascii_hexdigit() && (c.is_ascii_digit() || c.is_ascii_lowercase())),
        "trace_id must be lowercase hex only: {id}"
    );
}

#[test]
fn mint_trace_id_unique() {
    let id1 = super::mint_trace_id();
    let id2 = super::mint_trace_id();
    assert_ne!(id1, id2);
}

#[test]
fn is_legacy_trace_id_recognizes_old_format() {
    assert!(super::is_legacy_trace_id("trc_abc123"));
    assert!(!super::is_legacy_trace_id(&super::mint_trace_id()));
    assert!(!super::is_legacy_trace_id(""));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p foundry-core mint_trace_id is_legacy_trace_id -- --nocapture`
Expected: FAIL — `is_legacy_trace_id` doesn't exist; `mint_trace_id` still returns `trc_*`.

- [ ] **Step 3: Replace `mint_trace_id` implementation and add `is_legacy_trace_id`**

In `crates/foundry-core/src/event.rs`, replace the existing `mint_trace_id`:

```rust
use rand::RngCore;

/// Generate a fresh 128-bit trace ID as 32 lowercase hex characters.
///
/// The format is OpenTelemetry-compatible: no prefix, lowercase hex,
/// fixed length. Used as a workflow / cycle root identifier.
pub fn mint_trace_id() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// Generate a fresh 64-bit span ID as 16 lowercase hex characters.
///
/// The format is OpenTelemetry-compatible.
pub fn mint_span_id() -> String {
    let mut bytes = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// True if `id` is a pre-OTel-tracing trace ID (legacy `trc_<uuid>` format).
///
/// Used by analysis and migration code to distinguish events written before
/// the OTel-shaped tracing cutover from new-format ones.
pub fn is_legacy_trace_id(id: &str) -> bool {
    id.starts_with("trc_")
}
```

Remove the existing `mint_trace_id` (lines 155-157) — it's replaced, not retained alongside.

Add `rand` to `crates/foundry-core/Cargo.toml` under `[dependencies]` (use the workspace version if already declared in the workspace; otherwise add `rand = "0.8"`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p foundry-core` and grep for the three test names.
Expected: PASS.

- [ ] **Step 5: Add a test for `mint_span_id` format**

```rust
#[test]
fn mint_span_id_produces_16_hex_chars() {
    let id = super::mint_span_id();
    assert_eq!(id.len(), 16, "span_id must be exactly 16 hex chars");
    assert!(
        id.chars().all(|c| c.is_ascii_hexdigit() && (c.is_ascii_digit() || c.is_ascii_lowercase())),
        "span_id must be lowercase hex only: {id}"
    );
    assert_ne!(id, super::mint_span_id(), "two mints must differ");
}
```

Run: `cargo test -p foundry-core mint_span_id`
Expected: PASS.

- [ ] **Step 6: Update unrelated tests that hard-coded the `trc_` format**

Inspect `crates/foundry-core/src/event.rs` test module for any remaining `trc_` literals that aren't testing `is_legacy_trace_id`. Replace with calls to `mint_trace_id()` or accept any string. There are at least two callsites at lines ~469 and ~497.

Run: `cargo test -p foundry-core`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add crates/foundry-core/src/event.rs crates/foundry-core/Cargo.toml Cargo.lock
git commit -m "feat(core): replace mint_trace_id with OTel hex; add mint_span_id and is_legacy_trace_id"
```

### Task 1.2: Add `span_id` and `parent_span_id` to `Event`

**Files:**

- Modify: `crates/foundry-core/src/event.rs:10-29` (Event struct)

- [ ] **Step 1: Write the failing round-trip test**

Append to the `tests` module in `crates/foundry-core/src/event.rs`:

```rust
#[test]
fn span_fields_round_trip_when_present() {
    let event = Event::new(
        EventType::VulnerabilityDetected,
        "p".to_string(),
        Throttle::Full,
        serde_json::json!({}),
    )
    .with_trace_id(Some("0123456789abcdef0123456789abcdef".to_string()))
    .with_span_ids(Some("0123456789abcdef".to_string()), Some("fedcba9876543210".to_string()));

    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json["span_id"], "0123456789abcdef");
    assert_eq!(json["parent_span_id"], "fedcba9876543210");

    let restored: Event = serde_json::from_value(json).unwrap();
    assert_eq!(restored.span_id.as_deref(), Some("0123456789abcdef"));
    assert_eq!(restored.parent_span_id.as_deref(), Some("fedcba9876543210"));
}

#[test]
fn span_fields_omitted_from_json_when_none() {
    let event = Event::new(
        EventType::VulnerabilityDetected,
        "p".to_string(),
        Throttle::Full,
        serde_json::json!({}),
    );
    let json = serde_json::to_value(&event).unwrap();
    assert!(json.get("span_id").is_none(), "span_id must be absent when None");
    assert!(json.get("parent_span_id").is_none(), "parent_span_id must be absent when None");
}

#[test]
fn event_id_does_not_depend_on_span_fields() {
    let base = Event::new(
        EventType::VulnerabilityDetected,
        "p".to_string(),
        Throttle::Full,
        serde_json::json!({"x": 1}),
    );
    let with_spans = base.clone()
        .with_trace_id(Some(super::mint_trace_id()))
        .with_span_ids(Some(super::mint_span_id()), Some(super::mint_span_id()));
    assert_eq!(base.id, with_spans.id, "span metadata must not change Event::id");
}
```

Run: `cargo test -p foundry-core span_fields event_id_does_not_depend`
Expected: FAIL — `span_id`, `parent_span_id`, and `with_span_ids` don't exist.

- [ ] **Step 2: Add fields and builder method**

In `crates/foundry-core/src/event.rs`, modify the `Event` struct (around line 10):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub event_type: EventType,
    pub project: String,
    pub occurred_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
    pub throttle: Throttle,

    /// 32-char lowercase hex. Identifies the whole tree (cycle root through
    /// leaf blocks). Inherited from parent span; minted only at root spans.
    /// Legacy events use a `trc_*` UUID format — distinguishable via
    /// [`is_legacy_trace_id`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,

    /// 16-char lowercase hex. Identifies the span this event belongs to.
    /// Every event participating in a span shares the same `span_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,

    /// 16-char lowercase hex. The span that caused this one to open.
    /// `None` for root spans.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,

    pub payload: serde_json::Value,
}
```

Update `Event::new` to initialize `span_id: None, parent_span_id: None` alongside `trace_id: None`.

Add the builder method right after `with_trace_id`:

```rust
/// Attach span IDs to this event (builder pattern). `parent_span_id = None`
/// indicates a root span.
#[must_use]
pub fn with_span_ids(
    mut self,
    span_id: Option<String>,
    parent_span_id: Option<String>,
) -> Self {
    self.span_id = span_id;
    self.parent_span_id = parent_span_id;
    self
}
```

Confirm `compute_id` is **unchanged** — it must not hash span fields. The third test (`event_id_does_not_depend_on_span_fields`) enforces this.

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p foundry-core`
Expected: PASS — including the three new tests and all existing tests.

- [ ] **Step 4: Commit**

```bash
git add crates/foundry-core/src/event.rs
git commit -m "feat(core): add span_id and parent_span_id to Event; preserve deterministic id"
```

### Task 1.3: Add `span_id` and `parent_span_id` to `BlockExecution`

**Files:**

- Modify: `crates/foundry-core/src/trace.rs:18-67`

- [ ] **Step 1: Write failing round-trip test**

Append to the `tests` module in `crates/foundry-core/src/trace.rs`:

```rust
#[test]
fn block_execution_span_fields_round_trip() {
    let mut b = BlockExecution::new("X", "evt_abc", 10, serde_json::json!({}));
    b.span_id = Some("0123456789abcdef".to_string());
    b.parent_span_id = Some("fedcba9876543210".to_string());

    let json = serde_json::to_value(&b).unwrap();
    assert_eq!(json["span_id"], "0123456789abcdef");
    assert_eq!(json["parent_span_id"], "fedcba9876543210");

    let restored: BlockExecution = serde_json::from_value(json).unwrap();
    assert_eq!(restored.span_id.as_deref(), Some("0123456789abcdef"));
    assert_eq!(restored.parent_span_id.as_deref(), Some("fedcba9876543210"));
}

#[test]
fn block_execution_span_fields_deserialize_default_none() {
    let json = serde_json::json!({
        "block_name": "X",
        "trigger_event_id": "evt_abc",
        "success": true,
        "summary": "",
        "emitted_event_ids": [],
        "duration_ms": 0,
        "trigger_payload": {},
        "emitted_payloads": []
    });
    let b: BlockExecution = serde_json::from_value(json).unwrap();
    assert!(b.span_id.is_none(), "span_id missing from on-disk record must deserialize as None");
    assert!(b.parent_span_id.is_none());
}
```

Run: `cargo test -p foundry-core block_execution_span_fields`
Expected: FAIL — fields don't exist.

- [ ] **Step 2: Add fields**

In `crates/foundry-core/src/trace.rs`, modify the `BlockExecution` struct (lines 18-44):

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default)]
    pub audit_artifacts: Vec<String>,

    /// This block's own span_id. The block's span is a child of the
    /// workflow span this block runs inside. Events emitted by the
    /// block do **not** carry this `span_id` — they carry the workflow's
    /// `span_id` under the default propagation rule. Use
    /// `emitted_event_ids` to find what this block produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,

    /// The workflow span this block executes inside (= the trigger event's
    /// `span_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
}
```

Update `BlockExecution::new` to initialize the two new fields to `None`.

Update the test helper `fn block(...)` (~line 116) and `fn block(...)` in any tests that build a `BlockExecution` literally, to add `span_id: None, parent_span_id: None`.

- [ ] **Step 3: Run tests to verify they pass**

Run: `cargo test -p foundry-core`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/foundry-core/src/trace.rs
git commit -m "feat(core): add span_id and parent_span_id to BlockExecution"
```

### Task 1.4: Add the span-opener predicate

**Files:**

- Modify: `crates/foundry-core/src/event.rs` (append at module level, after `EventType`)

The full set of span openers is **only knowable** after Phase 4 adds the new variants. For Phase 1, define the predicate with the variants that exist today and `TODO` comments for the new ones — the predicate will be expanded in Phase 4.

- [ ] **Step 1: Write failing test**

Append to the `tests` module in `crates/foundry-core/src/event.rs`:

```rust
#[test]
fn is_span_opener_identifies_workflow_requests() {
    assert!(super::is_span_opener(&EventType::IterationRequested));
    assert!(super::is_span_opener(&EventType::MaintenanceRequested));
    assert!(super::is_span_opener(&EventType::ValidationRequested));
    assert!(super::is_span_opener(&EventType::DriftAssessmentRequested));
    assert!(super::is_span_opener(&EventType::ReleaseRequested));
    assert!(super::is_span_opener(&EventType::PipelineCheckRequested));
    assert!(super::is_span_opener(&EventType::GreetRequested));
    assert!(super::is_span_opener(&EventType::RemediationStarted));

    // Negative cases — completion events are NOT openers.
    assert!(!super::is_span_opener(&EventType::ProjectIterationCompleted));
    assert!(!super::is_span_opener(&EventType::VulnerabilityDetected));
    assert!(!super::is_span_opener(&EventType::GreetingDelivered));
}
```

Run: `cargo test -p foundry-core is_span_opener`
Expected: FAIL — function doesn't exist.

- [ ] **Step 2: Add the predicate**

In `crates/foundry-core/src/event.rs`, append after the `EventType` enum and its `as_str` impl:

```rust
/// True if `event_type` is a registered **span opener** — meaning the
/// engine's stamping pass should mint a fresh `span_id` for this event
/// and parent it to the emitting block's span_id.
///
/// See the OTel nested tracing design spec for the registry rules.
/// In Phase 4 the new cycle/project_run/strategic-loop openers are
/// added to this list.
pub fn is_span_opener(event_type: &EventType) -> bool {
    matches!(
        event_type,
        EventType::IterationRequested
            | EventType::MaintenanceRequested
            | EventType::ValidationRequested
            | EventType::DriftAssessmentRequested
            | EventType::ReleaseRequested
            | EventType::PipelineCheckRequested
            | EventType::GreetRequested
            | EventType::RemediationStarted
        // Phase 4 will add:
        //   MaintenanceCycleStarted, ProjectRunStarted,
        //   StrategicCycleStarted, InnerIterationStarted
        // and rename the *Requested variants above to their noun forms.
    )
}
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p foundry-core is_span_opener`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/foundry-core/src/event.rs
git commit -m "feat(core): add is_span_opener predicate for the span-opener registry"
```

### Task 1.5: Verify Phase 1 workspace builds cleanly

- [ ] **Step 1: Run all quality gates**

Run: `cargo fmt --all -- --check && cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: PASS.

If clippy complains about an unused `is_legacy_trace_id` (which Phase 1 leaves unused), suppress with `#[allow(dead_code)]` on the function — it will be used by Phase 4.5's startup check.

- [ ] **Step 2: Push to main**

```bash
git push origin main
```

---

## Phase 2 — Proto + Service Plumbing

**Goal:** Plumb the new `span_id` / `parent_span_id` fields and the new `Span` RPC through proto. Stub the RPC handler. Service handlers (`Emit`, `Trace`, `Watch`) carry the fields end-to-end but do not yet mint or stamp them — that's Phase 3.

### Task 2.1: Add new proto fields and `Span` RPC

**Files:**

- Modify: `proto/foundry.proto`

- [ ] **Step 1: Add fields to existing messages**

In `proto/foundry.proto`, add fields:

```protobuf
// In `message Event` (after line ~21, `string trace_id = 7;`):
    string span_id = 8;         // Identifies the span this event belongs to.
    string parent_span_id = 9;  // Empty for root spans.
```

```protobuf
// In `message EmitRequest` (after the existing trace_id = 5):
    string span_id = 6;         // Optional. Auto-minted if empty.
    string parent_span_id = 7;  // Optional. Empty ⇒ root span.
```

```protobuf
// In `message TraceBlockExecution` (after exit_code = 9, before line ~78
// trigger_payload_json = 10):
    string span_id = 13;
    string parent_span_id = 14;
```

```protobuf
// In `message TraceEvent` (after trace_id = 6):
    string span_id = 7;
    string parent_span_id = 8;
```

```protobuf
// In `message WatchResponse` (after trace_id = 5):
    string span_id = 6;
    string parent_span_id = 7;
```

Field numbers must not collide with existing ones — check each message before assigning. The numbers above are chosen to extend each message contiguously.

- [ ] **Step 2: Add the `Span` RPC**

In `proto/foundry.proto`, add at the bottom of the `service Foundry` block (after `rpc RegistryEdit`):

```protobuf
    // Retrieve every event and block execution within a single span.
    rpc Span(SpanRequest) returns (SpanResponse);
```

Add the request/response messages near the existing `TraceRequest`/`TraceResponse`:

```protobuf
// Request: retrieve a single span's events and block executions.
message SpanRequest {
    string span_id = 1;
}

// Response containing the events and blocks for one span.
message SpanResponse {
    bool found = 1;
    repeated TraceEvent events = 2;
    repeated TraceBlockExecution block_executions = 3;
    uint64 total_duration_ms = 4;
}
```

- [ ] **Step 3: Run a build to regenerate clients**

Run: `cargo build --workspace`
Expected: SUCCESS — both `foundryd` and `foundry-cli` have `build.rs` scripts that regenerate proto bindings. There will be **compile errors** in service.rs / commands.rs because the new fields and RPC exist in the generated code but are not yet referenced. That's fine — we fix those in Task 2.2.

Actually you may get errors even before fix-up. If the build fails with errors *unrelated* to the new fields (e.g. proto syntax errors), fix those before proceeding.

- [ ] **Step 4: Commit**

```bash
git add proto/foundry.proto
git commit -m "proto: add span_id, parent_span_id, and Span RPC"
```

### Task 2.2: Plumb new fields through `Emit`, `Trace`, and `Watch` handlers

**Files:**

- Modify: `crates/foundryd/src/service.rs`

Search for `trace_id` in `service.rs` to find every place it's read from a request or written to a response. The new fields parallel it.

- [ ] **Step 1: Update the `Emit` handler request parsing**

Around `service.rs:255` where `trace_id` is minted-on-empty, extend the same pattern:

```rust
// Currently: a line minting `trace_id` when the request omits it.
// Add similar handling for span_id / parent_span_id.
let request_trace_id = if req.trace_id.is_empty() {
    foundry_core::event::mint_trace_id()
} else {
    req.trace_id.clone()
};
let request_span_id = if req.span_id.is_empty() {
    None
} else {
    Some(req.span_id.clone())
};
let request_parent_span_id = if req.parent_span_id.is_empty() {
    None
} else {
    Some(req.parent_span_id.clone())
};
```

Then when constructing the synthesised `Event`, set `.with_span_ids(request_span_id, request_parent_span_id)` alongside `.with_trace_id(Some(request_trace_id))`.

(Note: actual Emit stamping — minting a fresh `span_id` when one isn't supplied — lands in Phase 3. For now we plumb whatever the request carried.)

- [ ] **Step 2: Update `Trace` handler response building**

Find where `Trace` response builds `TraceEvent` records (search for `TraceEvent {`). For each `TraceEvent` constructed, add:

```rust
span_id: event.span_id.clone().unwrap_or_default(),
parent_span_id: event.parent_span_id.clone().unwrap_or_default(),
```

Likewise for `TraceBlockExecution`:

```rust
span_id: block.span_id.clone().unwrap_or_default(),
parent_span_id: block.parent_span_id.clone().unwrap_or_default(),
```

- [ ] **Step 3: Update `Watch` handler**

Find where `WatchResponse` is built in `service.rs`. Add:

```rust
span_id: event.span_id.clone().unwrap_or_default(),
parent_span_id: event.parent_span_id.clone().unwrap_or_default(),
```

- [ ] **Step 4: Run build**

Run: `cargo build --workspace`
Expected: SUCCESS.

### Task 2.3: Stub the `Span` RPC

**Files:**

- Modify: `crates/foundryd/src/service.rs`

- [ ] **Step 1: Add stub handler**

Add a stub method on the service impl (the type that implements `Foundry`):

```rust
async fn span(
    &self,
    request: tonic::Request<SpanRequest>,
) -> Result<tonic::Response<SpanResponse>, tonic::Status> {
    let _ = request; // populated in Phase 6
    Ok(tonic::Response::new(SpanResponse {
        found: false,
        events: vec![],
        block_executions: vec![],
        total_duration_ms: 0,
    }))
}
```

The real implementation lands in Phase 6 once trace_store has indexes.

- [ ] **Step 2: Run build and tests**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS.

- [ ] **Step 3: Commit and verify quality gates**

```bash
cargo fmt --all -- --check && cargo clippy --workspace -- -D warnings
git add crates/foundryd/src/service.rs
git commit -m "feat(service): plumb span fields through Emit/Trace/Watch; stub Span RPC"
git push origin main
```

---

## Phase 3 — Engine span stamping

**Goal:** The engine becomes the authority on span propagation. `run_block` mints a fresh `block_span_id` per dispatch and records it on `BlockExecution`. `persist_and_broadcast_events` applies the two stamping rules — default and span-opener — to every emitted event.

### Task 3.1: Mint `block_span_id` in `run_block`

**Files:**

- Modify: `crates/foundryd/src/engine.rs:136-241` (the existing `run_block`)

- [ ] **Step 1: Write failing test for block span_id population**

In `crates/foundryd/src/engine.rs` tests module, add:

```rust
#[tokio::test]
async fn block_execution_records_block_span_id() {
    let block = simple_block("B", &[EventType::VulnerabilityDetected], vec![]);
    let engine = Engine::new().with_block(block);
    let trigger = Event::new(
        EventType::VulnerabilityDetected,
        "p".to_string(),
        Throttle::Full,
        serde_json::json!({}),
    )
    .with_trace_id(Some(foundry_core::event::mint_trace_id()))
    .with_span_ids(Some(foundry_core::event::mint_span_id()), None);

    let workflow_span = trigger.span_id.clone();
    let result = engine.process(trigger).await;

    let block_exec = &result.block_executions[0];
    let block_span = block_exec.span_id.as_ref().expect("block must have span_id");
    assert_eq!(block_span.len(), 16, "block span_id must be 16 hex chars");
    assert_eq!(block_exec.parent_span_id, workflow_span,
        "block parent_span_id must equal the triggering event's span_id");
}
```

You may need to inspect existing test helpers (`simple_block` or similar). If a suitable helper doesn't exist, add a minimal `TaskBlock` impl in the test module that sinks on `VulnerabilityDetected` and emits no events.

Run: `cargo test -p foundryd block_execution_records_block_span_id`
Expected: FAIL — `BlockExecution::span_id` is currently `None`.

- [ ] **Step 2: Mint `block_span_id` and record it**

In `run_block`, before the existing `let block_start = std::time::Instant::now();`, capture the workflow span context from the trigger:

```rust
let block_span_id = foundry_core::event::mint_span_id();
let workflow_span_id = current.span_id.clone(); // the parent workflow span
```

Then every `BlockExecution { ... }` literal inside `run_block` (there are three: dry-run, throttle-skip, success, error) must include:

```rust
span_id: Some(block_span_id.clone()),
parent_span_id: workflow_span_id.clone(),
```

You'll need to spread these through all four return paths in `run_block` (dry-run pretend-success, throttle skip, retry success, retry error).

- [ ] **Step 3: Run test to verify it passes**

Run: `cargo test -p foundryd block_execution_records_block_span_id`
Expected: PASS.

- [ ] **Step 4: Run all engine tests to catch regressions**

Run: `cargo test -p foundryd`
Expected: PASS. The existing `trace_id_propagates_*` tests should still pass — they don't yet assert anything about span_id.

- [ ] **Step 5: Commit**

```bash
git add crates/foundryd/src/engine.rs
git commit -m "feat(engine): mint per-block span_id and record it on BlockExecution"
```

### Task 3.2: Implement the two stamping rules in `persist_and_broadcast_events`

**Files:**

- Modify: `crates/foundryd/src/engine.rs:99-131` (`persist_and_broadcast_events`)

- [ ] **Step 1: Write failing tests for both stamping rules**

```rust
#[tokio::test]
async fn default_rule_emitted_event_inherits_trigger_span() {
    let workflow_span = foundry_core::event::mint_span_id();
    let trace = foundry_core::event::mint_trace_id();
    let trigger = Event::new(
        EventType::VulnerabilityDetected,
        "p".to_string(),
        Throttle::Full,
        serde_json::json!({}),
    )
    .with_trace_id(Some(trace.clone()))
    .with_span_ids(Some(workflow_span.clone()), Some("0000000000000001".to_string()));

    // A block that emits a non-opener event (e.g. ProjectChangesPushed):
    let block = emitting_block(
        "B",
        EventType::VulnerabilityDetected,
        vec![EventType::ProjectChangesPushed],
    );
    let engine = Engine::new().with_block(block);
    let result = engine.process(trigger).await;

    let emitted: Vec<&Event> = result
        .events
        .iter()
        .filter(|e| e.event_type == EventType::ProjectChangesPushed)
        .collect();
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].trace_id.as_deref(), Some(trace.as_str()));
    assert_eq!(emitted[0].span_id, Some(workflow_span));
    assert_eq!(emitted[0].parent_span_id.as_deref(), Some("0000000000000001"));
}

#[tokio::test]
async fn span_opener_rule_mints_fresh_span_parented_to_block() {
    let workflow_span = foundry_core::event::mint_span_id();
    let trace = foundry_core::event::mint_trace_id();
    let trigger = Event::new(
        EventType::PipelineChecked,
        "p".to_string(),
        Throttle::Full,
        serde_json::json!({}),
    )
    .with_trace_id(Some(trace.clone()))
    .with_span_ids(Some(workflow_span), None);

    // A block that emits a workflow opener (IterationRequested).
    let block = emitting_block(
        "B",
        EventType::PipelineChecked,
        vec![EventType::IterationRequested],
    );
    let engine = Engine::new().with_block(block);
    let result = engine.process(trigger).await;

    let opener = result.events.iter()
        .find(|e| e.event_type == EventType::IterationRequested)
        .expect("opener must be emitted");
    let block_exec = result.block_executions.iter().find(|b| b.block_name == "B").unwrap();

    assert_eq!(opener.trace_id.as_deref(), Some(trace.as_str()),
        "trace_id propagates through opener");
    assert_ne!(opener.span_id, block_exec.parent_span_id,
        "opener gets a FRESH span_id, not the workflow span");
    assert_eq!(opener.parent_span_id, block_exec.span_id,
        "opener's parent is the block's own span");
    assert_eq!(opener.span_id.as_ref().map(String::len), Some(16),
        "minted span_id is 16 hex chars");
}
```

(`emitting_block` is a test helper that builds a `TaskBlock` impl emitting a fixed list of event types when triggered. If it doesn't exist, add one to the test module.)

Run: `cargo test -p foundryd default_rule span_opener_rule`
Expected: FAIL — `persist_and_broadcast_events` does not stamp span fields yet.

- [ ] **Step 2: Replace the stamping logic**

In `engine.rs`, modify `persist_and_broadcast_events` to take `block_span_id` and apply the rules. The function signature changes; update `run_block`'s call sites at lines ~153, ~201 to pass it.

Replace the body of the loop in `persist_and_broadcast_events`:

```rust
for mut emitted in events {
    Self::stamp_span_context(&mut emitted, trigger, block_span_id);
    if let Some(writer) = &self.event_writer {
        if let Err(e) = writer.write(&emitted) {
            tracing::warn!(error = %e, event_id = %emitted.id, "failed to write event to JSONL");
        }
    }
    if let Some(tx) = &self.event_tx {
        let _ = tx.send(emitted.clone());
    }
    emitted_ids.push(emitted.id.clone());
    emitted_payloads.push(emitted.payload.clone());
    all_events.push(emitted.clone());
    if deliver {
        queue.push(emitted);
    } else {
        tracing::info!(event_type = %emitted.event_type, "event logged but delivery throttled");
    }
}
```

Add the helper as an associated function on `Engine`:

```rust
/// Apply OTel-shaped span stamping to an emitted event.
///
/// All stamping is "set if unset", so a block may emit an event with
/// explicit span context and that context is preserved.
fn stamp_span_context(emitted: &mut Event, trigger: &Event, block_span_id: &str) {
    use foundry_core::event::{is_span_opener, mint_span_id};

    if emitted.trace_id.is_none() {
        emitted.trace_id.clone_from(&trigger.trace_id);
    }

    if is_span_opener(&emitted.event_type) {
        // New workflow span: child of the emitting block's span.
        if emitted.span_id.is_none() {
            emitted.span_id = Some(mint_span_id());
        }
        if emitted.parent_span_id.is_none() {
            emitted.parent_span_id = Some(block_span_id.to_string());
        }
    } else {
        // Default: peer of trigger, attached to the same workflow span.
        if emitted.span_id.is_none() {
            emitted.span_id.clone_from(&trigger.span_id);
        }
        if emitted.parent_span_id.is_none() {
            emitted.parent_span_id.clone_from(&trigger.parent_span_id);
        }
    }
}
```

Update `run_block` to pass `&block_span_id` into both `persist_and_broadcast_events` calls.

- [ ] **Step 3: Run tests**

Run: `cargo test -p foundryd default_rule span_opener_rule`
Expected: PASS.

- [ ] **Step 4: Extend the existing trace_id propagation tests with span assertions**

Update `trace_id_propagates_through_chain` (around line 1091) to also assert that every chained event has a `span_id`. Update `trace_id_propagates_in_dry_run` (around line 1143) similarly — dry-run synthesised events should also be stamped.

Run: `cargo test -p foundryd trace_id_propagates`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/foundryd/src/engine.rs
git commit -m "feat(engine): apply OTel span stamping rules in persist_and_broadcast_events"
```

### Task 3.3: Phase 3 quality gates

- [ ] **Step 1: Run full workspace gates**

Run: `cargo fmt --all -- --check && cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: PASS.

- [ ] **Step 2: Push**

```bash
git push origin main
```

---

## Phase 4 — Event taxonomy rename

**Goal:** Split cycle vs project events; normalize workflow `*Requested` events to noun form; add `*Started` openers for the strategic loop. Cutover is hard — no aliases.

### Task 4.1: Add new `EventType` variants

**Files:**

- Modify: `crates/foundry-core/src/event.rs` (`EventType` enum, around line 203)

- [ ] **Step 1: Add new variants**

In `crates/foundry-core/src/event.rs`, modify `EventType`:

```rust
pub enum EventType {
    // ... existing variants ...

    // Maintenance cycle / project run lifecycle (replaces MaintenanceRunStarted/Completed split)
    MaintenanceCycleStarted,
    MaintenanceCycleCompleted,
    ProjectRunStarted,
    ProjectRunCompleted,

    // Strategic loop openers (newly added — closers already exist)
    StrategicCycleStarted,
    InnerIterationStarted,

    // ... existing variants ...
}
```

Add the variants in their logical groups in the enum (keep variants grouped by domain as the file already does).

- [ ] **Step 2: Update the snake_case round-trip tests**

In the `all_event_type_variants_serialize_as_snake_case` and `all_variants_round_trip_through_from_str` test arrays, append:

```rust
(EventType::MaintenanceCycleStarted, "maintenance_cycle_started"),
(EventType::MaintenanceCycleCompleted, "maintenance_cycle_completed"),
(EventType::ProjectRunStarted, "project_run_started"),
(EventType::ProjectRunCompleted, "project_run_completed"),
(EventType::StrategicCycleStarted, "strategic_cycle_started"),
(EventType::InnerIterationStarted, "inner_iteration_started"),
```

Run: `cargo test -p foundry-core`
Expected: PASS — new variants compile and round-trip.

- [ ] **Step 3: Commit**

```bash
git add crates/foundry-core/src/event.rs
git commit -m "feat(core): add MaintenanceCycle*, ProjectRun*, StrategicCycleStarted, InnerIterationStarted variants"
```

### Task 4.2: Rename workflow `*Requested` variants to noun form

**Files:**

- Modify: `crates/foundry-core/src/event.rs`
- Modify: every file that references `IterationRequested`, `MaintenanceRequested`, or `GreetRequested`

Renames in this task:

| Old | New |
|---|---|
| `IterationRequested` | `ProjectIterationRequested` |
| `MaintenanceRequested` | `ProjectMaintenanceRequested` |
| `GreetRequested` | `GreetingRequested` |

- [ ] **Step 1: Find all callsites**

Run:

```bash
grep -rln 'IterationRequested\|MaintenanceRequested\|GreetRequested' crates/ --include='*.rs'
```

Expect output to include `event.rs`, `payload.rs`, several blocks, `service.rs`, `commands.rs`, and tests. Save the list.

- [ ] **Step 2: Rename the variants in `event.rs`**

In `crates/foundry-core/src/event.rs`:

- Rename enum variants: `IterationRequested` → `ProjectIterationRequested`, `MaintenanceRequested` → `ProjectMaintenanceRequested`, `GreetRequested` → `GreetingRequested`.
- Update the two test arrays' string mappings:
  - `"iteration_requested"` → `"project_iteration_requested"`
  - `"maintenance_requested"` → `"project_maintenance_requested"`
  - `"greet_requested"` → `"greeting_requested"`
- Update `is_span_opener` to reference the new variant names. Also extend its match to include `MaintenanceCycleStarted`, `ProjectRunStarted`, `StrategicCycleStarted`, `InnerIterationStarted`:

```rust
pub fn is_span_opener(event_type: &EventType) -> bool {
    matches!(
        event_type,
        EventType::MaintenanceCycleStarted
            | EventType::ProjectRunStarted
            | EventType::ProjectIterationRequested
            | EventType::ProjectMaintenanceRequested
            | EventType::ValidationRequested
            | EventType::DriftAssessmentRequested
            | EventType::ReleaseRequested
            | EventType::PipelineCheckRequested
            | EventType::GreetingRequested
            | EventType::RemediationStarted
            | EventType::StrategicCycleStarted
            | EventType::InnerIterationStarted
    )
}
```

Update the `is_span_opener_identifies_workflow_requests` test to use the new variant names and add positive assertions for the new openers.

- [ ] **Step 3: Build to find all consumers**

Run: `cargo build --workspace 2>&1 | tee /tmp/build-errors.log`
Expected: Many compile errors — every consumer of the old variant names.

- [ ] **Step 4: Fix each compile error**

For each file in the grep list, replace:

- `EventType::IterationRequested` → `EventType::ProjectIterationRequested`
- `EventType::MaintenanceRequested` → `EventType::ProjectMaintenanceRequested`
- `EventType::GreetRequested` → `EventType::GreetingRequested`

For blocks where `sinks_on` lists the old variant, update those arrays.

For payload struct renames (Task 4.3 handles the actual struct rename), tolerate temporary compile errors here — Task 4.3 cleans them up.

Run: `cargo build --workspace`
Expected: SUCCESS (or remaining errors only about payload struct names, which 4.3 fixes).

- [ ] **Step 5: Commit (even if 4.3 hasn't run yet, this should compile)**

```bash
git add -u
git commit -m "refactor(events): rename workflow *Requested events to noun form"
```

### Task 4.3: Rename and split payload structs

**Files:**

- Modify: `crates/foundry-core/src/payload.rs`

- [ ] **Step 1: Rename workflow *Requested payloads**

In `crates/foundry-core/src/payload.rs`:

- `IterationRequestedPayload` (around line 431) → `ProjectIterationRequestedPayload`
- `MaintenanceRequestedPayload` (around line 446) → `ProjectMaintenanceRequestedPayload`
- `GreetRequestedPayload` (around line 114) → `GreetingRequestedPayload`

Update doc comments to match.

- [ ] **Step 2: Split MaintenanceRun payloads into cycle/project pair**

Replace `MaintenanceRunStartedPayload` and `MaintenanceRunCompletedPayload` (around lines 540-557) with:

```rust
/// Payload for `MaintenanceCycleStarted` (cycle-root, emitted by the scheduler / `foundry run`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceCycleStartedPayload {
    pub project_count: u64,
}

/// Payload for `ProjectRunStarted` (per-project, emitted by `FanOutMaintenance`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRunStartedPayload {
    // currently empty — the project name lives on the Event itself.
}

/// Payload for `MaintenanceCycleCompleted` (cycle-level, synthesised by `finalise_system_maintenance`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MaintenanceCycleCompletedPayload {
    /// Kept during the transition for `GenerateSummary`. Will be removed
    /// once that block queries `Span` instead.
    #[serde(default)]
    pub project_trace_ids: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub skipped_projects: Vec<String>,
    #[serde(default)]
    pub total_duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_event_id: Option<String>,
}

/// Payload for `ProjectRunCompleted` (per-project, emitted by `service.rs`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectRunCompletedPayload {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_event_id: Option<String>,
}
```

- [ ] **Step 3: Update all payload-test cases in the module**

Grep `payload.rs` for any remaining references to the old payload struct names (e.g. `IterationRequestedPayload`, `MaintenanceRunStartedPayload`) — there are several in `#[cfg(test)] mod tests`. Replace them with the renamed equivalents.

- [ ] **Step 4: Update every consumer of these payload structs**

Search workspace-wide:

```bash
grep -rln 'IterationRequestedPayload\|MaintenanceRequestedPayload\|MaintenanceRunStartedPayload\|MaintenanceRunCompletedPayload\|GreetRequestedPayload' crates/ --include='*.rs'
```

For each result, replace with the new payload struct name. Note that `MaintenanceRunStartedPayload` callers may need to choose between the cycle and project variants based on whether they emit at the system level or per-project — see Task 4.4.

- [ ] **Step 5: Build**

Run: `cargo build --workspace`
Expected: SUCCESS (or remaining errors only about MaintenanceRunStarted/Completed event-type usage, which 4.4 fixes).

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "refactor(core): rename and split MaintenanceRun payloads into cycle/project pair; noun-form workflow request payloads"
```

### Task 4.4: Replace `MaintenanceRunStarted`/`Completed` event usage

**Files:**

- Modify: `crates/foundryd/src/orchestrator.rs` (FanOutMaintenance — emits per-project)
- Modify: `crates/foundryd/src/service.rs` (cycle entry + finalise)
- Modify: `crates/foundryd/src/blocks/generate_summary.rs` (reads cycle completion)
- Modify: `crates/foundry-core/src/event.rs` — **remove** the old `MaintenanceRunStarted` and `MaintenanceRunCompleted` variants
- Modify: every block whose `sinks_on` includes them

- [ ] **Step 1: Locate every usage**

```bash
grep -rln 'MaintenanceRunStarted\|MaintenanceRunCompleted' crates/ --include='*.rs'
```

- [ ] **Step 2: For each callsite, determine cycle vs project**

The rule:

- Service-layer code that emits at `project == "system"` → use `MaintenanceCycleStarted` / `MaintenanceCycleCompleted`.
- Service-layer code that emits at `project == <actual project name>` (e.g. inside `FanOutMaintenance` and per-project completion in `finalise_system_maintenance`) → use `ProjectRunStarted` / `ProjectRunCompleted`.

Specific known sites:

- `crates/foundryd/src/orchestrator.rs` `FanOutMaintenance::execute` — emits one event per project; change to `EventType::ProjectRunStarted` with `ProjectRunStartedPayload`. **Remove** the `mint_trace_id()` call that currently mints a fresh trace_id per project (`orchestrator.rs:96`); the trace_id is now inherited from the cycle root.
- `crates/foundryd/src/service.rs` cycle entry (search for `MaintenanceRunStarted`) — change to `MaintenanceCycleStarted` with `MaintenanceCycleStartedPayload`.
- `crates/foundryd/src/service.rs` `finalise_system_maintenance` — the synthesised `MaintenanceRunCompleted` (line ~207-208) becomes `MaintenanceCycleCompleted`; the per-project synthesised completion becomes `ProjectRunCompleted`.
- `extract_per_project_traces` (`service.rs:119`) — filter on `EventType::ProjectRunStarted`, not `MaintenanceRunStarted`.

Update every `sinks_on` list that contains the old variants to use the new ones.

Update the orchestrator test `trace_id should start with trc_` (orchestrator.rs:272) — that assertion is now backwards. With `FanOutMaintenance` no longer minting a fresh trace_id, the per-project events should *inherit* the cycle's trace_id. If the test was emitting without a cycle root, give the test event an explicit `trace_id` (newly minted via `mint_trace_id()`) and assert each per-project event inherits *that* id.

- [ ] **Step 3: Remove the old enum variants**

In `crates/foundry-core/src/event.rs`, delete `MaintenanceRunStarted` and `MaintenanceRunCompleted` from the `EventType` enum. Also remove their entries from the two test arrays.

- [ ] **Step 4: Build and fix any remaining errors**

Run: `cargo build --workspace`
Expected: SUCCESS once all callsites are migrated.

- [ ] **Step 5: Run tests**

Run: `cargo test --workspace`
Expected: PASS. Some tests will fail if they assert the old event type names — update them.

- [ ] **Step 6: Commit**

```bash
git add -u
git commit -m "refactor(events): split MaintenanceRun into MaintenanceCycle/ProjectRun pair; remove old variants"
```

### Task 4.5: Update CLI command references

**Files:**

- Modify: `crates/foundry-cli/src/commands.rs`

- [ ] **Step 1: Grep for old event names in CLI**

```bash
grep -n 'iteration_requested\|maintenance_requested\|maintenance_run_started\|maintenance_run_completed\|greet_requested' crates/foundry-cli/src/commands.rs
```

Replace each string literal with its new name (`project_iteration_requested`, `project_maintenance_requested`, `maintenance_cycle_started`, `project_run_started` — choose based on emit context — `maintenance_cycle_completed`/`project_run_completed`, `greeting_requested`).

- [ ] **Step 2: Build and run CLI**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS.

- [ ] **Step 3: Update AGENTS.md examples**

In `AGENTS.md`, locate the "CLI Commands" table and any examples that mention the old event names. Update them. Update the "Event Naming Conventions" examples — the "Command `*Requested`" row's examples should be `ProjectIterationRequested`, `ProjectMaintenanceRequested`, `ReleaseRequested`, `PipelineCheckRequested`.

- [ ] **Step 4: Commit**

```bash
git add crates/foundry-cli/src/commands.rs AGENTS.md
git commit -m "refactor(cli): rename event-type string references; update AGENTS.md examples"
```

### Task 4.6: Phase 4 quality gates

- [ ] **Step 1: Full workspace check**

Run: `cargo fmt --all -- --check && cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: PASS.

- [ ] **Step 2: Push**

```bash
git push origin main
```

---

## Phase 4.5 — Historical event-name backfill

**Goal:** A one-shot jq script renames archived event names so old `~/.foundry/events/*.jsonl` and `~/.foundry/traces/**/*.json` files stay queryable under the new vocabulary. `foundryd` refuses to start if it detects pre-0.17.0 event names on disk.

### Task 4.5.1: Write the migration script

**Files:**

- Create: `scripts/migrate-event-names.sh`

- [ ] **Step 1: Create the script**

Write `scripts/migrate-event-names.sh`:

```bash
#!/usr/bin/env bash
# migrate-event-names.sh — one-shot rewrite of pre-0.17.0 event-type names
# in ~/.foundry/events/*.jsonl and ~/.foundry/traces/YYYY-MM-DD/*.json.
#
# - Rewrites event_type per the cycle/project/noun-form rules.
# - Recomputes Event::id deterministically from the new (event_type, project,
#   occurred_at, payload) so the new event_type stays consistent with the id.
# - Builds an old-id → new-id table during the first pass and substitutes
#   payload references (notably project_trace_ids inside cycle completion
#   payloads) in the second.
# - Writes to *.new then atomically renames; killing mid-run leaves the
#   original intact.
# - Idempotent: a second invocation is a no-op (no remaining old-name events).
#
# Usage:
#   scripts/migrate-event-names.sh            # apply
#   scripts/migrate-event-names.sh --dry-run  # report counts + sample rewrites

set -euo pipefail

DRY_RUN=0
[[ "${1:-}" == "--dry-run" ]] && DRY_RUN=1

EVENTS_DIR="${FOUNDRY_EVENTS_DIR:-$HOME/.foundry/events}"
TRACES_DIR="${FOUNDRY_TRACES_DIR:-$HOME/.foundry/traces}"

# jq expression that rewrites event_type per the cycle/project split.
# Applied as `... | <RENAME_JQ>` to each event object.
read -r -d '' RENAME_JQ <<'JQ' || true
if .event_type == "maintenance_run_started" then
    .event_type = (if .project == "system" then "maintenance_cycle_started" else "project_run_started" end)
elif .event_type == "maintenance_run_completed" then
    .event_type = (if .project == "system" then "maintenance_cycle_completed" else "project_run_completed" end)
elif .event_type == "iteration_requested"   then .event_type = "project_iteration_requested"
elif .event_type == "maintenance_requested" then .event_type = "project_maintenance_requested"
elif .event_type == "greet_requested"       then .event_type = "greeting_requested"
else . end
JQ

# Recompute Event::id after rewrite. Foundry hashes
# (event_type | project | occurred_at_rfc3339 | payload_compact_json) with SHA-256,
# then takes the first 12 bytes and hex-encodes them, prefixed with "evt_".
recompute_id() {
    # $1 = event JSON object on stdin
    jq -c '
        def compact_payload: (.payload | tojson);
        .id = (
            "\(.event_type)\(.project)\(.occurred_at)\(compact_payload)"
            | @base64d  # placeholder — see note below
        )
    '
    # NOTE: jq cannot natively SHA-256. We shell out per event:
    return 1
}

# Use python (always available) for the SHA-256 step.
rewrite_jsonl() {
    local f="$1"
    python3 - "$f" "$DRY_RUN" <<'PY'
import hashlib, json, sys

src, dry = sys.argv[1], sys.argv[2] == "1"

RENAMES_SYSTEM = {
    "maintenance_run_started":   "maintenance_cycle_started",
    "maintenance_run_completed": "maintenance_cycle_completed",
}
RENAMES_PROJECT = {
    "maintenance_run_started":   "project_run_started",
    "maintenance_run_completed": "project_run_completed",
}
RENAMES_SIMPLE = {
    "iteration_requested":   "project_iteration_requested",
    "maintenance_requested": "project_maintenance_requested",
    "greet_requested":       "greeting_requested",
}

def compute_id(ev):
    h = hashlib.sha256()
    h.update(ev["event_type"].encode())
    h.update(ev["project"].encode())
    h.update(ev["occurred_at"].encode())
    h.update(json.dumps(ev["payload"], separators=(",", ":"), sort_keys=False).encode())
    return "evt_" + h.digest()[:12].hex()

id_remap = {}
rewrites = 0
recomputes = 0
out_lines = []

with open(src) as fh:
    for line in fh:
        line = line.strip()
        if not line:
            out_lines.append(line)
            continue
        ev = json.loads(line)
        et = ev.get("event_type")
        new_et = et
        if et in RENAMES_SYSTEM and ev.get("project") == "system":
            new_et = RENAMES_SYSTEM[et]
        elif et in RENAMES_PROJECT and ev.get("project") != "system":
            new_et = RENAMES_PROJECT[et]
        elif et in RENAMES_SIMPLE:
            new_et = RENAMES_SIMPLE[et]

        if new_et != et:
            rewrites += 1
            old_id = ev["id"]
            ev["event_type"] = new_et
            ev["id"] = compute_id(ev)
            recomputes += 1
            id_remap[old_id] = ev["id"]

        out_lines.append(json.dumps(ev, separators=(",", ":"), sort_keys=False))

# Second pass: rewrite payload references to old ids (notably project_trace_ids).
if id_remap:
    fixed = []
    for line in out_lines:
        if not line:
            fixed.append(line)
            continue
        ev = json.loads(line)
        payload = ev.get("payload", {})
        ptids = payload.get("project_trace_ids")
        if isinstance(ptids, dict):
            # values may still be valid (trace_ids haven't changed), but
            # if any value is an old event-id form we map it through.
            payload["project_trace_ids"] = {k: id_remap.get(v, v) for k, v in ptids.items()}
        if "root_event_id" in payload and payload["root_event_id"] in id_remap:
            payload["root_event_id"] = id_remap[payload["root_event_id"]]
        fixed.append(json.dumps(ev, separators=(",", ":"), sort_keys=False))
    out_lines = fixed

print(f"{src}: rewrites={rewrites} recomputed_ids={recomputes}", file=sys.stderr)
if dry:
    sys.exit(0)

if rewrites == 0:
    sys.exit(0)

tmp = src + ".new"
with open(tmp, "w") as fh:
    fh.write("\n".join(out_lines) + ("\n" if out_lines else ""))
import os
os.replace(tmp, src)
PY
}

# Trace files have an `events` array (and `block_executions` array — blocks
# don't carry event_type). Wrap the same Python logic for that shape.
rewrite_trace_json() {
    local f="$1"
    python3 - "$f" "$DRY_RUN" <<'PY'
# … same logic but operating on doc["events"] array, recomputing ids,
# and rewriting payload references inside that array.
# (Implementation parallels rewrite_jsonl; omitted here for brevity but
# REQUIRED — implement it before the script is considered done.)
PY
}

# Walk the events directory.
if [[ -d "$EVENTS_DIR" ]]; then
    for f in "$EVENTS_DIR"/*.jsonl; do
        [[ -f "$f" ]] || continue
        rewrite_jsonl "$f"
    done
fi

# Walk the traces directory.
if [[ -d "$TRACES_DIR" ]]; then
    while IFS= read -r -d '' f; do
        rewrite_trace_json "$f"
    done < <(find "$TRACES_DIR" -name '*.json' -print0)
fi

if (( DRY_RUN )); then
    echo "Dry run complete. Re-run without --dry-run to apply."
fi
```

The Python heredoc embedded in `rewrite_jsonl` is the canonical implementation; `rewrite_trace_json` parallels it for the `{ events: [...], block_executions: [...] }` document shape. Implement that variant fully — it must rewrite each event in the `events` array using the same rename/recompute logic and the same id-remap second pass for payload references.

Make the script executable:

```bash
chmod +x scripts/migrate-event-names.sh
```

- [ ] **Step 2: Test idempotency with a fixture**

Create `scripts/tests/migrate-event-names_test.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
FIXTURE="$(mktemp -d)"
trap 'rm -rf "$FIXTURE"' EXIT

mkdir -p "$FIXTURE/events" "$FIXTURE/traces/2026-05-01"

cat > "$FIXTURE/events/2026-05.jsonl" <<'EVENTS'
{"id":"evt_abc","event_type":"maintenance_run_started","project":"system","occurred_at":"2026-05-01T00:00:00Z","recorded_at":"2026-05-01T00:00:00Z","throttle":"full","payload":{"project_count":2}}
{"id":"evt_def","event_type":"maintenance_run_started","project":"alpha","occurred_at":"2026-05-01T00:00:01Z","recorded_at":"2026-05-01T00:00:01Z","throttle":"full","payload":{}}
{"id":"evt_ghi","event_type":"iteration_requested","project":"alpha","occurred_at":"2026-05-01T00:00:02Z","recorded_at":"2026-05-01T00:00:02Z","throttle":"full","payload":{}}
{"id":"evt_jkl","event_type":"greet_requested","project":"beta","occurred_at":"2026-05-01T00:00:03Z","recorded_at":"2026-05-01T00:00:03Z","throttle":"full","payload":{}}
EVENTS

FOUNDRY_EVENTS_DIR="$FIXTURE/events" \
FOUNDRY_TRACES_DIR="$FIXTURE/traces" \
"$SCRIPT_DIR/scripts/migrate-event-names.sh"

# Idempotency: re-run and verify the file is unchanged.
md5_before=$(md5 -q "$FIXTURE/events/2026-05.jsonl" 2>/dev/null || md5sum "$FIXTURE/events/2026-05.jsonl" | awk '{print $1}')

FOUNDRY_EVENTS_DIR="$FIXTURE/events" \
FOUNDRY_TRACES_DIR="$FIXTURE/traces" \
"$SCRIPT_DIR/scripts/migrate-event-names.sh"

md5_after=$(md5 -q "$FIXTURE/events/2026-05.jsonl" 2>/dev/null || md5sum "$FIXTURE/events/2026-05.jsonl" | awk '{print $1}')

[[ "$md5_before" == "$md5_after" ]] || { echo "FAIL: script is not idempotent"; exit 1; }

# Content check: no old names remain.
if grep -E '"event_type":"(maintenance_run_started|maintenance_run_completed|iteration_requested|maintenance_requested|greet_requested)"' "$FIXTURE/events/2026-05.jsonl"; then
    echo "FAIL: old event names remain after migration"
    exit 1
fi

echo "PASS"
```

Make it executable and run:

```bash
chmod +x scripts/tests/migrate-event-names_test.sh
scripts/tests/migrate-event-names_test.sh
```

Expected: `PASS`.

- [ ] **Step 3: Commit**

```bash
git add scripts/migrate-event-names.sh scripts/tests/migrate-event-names_test.sh
git commit -m "feat(scripts): add migrate-event-names.sh for 0.17.0 historical backfill"
```

### Task 4.5.2: Add startup guard in `foundryd`

**Files:**

- Modify: `crates/foundryd/src/main.rs`

The daemon refuses to start if pre-0.17.0 event names are present on disk and the migration hasn't been run.

- [ ] **Step 1: Write failing test**

In `crates/foundryd/src/main.rs` or a new helper module `crates/foundryd/src/legacy_event_check.rs`, define:

```rust
/// Scan `~/.foundry/events/*.jsonl` for legacy event-type names that the
/// 0.17.0 cutover renames. Returns the first legacy name found, if any.
pub fn detect_legacy_event_names(events_dir: &std::path::Path) -> Option<String> {
    // … implementation per Step 2 …
    None
}
```

Add a test in the same module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use std::io::Write;

    #[test]
    fn detects_legacy_maintenance_run_started() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("2026-05.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(
            f,
            r#"{{"id":"evt_a","event_type":"maintenance_run_started","project":"system","occurred_at":"2026-05-01T00:00:00Z","recorded_at":"2026-05-01T00:00:00Z","throttle":"full","payload":{{}}}}"#
        ).unwrap();

        let result = detect_legacy_event_names(dir.path());
        assert_eq!(result.as_deref(), Some("maintenance_run_started"));
    }

    #[test]
    fn returns_none_for_clean_directory() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("2026-05.jsonl");
        let mut f = std::fs::File::create(&p).unwrap();
        writeln!(
            f,
            r#"{{"id":"evt_a","event_type":"maintenance_cycle_started","project":"system","occurred_at":"2026-05-01T00:00:00Z","recorded_at":"2026-05-01T00:00:00Z","throttle":"full","payload":{{}}}}"#
        ).unwrap();

        assert!(detect_legacy_event_names(dir.path()).is_none());
    }
}
```

Run: `cargo test -p foundryd detect_legacy_event_names returns_none_for_clean_directory`
Expected: FAIL — function is a stub.

- [ ] **Step 2: Implement the scan**

```rust
pub fn detect_legacy_event_names(events_dir: &std::path::Path) -> Option<String> {
    const LEGACY: &[&str] = &[
        "maintenance_run_started",
        "maintenance_run_completed",
        "iteration_requested",
        "maintenance_requested",
        "greet_requested",
    ];

    let read_dir = match std::fs::read_dir(events_dir) {
        Ok(d) => d,
        Err(_) => return None, // missing dir = nothing to check
    };

    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        for line in content.lines() {
            for name in LEGACY {
                // Cheap pre-filter then exact JSON match.
                if line.contains(name)
                    && line.contains(&format!(r#""event_type":"{name}""#))
                {
                    return Some((*name).to_string());
                }
            }
        }
    }

    None
}
```

Run: `cargo test -p foundryd detect_legacy_event_names`
Expected: PASS.

- [ ] **Step 3: Wire it into `main()`**

In `crates/foundryd/src/main.rs`, near the top of the async main, after resolving the events directory but **before** starting any gRPC servers or task processing:

```rust
let events_dir = foundry_core::paths::events_dir();
if let Some(legacy) = legacy_event_check::detect_legacy_event_names(&events_dir) {
    eprintln!(
        "ERROR: foundryd 0.17.0 detected legacy event-type name '{legacy}' on disk.\n\
         Run scripts/migrate-event-names.sh once to backfill, then restart foundryd."
    );
    std::process::exit(2);
}
```

(Adjust the `events_dir()` accessor name to match what exists in `foundry_core::paths`. If only `FOUNDRY_EVENTS_DIR` env-var resolution exists, replicate that logic here.)

Add `mod legacy_event_check;` to `main.rs` (or `lib.rs`).

- [ ] **Step 4: Run all tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/foundryd/src/legacy_event_check.rs crates/foundryd/src/main.rs
git commit -m "feat(foundryd): refuse to start when legacy event names present on disk"
```

### Task 4.5.3: Document the migration in CHANGELOG

**Files:**

- Modify: `CHANGELOG.md`

- [ ] **Step 1: Add 0.17.0 entry**

Add a `[Unreleased]` (or `[0.17.0]` if cutting now) entry:

```markdown
## [0.17.0] — YYYY-MM-DD

### Breaking changes

- **Event taxonomy renames** (no aliases — hard cutover):
  - `MaintenanceRunStarted` split into `MaintenanceCycleStarted` (cycle root) and `ProjectRunStarted` (per-project).
  - `MaintenanceRunCompleted` split similarly.
  - `IterationRequested` → `ProjectIterationRequested`.
  - `MaintenanceRequested` → `ProjectMaintenanceRequested`.
  - `GreetRequested` → `GreetingRequested`.

### Migration

Run once after upgrading and **before** restarting `foundryd`:

    scripts/migrate-event-names.sh --dry-run   # review counts
    scripts/migrate-event-names.sh             # apply

The script rewrites archived event names in `~/.foundry/events/*.jsonl` and `~/.foundry/traces/**/*.json`, recomputes `Event::id` for renamed events, and fixes up payload references (`project_trace_ids`, `root_event_id`). `foundryd` refuses to start if it detects legacy event names on disk.

### Added

- OpenTelemetry-shaped nested tracing: every event now carries `span_id` and `parent_span_id` (16-char hex). The previous `trc_*` `trace_id` format is replaced with a 32-char hex format. Legacy `trc_*` ids remain readable.
- New `Span` RPC: retrieve every event and block execution within a single span.
- `TRACEPARENT` (W3C Trace Context) injected into subprocesses, so spawned tools and agents can correlate their telemetry with the Foundry span tree.
- `foundry trace` renders a span tree by default. `--flat` reproduces the previous chronological output.
- `foundry status --span <id>` filter.
```

- [ ] **Step 2: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs(changelog): add 0.17.0 entry — event renames and OTel nested tracing"
```

### Task 4.5.4: Phase 4.5 quality gates

- [ ] **Step 1: Full gates + push**

Run: `cargo fmt --all -- --check && cargo clippy --workspace -- -D warnings && cargo test --workspace && scripts/tests/migrate-event-names_test.sh`
Expected: PASS.

```bash
git push origin main
```

---

## Phase 5 — Subprocess context propagation

**Goal:** Every subprocess spawned by `foundryd` (gate runners, agent processes, etc.) carries a `TRACEPARENT` env var so its events join the current span tree. CLI's `foundry emit` reads `TRACEPARENT` from its environment and forwards it to the daemon.

### Task 5.1: Create `span_context.rs` in foundryd

**Files:**

- Create: `crates/foundryd/src/span_context.rs`
- Modify: `crates/foundryd/src/lib.rs` (or `main.rs`) — add `pub mod span_context;`

- [ ] **Step 1: Write the new module**

```rust
//! Task-local span context for in-process span propagation across
//! subprocess spawn sites.
//!
//! Set by the engine before invoking a block's `execute`; read by
//! `shell.rs` and `agent_stream.rs` to inject `TRACEPARENT` into
//! spawned commands.

use foundry_core::event::Event;

/// The current block's span context.
#[derive(Debug, Clone)]
pub struct SpanContext {
    /// 32-char lowercase hex.
    pub trace_id: String,
    /// 16-char lowercase hex — the block's own span_id.
    pub span_id: String,
}

impl SpanContext {
    /// Build a W3C Trace Context `traceparent` header value:
    /// `00-<trace_id>-<span_id>-01`.
    #[must_use]
    pub fn traceparent(&self) -> String {
        format!("00-{}-{}-01", self.trace_id, self.span_id)
    }
}

tokio::task_local! {
    /// Set by the engine for the duration of a block's `execute` call.
    pub static SPAN_CONTEXT: SpanContext;
}

/// Inject `TRACEPARENT` into a `Command` if a span context is active in the
/// current tokio task. No-op outside a tokio task or when context is unset.
pub fn inject_traceparent(cmd: &mut tokio::process::Command) {
    let _ = SPAN_CONTEXT.try_with(|ctx| {
        cmd.env("TRACEPARENT", ctx.traceparent());
    });
}

/// Variant for `std::process::Command` (legacy callsites in subprocess
/// migration). Prefer migrating to `tokio::process::Command`.
pub fn inject_traceparent_std(cmd: &mut std::process::Command) {
    let _ = SPAN_CONTEXT.try_with(|ctx| {
        cmd.env("TRACEPARENT", ctx.traceparent());
    });
}

/// Extract a SpanContext from an Event's span fields. Returns None if any
/// required field is missing.
pub fn from_event(event: &Event) -> Option<SpanContext> {
    let trace_id = event.trace_id.clone()?;
    let span_id = event.span_id.clone()?;
    Some(SpanContext { trace_id, span_id })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traceparent_format_matches_w3c() {
        let ctx = SpanContext {
            trace_id: "0123456789abcdef0123456789abcdef".to_string(),
            span_id: "fedcba9876543210".to_string(),
        };
        assert_eq!(ctx.traceparent(), "00-0123456789abcdef0123456789abcdef-fedcba9876543210-01");
    }

    #[tokio::test]
    async fn inject_traceparent_within_scope_sets_env() {
        let ctx = SpanContext {
            trace_id: "0123456789abcdef0123456789abcdef".to_string(),
            span_id: "fedcba9876543210".to_string(),
        };
        let expected = ctx.traceparent();
        SPAN_CONTEXT
            .scope(ctx, async move {
                let mut cmd = tokio::process::Command::new("true");
                inject_traceparent(&mut cmd);
                // tokio::process::Command exposes env via as_std()
                let env: Vec<(String, String)> = cmd
                    .as_std()
                    .get_envs()
                    .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
                    .collect();
                assert!(env.iter().any(|(k, v)| k == "TRACEPARENT" && v == &expected));
            })
            .await;
    }

    #[tokio::test]
    async fn inject_traceparent_outside_scope_is_noop() {
        // No SPAN_CONTEXT::scope around this — try_with should silently fail.
        let mut cmd = tokio::process::Command::new("true");
        inject_traceparent(&mut cmd);
        assert!(cmd.as_std().get_envs().all(|(k, _)| k.to_str() != Some("TRACEPARENT")));
    }
}
```

In `crates/foundryd/src/lib.rs` (or `main.rs` if no lib.rs), add:

```rust
pub mod span_context;
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p foundryd span_context`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/foundryd/src/span_context.rs crates/foundryd/src/lib.rs
git commit -m "feat(foundryd): add SpanContext task-local + traceparent injection helpers"
```

### Task 5.2: Wrap `block.execute` in `SPAN_CONTEXT::scope`

**Files:**

- Modify: `crates/foundryd/src/engine.rs:192` (the `execute_with_retry` call) and the dry-run path at line ~151

- [ ] **Step 1: Write failing integration test**

In `crates/foundryd/src/engine.rs` tests module, add (requires `tokio::process::Command`):

```rust
#[tokio::test]
async fn block_inside_engine_sees_span_context_via_task_local() {
    use crate::span_context::SPAN_CONTEXT;

    struct ContextProbingBlock {
        seen_trace: std::sync::Mutex<Option<String>>,
        seen_span: std::sync::Mutex<Option<String>>,
    }

    #[async_trait::async_trait]
    impl TaskBlock for ContextProbingBlock {
        fn name(&self) -> &str { "Probe" }
        fn sinks_on(&self) -> &[EventType] { &[EventType::VulnerabilityDetected] }
        fn kind(&self) -> BlockKind { BlockKind::Observer }
        async fn execute(&self, _trigger: &Event) -> anyhow::Result<TaskBlockResult> {
            let _ = SPAN_CONTEXT.try_with(|ctx| {
                *self.seen_trace.lock().unwrap() = Some(ctx.trace_id.clone());
                *self.seen_span.lock().unwrap() = Some(ctx.span_id.clone());
            });
            Ok(TaskBlockResult::default())
        }
    }

    let block = Box::new(ContextProbingBlock {
        seen_trace: std::sync::Mutex::new(None),
        seen_span: std::sync::Mutex::new(None),
    });
    let seen_trace = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    let seen_span  = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
    // (Use Arc<Mutex> shared with the block — adjust the block to take them in `new()`.)

    // Trigger the block; observe what SPAN_CONTEXT carried.
    // …
    // Assert seen_trace == trigger.trace_id and seen_span equals the
    // block's own span_id (as recorded on BlockExecution.span_id).
}
```

(Implementation detail: thread the `Arc<Mutex<…>>` into the block via a constructor and read them after `engine.process(trigger).await`. Then assert the captured values match `result.block_executions[0].span_id` and the trigger's `trace_id`.)

Run: `cargo test -p foundryd block_inside_engine_sees_span_context`
Expected: FAIL — engine doesn't yet set SPAN_CONTEXT.

- [ ] **Step 2: Wrap `block.execute` in `SPAN_CONTEXT::scope`**

In `engine.rs`, `execute_with_retry` (or wherever `block.execute(trigger).await` is invoked) — wrap the inner call. The cleanest place is around line ~192 inside `run_block`:

Replace:

```rust
match execute_with_retry(block, current, block.retry_policy()).await {
```

with:

```rust
let ctx = (current.trace_id.clone(), block_span_id.clone());
let exec_future = async {
    execute_with_retry(block, current, block.retry_policy()).await
};

let result_or_err = match (ctx.0.clone(), Some(ctx.1.clone())) {
    (Some(trace_id), Some(span_id)) => {
        let span_ctx = crate::span_context::SpanContext { trace_id, span_id };
        crate::span_context::SPAN_CONTEXT.scope(span_ctx, exec_future).await
    }
    _ => exec_future.await,
};

match result_or_err {
```

Apply the same wrapping pattern to the dry-run branch (line ~151) where `block.dry_run_events(current)` is called — but only if any block's `dry_run_events` could trigger subprocess spawn, which is unlikely. Skip for now if not needed; the typical case is the retry path above.

- [ ] **Step 3: Run tests**

Run: `cargo test -p foundryd block_inside_engine_sees_span_context`
Expected: PASS.

Also run full engine tests:

Run: `cargo test -p foundryd`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/foundryd/src/engine.rs
git commit -m "feat(engine): scope SPAN_CONTEXT around block.execute for subprocess propagation"
```

### Task 5.3: Inject `TRACEPARENT` in `shell.rs::run_shell`

**Files:**

- Modify: `crates/foundryd/src/shell.rs`

- [ ] **Step 1: Read the current shell.rs**

```bash
grep -n "Command::new\|run_shell" crates/foundryd/src/shell.rs | head -10
```

Identify the `Command` builder. Note whether it uses `tokio::process::Command` or `std::process::Command`.

- [ ] **Step 2: Write failing integration test**

In `crates/foundryd/src/shell.rs` tests:

```rust
#[tokio::test]
async fn run_shell_injects_traceparent_when_span_context_set() {
    use crate::span_context::{SpanContext, SPAN_CONTEXT};

    let ctx = SpanContext {
        trace_id: "0123456789abcdef0123456789abcdef".to_string(),
        span_id: "fedcba9876543210".to_string(),
    };
    let expected = ctx.traceparent();

    let output = SPAN_CONTEXT
        .scope(ctx, async {
            // run_shell signature — adapt to whatever exists; we want
            // it to execute `printenv TRACEPARENT` and return stdout.
            run_shell("printenv TRACEPARENT").await
        })
        .await
        .expect("run_shell must succeed");

    assert!(output.stdout.contains(&expected), "stdout should contain {expected}, got {output:?}");
}

#[tokio::test]
async fn run_shell_does_not_set_traceparent_when_context_absent() {
    let output = run_shell("printenv TRACEPARENT; echo done").await.expect("ok");
    assert!(!output.stdout.contains("00-"),
        "no TRACEPARENT should leak in stdout when context is unset");
}
```

(Adjust `run_shell` invocation to match the actual function signature in your codebase — it likely takes `&str` or `&[&str]` and returns a `ShellOutput { stdout, stderr, exit_code }` or similar.)

Run: `cargo test -p foundryd run_shell_injects_traceparent`
Expected: FAIL.

- [ ] **Step 3: Add injection in `run_shell`**

In `shell.rs`, immediately after the `Command::new(...)` (or `.args(...)`) builder call and before `.spawn()` / `.output()`:

```rust
crate::span_context::inject_traceparent(&mut cmd); // tokio::process::Command
// — OR —
crate::span_context::inject_traceparent_std(&mut cmd); // std::process::Command
```

Match whichever Command variant `run_shell` uses.

Run: `cargo test -p foundryd run_shell_injects_traceparent`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/foundryd/src/shell.rs
git commit -m "feat(shell): inject TRACEPARENT env var from SPAN_CONTEXT into spawned commands"
```

### Task 5.4: Inject `TRACEPARENT` in `agent_stream.rs`

**Files:**

- Modify: `crates/foundryd/src/agent_stream.rs`

- [ ] **Step 1: Find the spawn site**

```bash
grep -n "Command::new\|spawn" crates/foundryd/src/agent_stream.rs
```

- [ ] **Step 2: Add injection**

Right after the `Command::new(...)` builder is configured (args, env, cwd) and before `.spawn()`, call `crate::span_context::inject_traceparent(&mut cmd)` (or `_std` variant).

- [ ] **Step 3: Smoke-test by running a real block that uses agent_stream**

If feasible, add a unit test that mocks the agent invocation and asserts `TRACEPARENT` is among the env vars passed to the spawned `Command` (using `as_std().get_envs()` like the test in Task 5.1). Otherwise rely on the existing agent_stream tests staying green.

Run: `cargo test -p foundryd`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add crates/foundryd/src/agent_stream.rs
git commit -m "feat(agent): inject TRACEPARENT from SPAN_CONTEXT at Claude Code spawn site"
```

### Task 5.5: Migrate direct `std::process::Command::new` callsites

**Files (subprocess migration):**

- Modify: `crates/foundryd/src/engine.rs`
- Modify: `crates/foundryd/src/blocks/validate.rs`
- Modify: `crates/foundryd/src/blocks/release.rs`
- Modify: `crates/foundryd/src/blocks/install.rs` (if it exists; otherwise skip)
- Modify: `crates/foundry-cli/src/gates_commands.rs`

- [ ] **Step 1: Inventory direct spawn callsites**

```bash
grep -rn 'std::process::Command::new\|Command::new(' crates/foundryd/src/ crates/foundry-cli/src/gates_commands.rs --include='*.rs'
```

For each callsite, decide:

- **Spawn for streamed output (long-running, line-by-line stdout)** — keep direct `Command::new`, add `crate::span_context::inject_traceparent(_std)(&mut cmd)` immediately before `.spawn()`.
- **One-shot command with `.output()`** — migrate to `shell.rs::run_shell` if it accepts the same form.

- [ ] **Step 2: Apply injection at each remaining direct callsite**

For each `Command::new(...)`, add `inject_traceparent_std` (or the tokio variant) right before `.spawn()` / `.output()`.

`gates_commands.rs` lives in `foundry-cli`, not `foundryd`. It does **not** have access to `SPAN_CONTEXT` because span context is a daemon concept. Instead, in `foundry-cli/src/gates_commands.rs`, forward whatever `TRACEPARENT` is in the CLI process's env (the CLI inherits from its parent — the daemon spawning the CLI as part of a block). Concretely:

```rust
if let Ok(tp) = std::env::var("TRACEPARENT") {
    cmd.env("TRACEPARENT", tp);
}
```

This is a no-op when invoked manually (env var absent) and propagates correctly when the CLI is spawned by a block.

- [ ] **Step 3: Build and test**

Run: `cargo build --workspace && cargo test --workspace`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
git add -u
git commit -m "feat(subprocess): inject TRACEPARENT at all subprocess spawn sites"
```

### Task 5.6: `foundry emit` CLI reads TRACEPARENT and forwards to daemon

**Files:**

- Modify: `crates/foundry-cli/src/commands.rs`

- [ ] **Step 1: Update the emit handler**

Find the function that builds `EmitRequest` (search for `EmitRequest {` in `commands.rs`). Add:

```rust
fn parse_traceparent_from_env() -> (Option<String>, Option<String>) {
    let tp = match std::env::var("TRACEPARENT") {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    // Format: 00-<trace_id 32 hex>-<span_id 16 hex>-<flags>
    let parts: Vec<&str> = tp.split('-').collect();
    if parts.len() != 4 || parts[0] != "00" || parts[1].len() != 32 || parts[2].len() != 16 {
        return (None, None);
    }
    (Some(parts[1].to_string()), Some(parts[2].to_string()))
}
```

Then in the emit invocation:

```rust
let (env_trace_id, env_parent_span_id) = parse_traceparent_from_env();
let request = EmitRequest {
    event_type: ...,
    project: ...,
    throttle: ...,
    payload_json: ...,
    trace_id: env_trace_id.unwrap_or_default(),
    span_id: String::new(),                            // let daemon mint
    parent_span_id: env_parent_span_id.unwrap_or_default(),
};
```

Add a unit test for `parse_traceparent_from_env` (set the env var in the test, assert parsing). Use `serial_test` or set/unset env carefully — the codebase may already have a pattern for this.

- [ ] **Step 2: Run tests**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 3: Commit**

```bash
git add crates/foundry-cli/src/commands.rs
git commit -m "feat(cli): forward TRACEPARENT from env into Emit requests"
```

### Task 5.7: Phase 5 quality gates

- [ ] **Step 1: Full gates**

Run: `cargo fmt --all -- --check && cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: PASS.

```bash
git push origin main
```

---

## Phase 6 — `Span` RPC implementation

**Goal:** Make the stubbed `Span` RPC actually return events and block executions for a span. Add secondary indexes `span_id → trace_id` and `trace_id → Vec<span_id>` in `trace_store.rs`. Update Emit to mint `span_id` defaults.

### Task 6.1: Add span indexes to `trace_store.rs`

**Files:**

- Modify: `crates/foundryd/src/trace_store.rs`

- [ ] **Step 1: Read current trace_store**

```bash
grep -n "fn\|impl\|HashMap" crates/foundryd/src/trace_store.rs | head -30
```

Locate where events are recorded (probably in a method like `record_event` or via `trace_writer`).

- [ ] **Step 2: Add the indexes**

Add to the trace store's state struct (whatever holds the in-memory state):

```rust
/// Map: span_id → trace_id, for resolving Span RPC requests quickly.
span_to_trace: HashMap<String, String>,

/// Map: trace_id → set of span_ids in that trace, for cycle-level queries.
trace_to_spans: HashMap<String, std::collections::HashSet<String>>,
```

- [ ] **Step 3: Populate them when events are recorded**

In whichever method handles "an event has been emitted and persisted" — extend with:

```rust
if let (Some(trace_id), Some(span_id)) = (&event.trace_id, &event.span_id) {
    self.span_to_trace.entry(span_id.clone())
        .or_insert_with(|| trace_id.clone());
    self.trace_to_spans.entry(trace_id.clone())
        .or_default()
        .insert(span_id.clone());
}
```

- [ ] **Step 4: Add a `find_span` method**

```rust
/// Return all events whose `span_id == requested_span`, and all block
/// executions whose `span_id == requested_span` or whose parent_span_id
/// equals `requested_span` (children of this span via block dispatch).
pub fn find_span(&self, span_id: &str) -> Option<SpanResult> {
    let trace_id = self.span_to_trace.get(span_id)?.clone();
    // Pull the trace's full event set and filter.
    let trace = self.traces.get(&trace_id)?;
    let events: Vec<Event> = trace.events.iter()
        .filter(|e| e.span_id.as_deref() == Some(span_id))
        .cloned()
        .collect();
    let blocks: Vec<BlockExecution> = trace.block_executions.iter()
        .filter(|b| b.parent_span_id.as_deref() == Some(span_id)
                  || b.span_id.as_deref() == Some(span_id))
        .cloned()
        .collect();
    let total_duration_ms = blocks.iter().map(|b| b.duration_ms).sum();
    Some(SpanResult { events, blocks, total_duration_ms })
}
```

`SpanResult` is a small new struct local to trace_store.

(Field/method names may differ from your actual `trace_store.rs` — adapt to match. The shape is: index on insert, query by span_id, return matching events and blocks.)

- [ ] **Step 5: Test**

Add to the tests module:

```rust
#[test]
fn find_span_returns_only_matching_span_events() {
    let mut store = TraceStore::new();
    let trace = "0123456789abcdef0123456789abcdef".to_string();
    let span_a = "aaaaaaaaaaaaaaaa".to_string();
    let span_b = "bbbbbbbbbbbbbbbb".to_string();

    // event in span_a
    let e1 = make_event(EventType::PreflightCompleted, &trace, Some(&span_a), None);
    // event in span_b
    let e2 = make_event(EventType::PreflightCompleted, &trace, Some(&span_b), None);

    store.record(e1.clone());
    store.record(e2.clone());

    let span_result = store.find_span(&span_a).expect("span_a must be found");
    assert_eq!(span_result.events.len(), 1);
    assert_eq!(span_result.events[0].id, e1.id);
}
```

(Adapt to the real `trace_store` API. Provide a `make_event` helper that builds an `Event` with given span fields.)

Run: `cargo test -p foundryd find_span_returns_only_matching_span_events`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add crates/foundryd/src/trace_store.rs
git commit -m "feat(trace_store): index span_id→trace_id and add find_span query"
```

### Task 6.2: Wire `Span` RPC handler to `trace_store::find_span`

**Files:**

- Modify: `crates/foundryd/src/service.rs`

- [ ] **Step 1: Replace the stub**

Replace the Phase 2 stub `span` method with:

```rust
async fn span(
    &self,
    request: tonic::Request<SpanRequest>,
) -> Result<tonic::Response<SpanResponse>, tonic::Status> {
    let req = request.into_inner();
    let store = self.trace_store.read().await; // or whatever the locking pattern is

    let response = match store.find_span(&req.span_id) {
        Some(r) => SpanResponse {
            found: true,
            events: r.events.iter().map(trace_event_from).collect(),
            block_executions: r.blocks.iter().map(trace_block_from).collect(),
            total_duration_ms: r.total_duration_ms,
        },
        None => SpanResponse {
            found: false,
            events: vec![],
            block_executions: vec![],
            total_duration_ms: 0,
        },
    };

    Ok(tonic::Response::new(response))
}
```

`trace_event_from` and `trace_block_from` are the same converters used by the `Trace` RPC — extract them if they're currently inline. Both must populate `span_id` and `parent_span_id` (use `unwrap_or_default()` for `Option<String>` → proto `string`).

- [ ] **Step 2: Make `Emit` mint a fresh `span_id` when none supplied**

Update the `Emit` handler from Phase 2 Task 2.2: when `req.span_id` is empty, mint via `foundry_core::event::mint_span_id()`. When `req.parent_span_id` is empty, leave as `None` (root span). This is the change deferred from Phase 2.

```rust
let span_id = if req.span_id.is_empty() {
    Some(foundry_core::event::mint_span_id())
} else {
    Some(req.span_id.clone())
};
let parent_span_id = if req.parent_span_id.is_empty() {
    None
} else {
    Some(req.parent_span_id.clone())
};
let event = Event::new(...)
    .with_trace_id(Some(request_trace_id))
    .with_span_ids(span_id, parent_span_id);
```

- [ ] **Step 3: Integration test for the full path**

Add a service-layer test that:

1. Emits a workflow root event (e.g. `ProjectIterationRequested`) with no trace/span set.
2. Calls `Trace` and confirms the response contains span fields populated end-to-end.
3. Picks the workflow span's `span_id` from the response.
4. Calls `Span` with that span_id, confirms `found = true` and the returned events all share that span_id.

Adapt to the existing service test harness pattern.

Run: `cargo test -p foundryd span_rpc`
Expected: PASS.

- [ ] **Step 4: Commit and gates**

```bash
git add crates/foundryd/src/service.rs
git commit -m "feat(service): implement Span RPC; mint fresh span_id on empty Emit request"
cargo fmt --all -- --check && cargo clippy --workspace -- -D warnings && cargo test --workspace
git push origin main
```

---

## Phase 7 — CLI display

**Goal:** `foundry trace <event-id>` renders a span tree by default; `--flat` preserves today's behavior. `foundry status --span <id>` filters.

### Task 7.1: Build a span-tree renderer

**Files:**

- Modify: `crates/foundry-cli/src/commands.rs` (or a new `crates/foundry-cli/src/trace_tree.rs` module)

- [ ] **Step 1: Define the tree node type**

In a new module `crates/foundry-cli/src/trace_tree.rs`:

```rust
//! Convert a flat `TraceResponse` into a span tree for rendering.

use crate::pb::{TraceBlockExecution, TraceEvent}; // adjust path to your proto module

#[derive(Debug)]
pub struct SpanNode {
    pub span_id: String,
    pub parent_span_id: Option<String>,
    pub events: Vec<TraceEvent>,
    pub blocks: Vec<TraceBlockExecution>,
    pub children: Vec<SpanNode>,
}

/// Build a forest of span trees from a flat list of events and blocks.
///
/// Roots are spans whose `parent_span_id` is empty (or refers to a span
/// not present in the response). Multiple roots are normal — e.g. when the
/// trace is rooted at a cycle, blocks within it are children of the cycle
/// span. When the trace is rooted at a workflow, the workflow span is root.
pub fn build_forest(
    events: Vec<TraceEvent>,
    blocks: Vec<TraceBlockExecution>,
) -> Vec<SpanNode> {
    use std::collections::HashMap;

    // Group events by span_id.
    let mut events_by_span: HashMap<String, Vec<TraceEvent>> = HashMap::new();
    for e in events {
        events_by_span.entry(e.span_id.clone()).or_default().push(e);
    }

    // Group blocks by their own span_id (block is its own span node).
    let mut node_by_span: HashMap<String, SpanNode> = HashMap::new();
    let mut parent_of: HashMap<String, Option<String>> = HashMap::new();

    for (span_id, evs) in events_by_span {
        let parent = evs.first().and_then(|e| {
            if e.parent_span_id.is_empty() { None } else { Some(e.parent_span_id.clone()) }
        });
        parent_of.insert(span_id.clone(), parent.clone());
        node_by_span.insert(
            span_id.clone(),
            SpanNode {
                span_id,
                parent_span_id: parent,
                events: evs,
                blocks: vec![],
                children: vec![],
            },
        );
    }

    // Attach blocks to their parent span (block.parent_span_id == workflow span_id).
    for b in blocks {
        if let Some(node) = node_by_span.get_mut(&b.parent_span_id) {
            node.blocks.push(b);
        }
    }

    // Wire the parent/child structure. Iterate twice to avoid borrow issues.
    let span_ids: Vec<String> = node_by_span.keys().cloned().collect();
    let mut roots = vec![];

    // Build a temp adjacency list.
    let mut children_of: HashMap<String, Vec<String>> = HashMap::new();
    for sid in &span_ids {
        if let Some(parent) = parent_of.get(sid).cloned().flatten() {
            if node_by_span.contains_key(&parent) {
                children_of.entry(parent).or_default().push(sid.clone());
            } else {
                roots.push(sid.clone());
            }
        } else {
            roots.push(sid.clone());
        }
    }

    // Recursively assemble.
    fn take(node_by_span: &mut HashMap<String, SpanNode>,
            children_of: &HashMap<String, Vec<String>>,
            sid: &str) -> Option<SpanNode> {
        let mut node = node_by_span.remove(sid)?;
        if let Some(cs) = children_of.get(sid) {
            for c in cs {
                if let Some(child) = take(node_by_span, children_of, c) {
                    node.children.push(child);
                }
            }
        }
        Some(node)
    }

    roots.into_iter()
        .filter_map(|r| take(&mut node_by_span, &children_of, &r))
        .collect()
}
```

- [ ] **Step 2: Test the forest builder**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn evt(span: &str, parent: &str, ty: &str) -> TraceEvent {
        TraceEvent {
            event_id: format!("evt_{ty}_{span}"),
            event_type: ty.to_string(),
            project: "p".to_string(),
            occurred_at: "2026-05-01T00:00:00Z".to_string(),
            trace_id: "trace1".to_string(),
            span_id: span.to_string(),
            parent_span_id: parent.to_string(),
            throttle: 0,
        }
    }

    #[test]
    fn two_level_tree() {
        let evs = vec![
            evt("a", "", "maintenance_cycle_started"),
            evt("b", "a", "project_run_started"),
        ];
        let blocks = vec![];
        let forest = build_forest(evs, blocks);
        assert_eq!(forest.len(), 1);
        assert_eq!(forest[0].span_id, "a");
        assert_eq!(forest[0].children.len(), 1);
        assert_eq!(forest[0].children[0].span_id, "b");
    }

    #[test]
    fn dangling_parent_treated_as_root() {
        // span b says its parent is "missing", but no event in the
        // response has span_id == "missing". b should be promoted to root.
        let evs = vec![evt("b", "missing", "x")];
        let forest = build_forest(evs, vec![]);
        assert_eq!(forest.len(), 1);
        assert_eq!(forest[0].span_id, "b");
    }
}
```

Run: `cargo test -p foundry-cli trace_tree`
Expected: PASS.

- [ ] **Step 3: Render the tree to text**

Add a render function:

```rust
pub fn render(forest: &[SpanNode], out: &mut String) {
    for node in forest {
        render_node(node, "", true, out);
    }
}

fn render_node(node: &SpanNode, prefix: &str, is_last: bool, out: &mut String) {
    let connector = if is_last { "└── " } else { "├── " };
    let span_label = node
        .events
        .first()
        .map(|e| e.event_type.as_str())
        .unwrap_or("span");
    out.push_str(&format!(
        "{prefix}{connector}[span] {span_label}  span={}  parent={}\n",
        short(&node.span_id),
        node.parent_span_id.as_deref().map(short).unwrap_or_else(|| "∅".to_string()),
    ));

    let child_prefix = format!("{prefix}{}", if is_last { "    " } else { "│   " });

    for (i, e) in node.events.iter().enumerate() {
        let last = i == node.events.len() - 1 && node.blocks.is_empty() && node.children.is_empty();
        let conn = if last { "└── " } else { "├── " };
        out.push_str(&format!("{child_prefix}{conn}{}  project={}\n", e.event_type, e.project));
    }

    for (i, b) in node.blocks.iter().enumerate() {
        let last = i == node.blocks.len() - 1 && node.children.is_empty();
        let conn = if last { "└── " } else { "├── " };
        out.push_str(&format!(
            "{child_prefix}{conn}[block: {}]  block_span={}  duration={}ms\n",
            b.block_name,
            short(&b.span_id),
            b.duration_ms,
        ));
    }

    for (i, child) in node.children.iter().enumerate() {
        let last = i == node.children.len() - 1;
        render_node(child, &child_prefix, last, out);
    }
}

fn short(id: &str) -> String {
    if id.len() > 8 { format!("{}…", &id[..8]) } else { id.to_string() }
}
```

Add a snapshot-style test that builds a small tree and asserts key strings appear in `out` (project names, block names, span_id prefixes).

- [ ] **Step 4: Commit**

```bash
git add crates/foundry-cli/src/trace_tree.rs crates/foundry-cli/src/commands.rs
git commit -m "feat(cli): add span-tree forest builder and renderer for foundry trace"
```

### Task 7.2: Wire tree renderer into `foundry trace` (default) with `--flat` fallback

**Files:**

- Modify: `crates/foundry-cli/src/commands.rs`

- [ ] **Step 1: Add `--flat` flag**

Find the `trace` subcommand definition (clap derive). Add:

```rust
#[derive(Args, Debug)]
pub struct TraceArgs {
    pub event_id: String,
    /// Print events in chronological order (legacy 0.16 format).
    #[arg(long, default_value_t = false)]
    pub flat: bool,
}
```

- [ ] **Step 2: Branch on the flag in the handler**

```rust
pub async fn trace_command(args: TraceArgs) -> Result<()> {
    let response = client.trace(TraceRequest { event_id: args.event_id }).await?.into_inner();
    if !response.found {
        eprintln!("Trace not found");
        return Ok(());
    }
    if args.flat {
        render_flat(&response);
    } else {
        let forest = crate::trace_tree::build_forest(response.events, response.block_executions);
        let mut out = String::new();
        crate::trace_tree::render(&forest, &mut out);
        print!("{out}");
    }
    Ok(())
}
```

Keep the existing flat rendering function as `render_flat`.

Auto-fallback: if **every** event in the response has empty `span_id` (legacy `trc_*` traces), render flat regardless of the flag — show a one-line note `(legacy trace: rendering flat)`.

- [ ] **Step 3: Test manually**

If a foundryd is running locally:

```bash
foundry trace <some-event-id>
foundry trace <some-event-id> --flat
```

Compare outputs.

- [ ] **Step 4: Commit**

```bash
git add crates/foundry-cli/src/commands.rs
git commit -m "feat(cli): foundry trace renders span tree by default; --flat preserves legacy view"
```

### Task 7.3: `foundry status --span <id>` filter

**Files:**

- Modify: `crates/foundry-cli/src/commands.rs`

- [ ] **Step 1: Add the flag**

Find the `StatusArgs` (or equivalent). Add:

```rust
/// Filter to workflows whose root span_id matches.
#[arg(long)]
pub span: Option<String>,
```

- [ ] **Step 2: Filter the response**

If `args.span` is `Some(id)`, after fetching the workflow list, filter to entries whose trace_id has any span equal to `id`. Since `WorkflowStatus` carries `trace_id` (and not yet a span identifier), one of two approaches:

- **Simple**: filter client-side by asking the daemon for the `Span` and matching the returned events' `trace_id` against listed workflows.
- **Cleaner**: add `string root_span_id = 9;` to the `WorkflowStatus` proto message, populate in `service.rs` from the tracker, and filter on that.

Pick the simple approach for this phase: call `Span(SpanRequest { span_id })`, take the first returned event's `trace_id`, and filter workflows to that trace.

- [ ] **Step 3: Commit**

```bash
git add crates/foundry-cli/src/commands.rs
git commit -m "feat(cli): foundry status --span <id> filter"
```

### Task 7.4: Phase 7 quality gates

- [ ] **Step 1: Full gates**

Run: `cargo fmt --all -- --check && cargo clippy --workspace -- -D warnings && cargo test --workspace`
Expected: PASS.

```bash
git push origin main
```

---

## Phase 8 — Docs

**Goal:** Document the trace model in mdBook, update AGENTS.md, refresh the skill, and finalize the CHANGELOG.

### Task 8.1: Write the mdBook tracing chapter

**Files:**

- Create: `book/src/architecture/tracing.md`
- Modify: `book/src/SUMMARY.md` to include it.

- [ ] **Step 1: Write the chapter**

`book/src/architecture/tracing.md` (outline — flesh out with prose):

```markdown
# Tracing

Foundry uses an OpenTelemetry-shaped span model to describe what happened during a run. Every event carries three identifiers:

- `trace_id` — 32-char lowercase hex. Identifies the whole tree.
- `span_id` — 16-char lowercase hex. Identifies the span this event belongs to.
- `parent_span_id` — 16-char lowercase hex. The span that caused this one to open. `None` for root spans.

## The four span levels

(table from spec — cycle / project_run / workflow / block, what opens and closes each)

## Stamping rules

(two rules: default vs span-opener, with a small worked example showing the same trace_id, peer span propagation, and the span-opener rule minting a fresh span_id parented to the emitting block)

## Span-opener registry

(list of `EventType` variants that open spans, where the registry lives in `foundry-core`, and how to extend it)

## Subprocess propagation (`TRACEPARENT`)

(W3C Trace Context format, env var injection by `shell.rs` and `agent_stream.rs`, how `foundry emit` reads it on the CLI side)

## Querying spans

(Trace RPC vs Span RPC, `foundry trace`, `foundry trace --flat`, `foundry status --span <id>`)

## Legacy traces

(`trc_*` ids are still readable; `--flat` is auto-applied; migration script reference)
```

- [ ] **Step 2: Add to SUMMARY.md**

```markdown
- [Tracing](architecture/tracing.md)
```

(Adjust the heading level / placement to match the existing SUMMARY structure.)

- [ ] **Step 3: Build the book**

Run: `mdbook build book/`
Expected: SUCCESS.

- [ ] **Step 4: Commit**

```bash
git add book/src/architecture/tracing.md book/src/SUMMARY.md
git commit -m "docs(book): add OTel-shaped tracing chapter"
```

### Task 8.2: Refresh AGENTS.md

**Files:**

- Modify: `AGENTS.md`

- [ ] **Step 1: Update the event-naming taxonomy section**

The `*Requested` examples must use noun-form names: `ProjectIterationRequested`, `ProjectMaintenanceRequested`, `ReleaseRequested`, `PipelineCheckRequested`.

The `*Started`/`*Completed` row should list the new pairs: `MaintenanceCycleStarted` / `MaintenanceCycleCompleted`, `ProjectRunStarted` / `ProjectRunCompleted`, `StrategicCycleStarted` / `StrategicCycleCompleted`, `InnerIterationStarted` / `InnerIterationCompleted`, `RemediationStarted` / `RemediationCompleted`.

- [ ] **Step 2: Add a short "Tracing" section pointing at the book**

After the "Payload Conventions" section, add:

```markdown
## Tracing

Foundry uses OpenTelemetry-shaped nested spans. Every event carries `trace_id`, `span_id`, and `parent_span_id`. The engine stamps these automatically per two rules (default + span-opener registry). Subprocesses inherit `TRACEPARENT`.

See [`book/src/architecture/tracing.md`](book/src/architecture/tracing.md) for the full model. When adding a new workflow `*Requested` event, register it as a span opener in `foundry_core::event::is_span_opener`.
```

- [ ] **Step 3: Commit**

```bash
git add AGENTS.md
git commit -m "docs(agents): document OTel tracing model and span-opener registry"
```

### Task 8.3: Update the deployable skill

**Files:**

- Modify: `skill/foundry/SKILL.md`
- Modify: `skill/foundry/references/workflows.md`
- Modify: `skill/foundry/references/event-model.md`

- [ ] **Step 1: Workflow doc — rename event references**

In `skill/foundry/references/workflows.md`, replace the old event names everywhere:

- `iteration_requested` → `project_iteration_requested`
- `maintenance_requested` → `project_maintenance_requested`
- `maintenance_run_started` (system) → `maintenance_cycle_started`
- `maintenance_run_started` (per-project) → `project_run_started`
- `maintenance_run_completed` (system) → `maintenance_cycle_completed`
- `maintenance_run_completed` (per-project) → `project_run_completed`
- `greet_requested` → `greeting_requested`

- [ ] **Step 2: Event model doc**

In `skill/foundry/references/event-model.md`, update any event taxonomy tables to match the new names. Add a short note that every event now carries `span_id` and `parent_span_id`.

- [ ] **Step 3: Bump skill version**

In `skill/foundry/SKILL.md`, update the metadata `version` field to `0.17.0`.

- [ ] **Step 4: Commit**

```bash
git add skill/foundry/
git commit -m "docs(skill): bump to 0.17.0; rename event references; note span fields"
```

### Task 8.4: Bump workspace version

**Files:**

- Modify: `Cargo.toml` (workspace `[workspace.package].version`)

- [ ] **Step 1: Bump**

```toml
[workspace.package]
version = "0.17.0"
```

- [ ] **Step 2: Refresh Cargo.lock**

Run: `cargo build --workspace`

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: bump workspace version to 0.17.0"
```

### Task 8.5: Final quality gates and release-readiness

- [ ] **Step 1: Full workspace gates**

Run:

```bash
cargo fmt --all -- --check
cargo clippy --workspace -- -D warnings
cargo test --workspace
mdbook build book/
scripts/tests/migrate-event-names_test.sh
```

Expected: all PASS.

- [ ] **Step 2: Smoke test locally**

```bash
./install.sh
launchctl unload ~/Library/LaunchAgents/com.mojility.foundryd.plist
launchctl load   ~/Library/LaunchAgents/com.mojility.foundryd.plist
foundry registry list
foundry iterate <some-project>   # if any project is safe to iterate
foundry trace <root-event-id>    # confirm tree output renders
foundry trace <root-event-id> --flat
```

Watch for the legacy-event-names startup guard to fire if you haven't run the migration. Run `scripts/migrate-event-names.sh` if needed.

- [ ] **Step 3: Push**

```bash
git push origin main
```

- [ ] **Step 4: Tag and release per AGENTS.md procedure**

Foundry is released manually (cannot self-release):

```bash
git tag v0.17.0
git push origin main --tags
```

Wait for the Release workflow to publish tarballs, then re-run `./install.sh` and reload launchd as the AGENTS.md procedure prescribes.

---

## Self-Review Checklist

Verified against the spec:

- ✅ Phases match spec's "Implementation phasing" order (1 → 8).
- ✅ Eight "Decisions locked" items all addressed (hex IDs, W3C TRACEPARENT, no span kind/status/attrs, MaintenanceRun rename, noun-form workflow request rename, hard cutover with self-identifying schemas, historical backfill).
- ✅ Data-model spec is reflected in Tasks 1.2, 1.3, 2.1 (Event + BlockExecution + proto).
- ✅ Span boundaries + stamping rules reflected in Tasks 3.1 (block_span_id) and 3.2 (stamp_span_context).
- ✅ Event taxonomy renames in Phase 4 cover all six rename rows in the spec.
- ✅ Historical backfill in Phase 4.5 implements all five rename rules from spec's table, plus id recomputation and payload-reference fixup.
- ✅ Subprocess propagation in Phase 5 covers shell.rs, agent_stream.rs, direct Command::new sites, and CLI `foundry emit`.
- ✅ Span RPC, trace_store indexes in Phase 6 match spec's "Storage and read-path" section.
- ✅ CLI tree rendering in Phase 7 matches spec's "CLI display" mock-up shape.
- ✅ Docs (book/AGENTS.md/skill/CHANGELOG) phase touches all locations listed in spec's "File-level impact."

Type-name consistency: `SpanContext`, `SPAN_CONTEXT`, `mint_trace_id`, `mint_span_id`, `is_legacy_trace_id`, `is_span_opener`, `stamp_span_context`, `inject_traceparent` / `inject_traceparent_std`, `from_event`, `find_span`, `build_forest`, `SpanNode`, `render`, `parse_traceparent_from_env`, `detect_legacy_event_names`, `SpanRequest`, `SpanResponse`, `SpanResult` — used consistently across tasks.

Open question accepted from spec: Phase 4.5's startup guard chose the "refuse to *start*" shape (not "refuse to *write summaries*"). The fail-fast behavior matches the spec's stated preference and is operationally clearer.
