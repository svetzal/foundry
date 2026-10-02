# Tasks and Campaigns

Foundry has two engineering dispatch primitives:

- A **task** executes one concrete objective immediately.
- A **campaign** holds a broader mission and derives one task at a time from
  current repository state until evidence proves the mission complete.

Campaigns do not contain a pre-cut queue. The next objective is created only
when the preceding task has a typed result, so stale downstream inventory and
per-item retry loops are unnecessary.

## Why campaigns reassess scope

Implementation is a source of new knowledge. It can expose hidden dependencies,
invalid assumptions, partial solutions, and work that is already complete.
Campaign formation reassesses the full remaining mission after each implementation
cycle. It uses actual implementation experience, reviewer findings, preserved work,
and current repository evidence to derive the next objective.

The reassessment can change scope, priority, or task decomposition within the
mission and owner constraints. It can retain an earlier objective when the evidence
still supports it. It does not merely select the next item from an initial plan
or repeat the reviewer's gap list.

The final reassessment checks the whole mission. A complete task verdict proves
its assigned objective, which can be narrower than campaign completion.

Evaluate formation by whether it uses new evidence to choose appropriate work and
preserves useful learning. Assessment token volume and the ratio of formation to
implementation sessions do not establish waste. When reducing cost, preserve the
full reassessment and the evidence it needs. The charter records this as a design principle.

## Choosing a Task or Campaign

Use a task when you can state one objective whose acceptance evidence is
available now:

```bash
foundry task parite-cli \
  "Add a --quiet flag and prove it suppresses progress output"
```

Use a campaign when any of these are true:

- the mission spans several independently reviewable changes;
- later work depends on what the repository reveals after earlier work lands;
- the work may require an explicit owner decision;
- production or other external evidence is part of completion;
- a bounded cycle budget is needed to limit autonomous work.

A campaign is deliberately not a substitute for a backlog. If the work is
already a known sequence of unrelated tasks, dispatch those tasks directly.
Campaign formation is valuable when each next objective should be derived from
mission minus current evidence.

## Lifecycle at a Glance

```mermaid
flowchart TD
    A["staged campaign"] --> B["advance"]
    B --> C{"formation decision"}
    C -->|"done"| D["completed"]
    C -->|"advance one objective"| E["isolated task"]
    C -->|"human judgement or budget"| F["escalated"]
    E --> G{"typed task result"}
    G -->|"complete or remainder"| B
    G -->|"defect"| B
    G -->|"blocked on decision"| F
    G -->|"provider unavailable"| H["paused"]
    H -->|"resume"| B
    F -->|"decide or extend budget"| B
    A -->|"cancel"| I["cancelled"]
    B -->|"cancel"| I
    F -->|"cancel"| I
    H -->|"cancel"| I
```

`cancelled` is terminal and has no edge back: it records that the mission was
abandoned, not achieved. Use `paused` for a campaign meant to resume.

Each dispatched task consumes one cycle. Formation, retries caused by a
transient decision-provider transport failure, and an automatic provider pause
do not consume cycles. A landed final task still receives completion evaluation. An exhausted budget
with an unlanded non-complete result escalates before gates or formation run.

## Durable Ownership

The daemon owns the durable campaign inventory. Online
`foundry campaign add/list/show/advance/pause/resume/decide/complete/cancel` all
go through typed gRPC and do not read, create, or mutate `FOUNDRY_CAMPAIGNS_PATH`.
Successful online reads and mutations render the daemon's typed response
directly, so stale client-side campaign files cannot mask the live daemon-owned
state. Pass `--offline` only for direct-file recovery while the daemon is
stopped. If the daemon is unreachable, the online command fails and leaves any
absent or pre-existing client-side `FOUNDRY_CAMPAIGNS_PATH` byte-identical.

The read-only inventory surface starts with two gRPC queries:

- **`ListCampaigns`** — returns summary/status records sorted by campaign name,
  with an optional exact `project` filter. Missing or empty stores return an
  empty list; malformed or unreadable stores return a gRPC error rather than an
  implicit empty inventory.
- **`GetCampaign`** — retrieves the complete definition of one campaign by exact
  name, including `intent_refs`, `context_paths`, all `done_evidence` entries
  with the `Gate`/`Review` type distinction preserved, and escalation rules.
  Returns `NOT_FOUND` (not an implicit empty) when the name is absent from the
  store.

## One-Shot Tasks

```bash
foundry task parite-cli "Add a --quiet flag and prove it suppresses progress output"
foundry task parite-cli "Fix the parser regression" --agent codex
```

The task formation:

1. Creates a disposable Git worktree under `~/.foundry/worktrees/`.
2. Runs the coding agent and quality gates inside that worktree.
3. Performs a read-only skeptical review against the objective and evidence.
4. Emits one typed verdict: `complete`, `remainder`, `defect`,
   `blocked_on_decision`, or `runner_error`.
5. Commits all task work before returning a terminal result.

Two verdicts may fast-forward the registered trunk branch. A `complete` verdict
with passing required gates lands, as does a `remainder` — the reviewer's term
for a finite list of missing work on a _converging_ implementation — provided at
least one required gate ran and every required gate passed. Converging work
integrates rather than accumulating a long-lived divergent branch, and the green
required gates are what keep trunk from going red; a `remainder` with no
required gate to vouch for it does not land. Its reviewer gaps travel forward in
the typed result and become the campaign's next objective.

### When trunk moves during a task

A task can run for an hour. On a shared repository, other people and sessions
push to trunk in that time. The task branch then cannot fast-forward trunk,
but that says nothing about the quality of the work. Foundry reconciles it:

1. It fetches trunk and rebases the task branch onto it inside the task
   worktree.
2. If the rebase is clean, it runs the project's required gates again on the
   rebased tree (fix commands do not run).
3. If the gates pass, the rebased work lands.
4. If trunk moved again while the gates ran, Foundry does steps 1–3 one more
   time. It makes no more than two rebase attempts in one task.

If the rebase conflicts, a required gate fails on the rebased tree, or trunk
is still moving after the second attempt, Foundry puts the branch back on the
reviewed commit and preserves it as usual. The reviewer's verdict does not
change. A `complete` that could not land is still `complete`, with `landed:
false`, `success: false`, and a typed `land_blocked` reason in the
`task_run_completed` payload:

| `land_blocked`             | Meaning                                                              |
| -------------------------- | -------------------------------------------------------------------- |
| `trunk_moved_conflict`     | Trunk moved and the rebase conflicted                                |
| `trunk_moved_gates_failed` | The rebase was clean, but a required gate failed on the rebased tree |
| `trunk_moved_repeatedly`   | Trunk moved again after the second rebase                            |
| `checkout_not_ready`       | The registered checkout was dirty or on the wrong branch             |
| `git_failed`               | Another git operation needed to land failed                          |

The payload also lists in `trunk_arrivals` the trunk commits that arrived
during the run (commit and subject, oldest first). This list is present when
the work landed after a rebase too. When trunk did not move, the payload has
neither field and the result is the same as before.

A `complete` with a `land_blocked` reason is preserved work to reconcile, not
a defect. Its work item settles `preserved` and its reason names the block.
The next campaign cycle starts from the preserved branch, and the campaign
history shows the cycle as `complete, did not land (<reason>)`. The nightly
majors lane does not dispatch the same upgrade again while that branch
exists.

`defect`, `blocked_on_decision`, and `runner_error` never land. Every result
that does not land is pushed to a named preservation branch; if no remote push
is possible, Foundry writes a Git bundle under `~/.foundry/preserved/`. Either
way the next cycle resumes from the preserved work; a bundle also carries
`HEAD`, so you can fetch or clone it by hand to recover the work yourself. Tasks
do not retry. A campaign decides whether the preserved result should seed
another objective.

## Campaign Definitions

Create a JSON definition file:

```json
{
  "name": "parite-phase-2d",
  "project": "parite-cli",
  "mission": "Prove both retrieval entrypoints preserve raw response identity.",
  "intent_refs": ["parite.intent.raw-retrieval-evidence"],
  "context_paths": [".alloy/projections/AGENTS.generated.md"],
  "done_evidence": [
    {
      "kind": "gate",
      "command": "cargo test -p parite-core retrieval_parity",
      "required": true,
      "artifacts": ["crates/parite-core/tests/retrieval_parity.rs"]
    },
    {
      "kind": "review",
      "statement": "The parity suite compares unmasked response IDs across both real entrypoints."
    }
  ],
  "budget": { "max_cycles": 12 },
  "escalation": ["A behavior choice requires an owner decision."],
  "authorized_by": "Stacey",
  "agent_provider": "codex"
}
```

### Definition fields

| Field               | Required      | Meaning                                                       |
| ------------------- | ------------- | ------------------------------------------------------------- |
| `name`              | Yes           | Stable campaign identifier, unique in the campaign store      |
| `project`           | Yes           | Exact registered project name                                 |
| `mission`           | Yes           | Durable outcome the formation evaluates                       |
| `intent_refs`       | No            | Opaque identifiers connecting the mission to source intent    |
| `context_paths`     | No            | Existing repository-relative neutral artifacts                |
| `done_evidence`     | Yes           | At least one mechanical gate or review statement              |
| `budget.max_cycles` | No            | Maximum dispatched tasks; defaults to 20                      |
| `escalation`        | No            | Conditions that require the formation to stop for an owner    |
| `authorized_by`     | Operationally | Owner identity required for decisions, completion, and resume |
| `agent_provider`    | No            | Campaign-specific provider override                           |

`context_paths` must be existing repository-relative files under the registered
checkout. Absolute paths, parent traversal, missing files, and symlink escapes
are rejected before the definition is saved. Foundry reads these neutral
artifacts but never invokes the tool that produced them.

Formation uses the balanced model tier with medium reasoning effort. The
independent task reviewer continues to use the deep tier with high effort.
Context files are listed by path and size for selective reading. Their normative
requirements still apply. Formation receives the mission, owner decisions,
reviewer gaps, recent commits, changed paths, and compact gate results.
It returns a decision; the daemon dispatches that decision. It must not probe
localhost or start nested Foundry workflows.

Passing gate output is omitted. Failed output keeps at most 512 bytes per gate
and 2 KiB across all gates. Full results remain in `CampaignAdvanceCompleted`.
An oversized packet fails before an agent starts. Binding declarations are
never silently truncated to fit the packet.

### Stage limits and repository scope

Campaign definitions accept these limits. Older definitions use these defaults:

```json
{
  "writable_repositories": ["my-project"],
  "budget": {
    "max_cycles": 4,
    "stages": {
      "formation_prompt_bytes": 32768
    }
  }
}
```

The prompt byte limit must be positive. Campaign formation, execution, and
review have no clock deadline, including the project's agent timeout. Legacy
stage time fields are ignored when reading existing definitions. The byte limit
applies to the rendered Foundry prompt, not provider instructions or files read
during a turn.

An empty `writable_repositories` means the campaign's project alone. Admission
rejects any other declaration. Multi-repository isolation and landing are not
supported. Sibling repositories remain read-only.

Campaign execution currently needs Codex to enforce this filesystem boundary.
An omitted campaign provider selects Codex. Other campaign providers fail before
formation starts. Codex uses `workspace-write`, clears inherited extra writable
roots, and permits Foundry's command-log directory. Foundry owns Git finalization.
Standalone tasks retain their existing provider and access behavior.

### Capture command output

Use the following command for verbose builds and tests:

```bash
foundry capture -- cargo test --workspace
```

This command needs no daemon. It returns the child's exit code and stores both
complete streams in `~/.foundry/tool-logs/`. Passing commands print only status
and log paths. Failures print at most 2 KiB from each stream. Use `--log-dir`
to choose another directory. Logs remain until you remove them.

Coding prompts request this command for verbose output. Codex also applies a
2,000-token limit to tool output retained in conversation history. The native
limit does not preserve a separate complete log; use `capture` for that.

### Prove acceptance early

Before broad fixture or documentation changes, the executor must exercise the
hardest acceptance behavior through the real boundary. It must record the
rejecting case and the corrected passing case in `.foundry/proof.json`:

```json
{
  "kind": "behavioral",
  "source_change": "Describe the changed source/input and its paths",
  "rejecting": {"command": "test command", "exit_code": 1, "log": "full-log-path"},
  "corrected": {"command": "test command", "exit_code": 0, "log": "full-log-path"}
}
```

Write `source_change` as a single non-empty string. For compatibility, a
non-empty array of non-empty strings is interpreted as their joined text.
`rejecting` and `corrected` are top-level objects. Validate the file's JSON,
field types, actual exit codes, and log paths before finishing.

For a non-behavioral objective, use this direct shape:

```json
{
  "kind": "direct",
  "reason": "Explain why this objective is non-behavioral",
  "corrected": {"command": "acceptance command", "exit_code": 0, "log": ".foundry/logs/corrected.log"}
}
```

Review reads the worktree proof before finalization. Before committing,
Foundry copies the proof and named logs into
`~/.foundry/evidence/<trace-id>/<unique-attempt>/` (event id when no trace is
present). The task result's `proof_evidence` field records that directory for
later audits. `original-proof.json` retains the exact submitted bytes;
`proof.json` uses relative paths to copied `rejecting.log` and `corrected.log`,
so the archive is readable after worktree removal. Malformed proofs and absent
logs remain failed evidence; archival never invents results. An archival I/O
failure stops finalization before committing or cleanup.

New files under the worktree's `.foundry` directory are excluded from the task
commit, including accidentally staged proof and raw build logs. Files already
tracked in HEAD remain project content: their modifications and deletions are
committed normally. After archival and committing, excluded artifacts are
removed so the worktree can be cleaned up; tracked `.foundry` content is left
alone. Keep durable project configuration tracked explicitly.

New campaign tasks fail with a typed defect if the record is
missing, malformed, or refers to absent logs. This check spends no reviewer
session. The independent reviewer checks the source, logs, ordering, and whether
the probe proves the intended behavior. Schema checks alone cannot establish
semantic correctness.

### Report campaign efficiency

```bash
foundry campaign report my-campaign
foundry campaign report my-campaign --json
```

The online command reads daemon-owned events through `GetCampaignReport`.
`--offline` reads local stores and logs explicitly. The report separates
formation, execution, and review time and tokens. It also shows dispatched and
landed cycles, running sessions, missing usage, unpriced models, stage limits,
and external completion reasons. Digest sessions do not enter these totals.

The report also lists formation sessions and decision events separately. JSON
includes session IDs, trace IDs, timestamps, transcript paths, terminal usage,
exact Foundry prompt bytes when recorded, and command activity. Captured output
bytes are not tokens and do not prove how much output the model saw. Exact
command repeats exclude duplicate transcript events.

For Codex, native observations include the first request's input, the latest
cumulative usage, and a native transcript path. These observations never add to
terminal accounting. An interrupted session can have a partial observation while
its final usage remains unmeasured. Missing or unsupported transcripts produce
explicit audit errors, not zero counts. Native transcript lookup uses the daemon's
`CODEX_HOME`, or `~/.codex`, and reads only the matching thread's file.

See [the context-mixer2 formation audit](campaign-formation-audit.md) for a worked
example and the limits of each measurement.

Older session records lack an explicit stage. Their role is inferred from access
and model tier, and the report gives the count of inferred sessions. Missing
usage means unmeasured spend. A partial list-price estimate is not total cost.

Formation runs before the first task and after each result that needs a new
objective or completion evaluation. Thus one campaign with N tasks normally has
N+1 formation sessions. Separate campaigns each have an initial call. Forced
terminal decisions bypass the agent. Transport retries can add sessions within
the formation time budget.

### Designing done evidence

Use a `gate` for a deterministic command that can run against the delivered
repository checkout:

```json
{
  "kind": "gate",
  "command": "cargo test -p parite-core retrieval_parity",
  "required": true,
  "artifacts": ["crates/parite-core/tests/retrieval_parity.rs"]
}
```

Every declared artifact must exist before the command is eligible to pass. This
prevents a test runner that silently ignores a missing path from producing
false-green evidence.

Use a `review` for a semantic or externally verified claim:

```json
{
  "kind": "review",
  "statement": "Production preserves raw identity across both entrypoints."
}
```

Required gates are re-run by formation against delivered trunk and block `done`.
A required command that asserts another host's state through `ssh`, `rsync`,
`systemctl`, `launchctl`, or similar tooling is rejected when the campaign is
added: code in a disposable task worktree cannot make such a gate pass reliably.
Express deployment evidence as a review statement that the owner verifies, or
mark a remote probe as optional.

Campaign gates are not task acceptance criteria. A dispatched task runs the
project gates resolved from its own checkout. Formation must state acceptance
evidence the task can actually produce inside that worktree; it does not copy
campaign gate commands into the objective.

## Managing a Campaign

```bash
foundry campaign add ./parite-phase-2d.json
foundry campaign list
foundry campaign show parite-phase-2d
foundry campaign advance parite-phase-2d
foundry campaign pause parite-phase-2d
foundry campaign decide parite-phase-2d --decision "Use the generated tonic client path."
foundry campaign resume parite-phase-2d
# When the cycle budget was exhausted, explicitly authorize more work:
foundry campaign resume parite-phase-2d --add-cycles 1
```

New definitions start as `staged`. An authorized staged campaign becomes
`active` on its first advance. Each advance re-runs mechanical done-evidence,
reviews the repository and context artifacts, then makes exactly one decision:

- `done` — all required gate and review evidence is satisfied.
- `advance` — dispatch exactly one next objective from mission minus current
  state.
- `escalate` — stop because the budget, an escalation rule, runner failure, or
  owner judgment requires attention.

| Status      | Meaning                                                | Valid next control                       |
| ----------- | ------------------------------------------------------ | ---------------------------------------- |
| `staged`    | Definition exists; no cycle has started                | `advance`, `pause`, `cancel`             |
| `active`    | Formation or a task may advance the mission            | `advance`, `pause`, `cancel`             |
| `paused`    | Advancement is intentionally stopped                   | `resume`, `complete`, `cancel`           |
| `escalated` | Budget, policy, or human judgement stopped the mission | `decide`, `resume`, `complete`, `cancel` |
| `completed` | Evidence or owner authorization closed the mission     | None                                     |
| `cancelled` | An owner abandoned the mission before its evidence     | None                                     |

### The status transition table

Every rule governing which control is legal from which status lives in exactly
one place: `foundry_sdk::campaign::transition`. The daemon's gRPC handlers, the
CLI's `--offline` recovery path, and the `AdvanceCampaign` formation block all
delegate to the same `Campaign` methods (`pause`, `resume`,
`record_owner_decision`, `complete`, `cancel`, `check_advanceable`) rather than
re-deriving the rules independently — `--offline` differs from the online path
only in transport and event emission, never in legality. The table below is
generated from that module's exhaustive state-machine test and is the
authoritative specification:

| Status      | `pause`   | `resume`               | `decide`               | `complete`                   | `cancel`                   | `advance`              |
| ----------- | --------- | ---------------------- | ---------------------- | ---------------------------- | -------------------------- | ---------------------- |
| `staged`    | allowed † | rejected: wrong status | rejected: wrong status | allowed                      | allowed                    | allowed                |
| `active`    | allowed † | rejected: wrong status | rejected: wrong status | allowed                      | allowed                    | allowed                |
| `paused`    | allowed † | allowed                | rejected: wrong status | allowed                      | allowed                    | rejected: wrong status |
| `escalated` | allowed † | allowed                | allowed                | allowed                      | allowed                    | rejected: wrong status |
| `completed` | allowed † | rejected: wrong status | rejected: wrong status | no-op (already settled)      | rejected: already complete | rejected: wrong status |
| `cancelled` | allowed † | rejected: wrong status | rejected: wrong status | rejected: cancelled campaign | no-op (already settled)    | rejected: wrong status |

`resume` and `complete` additionally require `authorized_by` to be set,
regardless of status; `cancel` deliberately does not, so an unauthorized
campaign is never stranded with no reachable terminal state.

† `pause` is unconditional today, including on a `completed` or `cancelled`
campaign — see the `TODO(campaign-status)` on `Campaign::pause` in
`foundry-sdk`. That this can resurrect a terminal status into `paused` is a
known open question, not a decision this table is asserting is correct.

A `done` decision made while a required done-evidence gate is red is rewritten
into an `advance`. The synthesized objective carries the campaign mission and
each failing gate's own output, not just the command that failed, and forbids
reverting or shrinking landed mission work — or broadening a lint allowance — to
turn the gate green.

Task results auto-request the next advance. A result that did not land carries
its preserved branch into the next task, so the campaign resumes warm. A
`blocked_on_decision` escalates immediately, and so does a `runner_error` that
describes a fault in the run. `cycles_completed` counts dispatched tasks, while
`cycles_landed` counts only task results whose work actually landed on trunk.

A provider failure is not treated as a campaign failure. If the decision agent
cannot be reached, Foundry re-asks it up to three times with a widening backoff
before giving up, and the resulting escalation names every attempt — a single
transport blip no longer ends a healthy campaign. A malformed decision is not
retried: the agent answered, so re-asking would only repeat it.

When the provider itself is unusable — an exhausted account, revoked
authentication, or an open circuit breaker — the campaign moves to `paused`
rather than `escalated`, whether that surfaces during formation or as the
executor's `runner_error` verdict. Nothing about the campaign's own work is
wrong, so no cycle is consumed and the pending run result is preserved. Once the
provider is usable again, `foundry campaign resume` continues from exactly where
it stopped.

The stop emits a `campaign_paused` event carrying the reason. It is deliberately
not a terminal event — it neither ends the campaign nor requires resurrection —
but it is emitted because an automatic pause is the one kind nobody is watching:
an operator who runs `foundry campaign pause` already knows, whereas a campaign
that quietly stops itself is otherwise indistinguishable from one still working.
The reason also appears in the advance block's summary in the run trace.

Formation reasons about two trees, because they can differ. The live repository
snapshot is the delivered trunk state and is what a `done` decision is judged
against. Separately, when the previous cycle did not land, its preserved branch
becomes the next execution's base ref — so formation is also shown an
`ACCUMULATED UNMERGED WORK` section listing the commits and changed files
reachable from that ref but absent from trunk. An `advance` objective is cut
from mission minus trunk-plus-accumulated. Without this the agent inspects only
trunk and re-cuts objectives the preserved branch already satisfied. When the
final budgeted task lands, Foundry still evaluates the repository for
completion. Only a decision to dispatch another task is converted to a budget
escalation.

Formation is also shown an `OBJECTIVE HISTORY` section: the objectives this
campaign has already cut, oldest first, each with the typed verdict its
execution returned and whether that work landed. The campaign retains the eight
most recent — the full text of every objective is already durable in the
`campaign_advance_completed` event stream, so the stored history is formation's
working memory rather than an archive, and stays bounded on a long mission.

The prompt forbids restating an entry from that history. When the objective
being cut substantially repeats an earlier one, exactly two readings are
available. Either the earlier work exists on the preserved branch and the
inspection missed it, in which case the accumulated section applies and the
objective becomes reconcile-and-land. Or the earlier cycle returned `remainder`
and its gaps are genuinely open, in which case re-dispatching the same request
has already failed once: the agent must name the blocking sub-gap, change the
approach, or escalate for an owner decision.

## Observing and Reconstructing a Cycle

`foundry campaign advance` prints the root event ID, streams block progress, and
renders the completed trace. The same run remains available through:

```bash
foundry history --project parite-cli
foundry trace <campaign-advance-event-id> --verbose
```

A campaign run mints a trace ID, and every advance mints a fresh cycle span
within it. Task-side events carry both `campaign` and `campaign_cycle`, so
concurrent campaigns in the same project cannot make cycle boundaries ambiguous.

`CampaignAdvanceCompleted` records the formation inputs that matter for audit:

- the exact prompt shown to the decision agent;
- the selected agent provider;
- the formation decision and reason;
- the objective, when one was dispatched; and
- the gate results produced by done-evidence commands.

Forced decisions that do not consult an agent record no prompt or provider. This
is intentional evidence that formation was bypassed, not missing telemetry.

`cycles_completed` counts dispatched tasks. `cycles_landed` counts task results
whose changes reached trunk. The bounded `objective_history` stored with the
campaign is working memory for formation; the append-only event stream is the
complete historical record.

## Pausing and Resuming

Pausing prevents automatic or manual advancement. If an already-running task
finishes after the pause, Foundry records its result without changing the paused
state. The typed result and preservation ref remain pending in the campaign
store; the next manual advance after resume consumes them, so formation sees the
exact reviewer gaps and the task continues from preserved work.

Resume is valid for both `paused` and `escalated` campaigns. When an escalation
is budget-only (the engine stopped because the cycle limit was reached but no
human judgment question was recorded), `resume` is the right command — it
returns the campaign to `active` without requiring an owner-decision record:

```bash
foundry campaign resume parite-phase-2d
```

`resume` requires `authorized_by` to be set and will not silently reactivate an
exhausted campaign. When `cycles_completed >= max_cycles`, pass `--add-cycles N`
to explicitly authorize more work; the engine rejects `resume` without an
extension on an exhausted budget:

```bash
foundry campaign resume parite-phase-2d --add-cycles 1
```

## Recording an Owner Decision

Completion and escalation are terminal events and are forced into the next ops
digest as an anomaly. Campaign-store mutations are serialized across the CLI and
daemon, so a control command cannot overwrite an in-flight formation decision.
Online `add`, `advance`, `pause`, `resume`, `decide`, `complete`, and `cancel`
wait for a formation holding the store lock, including a formation for another
campaign. They acquire the lock on Tokio's blocking pool, leaving runtime workers
free to process agent output, finish formations, and answer other RPCs. Campaign
list and queue reads remain available while controls wait. Once admitted to the
blocking pool, a control operation finishes even if its client disconnects;
inspect campaign state before retrying an interrupted command.

Waiting controls validate the latest stored state after acquiring the lock;
a transition that became invalid while waiting returns `FAILED_PRECONDITION`.
`cancel --now` aborts its target workflow before waiting, but may still wait for
another campaign's formation. Formation itself also acquires the file lock on
the blocking pool. Offline commands retain their synchronous file-lock behaviour.

If a daemon-side save fails during `pause`, `resume`, `decide`, or `complete`,
the RPC returns `INTERNAL`, leaves the persisted daemon-owned store
byte-identical, and `complete` does not emit `CampaignCompleted`.

When a task escalates with a human judgment question, record the owner's policy
before the next advance:

```bash
foundry campaign decide parite-phase-2d \
  --decision "Keep the generated tonic client boundary; do not add raw JSON shims."
```

`decide` is valid only for an `escalated` campaign. It appends an owner decision
record with the decision text, the campaign's `authorized_by` value, and a
timestamp, then returns the campaign to `active`. Subsequent formation prompts
include every recorded owner decision as binding context, so the next advance
can continue under explicit policy instead of re-escalating on the same
question.

By default, `foundry campaign decide` is an online mutation: it requires a
reachable `foundryd` daemon and sends the decision through the `DecideCampaign`
RPC. If the daemon is unreachable, the command fails and does not touch the
client-side `FOUNDRY_CAMPAIGNS_PATH`.

If you need to update the file while the daemon is stopped, opt into the direct
store path explicitly:

```bash
foundry campaign decide parite-phase-2d \
  --decision "Keep the generated tonic client boundary; do not add raw JSON shims." \
  --offline
```

## External Completion

When production or other owner-reviewed evidence proves the mission shipped
without another formation cycle, close the campaign explicitly:

```bash
foundry campaign complete parite-phase-2d \
  --reason "Production verification confirms every required outcome shipped."
```

This is an owner-authorized terminal transition. Foundry retains the reason and
timestamp, clears any stale pending result, and emits the same completion event
used by an internally completed campaign. Use `--offline` only while the daemon
is stopped; the direct-file path cannot emit the terminal event.

## Cancelling a Campaign

When a mission is abandoned rather than achieved — superseded by a different
approach, overtaken by events, or simply wrong — cancel it:

```bash
foundry campaign cancel parite-phase-2d \
  --reason "Superseded by the streaming rewrite."
```

Cancellation is a distinct `cancelled` status, not a flavour of `completed`.
Completion in Foundry is an evidence claim, so recording an abandoned campaign
as complete would put a false assertion into the audit trail and the ops digest.
It is terminal and not resumable; if the campaign should come back later, use
`pause` instead.

Unlike `complete`, cancellation does not require `authorized_by` — an
unauthorized campaign cannot be advanced to completion either, so requiring an
owner would leave it stranded with no reachable terminal state. The `--reason`
is always mandatory and always reaches the `campaign_cancelled` event; it is
additionally recorded as an owner decision when the campaign has an owner.

By default the cancellation is **graceful**: the in-flight cycle runs to
completion, its work is committed and preserved exactly as usual, and no
successor cycle is dispatched. To stop immediately instead:

```bash
# Kill the running agent, keep its work (committed and pushed or bundled)
foundry campaign cancel parite-phase-2d --reason "Wrong approach." --now

# Kill the running agent and throw its uncommitted work away
foundry campaign cancel parite-phase-2d --reason "Wrong approach." --now --discard-work
```

A whole campaign runs inside a single daemon task, so `--now` aborts that task:
the running agent process is killed, and the cycle's worktree is left orphaned
because normal finalization never ran. Foundry then disposes of that worktree
according to `--discard-work` — preserving the work to a branch or bundle by
default, or deleting the worktree and its local branch when asked. A remote
branch pushed by an earlier cycle is retained until its owning item lands; it is the audit trail for
work that did reach a durable ref.

`--discard-work` requires `--now`, because a graceful cancellation has already
committed and preserved the cycle's work by the time it stops — there would be
nothing uncommitted left to discard.

A `--now` cancellation also closes the cycle's entry in the work-item ledger. The
cycle was recorded `running` when it was dispatched, and killing it means the
`TaskRunCompleted` that normally settles that entry never arrives — so Foundry
settles it `cancelled` itself, with your `--reason` text as the reason and a
disposition naming the cycle's worktree, whether that worktree is gone, and the
ref any preserved work is recoverable from. A `work_item_cancelled` event records
it, on the aborted cycle's own trace. `foundry queue show <id>` reads the result
back. A graceful cancellation changes nothing there: its cycle finishes and
settles the usual way. See [The work queue](./work-queue.md).

Settlement happens asynchronously after the cancellation RPC returns. The ledger
is saved before the engine writes and broadcasts `work_item_cancelled`, so a
queue read can show `cancelled` before a Watch subscriber receives the event.
Consumers waiting for the event should await its arrival rather than treating a
ledger read as proof that broadcast has finished.

Two limits worth knowing. `--now` kills the agent process itself, but not the
tool subprocesses that agent spawned; those are reparented and run to their own
completion. And the aborted run produces no trace file, so reconstruct it from
the `aborted_event_id` recorded on the `campaign_cancelled` event rather than
from `foundry trace`.

`--offline` cancellation is graceful-only and emits no terminal event, matching
offline `complete`. `--offline --now` is refused rather than quietly downgraded:
with no daemon there is no workflow to abort, and reporting a kill that never
happened would be worse than failing. The _legality_ of the cancellation
itself — whether this status may become `cancelled` at all — is not a separate
offline rule; it is the same `Campaign::cancel` the daemon calls, so a
`completed` campaign is rejected and an already-`cancelled` one is a no-op on
both paths identically. See "The status transition table" above.

## Online and Offline Control

By default, every campaign control command is daemon-authoritative:

- `add` sends the JSON definition through `AddCampaign`; the daemon validates
  the referenced project and context paths against daemon-owned registry state,
  persists atomically, and returns the durable `CampaignDetail` that the CLI
  renders directly.
- `list` renders `ListCampaigns` directly.
- `show` renders `GetCampaign` directly.
- `advance` dispatches `AdvanceCampaign`, prints the returned root event ID,
  watches the workflow, and then renders the trace from that daemon-owned event.
- `pause`, `resume`, `decide`, and `complete` render the typed `CampaignDetail`
  returned by their respective RPCs.

Without `--offline`, an unreachable daemon is always an error. The CLI does not
warn and fall back to direct file mutation automatically.

If `foundryd` is not running, pass `--offline` to opt into direct-file recovery:

```bash
foundry campaign add ./parite-phase-2d.json --offline
foundry campaign list --offline
foundry campaign show parite-phase-2d --offline
foundry campaign pause parite-phase-2d --offline
foundry campaign resume parite-phase-2d --offline
foundry campaign resume parite-phase-2d --add-cycles 1 --offline
foundry campaign decide parite-phase-2d --decision "Keep the daemon boundary." --offline
foundry campaign complete parite-phase-2d --reason "Production verification confirms every required outcome shipped." --offline
```

The offline path reads or mutates the file directly and cannot emit workflow
events. Restart `foundryd` afterward so its in-memory state is refreshed from
disk before you resume normal online control.

`advance` has no offline execution path because formation requires the daemon's
engine, registered blocks, live project state, and event persistence.

## Recovery and Preservation

Task execution never retries inside a single formation. When work does not land,
`FinalizeTask` commits it before returning:

- it first pushes a named task branch to the project's remote;
- if the push is unavailable, it writes a Git bundle under
  `~/.foundry/preserved/`;
- the next campaign cycle uses that preservation reference as its base;
- bundle recovery discovers the branch ref directly, and new bundles also
  include `HEAD` for ordinary `git clone` or `git fetch` recovery.

Formation judges `done` only against delivered trunk. It uses the accumulated
preserved branch to decide what the next task should do. This prevents a
campaign from declaring success for unintegrated work without forgetting work
that has not landed yet.

## Dry Run

Campaign advancement honors event throttle. A dry-run
`campaign_advance_requested` simulates the next objective without mutating the
campaign store or repository. It executes exactly one simulated task through
review and terminal result, then stops without recursively auto-advancing:

```bash
foundry emit campaign_advance_requested \
  --project parite-cli \
  --throttle dry_run \
  --payload '{"campaign":"parite-phase-2d"}' \
  --wait
```
