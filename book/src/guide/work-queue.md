# The Work Queue

Foundry runs engineering work continuously — one-shot tasks, campaign cycles,
nightly maintenance, releases, remediations. The **work-item ledger** is the
durable record of that work *as units*, so you can answer "what is running,
and what still needs me?" without reading raw events, worktrees on disk and
remote branches.

`foundry queue` is the operator's view of that ledger. It is read-only: it
changes no item, dispatches nothing, and cancels nothing.

## What a work item is

A work item is one unit of work Foundry dispatched. It is recorded at the
**root** of its chain — the event that set the work in motion — so a dispatch
that stops early is still in the ledger rather than invisible.

Each item carries a stable id (`wi_` followed by 24 hex characters), the project
it belongs to, the objective it serves, its kind, its lane, an opaque origin
string Foundry never interprets, the timestamps of its life, its state, a
one-line reason for that state, the workflow `trace_id` it runs under, and —
once it settles — its settlement fields.

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

## Commands

```bash
foundry queue [--json] [--offline]
foundry queue show <id> [--json] [--offline]
foundry queue open [--json] [--offline]
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

Each line shows the item's id, project, kind, lane, state, the timestamp that
placed it in its group, and the one-line reason:

```text
Running
  wi_aaa1  alpha  task  interactive  running  2026-09-30T01:00:05+00:00  running

Queued
  (none)

Open — needs a person
  wi_ccc3  beta  major_upgrade  maintenance  preserved  2026-09-29T03:10:00+00:00  gates red after the bump

Settled (last 20)
  wi_ddd4  alpha  release  maintenance  landed  2026-09-28T02:40:00+00:00  released v1.2.0
```

The group order and the order within each group come from the daemon's
`ListWorkItems` response. The CLI groups by state; it never re-sorts.

### `foundry queue show <id>`

Prints one item's full durable record, one field per line, including every
settlement field. An optional field the ledger never recorded produces **no
line at all**, so you never have to tell a recorded empty string from an unset
field. `worktree_removed` is the sharpest case: a recorded `false` prints
`Worktree removed: no`, while "no worktree recorded" prints nothing.

```text
Id:               wi_ccc3
Project:          beta
Objective:        bump serde to 2.0
Kind:             major_upgrade
Lane:             maintenance
Origin:           majors
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
```

`show` prints the record alone. It does not list the item's four
`work_item_*` events: the item correlates with them by `trace_id`, and no
existing read RPC accepts a trace id — `foundry trace` takes a root *event*
id, span lookup takes a span id, and history is bounded to a date window, so it
would report "no events" for any item older than that window. Use the `Trace`
field with `foundry history` to locate the run by hand.

### `foundry queue open`

Prints only the open group — the work that still needs a person. This is the
command to run when you want the shortest possible answer to "what is waiting
on me?"

### `--json`

All three commands take `--json`. The JSON form renders from the same fetched
data as the human form, so the two can never disagree about which items the
daemon returned.

- `foundry queue --json` and `foundry queue open --json` emit a JSON array.
- `foundry queue show <id> --json` emits a single JSON object.

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
    "origin": "majors",
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
was never recorded, so it is absent. The `--json` array is the fetched list
unchanged — neither grouped nor capped at 20 — so a consumer can apply its own
rules.

## Online versus offline

| Command | Daemon required? | Notes |
|---------|-----------------|-------|
| `foundry queue` | Yes (or `--offline`) | Renders `ListWorkItems` |
| `foundry queue show <id>` | Yes (or `--offline`) | Renders `GetWorkItem` |
| `foundry queue open` | Yes (or `--offline`) | Renders `ListWorkItems`, open group only |

**Online** is the default and is daemon-authoritative. All three commands render
the daemon's response directly and never read, create or mutate the client-side
ledger file: if `FOUNDRY_WORK_ITEMS_PATH` is absent, the online path leaves it
absent, and an existing file is left byte-for-byte untouched. If `foundryd` is
not listening, the command fails with a stable actionable error naming the
matching `--offline` recovery command. There is no silent fallback.

A `NOT_FOUND` from `GetWorkItem` surfaces as an id-not-found error and a
non-zero exit, not as an empty record.

**`--offline`** is explicit recovery for when the daemon is stopped and you
intentionally want direct file access. It reads `FOUNDRY_WORK_ITEMS_PATH`
directly and never contacts the daemon, applying the same grouping order the
`ListWorkItems` contract documents — `--offline` differs from the online path
only in transport, never in reading order.

- A missing ledger file renders four empty groups and exits zero. Absence is
  the normal starting state, not a fault, and a read never creates the file.
- A malformed ledger file is an error: the command exits non-zero and names the
  parse failure.

## What `queue` does not do

`foundry queue` is read-only by design. There is no `foundry queue cancel` yet —
to stop an in-flight campaign cycle use `foundry campaign cancel <name>
--reason … --now`.
