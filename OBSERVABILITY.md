# Observability

A plan for one normalized record of everything an agent does under Foundry,
captured as it happens, stored where the daemon can answer questions about it,
and rolled up through blocks, work items, campaigns and initiatives so the whole
estate reads top to bottom from one API.

Status: plan for review, 2026-10-16. Nothing here is built. Every number in
section 1 was measured on `mojility-ops-01` on 2026-10-16 unless the sentence
says otherwise.

## 0. Why

The charter says observability is paramount: if it happened, you can see it.
Today that is true of workflow events and false of agent work. Between
`agent_session_started` and `agent_session_ended` Foundry emits nothing, and a
session can run for an hour (BACKLOG, "Show that an agent session is alive").
Everything a reader wants during that hour, and most of what an investigator
wants afterwards, lives in provider transcripts that only ops-visualizer
parses, by tailing files on the daemon's disk.

Three things follow from that, and this plan fixes all three:

1. There is no normalized account of agent activity. Claude Code and Codex
   write different dialects, and three parsers exist for them (foundry's
   `token_usage.rs`, foundry's `codex.rs`, ops-visualizer's `Metrics`),
   none complete, none shared.
2. The daemon cannot answer questions about agent work. There is no RPC for
   sessions, tool calls, tokens or cost. Consumers read `~/.foundry/events`,
   `~/.foundry/agent-sessions` and `~/.foundry/traces` from disk, so a
   dashboard must run on the daemon host and must re-derive attribution that
   Foundry knew at dispatch time and did not write down.
3. Cost is known only at the end of a session, only from a terminal record,
   and not at all for OpenCode or an interrupted session.

## 1. What exists today

### 1.1 Agent sessions

`CliAgentGateway` (`crates/foundry-blocks/src/gateway/engine.rs`) runs one
lifecycle for every provider: mint a UUID session id, resolve model and
effort from `~/.foundry/agents.json`, build argv through a `CliAgentAdapter`,
emit `agent_session_started`, spawn the CLI through `AgentStreamRunner`
(`agent_stream.rs`), tee stdout line by line to
`~/.foundry/agent-sessions/<session_id>.jsonl`, wait for exit, then interpret
the collected lines and emit `agent_session_ended`.

The runner collects every line into a `Vec` and hands the whole thing to the
adapter after the process exits. Nothing reads a line while the agent is
running. The `TRACEPARENT` env var is injected so a child that runs
`foundry emit` parents correctly, but the agent CLIs themselves do not use it.

| Provider | Invocation | Transcript dialect | Usage parsed |
| --- | --- | --- | --- |
| claude (`claude-code`) | `claude --print --output-format stream-json --verbose --session-id <id>` | `system` (init, thinking_tokens), `assistant` (per API request, with `message.usage`, `message.model`, `request_id`, `parent_tool_use_id`), `user` (tool results), `tool_progress`, `rate_limit_event`, terminal `result` | Terminal `result` only: `total_cost_usd`, `modelUsage`, cache TTL split |
| codex | `codex exec --json -m <model> -c model_reasoning_effort=<e> -o <last>.txt -` | `thread.started` (with `thread_id`), `turn.started`, `item.started` / `item.completed` (`command_execution` with command, `aggregated_output`, `exit_code`; `agent_message`; `file_change`; `web_search`; `collab_tool_call`), `turn.completed` (with `usage`), `turn.failed`, `error` | Terminal `turn.completed` only; model from Foundry's tier hint because the stream never names it |
| opencode | `opencode run --format json --model <m>` | JSON events, session id extracted, `opencode export` for the answer | None. Every OpenCode session is unmeasured spend |

Line mix across the last 300 transcripts on ops-01:

| Line type | Count | Provider |
| --- | ---: | --- |
| `item.completed` | 7,738 | codex |
| `item.started` | 6,696 | codex |
| `system` | 939 | claude |
| `assistant` | 624 | claude |
| `user` | 262 | claude |
| `tool_progress` | 253 | claude |
| `turn.started` / `thread.started` | 205 each | codex |
| `turn.completed` | 194 | codex |
| `rate_limit_event` | 108 | claude |
| `turn.failed`, `error` | 1 each | codex |

Codex items by type: 13,564 `command_execution`, 1,108 `agent_message`, 74
`web_search`, 42 `file_change`, 4 `collab_tool_call`. Codex emits no
timestamps on stream lines and no model id. Claude Code emits `timestamp`,
`request_id` and `usage` on every `assistant` line, so per-request spend is
exact for Claude and per-turn for Codex.

Both CLIs also keep their own richer session stores on the host, outside
Foundry's view: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (1,338 files,
2.1 GB; `token_count` per response with `model_context_window` and rate
limits, `reasoning` items, `session_meta` with cwd and cli version) and
`~/.claude/projects/<cwd>/<session>.jsonl` (1,764 files, 610 MB). The codex
`thread_id` on `thread.started` is the join key to its rollout.

### 1.2 Pricing

`crates/foundry-sdk/src/token_usage.rs` scans a transcript from the end for
the one terminal record and normalizes it to `SessionUsage` (fresh input,
output with reasoning as a subset, cache read, cache write, 1h share).
`token_rates.rs` prices it from `~/.foundry/token-rates.json` into a
`CostEstimate` with a `basis` (`provider_reported`, `list_priced`,
`partially_priced`, `unmeasured`). Both ride on `agent_session_ended`. The
model is sound; it just runs once, at the end, over one record.

### 1.3 Workflow events and traces

Every event carries `trace_id`, `span_id`, `parent_span_id`, `causation_id`,
`gather_id` and `source` (`book/src/architecture/tracing.md`). The engine
emits `block_started` and `block_completed` around every block, and these are
the bulk of the log: 13,166 and 13,160 this month against 2,317
`agent_session_started`. Events are appended to `~/.foundry/events/YYYY-MM.jsonl`
by `spawn_event_audit_writer` in `foundryd/main.rs`, a subscriber on the
256-slot broadcast channel. If it lags it logs an error and the events are
gone from the durable log. BACKLOG also records a partial-line corruption from
a kill mid-write.

Completed chains are written as `ProcessResult` (every event plus every
`BlockExecution` with `raw_output`, trigger and emitted payloads) to
`~/.foundry/traces/YYYY-MM-DD/<root event id>.json`, pretty-printed. The
`TraceStore` keeps recent results in memory with a TTL, falls back to disk for
`Trace` by root event id, and answers `Span` from memory only.

### 1.4 The API surface

`proto/foundry.proto` has 50 RPCs. The observability-shaped ones:

| RPC | What it answers | What it cannot |
| --- | --- | --- |
| `Watch` (stream) | Events as they happen, filtered by project | No cursor, no replay. A subscriber that lags skips events with a warning. A reconnect misses whatever happened while it was down |
| `Trace` | Every event and block execution under a root event id | Lookup by `trace_id`, by work item, by time |
| `Span` | One span's events and blocks | Anything evicted from memory |
| `History` | Root traces per day with success and duration | Anything below the root |
| `ListWorkItemEvents` | One item's `work_item_*` events from the durable log | Other event types, other filters |

There is no RPC for an agent session, a tool call, a token count or a cost.

### 1.5 What ops-visualizer reads from disk

From its `AGENTS.md` and `lib/ops_visualizer/`:

| Reader | Reads | Why it must |
| --- | --- | --- |
| `Foundry.EventLog` | `~/.foundry/events/*.jsonl`, all files for campaign history, two months for a trace | Watch has no replay and no query |
| `AgentSessions.Ingest` | 24 hours of the event log at boot | A running session emits nothing, so a restart forgets it |
| `AgentSessions.Tailer` | The transcript at `source_log_path`, polled every 250 ms | Nothing between started and ended; fsevents misses codex appends |
| `AgentSessions.Liveness` | `stat` of each running transcript every 5 s | Same |
| `AgentSessions.Metrics` | Both dialects, incrementally by byte offset | Tool-call counts exist nowhere else |
| `Foundry.Spend` | `agent_session_ended` payloads | Cost exists nowhere else |
| `ProjectsLive` | `maintenance-audits/runs/<date>/summary.md` | The one remaining nightly file |

It also reconstructs attribution Foundry had and dropped: a session's campaign
from the trace that opened it, its cycle from position in the timeline, its
work item from `trace_id` plus `project`. A session for a `system` steer
formation on ops-01 today carries `"trace_id": ""`.

### 1.6 Sizes on ops-01

| Store | Size | Files |
| --- | ---: | ---: |
| `~/.foundry/events` | 201 MB | 3 months |
| `~/.foundry/traces` | 675 MB | 28 to 68 MB per day this week |
| `~/.foundry/agent-sessions` | 667 MB | 3,036 |
| `~/.foundry/tool-logs` | 2.5 GB | gate stdout/stderr, unindexed |

Nothing prunes any of these.

### 1.7 Adjacent, not in scope

The business event stream (`evt`, `~/Work/Operations/Events/intake`) receives
`ai_session_start`, `ai_tool_use` and `ai_session_end` from Claude Code hooks
on interactive sessions. That is a different system with a different owner,
and this plan does not merge the two. Section 3.6 keeps the door open.

## 2. The target

One sentence: Foundry writes a normalized activity record for every unit of
agent work as it happens, attributed to the work item, span, trace, campaign
and initiative that caused it, stores it durably, indexes it, and serves it
over gRPC with a cursor, so no consumer reads a Foundry file.

Principles, in the order they bind:

1. **Transcripts stay the evidence.** The raw provider stream keeps being
   tee'd byte for byte. Normalized records are derived from it and point back
   to it (session id, byte offset). A normalization bug is fixed by re-deriving,
   never by editing.
2. **Records are append-only facts**, in the MBOS sense. Rollups, costs and
   dashboards are derived from them. Any index can be deleted and rebuilt.
3. **Attribution is written at dispatch, not inferred at read.** The gateway
   receives the work item, source and stage with the request and stamps every
   record. ops-visualizer's by-trace and by-position rules become
   unnecessary.
4. **Unmeasured is not zero.** Every cost figure carries a basis and the
   count of sessions it could not price. That rule already exists; it extends
   to partial sessions and running sessions.
5. **The daemon answers; files are its storage.** Every consumer need in
   section 1.5 becomes an RPC with a cursor. A reader that still opens a file
   under `~/.foundry` is a gap in the API.
6. **Bounded by content, never by clock.** Record excerpts are size-bounded.
   Heartbeats describe progress; they never time anything out.

## 3. Design

### 3.1 The activity record

One JSON object per line, versioned (`v: 1`), with a common envelope and a
`kind`-specific body. Field names are `snake_case`. Every record carries the
envelope so a consumer can filter on any level without a join.

Envelope:

| Field | Meaning |
| --- | --- |
| `seq` | Monotonic per session, from 1. The cursor |
| `at` | RFC 3339, Foundry's receive time. Provider time in the body when the provider gives one |
| `session_id` | Foundry's session id (also the Claude session id; codex `thread_id` kept in the body) |
| `provider`, `agent_type`, `model` | Resolved at dispatch. `model` is the id Foundry asked for; a Claude `assistant` line may name a different one, kept on the turn |
| `project`, `working_dir` | From the request |
| `stage`, `tier`, `effort`, `effective_effort`, `access` | From the request |
| `trace_id`, `span_id`, `parent_span_id` | From the triggering event's envelope |
| `work_item_id` | From the ledger, when the dispatch opened or belongs to an item |
| `source` | `{kind, ref, cycle}` as on the event envelope: campaign and cycle, initiative, sentinel, operator host, parent work item |
| `transcript_offset` | Byte offset of the first raw line this record was derived from |

Kinds, with the body each carries:

| Kind | Body | Claude source | Codex source |
| --- | --- | --- | --- |
| `session_started` | prompt bytes, argv shape (flags, never the prompt), sandbox posture | `system/init` (tools, model, cli version, permission mode) | `thread.started` (`thread_id`), codex version from argv probe |
| `turn` | `request_id`, provider model, `stop_reason`, `usage` (fresh input, cache read, cache write 5m and 1h, output, reasoning), incremental `cost` with basis, `service_tier`, `speed`, `inference_geo`, `context_window` when known | Each `assistant` line (one per API request) | `turn.completed` (one per exec); `token_count` from the rollout when the secondary source is enabled |
| `tool_call` | `tool_id`, normalized `tool_kind` (`shell`, `file_read`, `file_edit`, `file_write`, `search`, `web_fetch`, `web_search`, `mcp`, `subagent`, `other`), raw `tool_name`, `summary` (the command or path, bounded), `parent_tool_id` for sub-agent work, state `started` | `tool_use` block on `assistant` | `item.started` for `command_execution`, `file_change`, `web_search`, `collab_tool_call` |
| `tool_result` | `tool_id`, `duration_ms`, `exit_code`, `is_error`, `output_bytes`, `output_excerpt` (bounded, head and tail) | `tool_result` block on `user` | `item.completed` with `aggregated_output`, `exit_code`, `status` |
| `tool_progress` | `tool_id`, elapsed, provider note | `tool_progress` | none |
| `message` | role, `text_bytes`, `excerpt` (bounded), `thinking_tokens_estimate` | `text` blocks; `system/thinking_tokens` | `agent_message` |
| `notice` | `notice_kind` (`rate_limit`, `provider_error`, `permission_denied`, `model_warning`, `safety_refusal`), text, structured fields (utilization, resets_at, status) | `rate_limit_event`, `result.is_error`, `permission_denials` | `error`, `turn.failed`, `item` of type `error` |
| `heartbeat` | `bytes_written`, `last_line_at`, counts so far (turns, tool calls, messages), `cost_so_far` with basis, `quiet_seconds` | Foundry's timer, every 15 s while the process lives | same |
| `session_ended` | status, exit code, `bytes_written`, failure metadata, final `usage` and `cost` (the current `AgentSessionEndedPayload`), plus a `rollup` (counts by `tool_kind`, turns, duration, excerpt of the final answer) | `result` | `turn.completed`, process exit |

Two cross-checks are built in. The live `turn` records summed must agree with
the terminal record where the provider gives one (Claude `result.usage`);
disagreement is a `notice` of kind `accounting_mismatch`, not a silent choice.
And the tool-call count must agree with ops-visualizer's `Metrics` on the
fixture transcripts until that module is retired.

The schema is an executable contract in `contracts/activity-record-v1.json`,
like the initiative contracts, with fixture transcripts (redacted real ones
from ops-01, both dialects, including the `turn.failed`, `error`,
`rate_limit_event` and interrupted cases) and golden normalized output.
ops-visualizer pins the hash the way it pins the initiative contracts.

### 3.2 Adapters

A `TranscriptAdapter` trait in `foundry-blocks` beside the `CliAgentAdapter`
it pairs with:

```text
fn ingest(&mut self, raw: &str, offset: u64, at: DateTime) -> Vec<ActivityBody>
fn finish(&mut self, exit: &SettledRun) -> SessionRollup
```

Stateful, because both dialects split a tool call across two lines (Claude
`tool_use` then `tool_result`; codex `item.started` then `item.completed`) and
the adapter holds the open calls by id. Three adapters:

- **Claude Code** (stream-json). Complete today from the stream alone.
  `--include-partial-messages` is not needed for v1 and would multiply line
  volume; `tool_progress` already gives in-flight signal.
- **Codex** (`exec --json`). Complete for tool calls and messages; `turn`
  usage once per exec; no model, no timestamps (Foundry stamps receive time).
  Optional secondary source: the rollout file under `~/.codex/sessions`
  joined on `thread_id`, read at session end, for per-response `token_count`
  and `reasoning` items. Off by default until its fields are verified against
  two codex versions.
- **OpenCode**. Tool calls and messages from its JSON events; `turn` absent;
  cost basis `unmeasured`. OpenCode has no proven writable-root enforcement
  and no usage parser. The plan does not spend effort on it beyond keeping
  its sessions visible. Decision for Stacey in section 6.

The `AgentStreamRunner` gains a per-line observer instead of returning the
collected `Vec<StreamedLine>`. The observer calls the adapter, appends records
to the activity log, and publishes them. Interpretation after exit keeps
working from the records the adapter accumulated, so `interpret` loses its
dependence on re-reading the transcript. `token_usage::parse_transcript` stays
as the cross-check and the backfill parser.

The heartbeat is a `tokio` interval task started with the process and stopped
at exit, as BACKLOG's first entry asks. It writes a `heartbeat` record and
updates the in-memory session row the RPCs read. It never goes into the
workflow event log.

### 3.3 Attribution

`AgentRequest` carries `trace_id` only. It gains an `ObservationScope`:

```text
trace_id, span_id, parent_span_id, work_item_id, source, stage
```

The engine knows the triggering event's envelope when it dispatches a block,
and `work_admission` knows the work item. Blocks already know their stage.
The gateway copies the scope onto every record and onto both lifecycle
events, so `agent_session_started` finally names its work item and source.
`foundry queue show <id>` then names the provider, tier, model and session id
working the item, from the ledger rather than from a trace match.

The scope is the hierarchy key for section 3.5. Nothing in it is inferred.

### 3.4 Store, index, API

**Durable form.** `~/.foundry/activity/YYYY-MM-DD/<session_id>.jsonl`, one
file per session, appended as records arrive, with the newline-repair rule
BACKLOG asks for applied to this writer and to the event writer. Per session
rather than per day because the hot path is "follow this session", and
because a session file is complete and immutable once `session_ended` lands,
which makes retention a directory move.

**Write before publish.** The activity writer is on the ingest path, not a
broadcast subscriber. A record is appended, then published to the live
channel. The same change should be made for the event audit writer: today a
lagging subscriber silently loses durable events, and that is the one place
the current design can lie.

**Index.** A SQLite file at `~/.foundry/observe.db` (`rusqlite`, bundled),
derived from the activity and event logs, rebuildable with
`foundry observe reindex`. Tables: `sessions` (one row per session with the
envelope and rollup), `turns`, `tool_calls`, `notices`, `events` (the
envelope of every workflow event, so `trace_id`, type, project and time
queries no longer scan 200 MB of JSONL), `scopes` (section 3.5). The index
exists for queries the in-memory `TraceStore` cannot answer: three months of
events, lookup by `trace_id`, aggregates by model or by day. The in-memory
store stays for the hot window. This is the one new dependency the plan
introduces, and it is a decision (section 6).

**RPCs**, all additive:

| RPC | Shape | Replaces |
| --- | --- | --- |
| `ListSessions` | Filters: project, provider, state, work item, trace, source, since/until; paged | `EventLog.agent_session_events_since`, `Ingest` replay |
| `GetSession` | Envelope, rollup, cost, liveness (last line at, quiet seconds) | `Liveness`, `Store` |
| `SessionActivity` | Records for one session from `after_seq`; `follow: true` keeps streaming until `session_ended` | `Tailer`, `Metrics` |
| `WatchActivity` (stream) | Live records across sessions, filtered by scope; heartbeats included; `after` cursor replays from the index on reconnect | the transcript poll loops |
| `QueryEvents` | Durable workflow events by trace, type, project, work item, time; cursor | `EventLog.for_trace`, `recent_months_events`, `campaign_events` |
| `Watch` | Gains `after_event_id`; the daemon replays from the index then joins the live channel. Lag is reported as a gap record the client can fill with `QueryEvents` | the lossy stream |
| `Trace` | Accepts `trace_id` as well as a root event id; `Span` falls back to disk | memory-only lookups |
| `SpendReport` | Aggregate cost and tokens for a scope (session, work item, cycle, campaign, initiative, project, day), by model, with basis and unmeasured count | `Foundry.Spend` |

CLI: `foundry observe sessions [--project] [--running]`, `observe show
<session>`, `observe tail <session|work item>` (the live view BACKLOG asks
for), `observe cost <scope>`, `observe reindex`, `observe prune --older-than`.
Every command takes `--json`.

### 3.5 Mirroring the hierarchy

Once elemental records carry the scope, the hierarchy is a set of rollups
over them, computed in the index and served by one RPC:

```text
initiative  →  campaign  →  cycle (work item)  →  trace  →  span (workflow, block)  →  session  →  turn  →  tool call
                              sentinel run / operator task  ↗
```

`GetScope(kind, ref)` returns the node's own facts (from the campaign store,
ledger or trace store, whichever owns it), its rollup (sessions, turns, tool
calls by kind, tokens by model, cost with basis, wall time, agent time, idle
time, outcome), and its children with their rollups. The same call answers
"what did initiative `mojentic-3-0` spend this week", "which cycle of
`mojentic-3-0-ex` ran the most shell commands", and "what is this running
item doing right now". The usage study measured 310 hours of owner wait
against 119 hours of execution by hand; this makes that a query.

Span and block nodes come from the events already stamped; the `block_*`
events become the block level without new emission. Agent sessions attach to
the block span that dispatched them through the scope's `span_id`.

ops-visualizer's `/work`, `/sources`, `/sources/campaigns/:name`,
`/sources/initiatives/:name`, `/traces/:id` and `/sessions/:id` become
renderings of `GetScope` and `SessionActivity`. Its `AgentSessions.*`,
`Foundry.EventLog`, `Foundry.Spend` and `CampaignActivity` attribution code
are deleted when the RPCs land. That is a consumer migration with its own
plan in that repository; this document only commits to the API that makes it
possible.

### 3.6 Later, kept possible

- **Interactive sessions.** A Claude Code `Stop` hook today logs to the
  business stream. A `foundry observe ingest --provider claude <transcript>`
  command that runs the same adapter over a native session file would bring
  Stacey's own sessions into the same index with `source.kind = operator`.
  Not in this plan; the record shape allows it.
- **Off-host dashboards.** With no file reads left, ops-visualizer can run
  anywhere that reaches the daemon over the trusted LAN control plane.
- **OTLP.** Both CLIs can export OpenTelemetry. A receiver in `foundryd`
  would be a fourth adapter with no file at all. Worth measuring against the
  transcript path once v1 exists; not before, because the transcript is the
  evidence we already keep.
- **Control.** Interrupt and follow-up on a running session were deferred by
  the 2026-05-09 visibility plan and stay deferred. Observation first.

## 4. Phases

Each phase lands on `main` by ordinary commits on the Mac daemon, proven on
the sandbox, then promoted by the release procedure. Done evidence is what a
reviewer checks, not what the code claims.

### Phase 0: contract and fixtures

- `contracts/activity-record-v1.json` and the record types in `foundry-sdk`
  (`activity.rs`), with serde round-trip tests.
- Fixture transcripts under `crates/foundry-blocks/tests/fixtures/transcripts/`:
  one claude and one codex session from ops-01, redacted; plus the
  `turn.failed`, `error`, `rate_limit_event`, `permission_denials` and
  interrupted (no terminal record) cases.
- Golden normalized output for each fixture; a test that the tool-call count
  matches ops-visualizer's `Metrics` on the same files.

Done: the contract exists, the goldens pass, and no daemon behaviour changed.

### Phase 1: streaming adapters and heartbeat

- `AgentStreamRunner` line observer; `TranscriptAdapter` for claude and
  codex; opencode adapter emitting `tool_call`, `message`, `session_*` only.
- Activity writer (append, newline repair) to
  `~/.foundry/activity/<date>/<session>.jsonl`; live channel; heartbeat task.
- `session_ended` rollup from live records, cross-checked against
  `parse_transcript`; mismatch recorded as a notice.
- `agent_session_started` and `agent_session_ended` keep their shape and gain
  nothing yet.

Done: a `foundry task sample-rs` on the Mac daemon produces an activity file
whose `tool_call` records appear while the agent runs (watched with `tail
-f`, since the RPCs come next), `heartbeat` every 15 s, and a `session_ended`
whose cost equals the current event's cost. CI passes with the fixtures.

### Phase 2: store, index, API

- SQLite index and `foundry observe reindex`; `ListSessions`, `GetSession`,
  `SessionActivity`, `WatchActivity`, `QueryEvents`, `SpendReport`; `Watch`
  cursor; `Trace` by `trace_id`; `Span` disk fallback.
- Event audit writer moved onto the write-before-publish path.
- `foundry observe` CLI.
- Backfill: `reindex --from-transcripts` runs the adapters over the 3,036
  existing transcripts on ops-01 with `backfilled: true` on every record and
  the scope recovered the way ops-visualizer recovers it today (by trace,
  labelled `inferred`). History becomes queryable without pretending it was
  stamped.

Done: `foundry observe tail <running work item>` on ops-01 shows commands as
codex runs them. `foundry observe cost initiative:mojentic-3-0 --json` returns
a figure whose total matches `foundry campaign report` for each member.
ops-visualizer `EventLog.for_trace` and `SessionActivity` return the same
events for ten sampled traces.

### Phase 3: attribution at dispatch

- `ObservationScope` on `AgentRequest`, filled by every block that invokes an
  agent (formation, execution, review, steer, summaries, digests).
- `work_item_id` and `source` on both lifecycle events and every record.
- `foundry queue show <id>` names the session; `queue` running rows show
  last-line age and tool-call count from the heartbeat.

Done: on ops-01, no `agent_session_started` emitted after the release has an
empty `trace_id`, and every one dispatched by a campaign carries the campaign
and cycle. ops-visualizer's `CampaignActivity.attribute/2` agrees with the
stamped source on a day of events, then is deleted in that repo.

### Phase 4: hierarchy rollups

- `scopes` table and `GetScope`; rollups for initiative, campaign, cycle,
  trace, span, session; wall, agent and idle time; outcome from the terminal
  event the way `Outcome` derives it today.
- `foundry observe scope <kind>:<ref>` with a tree view.
- ops-visualizer migration plan written in that repository.

Done: `GetScope(initiative, mojentic-3-0)` returns every member, every cycle,
every session, and a total that reconciles exactly with the sum of its
leaves. The 2026-10-11 usage study's owner-wait and execution hours are
reproduced from the API for the same window.

### Phase 5: retention and the daemon's own health

- `foundry observe prune` and a `retention` sentinel with per-store budgets
  (proposal: transcripts and activity 90 days, traces 30 days with
  `raw_output` dropped after 7, tool-logs 14 days, index unbounded).
- Daemon self-observation: the index records `Watch` lag gaps, writer
  failures and adapter parse failures as `notice` records on a `system`
  session, so the observability layer reports its own blind spots.

Done: sizes on ops-01 stop growing past budget; a deliberately corrupted
transcript line shows up as one parse notice and nothing else breaks.

## 5. Risks

- **Volume.** About 45 tool records per codex session today; 3,000 sessions
  over three months. Tens of thousands of rows a month is small for SQLite
  and for the live channel. The bound that matters is excerpt size:
  `aggregated_output` and tool results can be megabytes. Excerpts are capped
  (proposal: 2 KB head, 1 KB tail) and the transcript keeps the rest.
- **Two writers, one truth.** Writing the record before publishing it costs
  a disk write on the hot path. `BufWriter` with a flush per record is what
  the transcript tee already does; measure on the sandbox before deciding to
  batch.
- **Dialect drift.** The CLIs change under us: a codex version mismatch
  between the daemon's PATH and the shell's cost a day this month, and the
  stream shapes move between releases. The fixtures pin the dialects we have
  seen; an unrecognized line is a counted `notice`, never a crash and never a
  silent drop. Claude Code's
  `result` has grown fields (`subagent_stats`, `terminal_reason`); the adapter
  keeps unknown fields in a `raw` map on `session_ended` rather than losing
  them.
- **Reindex cost.** Rebuilding from 1.5 GB of JSONL is minutes, not seconds.
  Fine for a repair; not something a page does. The index is the daemon's.
- **Scope creep toward control.** Follow-up and interrupt are tempting once
  a live session is on screen. They stay out until observation is trusted.
- **Private content.** Prompts and tool output contain repository content and
  occasionally credentials an agent read. The activity store is as sensitive
  as the transcripts already are, lives in the same home, and never leaves
  the host over anything but the loopback or trusted-LAN gRPC endpoint.

## 6. Decisions for Stacey

Each blocks the phase named.

1. **SQLite as the derived index** (phase 2), or stay with in-memory indexes
   rebuilt at daemon start. Recommendation: SQLite, rebuildable, because the
   questions span months and the daemon should not hold three months of
   events in memory.
2. **Crate placement** (phase 1). Recommendation: record types in
   `foundry-sdk`, adapters beside the gateways in `foundry-blocks`, store and
   RPCs in `foundryd`. A separate `foundry-observe` crate only if `foundryd`
   grows past comfort.
3. **OpenCode** (phase 1): keep it visible but unmeasured, or remove the
   provider. Recommendation: keep visible, spend nothing on a usage parser.
4. **Codex rollout as a secondary source** (phase 2): read
   `~/.codex/sessions` for per-response token counts, or trust `turn.completed`
   alone. Recommendation: off by default, fields verified first.
5. **Retention budgets** (phase 5): the numbers proposed there, or others.
6. **Interactive sessions** (3.6): in or out of Foundry's index. No
   recommendation yet; it touches the business stream's ownership.

## 7. References

- `CHARTER.md` principle 4; `BACKLOG.md` first and second entries.
- `book/src/architecture/tracing.md`, `book/src/guide/inference-costs.md`.
- `crates/foundry-blocks/src/gateway/engine.rs`, `agent_stream.rs`,
  `gateway/codex.rs`, `gateway/opencode.rs`; `crates/foundry-sdk/src/token_usage.rs`,
  `token_rates.rs`, `payload/agent_session.rs`; `crates/foundryd/src/trace_store.rs`,
  `service/eventing_ops.rs`, `main.rs` (`spawn_event_audit_writer`).
- ops-visualizer `AGENTS.md` ("The event stream goes silent", "Agent sessions
  belong to a campaign by trace") and `lib/ops_visualizer/agent_sessions/`.
- Operations `Planning/2026-05-09-foundry-agent-session-visibility-v1.md` (the
  plan this one succeeds) and `Planning/2026-10-11-foundry-usage-study.md`
  (the questions this one makes answerable).
