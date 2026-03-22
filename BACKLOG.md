# Foundry Backlog — Observability & Audit Trail

Gaps identified by comparing foundry's current capabilities against the
audit trail produced by the maintenance shell scripts it replaces.

Each suggestion references the maintenance system pattern it derives from.

---

# Consolidated Strategy

The 12 gaps below share a root cause: **the data pipeline from shell
command to persistence is too narrow.** `shell::run()` captures full
stdout/stderr/exit_code in `CommandResult`, but blocks discard everything
after extracting a summary string. By the time data reaches
`BlockExecution` and the trace store, only a text summary survives.

Rather than 12 independent features, three foundational layers unlock
most of the observability value. Each layer builds on the one before it.

## Layer 1 — Enrich the Data Model

**Root fix.** Widen the channel so block execution data stops being
discarded.

### 1a. Add `raw_output` to `TaskBlockResult`

```rust
pub struct TaskBlockResult {
    pub events: Vec<Event>,
    pub success: bool,
    pub summary: String,
    pub raw_output: Option<String>,   // NEW — full stdout+stderr
    pub exit_code: Option<i32>,       // NEW — process exit code
}
```

Blocks that shell out attach the combined stdout/stderr. Blocks that
don't (e.g., `RouteProjectWorkflow`) leave these `None`. The engine
copies them into `BlockExecution` automatically — blocks don't need to
think about persistence.

*Addresses gaps: 2 (raw command output), 5 (decision rationale), 9
(structured error context), 12 (quality gate results).*

### 1b. Enrich `BlockExecution` with payloads and output

```rust
pub struct BlockExecution {
    // existing fields
    pub block_name: String,
    pub trigger_event_id: String,
    pub success: bool,
    pub summary: String,
    pub emitted_event_ids: Vec<String>,
    pub duration_ms: u64,
    // new fields
    pub raw_output: Option<String>,
    pub exit_code: Option<i32>,
    pub trigger_payload: serde_json::Value,
    pub emitted_payloads: Vec<serde_json::Value>,
}
```

The engine already has both the trigger event and emitted events in
scope when it builds `BlockExecution` — it just doesn't copy the
payloads today. This makes each execution record **self-contained**: you
can see what went in, what the block decided, what commands it ran, and
what came out, without chasing event IDs across JSONL files.

*Addresses gaps: 8 (block input/output payloads), 9 (structured error
context).*

### 1c. Expose enriched data via gRPC

Update `TraceBlockExecution` in `foundry.proto` to carry the new fields.
The `foundry trace` CLI renders raw output when present (e.g., behind a
`--verbose` flag to keep default output clean).

## Layer 2 — Persistent Traces

**Make traces survive daemon restarts and TTL expiry.**

Today the `TraceStore` is in-memory with a 1-hour TTL. Once expired, the
structured trace (which blocks ran, their timing, their raw output) is
gone. Events survive in JSONL but lack the tree structure that makes
traces useful.

### 2a. Write traces to disk on completion

When `engine.process()` finishes and the result is inserted into
`TraceStore`, also serialize the full `ProcessResult` (now enriched with
raw output and payloads) to disk:

```
~/.foundry/traces/YYYY-MM-DD/{event_id}.json
```

Date-partitioned directories give natural browsability. Each file is a
complete, self-contained record of one event chain — the equivalent of
the old system's entire run directory, but for any event chain, not just
maintenance runs.

### 2b. Disk fallback in `TraceStore`

When `TraceStore::get()` misses the in-memory cache, check disk before
returning `None`. This means `foundry trace <event_id>` works
indefinitely, not just within the 1-hour TTL window.

### 2c. Configurable traces directory

`FOUNDRY_TRACES_DIR` env var, defaulting to `~/.foundry/traces/`.

*Addresses gaps: 1 (run directories — persistent traces ARE the run
artifacts), 6 (long-term trace persistence), 7 (run metadata — embedded
in each trace file).*

**Why this replaces run directories:** The old maintenance system needed
separate run directories because it had no structured trace format —
artifacts were scattered across `run.json`, `maintain-results.json`,
`audit-results.json`, etc. Foundry's `ProcessResult` already contains
all of this in one structure. A single persistent trace file per event
chain is simpler, more composable, and works for any workflow — not just
nightly maintenance runs.

## Layer 3 — Historical Queries and Summaries

**Derived views on top of persistent traces.**

### 3a. `foundry history` command

```
foundry history                         # list recent traces by date
foundry history 2026-03-22              # list that day's traces
foundry history 2026-03-22 --project X  # filter by project
```

Reads from the `~/.foundry/traces/` directory. Each trace file has the
project name, event type, timing, and success/failure — enough to render
a useful listing without loading full raw output.

### 3b. `foundry summary` command

```
foundry summary <event_id>
```

Generates a human-readable markdown report from a persistent trace.
`summary.rs` already has the rendering logic — it just needs a consumer
that populates its input structs from a `ProcessResult`. This replaces
the old `summary.md` file with an on-demand command that works for any
event chain at any time.

### 3c. Structured event payloads for gate results

Parse hone's JSON output for the `gate_results` array and include it in
`ProjectIterateCompleted` and `ProjectMaintainCompleted` event payloads.
This improves on the old system which lost gate detail at the
aggregation layer. With enriched traces, these payloads are preserved
automatically.

*Addresses gaps: 3 (structured per-phase results — queryable from
traces), 4 (human-readable summary), 11 (historical query), 12 (quality
gate results in event payloads).*

## Remaining Standalone Item

### Event persistence target directory (Gap 10)

`FOUNDRY_EVENTS_DIR` already supports arbitrary paths. Change the
default from `~/.foundry/events/` to `~/Work/Operations/Events/intake/`
so Foundry events land in the same JSONL stream as all other business
events. This is a one-line config change, not an architecture item.

## Implementation Order

1. **Layer 1a + 1b** — enrich `TaskBlockResult` and `BlockExecution`,
   update engine to copy new fields, update blocks to populate raw output
2. **Layer 1c** — update proto and CLI trace rendering
3. **Layer 2a + 2c** — persistent trace writer with date-partitioned dirs
4. **Layer 2b** — disk fallback in `TraceStore::get()`
5. **Layer 3a** — `foundry history` command
6. **Layer 3b** — `foundry summary` command (wire existing `summary.rs`)
7. **Layer 3c** — gate results in event payloads
8. **Gap 10** — event directory default

Each layer is independently valuable and shippable.

---

# Original Gap Analysis

The items below are the original gaps identified from the maintenance
shell scripts. The consolidated strategy above addresses all of them.
They are preserved here for traceability.

---

## 1. Date-Partitioned Run Directories

**Maintenance pattern:** Every nightly run creates
`maintenance-audits/runs/YYYY-MM-DD/` containing all artifacts for that
run: `run.json`, `maintain-results.json`, `audit-results.json`,
`install-results.json`, `changed-projects.txt`, `summary.md`, and
`auto-release/` logs. Any run from the past month can be fully
reconstructed from its directory.

**Foundry gap:** No run directory concept. Events persist to JSONL and
the trace store holds results for 1 hour, but there is no single location
per run where all artifacts land.

**Suggestion:** When the orchestrator completes a maintenance run, write a
dated run directory (e.g., `~/.foundry/runs/YYYY-MM-DD/`) containing
aggregated results, the trace, and any raw output captured during the
run. This becomes the primary forensic artifact for investigating a run.

---

## 2. Raw Command Output Capture

**Maintenance pattern:** `maintain.sh` writes per-project timestamped
logs (`logs/{PROJECT}_{DATE}_{TIMESTAMP}.log`) containing the full hone
CLI JSON output — assessment, plan, execution actions, gate results.
`auto-release.sh` captures Claude's complete session transcript.
`audit-releases.sh` captures raw scanner output (pip-audit JSON, npm
audit JSON, cargo audit JSON).

**Foundry gap:** Task blocks log a summary string via `tracing::info!`
but do not capture or persist the raw stdout/stderr of the commands they
run. The shell runner returns `CommandResult` with stdout/stderr, but
blocks discard this after extracting what they need.

**Suggestion:** Add a `raw_output: Option<String>` field to
`TaskBlockResult`. Blocks that shell out should attach the raw
stdout/stderr. The engine can persist this alongside events in the run
directory, or as a separate log file per block execution. This is
critical for:

- Investigating why an audit found (or missed) a vulnerability
- Understanding Claude's reasoning when CutRelease invokes the CLI
- Debugging hone iterate/maintain failures from their full output

---

## 3. Structured Per-Phase Results

**Maintenance pattern:** Three separate JSON files aggregate outcomes by
phase — `maintain-results.json` (iterate/maintain per project),
`audit-results.json` (vulnerability verdicts per project),
`install-results.json` (install outcomes per project). These are
queryable with `jq` for quick answers like "which projects failed?" or
"which tags are vulnerable?"

**Foundry gap:** Events are the primary record, but there are no
aggregated result files. To answer "which projects failed last night?"
you'd need to parse JSONL and filter by event type and run_id.

**Suggestion:** After the orchestrator fan-in, write aggregated JSON
files to the run directory:

- `results-iterate.json` — per-project iterate outcomes
- `results-maintain.json` — per-project maintain outcomes
- `results-audit.json` — per-project vulnerability verdicts
- `results-install.json` — per-project install outcomes

These are derived views of the event stream, optimized for quick
querying. The events remain the source of truth.

---

## 4. Human-Readable Run Summary

**Maintenance pattern:** `summary.md` integrates all results into a
markdown report with overview table (processed/passed/failed),
per-project status table, release audit section, local installs section,
and auto-release logs in collapsible `<details>` sections.

**Foundry gap:** No summary generation. The `foundry trace` command
shows a tree for a single event chain, but there's no aggregate
human-readable report.

**Suggestion:** Generate `summary.md` in the run directory after fan-in.
Match the maintenance system's format: overview counts, per-project
table with iterate/maintain headlines and gate status, audit verdicts
table, install outcomes table, and embedded auto-release transcripts.
This is the document humans will actually read.

---

## 5. Decision Rationale Capture

**Maintenance pattern:** `auto-release.sh` captures Claude's full
session transcript to `auto-release/{PROJECT}.log`. This proved its
value on 2026-03-22 when Claude declined to release context-mixer2
because the stated premise (CVE-driven dependency updates) didn't match
reality — the transcript shows exactly why.

**Foundry gap:** CutRelease will invoke Claude CLI but the
IMPLEMENTATION_PLAN only mentions "capture output, check exit code."
No plan to preserve the full reasoning transcript.

**Suggestion:** When CutRelease invokes Claude, capture the complete
stdout/stderr to a log file in the run directory
(`auto-release/{PROJECT}.log`). Also store in the `raw_output` field
of TaskBlockResult. Auto-release decisions are high-stakes (they push
tags and create GitHub releases) — the full transcript is the most
valuable audit artifact in the system.

---

## 6. Long-Term Trace Persistence

**Maintenance pattern:** All run directories are kept indefinitely.
29+ days of history are available for comparison and investigation.

**Foundry gap:** TraceStore is in-memory with a 1-hour TTL (configured
in `main.rs`). After the TTL expires, the structured trace (which blocks
ran, their timing, their emitted events) is gone. Events persist in
JSONL but lack the tree structure that makes traces useful.

**Suggestion:** When a trace completes, serialize the full
`ProcessResult` (events + block executions + timing) to JSON in the run
directory as `trace-{event_id}.json`. This preserves the structured
trace indefinitely without keeping the in-memory store alive. The trace
store can remain as a hot cache for the `foundry trace` command, with
a fallback to reading from disk when the cache misses.

---

## 7. Run Metadata Record

**Maintenance pattern:** `run.json` records `date`, `completed_at`,
`projects_processed`, `passed`, `failed`, and `filter`. This is the
first file you check when investigating a run.

**Foundry gap:** The orchestrator (IMPLEMENTATION_PLAN Phase 5) will
emit `MaintenanceRunCompleted` with aggregate counts, but this is an
event in the JSONL stream, not a standalone metadata file.

**Suggestion:** Write `run.json` to the run directory with: `run_id`,
`started_at`, `completed_at`, `throttle`, `projects_processed`,
`passed`, `failed`, `skipped`, `registry_version`, `foundry_version`.
This is the run's identity card — quick to read, easy to compare across
days.

---

## 8. Block Input/Output Payload Snapshots

**Maintenance pattern:** `audit-results.json` records not just the
verdict but the raw scanner output and intermediate state (tag checked
out, vulnerabilities found, main branch status). You can see exactly
what the audit tool reported.

**Foundry gap:** `BlockExecution` records `block_name`, `success`,
`summary`, `emitted_event_ids`, and `duration_ms`. It does not capture
the trigger event's payload (what the block received) or the emitted
events' payloads (what the block produced). To reconstruct a decision,
you'd need to cross-reference multiple events in the JSONL.

**Suggestion:** Add `trigger_payload: serde_json::Value` and
`emitted_payloads: Vec<serde_json::Value>` to `BlockExecution`. This
makes each execution record self-contained — you can see what went in,
what came out, and what the block decided, without chasing event IDs
across files.

---

## 9. Structured Error Context

**Maintenance pattern:** `install-results.json` captures the full error
output when an install fails (e.g., "Executables already exist:
image-namer, image-namer-ui"). `audit-results.json` captures specific
failure modes ("dirty working tree", "audit tool unavailable",
"checkout failed", "stuck in detached HEAD"). These are structured
and queryable.

**Foundry gap:** `BlockExecution.summary` is a `String`. When a block
fails, the summary might say "error: hone maintain failed" but not
include the exit code, stderr, or specific failure category.

**Suggestion:** Add structured error fields to `BlockExecution`:

```rust
pub struct BlockExecution {
    // ... existing fields ...
    pub error_category: Option<String>,  // e.g., "git_checkout_failed", "scanner_unavailable"
    pub exit_code: Option<i32>,
    pub stderr: Option<String>,
}
```

Or introduce a `BlockError` enum with structured variants. This enables
filtering run results by error type ("show me all git failures across
the last week").

---

## 10. Event Persistence Target Directory

**Maintenance pattern:** Events are logged to
`~/Work/Operations/Events/intake/YYYY-MM.jsonl` via `evt log`. This is
where evt-cli reads from, where schemas are validated, and where all
business observability queries run.

**Foundry gap:** EventWriter currently writes to
`~/.foundry/events/YYYY-MM.jsonl`. This is a separate location from the
established event system. Events written by foundry are invisible to
`evt query`, `evt stats`, and any tooling built on the Operations event
log.

**Suggestion:** Make the EventWriter's output directory configurable,
defaulting to `~/Work/Operations/Events/intake/`. This is already noted
in IMPLEMENTATION_PLAN Phase 4.3 but worth emphasizing: foundry events
should land in the same stream as all other business events.

---

## 11. Historical Query Command

**Maintenance pattern:** Run directories are browsable by date. You can
`ls runs/` and `cat runs/2026-03-20/audit-results.json | jq` to
investigate any past run.

**Foundry gap:** No `foundry history` or `foundry runs` command. To
investigate a past run, you must know the root event_id and query within
the 1-hour TTL window, or manually parse JSONL files.

**Suggestion:** Add `foundry history` command that:

- Lists recent runs by date (from run directories or JSONL scan)
- `foundry history 2026-03-22` shows that run's summary
- `foundry history 2026-03-22 --project hone-cli` shows one project's
  trace
- Falls back to JSONL event parsing when run directories aren't
  available

---

## 12. Quality Gate Results Capture

**Maintenance pattern:** Raw hone CLI output in project logs contains
full gate verification results — which gates ran, pass/fail status,
output from each gate command. However, `maintain-results.json` only
records gates as "unknown" — a known gap in the maintenance system too.

**Foundry gap:** RunHoneIterate and RunHoneMaintain blocks will parse
hone's JSON output for headlines, but the IMPLEMENTATION_PLAN doesn't
specify capturing structured gate results.

**Suggestion:** Parse hone's JSON output for the `gate_results` array
and include it in the `ProjectIterateCompleted` and
`ProjectMaintainCompleted` event payloads:

```json
{
  "gates_passed": true,
  "gate_results": [
    { "name": "test", "passed": true, "duration_ms": 4200 },
    { "name": "lint", "passed": true, "duration_ms": 800 },
    { "name": "format", "passed": false, "output": "2 files need formatting" }
  ]
}
```

This would actually improve on the maintenance system, which loses this
detail at the aggregation layer.
