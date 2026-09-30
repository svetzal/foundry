# Work Ledger V1

## Intent

Foundry runs engineering work but keeps no record of the work as a unit.
`foundry task` executes at once; a campaign derives one objective at a time;
the nightly fans out per project. Nothing answers "what is queued, what is
running, what still needs a person" without reading raw events, worktrees on
disk and remote branches. On 2026-09-29 an inventory found 102 unmerged local
branches on the Mac, 63 of them `foundry-task/*`, and 15 preserved bundles
that no record holds as an obligation.

This slice adds the **work item**: one durable record per unit of work Foundry
dispatches, owned by the daemon, with a typed read API, work item events on
the Watch stream, and a `foundry queue` command. It is phase 1 of the plan in
the Operations repository, `Planning/2026-09-29-foundry-work-intake-v1.md`.
Later slices add pacing (when an item may start), planning (duplicates,
conflicts, resume of preserved work), and a reconciler for orphans. This
slice changes no dispatch behaviour: every item is admitted and started at
once, exactly as today.

## The work item

Fields, all durable:

| Field | Meaning |
| --- | --- |
| `id` | Stable identity, minted at submission. Carried on every event, trace, worktree name and branch name the item produces. |
| `project` | Registry project name. |
| `objective` | The task description, campaign objective, or a fixed description for maintenance, release and remediation work. |
| `kind` | `task`, `campaign_cycle`, `maintenance`, `major_upgrade`, `release`, `remediation`. |
| `lane` | `interactive` (CLI submissions), `campaign`, `maintenance` (sentinel-driven). |
| `origin` | Opaque submitter text: the CLI's client host plus an optional `--origin` string; the sentinel name; the campaign name and cycle. Foundry never interprets it. |
| `submitted_at`, `started_at`, `settled_at` | Timestamps. |
| `state` | `submitted`, `queued`, `running`, `landed`, `preserved`, `needs_decision`, `failed`, `cancelled`. |
| `reason` | Why the item is in its state, in one line. |
| `disposition` | For a settled item: verdict, landed commit or preservation ref, worktree path and whether it was removed, trace id. |

Open states are `preserved`, `needs_decision` and `failed`. Terminal states
are `landed` and `cancelled`. In this slice `submitted` moves to `queued` and
`queued` moves to `running` immediately.

## Completion boundary

V1 is complete when:

- Every dispatch Foundry performs creates a work item before any agent runs:
  `foundry task`, each campaign cycle, each per-project maintenance run, each
  major-upgrade task, each release, each remediation. A dispatch that fails
  before an agent starts settles the item as `failed` with the reason.
- The item store lives at `FOUNDRY_WORK_ITEMS_PATH` (default
  `~/.foundry/work-items.json`), is daemon-owned, and is written by
  same-directory temp-file rename like the campaign and sentinel stores.
- The task, campaign and maintenance formations settle the item from their
  existing typed results: `complete` or a landing `remainder` settles
  `landed`; a non-landing `remainder`, `defect` or preserved result settles
  `preserved` with the preservation ref; `blocked_on_decision` settles
  `needs_decision`; `runner_error` settles `failed`. The settled item records
  the worktree path and whether `remove_workspace` succeeded.
- On daemon start, every item still `running` settles `failed` with reason
  `daemon restarted`, so a restart never leaves an item visibly running.
- Work item state changes are events on the Watch stream and in the durable
  event log: `work_item_submitted`, `work_item_started`, `work_item_settled`,
  and `work_item_cancelled`, each carrying the item id, project, kind, lane,
  state, reason and origin, and the settlement fields on settle.
- Typed gRPC: `ListWorkItems` with optional exact project filter and optional
  state filter, deterministic order (running first by `started_at`, then
  queued by `submitted_at`, then open by `settled_at` descending, then
  terminal by `settled_at` descending); `GetWorkItem` by id, `NOT_FOUND`
  when absent. Both load the store at request time. A missing or empty store
  is an empty list; a malformed store is a gRPC error.
- CLI: `foundry queue` prints four groups on one screen: running, queued,
  open (needs a person), and the last 20 settled. `foundry queue show <id>`
  prints one item with its events. `foundry queue open` prints only the open
  group. All three take `--json`. Online commands go through gRPC and never
  read the store file; `--offline` reads the file directly.
- `foundry task` and `foundry campaign advance` accept `--origin <text>`;
  the CLI also records the client's hostname in the origin.
- `CHARTER.md` gains a scope entry that names work admission, pacing and
  settlement as in scope, as a deliberate expansion decided by the owner on
  2026-09-29, and states that the queue holds only executable work and is
  not a backlog.
- Documentation: a guide page `book/src/guide/work-queue.md`, the proto, the
  CLI reference, and the command table in `AGENTS.md`.
- Tests at the service boundary use temporary stores. Formation tests prove
  each verdict settles the right state. A restart test proves `running`
  becomes `failed`.

## Scope guards

- Do not delay, reorder, merge, reject or deduplicate any dispatch. Pacing and
  planning are later slices.
- Do not add a reconciler, a sentinel, or any scan of worktrees or branches.
- Do not change how tasks land, preserve, or clean up worktrees, beyond
  recording the outcome on the item.
- Do not add owner controls other than what `cancel` already does through
  campaigns. `foundry queue cancel` is a later slice.
- Do not touch ops-visualizer. Its consumer slice starts after this API is
  released into the live daemon.
- Do not cache items in the daemon between requests; request-time loading
  keeps the file the single source of truth.
- Keep the item store independent of the campaign store. A campaign cycle's
  item references the campaign by name and cycle; the campaign does not
  embed items.

If recording an item for some dispatch path requires changing that path's
behaviour, escalate rather than change it.

## Growth path

1. This slice: the record, the read API, the events, `foundry queue`.
2. ops-visualizer Queue page reading `ListWorkItems` and the Watch stream.
3. Reconciler sentinel: items versus worktrees on disk versus
   `foundry-task/*` refs; orphans to the ops digest.
4. Pacing: one mutating item per repository, host and provider caps, starts
   per hour, lanes, provider-health awareness, owner controls.
5. Planning: duplicate and conflict checks, resume of preserved work, the
   start-time check, `queue explain`, GitHub issue and board intake.

## Owner decisions

### 2026-09-30: one more cycle, for cleanup

After ten cycles the completion evaluation found every required gate green
and every review statement true, with one line of the completion boundary
unmet. The owner (Stacey) extended the budget by one cycle and decided:

- The final cycle delivers the unmet line and nothing else:
  `foundry queue show <id>` prints the item together with its `work_item_*`
  events, served through an additive typed read path. `ListWorkItems`
  ordering, dispatch, and ledger writes do not change.
- Two divergences from the field table are accepted for v1 and are not to be
  reopened: the item id is carried on `work_item_*` events but not on
  worktree or branch names (correlation is by trace id), and sentinel-driven
  items record the origin as `maintenance cycle` or `nightly majors lane`
  instead of the sentinel's name.
- When that cycle lands with the required gates green, the campaign is
  complete.
