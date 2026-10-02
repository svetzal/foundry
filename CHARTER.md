# Charter

## Mission

Provide a reliable, observable, event-driven engine for automating
engineering workflows — from vulnerability remediation to dependency
maintenance to release management.

## Principles

1. **Events over orchestration.** Work is triggered by events, not by
   position in a script. Any emitter can start any workflow.

2. **Composable task blocks.** Small, reusable units of work. The same
   "Cut Release" block serves the vulnerability workflow and the
   maintenance workflow.

3. **Throttle controls depth.** Every invocation declares how far the
   ripple should go. Audit without releasing. Release without installing.
   The same workflow, different throttle.

4. **Observability is paramount.** Every event, every task block execution,
   every throttle decision is logged and traceable. If it happened, you
   can see it.

5. **Correctness through types.** Rust's type system enforces exhaustive
   event handling, valid throttle states, and safe concurrency. Malformed
   events are compiler errors, not runtime surprises.

6. **Implementation informs scope.** Campaigns reassess the full remaining
   mission after each implementation cycle. Actual implementation experience,
   review findings, and delivered evidence can change the next objective,
   priority, or decomposition. The mission and owner constraints remain binding.
   This learning loop is a core design feature. Efficiency improvements must
   preserve it.

## Scope

Foundry automates engineering workflows for a registered project portfolio:

- Adaptive engineering campaigns that derive the next task from the mission,
  current evidence, and experience from previous implementation cycles
- Vulnerability detection and remediation
- Dependency maintenance (iterate, maintain, commit, push)
- Release management (tag, build, distribute)
- Local tool installation
- Release pipeline observation
- Work admission, pacing and settlement — the work-item ledger records every
  unit of work Foundry admitted, how it reached Foundry, and how it settled.
  This is a deliberate scope expansion, decided by the owner on 2026-09-29:
  automating work that a person cannot see admitted or settled is not
  automation they can rely on. The queue holds only executable work — work
  Foundry has dispatched or is about to — and is deliberately **not** a
  backlog: intent that nobody has committed to executing belongs in planning
  tools, not here.

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

It does **not** replace the existing `evt-cli` event logging system.
Foundry emits events into the same JSONL intake files, coexisting with
the current tooling. Over time, `evt-cli` may be rewritten in Rust to
share Foundry's event type crate.

## Non-Goals

- General-purpose workflow engine (this serves specific engineering needs)
- CI/CD replacement (Foundry orchestrates local work and observes pipelines)
- Real-time monitoring dashboard (use Grafana, ops-visualizer, or similar)
