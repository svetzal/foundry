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

Once an item settles landed, Foundry deletes only its recorded task branch
locally and its recorded preservation branch locally and on origin, best-effort.
Each deletion requires fresh ancestry, patch-equivalence or content-containment
proof against the registered trunk and refuses branches checked out in any worktree. A changed
ref fails the guarded deletion. The origin deletion pushes with `--no-verify`:
it carries no code, so the project's own pre-push hook (which may run the whole
gate suite) does not run. Hooks on the remote still apply. Unowned refs, bundles and refs also owned by
unlanded items in any registered project sharing the repository are retained.
Ownership compares exact refs across repository slugs and shared Git common
directories, including separate clones and linked worktrees. The disposition
and settlement event retain ref names, observed commits and per-ref deletion
results; deletion failure leaves
the item landed. Preservation evidence for unlanded work is never deleted.
Cleanup decides eligibility under the ledger write gate, releases it for all
Git operations, then reloads under the gate to record outcomes. Concurrent
ledger updates are retained; a changed state or ref ownership is never
overwritten by stale cleanup evidence.

Reconciliation does not remove worktrees or bundles, run agents or
manufacture task terminal events. Git fetch updates objects and tracking refs. The fetch leaves `FETCH_HEAD` byte-for-byte
unchanged, so concurrent task startup and remote or bundle continuation retain
their selected commit. It neither imports nor changes nor prunes local tags,
including local-only and divergent annotated tags, even when global or
origin-specific settings enable tag pruning or tag fetching. Orphan, broken,
unresolved and error findings inject a Foundry anomaly through `ObserveEvents`,
so the ops digest proceeds even below its normal 25-event pressure threshold.
