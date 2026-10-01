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
  the registered trunk. Fetch with prune refreshes origin tracking refs first.
- Broken item workspace references and unresolved preservation or inspection
  evidence. Unmatched patches and ambiguous heads keep obligations open.
- Informational non-Foundry worktrees and dirty registered checkouts.

Running work is associated through recorded workspace fields or exact project
and trace evidence in the durable event log, never by parsing item IDs out of
workspace names. A running item without that evidence makes ownership unresolved;
its possible workspace is not declared orphaned.

Preserved work settles `landed` only when the existing conservative Git proof
shows it is an ancestor of trunk, or all its non-merge commits have equivalent
patches on trunk (`git cherry`). Shallow history, missing objects, divergent local
and remote heads, ambiguous bundles and unmatched patches remain open by exact
ID. The settlement records the verified trunk commit and exact reason
`superseded by <commit>`, retaining submission, continuation and preservation
fields. Its `work_item_settled` event stays on the original trace.

Before saving, the reconciler reloads the ledger under the same write gate as
owner controls and compares the entire inspected record. Owner cancellation and
concurrent changes win over stale proof. Unrelated records remain intact and
repeated reconciliation emits no second settlement. Historical event bytes are
never rewritten.

Reconciliation does not remove worktrees, branches or bundles, run agents or
manufacture task terminal events. Git fetch's object and tracking-ref updates
are its only repository mutations. Orphan, broken, unresolved and error findings
inject a Foundry anomaly through `ObserveEvents`, so the ops digest proceeds even
below its normal 25-event pressure threshold.
