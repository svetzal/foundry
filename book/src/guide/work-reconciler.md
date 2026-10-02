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
- Orphan local and origin task branches, their commit and ancestry status against
  either the local registered trunk or its freshly fetched origin tracking branch.
  Findings name the trunk that proves supersession and report differing trunk
  commits. A branch-only fetch refreshes origin tracking refs and
  retains stale origin tracking branches, even when Git configuration enables pruning.
- Broken item workspace references and unresolved preservation or inspection
  evidence. Unmatched patches and ambiguous heads keep obligations open.
- Informational non-Foundry worktrees and dirty registered checkouts.

Running work is associated through recorded workspace fields or exact project
and trace evidence in the durable event log, never by parsing item IDs out of
workspace names. A running item without that evidence makes ownership unresolved;
its possible workspace is not declared orphaned.

Preserved work settles `landed` only when the existing conservative Git proof
shows it is an ancestor of trunk, or all its non-merge commits have equivalent
patches on trunk (`git cherry`). After a successful origin fetch, either the local
registered trunk (`refs/heads/<branch>`) or an origin trunk observed by that
fetch (`refs/remotes/origin/<branch>`) can
provide that proof: a stale checkout does not hide landed work, and local work
ahead of origin still counts. With no origin or a failed fetch, existing
behaviour is retained. Retained tracking refs absent from the successful fetch
are never used as origin trunk proof.
Shallow history, missing objects, divergent local and remote preservation heads,
ambiguous bundles and unmatched patches remain open by exact ID. The settlement records the verified trunk commit and exact reason
`superseded by <commit>`, retaining submission, continuation and preservation
fields. Its `work_item_settled` event stays on the original trace.

Before saving, the reconciler reloads the ledger under the same write gate as
owner controls and compares the entire inspected record. Owner cancellation and
concurrent changes win over stale proof. Unrelated records remain intact and
repeated reconciliation emits no second settlement. Historical event bytes are
never rewritten.

Once an item settles landed, Foundry deletes only its recorded task branch
locally and its recorded preservation branch locally and on origin, best-effort.
Each deletion requires fresh ancestry or patch-equivalence proof against the
registered trunk and refuses branches checked out in any worktree. A changed
ref fails the guarded deletion. Unowned refs, bundles and refs also owned by
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
