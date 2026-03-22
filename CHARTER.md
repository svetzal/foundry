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

## Scope

Foundry automates engineering workflows for Mojility's project portfolio:

- Vulnerability detection and remediation
- Dependency maintenance (iterate, maintain, commit, push)
- Release management (tag, build, distribute)
- Local tool installation
- Release pipeline observation

It does **not** replace the existing `evt-cli` event logging system.
Foundry emits events into the same JSONL intake files, coexisting with
the current tooling. Over time, `evt-cli` may be rewritten in Rust to
share Foundry's event type crate.

## Non-Goals

- General-purpose workflow engine (this serves Mojility's engineering needs)
- CI/CD replacement (Foundry orchestrates local work and observes pipelines)
- Real-time monitoring dashboard (use Grafana, ops-visualizer, or similar)
