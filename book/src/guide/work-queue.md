# The Work Queue

Foundry runs engineering work continuously — one-shot tasks, campaign cycles,
nightly maintenance, releases, remediations. The **work-item ledger** is the
durable record of that work *as units*, so you can answer "what is running,
and what still needs me?" without reading raw events, worktrees on disk and
remote branches.

`foundry queue` is the operator's view of that ledger. The overview, `show` and `open`
read it; `close` and `cancel` let an owner discharge specific items.

## What a work item is

A work item is one unit of work Foundry dispatched. It is recorded at the
**root** of its chain — the event that set the work in motion — so a dispatch
that stops early is still in the ledger rather than invisible.

Each item carries a stable id (`wi_` followed by 24 hex characters), the project
it belongs to, the objective it serves, its kind, its lane, an opaque origin
string Foundry never interprets, a typed source saying what dispatched it, the
timestamps of its life, its state, a one-line reason for that state, the
workflow `trace_id` it runs under, and — once it settles — its settlement
fields.

The ledger lives in a single JSON file, `~/.foundry/work-items.json`
(override with `FOUNDRY_WORK_ITEMS_PATH`). It is owned by `foundryd` and
authoritative: every mutation loads the file, applies the change, and saves it
through a same-directory temp-file rename.

## The six kinds

| Kind | What it is | Recorded at | Settled from |
|------|------------|-------------|--------------|
| `task` | A one-shot `foundry task` execution | `ExecutionRequested` | `TaskRunCompleted` |
| `campaign_cycle` | One cycle of a durable campaign | `ExecutionRequested` | `TaskRunCompleted` |
| `major_upgrade` | A major dependency upgrade from the nightly majors lane | `ExecutionRequested` | `TaskRunCompleted` |
| `maintenance` | A per-project maintenance run | `ProjectRunStarted` | `ProjectRunCompleted` |
| `release` | A release run | `ReleaseRequested`, or a clean `MainBranchAudited` | `ReleaseCompleted` |
| `remediation` | A pipeline or vulnerability remediation run | a dirty `MainBranchAudited`, or a failing `PipelineChecked` | `RemediationCompleted` |

## The three lanes

| Lane | Who submitted the work |
|------|------------------------|
| `interactive` | A person at the CLI |
| `campaign` | A campaign derived it |
| `maintenance` | A scheduled maintenance run dispatched it |

## Origin: how the work reached Foundry

An item's `origin` says how the work arrived. For work an automation dispatched
that is the lane itself:

| Dispatched by | Origin |
|---------------|--------|
| `foundry task` | `foundry task` |
| A campaign cycle | `campaign <name> cycle <n>` |
| The nightly majors lane | `nightly majors lane` |

For work a person asked for by hand, the useful extra fact is *who* asked and
from *where*. `foundry task` and `foundry campaign advance` therefore record the
CLI client's hostname, and accept an optional `--origin <text>` note that is
recorded verbatim beside it:

```bash
foundry task acme "Add a --quiet flag" --origin "asked by Stacey in standup"
foundry campaign advance tidy-cli --origin "kicked off by hand after the fix"
```

The recorded origins then read:

```text
foundry task (host workbench: asked by Stacey in standup)
campaign tidy-cli cycle 4 (host workbench: kicked off by hand after the fix)
```

The operator context is appended to the dispatch origin rather than replacing
it, so a campaign cycle still says which campaign and cycle it is. If the
hostname cannot be read, the origin states `host unknown host` rather than going
silent — "the lookup failed" is a more honest record than "no operator was
involved".

Origin is **opaque**. Nothing parses, validates, filters or groups on it;
`foundry queue` only displays it, and no dispatch is ever rejected, delayed or
altered because of what its origin says — an empty `--origin` is accepted and
simply adds nothing. Cycles dispatched by the automatic post-result advance
carry no operator context, because no operator issued them.

## Source: what dispatched the work

Origin is text for a person. `source` is the typed answer to the same
question, recorded at submission and never changed, so the ledger alone can
answer "everything this campaign dispatched" or "what did the nightly start"
without reading the event log to recover a campaign from a trace.

A source is a `kind` from a closed set, the one `ref` that names the
dispatcher, and, for a campaign, the `cycle` number:

| Kind | `ref` | Recorded when |
|------|-------|---------------|
| `campaign` | the campaign name | a campaign advance dispatches a cycle; `cycle` is the cycle number, whoever asked for the advance |
| `sentinel` | the sentinel name | a sentinel-fired chain dispatches an item: the nightly's per-project maintenance runs, its majors-lane upgrades, a remediation or release it reaches |
| `operator` | the hostname of the CLI that asked | `foundry task`, `foundry iterate`, `foundry run`, `foundry release`, `foundry pipeline`, and any item a chain they start reaches |
| `work_item` | the parent item id | an item created from another item: a `queue resume` child or a nightly continuation (its `resumes` link is unchanged), or a release cut after a remediation on the same run |

The kinds are a closed enum; adding one is a code change, never a free
string. The source travels on the event envelope: whatever emits a root event
names it there, and every event below the root inherits it verbatim (see
[Tracing](../architecture/tracing.md)), which is how a per-project run three
hops below the nightly's root still knows which sentinel fired it. A release
that follows a remediation is the one case resolved from the ledger instead:
the newest remediation item on the same trace and project is its parent.

An item recorded before the source existed has none, and so does one whose
root named none (a raw `foundry emit`). Every reader treats that as **not
recorded**, exactly as `worktree_removed` is handled: `queue show` prints no
`Source:` line, `--json` emits no `source` key, and a row shows `-`. The
free-text `origin` is untouched by any of this; the two sit side by side.

To list one source's items:

```bash
foundry queue --source campaign:bedrock-gated-trials-v1   # every cycle of that campaign
foundry queue --source sentinel:nightly-maintenance       # what the nightly started
foundry queue open --source operator:workbench            # open work asked for from that host
```

The filter is an exact match on kind and ref, applied by the daemon's
`ListWorkItems` (and by the same selection offline); an unknown kind, or a kind
with no ref, is refused. `foundry campaign show <name>` lists the campaign's
cycles through this same filter.

## The states

An item moves through unsettled states, then settles. Settled does not mean
finished: three of the settled states still hold an obligation for a person,
which is why the ledger says an item **settles** rather than completes.

| State | Meaning | Group |
|-------|---------|-------|
| `submitted` | Admitted to the ledger, not yet queued | Queued |
| `queued` | Waiting to start | Queued |
| `running` | An agent is working on it | Running |
| `preserved` | Work is held on a durable ref for a later cycle | **Open** |
| `needs_decision` | Work stopped on a question only a person can answer | **Open** |
| `failed` | Work stopped on a fault | **Open** |
| `landed` | The work reached trunk | Terminal |
| `cancelled` | An operator stopped the work | Terminal |

`landed` and `cancelled` are **terminal** — nothing more is owed.
`preserved`, `needs_decision` and `failed` are **open** — settled, but still
carrying something for you to discharge. `foundry queue open` is exactly that
set.

Foundry does not currently pace work: an item that reaches the ledger is already
under way, so `submitted` → `queued` → `running` happen together. The `submitted`
and `queued` states exist in the model and are rendered, but in practice you will
rarely catch an item sitting in them.

### Daemon restarts settle running items as failed

A `running` item means "an agent is working on it", and after a daemon restart
no agent is. On every start `foundryd` settles each item still `running` as
`failed`, with the reason `daemon restarted`. This is deliberate: reporting
long-dead work as running is worse than reporting it as failed, and a failed
item is visible in `foundry queue open` where you can decide what to do with it.

## The four lifecycle events

The ledger emits four events. They are the owner-specified exception to
Foundry's `*Started`/`*Completed` pairing rule.

| Event | Meaning |
|-------|---------|
| `work_item_submitted` | A unit of work entered the ledger |
| `work_item_started` | An agent started on a ledger item |
| `work_item_settled` | A ledger item reached a settled state, with its disposition |
| `work_item_cancelled` | An operator stopped a ledger item |

`work_item_started` pairs with `work_item_settled` rather than a
`work_item_completed`, because an item does not *complete* — it settles, into a
state that may still hold an obligation.

`work_item_cancelled` is emitted by exactly one thing today:
`foundry campaign cancel <name> --reason … --now`, which stops the in-flight
cycle outright. It carries the item's id, project, kind, lane, state, reason and
origin, plus the settlement fields, and it rides the **aborted cycle's** trace
rather than the cancellation's — so it sits with the rest of the events about
that unit of work. Like the other three it reaches `foundry watch` and the
durable JSONL event log.

## Commands

```bash
foundry queue [--source <kind>:<ref>] [--json] [--offline]
foundry queue show <id> [--json] [--offline]
foundry queue open [--source <kind>:<ref>] [--json] [--offline]
```

### `foundry queue`

Prints four groups on one screen, in this order:

1. **Running** — items an agent is working on, by start time.
2. **Queued** — items in `submitted` or `queued`, by submission time.
3. **Open — needs a person** — `preserved`, `needs_decision` and `failed`,
   newest settlement first.
4. **Settled (last 20)** — the newest 20 terminal (`landed`, `cancelled`)
   items. Older terminal items are omitted; the terminal group grows forever, so
   the overview shows only its newest page. The cap applies to this group alone.

Each line shows the item's id, project, kind, lane, source (`-` when none was
recorded), state, the timestamp that placed it in its group, and the one-line
reason:

```text
Running
  wi_aaa1  alpha  task  interactive  operator:workbench  running  2026-09-30T01:00:05+00:00  running

Queued
  (none)

Open — needs a person
  wi_ccc3  beta  major_upgrade  maintenance  sentinel:nightly-maintenance  preserved  2026-09-29T03:10:00+00:00  gates red after the bump

Settled (last 20)
  wi_ddd4  alpha  release  maintenance  -  landed  2026-09-28T02:40:00+00:00  released v1.2.0
```

The group order and the order within each group come from the daemon's
`ListWorkItems` response. The CLI groups by state; it never re-sorts. With
`--source <kind>:<ref>` the daemon returns only that source's items, in the
same order, and the groups show those.

### `foundry queue show <id>`

Prints one item's full durable record, one field per line, including every
settlement field, then a blank line and the item's own `work_item_*` events.
An optional field the ledger never recorded produces **no line at all**, so you
never have to tell a recorded empty string from an unset field.
`worktree_removed` is the sharpest case: a recorded `false` prints
`Worktree removed: no`, while "no worktree recorded" prints nothing. `Source:`
follows the same rule: an item recorded before the source existed prints no
`Source:` line.

```text
Id:               wi_ccc3
Project:          beta
Objective:        bump serde to 2.0
Kind:             major_upgrade
Lane:             maintenance
Origin:           nightly majors lane
Source:           sentinel:nightly-maintenance
State:            preserved
Reason:           gates red after the bump; work held on a branch
Submitted:        2026-09-29T02:00:00+00:00
Started:          2026-09-29T02:00:01+00:00
Settled:          2026-09-29T03:10:00+00:00
Trace:            abcdef0123456789abcdef0123456789
Verdict:          remainder
Preservation ref: foundry/majors/serde
Worktree:         /home/you/.foundry/worktrees/beta/x
Worktree removed: no

Events:
  2026-09-29T02:00:00+00:00  work_item_submitted  submitted       evt_1a2b…  submitted
  2026-09-29T02:00:01+00:00  work_item_started    running         evt_3c4d…  agent started
  2026-09-29T03:10:00+00:00  work_item_settled    preserved       evt_5e6f…  gates red after the bump; work held on a branch
```

Each event line shows when it occurred, its type, the state it left the item
in, the event id and the one-line reason. The events are read from the durable
event log (`FOUNDRY_EVENTS_DIR`, one `YYYY-MM.jsonl` file per month) through the
daemon's `ListWorkItemEvents` RPC, and are:

- **selected by the item's id in the event payload** — never by trace or
  project. One maintenance run can hold a `maintenance`, a `remediation` and a
  `release` item on the same trace and project; each shows only its own events.
- **read from every monthly file**, however old, so an item from last year still
  shows its history rather than a false "no events".
- **in chronological order** — oldest first, ties in the order they were logged.

An item with no events in the log (or no event log at all) prints
`  (no events)` under the heading rather than nothing. A line in the log that is
not valid JSON — or that holds two events run together — is skipped with a
warning in the daemon log; the item's other events are still shown. A fault
reading the log itself is an error, never an empty history.

### `foundry queue open`

Prints only the open group — the work that still needs a person. This is the
command to run when you want the shortest possible answer to "what is waiting
on me?"

### `--json`

All three commands take `--json`. The JSON form renders from the same fetched
data as the human form, so the two can never disagree about which items the
daemon returned.

- `foundry queue --json` and `foundry queue open --json` emit a JSON array.
- `foundry queue show <id> --json` emits a single JSON object: the record's
  keys, unchanged, plus an `events` array. Each event carries `id`,
  `event_type`, `occurred_at`, `state`, `reason` and, when it has one,
  `trace_id`; an item with no events has `"events": []`.

Optional fields are **absent** when unset rather than emitted as `null`, `""`
or `false`, so the JSON round-trips the same facts the record carries:

```json
[
  {
    "id": "wi_ccc3",
    "project": "beta",
    "objective": "bump serde to 2.0",
    "kind": "major_upgrade",
    "lane": "maintenance",
    "origin": "nightly majors lane",
    "source": { "kind": "sentinel", "ref": "nightly-maintenance" },
    "submitted_at": "2026-09-29T02:00:00+00:00",
    "started_at": "2026-09-29T02:00:01+00:00",
    "settled_at": "2026-09-29T03:10:00+00:00",
    "state": "preserved",
    "reason": "gates red after the bump; work held on a branch",
    "trace_id": "abcdef0123456789abcdef0123456789",
    "verdict": "remainder",
    "preservation_ref": "foundry/majors/serde",
    "worktree": "/home/you/.foundry/worktrees/beta/x",
    "worktree_removed": false
  }
]
```

Note what is *not* there: `landed_commit`. The item did not land, so the field
was never recorded, so it is absent. A `source` has the same keys the ledger
writes (`kind`, `ref`, and `cycle` for a campaign); an item that records none
has no `source` key. The `--json` array is the fetched list unchanged — neither
grouped nor capped at 20 — so a consumer can apply its own rules.

## Online versus offline

| Command | Daemon required? | Notes |
|---------|-----------------|-------|
| `foundry queue [--source <kind>:<ref>]` | Yes (or `--offline`) | Renders `ListWorkItems`; `--source` is its exact-match source filter |
| `foundry queue show <id>` | Yes (or `--offline`) | Renders `GetWorkItem`, then `ListWorkItemEvents` |
| `foundry queue open [--source <kind>:<ref>]` | Yes (or `--offline`) | Renders `ListWorkItems`, open group only |

**Online** is the default and is daemon-authoritative. All three commands render
the daemon's response directly and never read, create or mutate the client-side
ledger or events files: if `FOUNDRY_WORK_ITEMS_PATH` or `FOUNDRY_EVENTS_DIR` is
absent, the online path leaves it absent, and an existing file is left byte-for-byte untouched. If `foundryd` is
not listening, the command fails with a stable actionable error naming the
matching `--offline` recovery command. There is no silent fallback.

A `NOT_FOUND` from `GetWorkItem` surfaces as an id-not-found error and a
non-zero exit, not as an empty record.

**`--offline`** is explicit recovery for when the daemon is stopped and you
intentionally want direct file access. It reads `FOUNDRY_WORK_ITEMS_PATH`
(and, for `show`, `FOUNDRY_EVENTS_DIR`) directly and never contacts the daemon,
applying the same grouping order the `ListWorkItems` contract documents and the
same event selection and order `ListWorkItemEvents` documents — `--offline`
differs from the online path only in transport, never in reading order.

- A missing ledger file renders four empty groups and exits zero. Absence is
  the normal starting state, not a fault, and a read never creates the file.
- A malformed ledger file is an error: the command exits non-zero and names the
  parse failure.
- `--source` applies the same exact-match selection to the file the daemon
  applies to its store, so a filtered `--offline` read lists the same items.


### Close or cancel an item

```bash
foundry queue close wi_0123456789abcdef01234567 --reason "Reviewed; no further work required" --origin "owner review"
foundry queue cancel wi_0123456789abcdef01234567 --origin "withdrawn request"
```

`close` discharges an open obligation in `preserved`, `needs_decision` or
`failed`; it requires a nonblank `--reason`. `cancel` stops a `submitted` or
`queued` item and records the reason `cancelled by operator`, without requiring
a reason argument. Both settle exactly the requested id as `cancelled`.

Both require the daemon, reject `--offline`, and never fall back to local
writes. They record this CLI's hostname and optional `--origin` in
`operator_action`, separately from the original submission origin. The action
retains the previous state, reason and settlement timestamp. Submission
identity, objective, trace and known disposition remain intact, including
preserved refs and worktree-removal observations. No agent starts, running
workflow stops, or preserved work is disposed of by either command.

The daemon reloads the ledger under its shared write gate and saves by atomic
replacement before emitting `work_item_cancelled` to Watch and the durable
log. Earlier event bytes stay untouched. Unknown ids return `NOT_FOUND`,
blank input returns `INVALID_ARGUMENT`, and other states return
`FAILED_PRECONDITION`. Malformed ledgers also return `FAILED_PRECONDITION`;
read or save failures return `INTERNAL` and a failed save emits no cancellation.
Concurrent requests for one item can succeed only once.

### Resume preserved work

```bash
foundry queue resume wi_0123456789abcdef01234567 --origin "finish preserved work"
```

Owner-directed preserved-work continuation uses `foundry queue resume <id>
[--origin <text>]`. It requires a live daemon and refuses `--offline`. Only a
`preserved` item with a usable preservation ref and a registered project can be
resumed. Foundry dispatches a new task with the original objective and starts
from the preserved local branch, remote ref or `bundle:<path>` through the
existing continuation path. The new record and its lifecycle payloads expose
`resumes`, the exact original id. Queue reads show this link in human and JSON
output. Submission identity and the parent's prior evidence remain intact;
the child records the hostname and optional origin in its `resume` operator
action.

The original obligation stays preserved until the linked task actually lands.
Then Foundry records its landing commit and appends `work_item_settled` for the
original. Failed, blocked, preserved and no-landing results leave it open.
An original cancelled by its owner stays cancelled even if its child lands.
Unknown ids return `NOT_FOUND`, blank inputs `INVALID_ARGUMENT`, ineligible
states or unusable evidence `FAILED_PRECONDITION`, and persistence failures
`INTERNAL`. A rejected admission dispatches no execution and invokes no agent.

Admission first saves a child in `failed` with reason `resume admission incomplete;
execution not dispatched`, no `started_at`, a `settled_at`, and the exact `resumes`
and operator context. Only after both `work_item_submitted` and `work_item_started`
roots have been appended successfully does Foundry save that child as `running`
and dispatch it. If the initial ledger save fails, no child or lifecycle event is
created. A later lifecycle or final ledger-save failure leaves the staged failed
child; the parent's obligation and unrelated records remain unchanged. The failed
child has no preservation disposition; continue using the preserved parent's id.
An interruption during admission also leaves this non-running record.

Watch publishes each admission lifecycle root only after its write succeeds.
A rejected admission may retain a successfully written submitted event, or both
roots if the final ledger save fails. These are evidence of an admission attempt,
not proof of execution: consult the ledger and RPC result. Earlier event bytes,
including any partial failed append, remain intact; Foundry never truncates or
rewrites history to undo admission. Ordinary engine roots, block outputs,
scatter/gather, progress, close/cancel and restart settlement retain their existing
best-effort event-persistence behaviour.

When an otherwise eligible nightly major upgrade matches a `preserved` ledger
item by exact registered project, package and target version, the majors lane
resumes that obligation from its preservation ref and original objective. Its
child records `kind: major_upgrade`, `lane: maintenance`, origin `nightly majors
lane` and the exact `resumes` id; automation records no operator action. The
parent retains its submission identity and evidence. Only an actual child
landing settles the preserved parent landed; other outcomes leave it open,
and owner cancellation remains terminal. A selected resume admission failure
is reported and never falls back to a fresh task.

Continuations count against the existing nightly caps and run sequentially
with fresh upgrades in the existing order. Update policies, holds, security
eligibility, successful maintenance and in-flight suppression still apply.
Reviews, dry runs and interrupted-cycle summaries dispatch nothing. Without a
matching preserved ledger item, the existing fresh-task and history-suppression
rules apply. Owner `queue resume` remains an interactive, asynchronous task.

After a task actually lands, Foundry also checks that registered project's
other `preserved` ledger items against the registered trunk branch, using the
registered checkout path. A preserved commit reachable from trunk, or a
nonempty set of commits whose patches are all matched by `git cherry`, settles
`landed`. Its `landed_commit` is the verified trunk commit and its reason is
exactly `superseded by <commit>`. The appended `work_item_settled` event carries
that item's exact id, project and original trace. Objective, submission origin,
`resumes` and prior preservation evidence remain intact. Once an item settles
landed, its recorded task branch locally and preservation branch locally and on
origin are eligible for best-effort deletion. Each ref needs fresh ancestry or
patch-equivalence proof against the registered trunk. Unowned refs, bundles,
branches checked out in any worktree, and refs also owned by unlanded items in
any registered project sharing the repository are kept. Ownership compares
exact refs across repository slugs and shared Git common directories, including
separate clones and linked worktrees. Deletion uses the proved commit as a guard against concurrent ref changes.
The disposition and settlement event retain each ref name, observed commit,
deletion result and failure reason. A failed deletion leaves the item landed.
Preservation evidence for unlanded work is never deleted.
Cleanup decides eligibility under the ledger write gate, releases it for all
Git operations, then reloads under the gate to record outcomes. Concurrent
ledger updates are retained; a changed state or ref ownership is never
overwritten by stale cleanup evidence.

This check runs only after actual task landing. Failed, blocked, preserved and
no-landing task results do not trigger it. It checks only preserved items in
the same registered project; owner-cancelled items remain cancelled. A repeated
terminal does not append another settlement. The ledger is reloaded under the
shared write gate before saving, so concurrent owner actions and unrelated
updates are retained. A failed save emits no settlement.

Missing refs or objects, failed Git commands, unmatched patches, edited
squashes without patch-equivalence proof, and ambiguous evidence leave the item
open with an explicit supersession diagnostic in the settlement block's result
and structured log. Empty Git output is never proof. Local branch refs and
commit ids are checked directly. Branch names are also checked by exact name
on `origin` when configured, without fetching; conflicting local and remote
heads remain unresolved. A remote-only head must already have locally available
objects. A bundle must advertise exactly one head and that commit must already be
locally available. Patch comparison requires complete, shared history and
rejects merge commits that `git cherry` cannot account for. Queue show and JSON
expose the persisted commit and reason through the daemon's read path.
Scheduled invocation and the reporting reconciler are not delivered by this
landing-triggered check.

## What `queue` does not do

`queue close` and `queue cancel` do not stop running work.
To stop an in-flight campaign cycle use `foundry campaign cancel <name>
--reason … --now`.

### What `campaign cancel --now` does to the cycle's item

A `--now` cancellation kills the in-flight cycle, so the `TaskRunCompleted` that
normally settles its item never arrives. Foundry therefore settles that item
itself:

- The `running` item carrying the aborted run's trace settles **`cancelled`**,
  with your `--reason` text as its reason and the settlement time recorded. It
  moves out of `Running` and into the `Settled (last 20)` group; `cancelled` is
  terminal, so it never shows up under `queue open`.
- Its disposition records the cycle's worktree and whether that worktree is gone,
  observed *after* disposal ran — plus the branch (or `bundle:` path) the work was
  preserved on. With `--discard-work` there is no preservation ref, because the
  work was thrown away. A cycle that had built no worktree records no
  disposition at all.
- Correlation is by trace alone, exactly as a normal settlement: if no running
  item carries the aborted run's trace, nothing is settled. Foundry never falls
  back to "the project's newest running item", because that would report an
  unrelated concurrent run as cancelled.
- A **graceful** cancel (no `--now`) changes no item: the cycle finishes on its
  own and settles the usual way. A `--now` cancel with nothing in flight, and
  cancelling an already-cancelled campaign, both change no item either.

The ledger is bookkeeping beside the cancellation, never a precondition for it:
if the ledger cannot be read or written, the campaign is still cancelled and the
`CampaignCancelled` event is still emitted — the fault is logged, and the item is
left for the restart sweep.

## Work reconciliation

The `work-reconciler` sentinel runs at `30 */3 * * *`, or use
`foundry queue reconcile` against the daemon to run it now and print that
invocation’s report. It reports exact inventory identities and conservatively
settles verified preserved obligations without deleting their evidence. Orphan,
broken and unresolved findings, including inspection errors, reach the ops
anomaly path even below the normal pressure threshold. See the
[work reconciler guide](work-reconciler.md) for proof and failure semantics.

### Worktree housekeeping after validation

Successful validation runs Cleanup Branches, including during nightly maintenance
and `foundry validate`. It prunes Git metadata for directories that no longer
exist. An existing secondary worktree is removed only when it is under that
project's Foundry worktree root (`FOUNDRY_WORKTREES_DIR`, default
`~/.foundry/worktrees`), has no submitted, queued or running work-item owner or
active campaign cycle, has no uncommitted files (including ignored files), and
has no commits absent from every remote-tracking ref. Cleanup uses ordinary
`git worktree remove`, without force.

Every retained worktree is named in the validation housekeeping summary and
structured log, with its reason: outside Foundry ownership, owned by live work,
or holding unpreserved work. A failed safety check also retains the worktree and
reports the error. Cleanup reloads the daemon-owned ledger for each deletion and
releases its write gate and campaign store lock before every Git await. Ownership
is checked again immediately before each individual removal, after inspecting
worktree contents. A task is admitted on `ExecutionRequested`, before workspace
creation on `PlanCompleted`; the worktree path is recorded after `git worktree
add`. Until that path is known, the live dispatch conservatively protects every
candidate for its project. Creation refuses an existing path, so a later admission
cannot adopt the existing worktree that cleanup has selected for removal.

Merged branches still use `git branch -d`. Branches used by live work or recorded
preservation refs are retained. This housekeeping does not dispose of the
workspace of a task undergoing review; task finalization remains responsible
for its own workspace after landing or preservation.
