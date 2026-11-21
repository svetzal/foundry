# Work reconciler

The reconciler answers what work remains and where its evidence lives. It compares
open and running ledger records against each registered checkout, Git worktree
registrations, Foundry worktree directories, and local and origin
`foundry-task/*` branches. It uses the project's registered trunk, including
projects whose trunk is not named `main`.

Run it now with:

```bash
foundry queue reconcile
```

To delete the orphan `foundry-task/*` branches that already landed, run:

```bash
foundry queue reconcile                   # plan: lists each orphan branch with its proof
foundry queue reconcile --prune-branches  # apply
```

An orphan branch is one that no open, waiting or running item owns or starts
from (`base_ref`). With `--prune-branches`,
the reconciler deletes each orphan branch, local or on origin, that origin's
registered branch is proved to contain (ancestry, patch equivalence or content
containment), with the same guarded deletion as landed-item cleanup: fresh
proof, no worktree holding it, a lease on the observed commit and no local
pre-push hook. Proof against a local trunk that is ahead of origin is not
enough. Each deleted branch becomes a `pruned_branch` finding and the report
totals gain `pruned branches: N`. A branch whose work origin's trunk does not
contain stays an `orphan_branch` finding. A failed deletion stays one too, with
the reason. The scheduled run never prunes, but it does sweep aged orphans
(see [Sweep of aged orphans](#sweep-of-aged-orphans)).

The daemon runs the same `work_reconcile_started` workflow that the canonical
`work-reconciler` sentinel starts at `30 */3 * * *` (local time). The paired
`work_reconcile_completed` records exact findings, settlement identities and
counts. Sentinel seeding is additive; existing enabled settings and schedules
are preserved.

The CLI prints **that invocation's** Markdown report returned by the daemon.
There is no offline fallback, and `--offline` is rejected. An unreachable daemon
is an error. Inspection, fetch, ledger or report-write failures return gRPC
`INTERNAL`, with diagnostics; they never become a healthy empty inventory.
Verified settlements may already be durable when a later report write fails.
The completion still records the failure and reaches operational observation.

Reports are written atomically to `{FOUNDRY_RECONCILE_DIR}/{YYYY-MM-DD}.md`,
defaulting to `~/.foundry/reconcile/`. Each invocation replaces that day's report;
the report is evidence, not a second authoritative store. Findings name exact
item IDs, full paths and refs:

- Orphan Foundry directories and Git worktree registrations, with their branch,
  registration and directory status.
- Orphan local and origin task branches, their commit and supersession status
  (`ancestor of`, `patch-equivalent to` or `content contained in`) against
  either the local registered trunk or its freshly fetched origin tracking branch.
  Findings name the trunk that proves supersession and report differing trunk
  commits. A branch-only fetch refreshes origin tracking refs and
  retains stale origin tracking branches, even when Git configuration enables pruning.
- Broken item workspace references and unresolved preservation or inspection
  evidence. Changes trunk does not hold and ambiguous heads keep obligations open.
- Informational non-Foundry worktrees and dirty registered checkouts.

Running work is associated through recorded workspace fields or exact project
and trace evidence in the durable event log, never by parsing item IDs out of
workspace names. A running item without that evidence makes ownership unresolved;
its possible workspace is not declared orphaned.

Every open item (`preserved`, `needs_decision` or `failed`) that recorded a
preservation ref is checked. Owner-cancelled and other settled items are not.
An open item without a preservation ref is not checked; only a `preserved` one
is reported as a broken item. An item settles `landed` only when a conservative
Git proof succeeds. The proofs run in this order:

- Ancestry: the preserved commit is an ancestor of trunk.
- Patch equivalence: all its non-merge commits have equivalent patches on trunk
  (`git cherry`).
- Content containment: `git merge-tree --write-tree` merges the preserved
  commit into trunk without a conflict, and the merged tree is trunk's own
  tree. Trunk then already holds every change the branch makes since their
  merge base, as it does after a later cycle squashed the work or rebased it
  with the same conflict resolution. This needs Git 2.38 or later. It changes
  no ref, index or worktree, and writes the merged tree only as unreferenced
  loose objects.

The informational finding for a settled item names the proving trunk, its
commit and the proof, for example `superseded by origin trunk
refs/remotes/origin/main at <commit> (content containment)`. When patch
equivalence and content containment both fail, the unresolved finding gives
each reason.

A `needs_decision` or `failed` run keeps a preservation ref even when it made
no commits, and that ref then points at the trunk commit the run started from.
Such an item settles only when its preserved tip was committed after the item
started, compared at whole seconds. Otherwise it stays open with an unresolved
finding: its obligation is the question or the fault, not work that can land.

After a successful origin fetch, either the local
registered trunk (`refs/heads/<branch>`) or an origin trunk observed by that
fetch (`refs/remotes/origin/<branch>`) can
provide that proof: a stale checkout does not hide landed work, and local work
ahead of origin still counts. With no origin or a failed fetch, existing
behaviour is retained. Retained tracking refs absent from the successful fetch
are never used as origin trunk proof.
Shallow history, missing objects, divergent local and remote preservation heads,
ambiguous bundles, merge conflicts and changes trunk does not hold remain open by exact ID. The settlement records the verified trunk commit and exact reason
`superseded by <commit>`, retaining submission, continuation and preservation
fields. Its `work_item_settled` event stays on the original trace.

Before saving, the reconciler reloads the ledger under the same write gate as
owner controls and compares the entire inspected record. Owner cancellation and
concurrent changes win over stale proof. Unrelated records remain intact and
repeated reconciliation emits no second settlement. Historical event bytes are
never rewritten.

## Disposal when an item settles

When a work item settles, Foundry removes what the item no longer needs. The
rule depends on the state:

| State | What Foundry removes |
| --- | --- |
| `landed` | The worktree, and each `foundry-task/*` branch the item owns, locally and on origin. Each branch needs fresh proof that the registered trunk contains it. |
| `cancelled` | The same items, with no trunk proof. This includes `queue cancel`, `queue close`, a campaign carry and a campaign cancellation. |
| Superseded through `resumes` or `continues` by a later landing | The same as `cancelled`. |
| `preserved`, `needs_decision`, `failed` | Nothing. |
| `submitted`, `queued`, `held`, `running` | Nothing. |

A ref that another open or waiting item owns, or starts from (`base_ref`), is
kept. When that item settles, Foundry examines the ref again and removes it if
nothing else holds it. A campaign cycle that Foundry carries forward keeps its branch
while the next cycle runs from it, and loses it when that cycle settles.

Foundry removes only:

- branches in `refs/heads/foundry-task/` and their origin copies
- worktrees in the Foundry worktrees root

It does not delete a branch that a worktree has checked out, or a branch whose
commit changed after inspection. The origin deletion pushes with `--no-verify`
and a lease on the observed commit: it carries no code, so the project's own
pre-push hook does not run. Hooks on the remote still apply. Ownership compares
exact refs across every registered project that shares the repository (by
repository slug or Git common directory).

Disposal never fails a settlement. For a missing origin, an authentication
failure or a refused deletion, Foundry logs a warning and records on the item
what it left. A second disposal pass finds nothing to do and changes nothing.

Foundry records each disposal in the item's events. A landing records it on
`work_item_settled`. After a cancellation, the Dispose Cancelled Work block
emits `work_item_disposed`. A ref released by a later settlement
also gets `work_item_disposed`, on the trace of the item that owned it.
`foundry queue show` prints the result on a second line under the event, for
example:

```text
  removed worktree /home/foundry/.foundry/worktrees/app/wi_123, local refs/heads/foundry-task/wi_123; left origin refs/heads/foundry-task/wi_123 (origin unreachable)
```

Disposal holds the ledger write gate while it decides what to remove. It
releases the gate for all Git operations, then holds it again to record the
outcome. A concurrent change to the item's state or refs has priority: Foundry
does not record a stale result.

## Sweep of aged orphans

Each reconcile run, scheduled or requested, sweeps orphans that disposal
missed (for example after a crash or a failed removal) and orphans from before
disposal existed. An orphan is a Foundry worktree or a `foundry-task/*` branch
that no open or waiting item owns or starts from.

The sweep removes an orphan only when nothing changed it for the sweep age.
The default age is 7 days. For a worktree, the age is the newest modification
time of the directory and its top-level entries. For a branch, the age is the
later of the tip's commit date and the ref's last reflog entry. The sweep:

- removes an orphan worktree in the Foundry worktrees root
- deletes an orphan branch locally or on origin, with no trunk proof
- deletes a stale `refs/remotes/origin/foundry-task/*` ref when origin no
  longer has the branch
- does nothing in a run that cannot account for a running item

Each removal becomes a `swept_worktree` or `swept_branch` finding, and the
report totals gain `swept worktrees: N` and `swept branches: N`. A failed
removal stays an orphan finding, with the reason. A young orphan stays an
orphan finding, so each three-hourly run reports it (about 56 times before the
sweep removes it).

To change the age, set `FOUNDRY_ORPHAN_SWEEP_DAYS` in the daemon's
environment. Set it to `0` to turn the sweep off.

Apart from the sweep, reconciliation does not remove worktrees. It does not
remove bundles, run agents or manufacture task terminal events. Git fetch updates objects and tracking refs. The fetch leaves `FETCH_HEAD` byte-for-byte
unchanged, so concurrent task startup and remote or bundle continuation retain
their selected commit. It neither imports nor changes nor prunes local tags,
including local-only and divergent annotated tags, even when global or
origin-specific settings enable tag pruning or tag fetching. Orphan, broken,
unresolved and error findings inject a Foundry anomaly through `ObserveEvents`,
so the ops digest proceeds even below its normal 25-event pressure threshold.
