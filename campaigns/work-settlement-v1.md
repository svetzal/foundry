# Work Settlement V1

## Intent

Foundry 0.40.x keeps a work-item ledger and shows it through `foundry queue`
and ops-visualizer's `/queue`. The open group (items in `preserved`,
`needs_decision` or `failed`) is meant to be the short list of things a
person must act on. After one day of use it held seven items, and every one
of them was stale: work that later landed by another route, a cycle a later
cycle superseded, a task a stray daemon mis-marked, a review that died on a
transient provider error. Nothing in Foundry could close them, and nothing
compared the ledger with the worktrees and branches on disk.

The same day, the nightly majors lane opened a fresh branch for the same
dependency upgrade on consecutive nights (parite cmx-core, parite quick-xml,
epilogue-tracker vite) because nothing told it a preserved item for that
objective already existed.

This slice makes the open list honest and gives Foundry the reconciliation
the plan calls settlement. It is the next slice of
`Planning/2026-09-29-foundry-work-intake-v1.md` in the Operations repository.

## Completion boundary

V1 is complete when:

- **Automatic supersession.** A preserved item whose preserved commit is
  already reachable from the registered trunk, or is patch-equivalent to it
  (`git cherry` reports no unmatched commit), settles as `landed` with the
  trunk commit that carries it and the reason `superseded by <commit>`. The
  check runs when a task lands on that project, and on the reconciler's
  schedule. It emits `work_item_settled` like any other settlement.
- **Owner controls** in `foundry queue`, each going through a typed gRPC
  mutation and recording the operator origin the CLI already captures:
  - `foundry queue close <id> --reason <text>`: an open item becomes
    `cancelled` with that reason (`work_item_cancelled`).
  - `foundry queue resume <id>`: an item in `preserved` is dispatched as a
    new task whose base is the preserved branch, with the original objective
    and a `resumes: <old id>` link; the old item settles `landed` when the
    new one lands, and stays `preserved` otherwise.
  - `foundry queue cancel <id>`: an item in `submitted` or `queued` becomes
    `cancelled` without running. (Nothing waits in those states today; the
    command exists so pacing can use it.)
  Each control refuses, with a typed error, any state it does not apply to.
- **Resume before re-dispatch in the majors lane.** When the nightly plans a
  major-upgrade task and an open `preserved` item exists for the same
  project and the same upgrade (same package and target version), the lane
  dispatches a resume of that item instead of a fresh task. A second night
  therefore continues one branch rather than opening another.
- **The reconciler.** A canonical sentinel `work-reconciler` (default
  schedule `30 */3 * * *`, so it runs after `ops-digest` reads) emits
  `work_reconcile_started`. Its block compares, per registered project:
  - work items (open and running);
  - worktrees under the Foundry worktrees directory, plus any worktree
    `git worktree list` reports for the registered checkout;
  - `foundry-task/*` branches, local and on origin (a fetch with prune first,
    read-only).
  It writes a digest to `{FOUNDRY_RECONCILE_DIR}/{YYYY-MM-DD}.md` (default
  `~/.foundry/reconcile/`) listing: orphan worktrees (no open or running
  item), orphan branches (no open item; and whether each is on trunk),
  broken items (open or running with no worktree and no branch), and, as
  information only, worktrees Foundry did not create and registered
  checkouts with uncommitted changes. It emits `work_reconcile_completed`
  with counts, and the ops digest treats any orphan or broken item as an
  anomaly. It changes nothing: no deletion, no settlement except the
  supersession rule above.
- **`foundry queue`** shows a superseded item's trunk commit in its reason,
  shows `resumes` links, and gains `foundry queue reconcile` to run the
  reconciler now and print its digest.
- **Documentation:** `book/src/guide/work-queue.md` (controls and
  supersession), a new `book/src/guide/work-reconciler.md`, the sentinel
  list in `book/src/guide/sentinels.md`, the proto, the CLI reference, and
  the command table in `AGENTS.md`.
- **Tests:** supersession by ancestry and by patch-equivalence in a real
  temporary repository; each control's allowed and refused states at the
  service boundary with temporary stores; the majors lane choosing resume
  over re-dispatch; the reconciler classifying each category from a
  temporary repository with planted worktrees and branches, and leaving all
  of it untouched.

## Scope guards

- The reconciler reports; it never deletes a worktree, a branch or a bundle.
- No pacing: nothing is delayed, reordered or held. `queue cancel` on a
  queued item is the only forward reference to that slice.
- No duplicate or conflict detection at admission beyond the majors-lane
  resume rule.
- No ops-visualizer change. Its consumer slice follows the release.
- Do not read another tool's data; git and the Foundry stores are the only
  sources.
- Keep the ledger the single source of truth: the reconciler builds its
  comparison in memory and writes only its digest.

If supersession cannot be decided safely for some case (for example a
preserved commit that was squash-landed with edits), leave the item open and
say why in the reconciler digest; do not guess.

## Growth path

1. This slice.
2. ops-visualizer: `superseded` rendering, `resumes` links, the reconciler
   digest as a fifth group on `/queue`.
3. Pacing: per-repository serialisation, host and provider caps, a disk
   floor before start, lanes, owner holds.
4. Planning: duplicate and conflict checks at admission, the start-time
   check, GitHub issue and board intake, `queue explain`.
