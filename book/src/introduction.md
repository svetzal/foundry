# Foundry

Foundry is an event-driven workflow engine for engineering automation.
It replaces imperative shell scripts with composable **task blocks**
connected by **events**, with **throttle** controlling how far each
event ripples through the system.

Foundry runs as a daemon (`foundryd`) with a CLI controller (`foundry`)
communicating over gRPC. Any emitter — a scheduled job, a webhook, a
manual command — can fire an event and trigger the same downstream
workflow.

## How We Got Here

Foundry has been evolving for roughly four months. It started as a
patchwork of shell scripts and launchd jobs — nightly maintenance
automation that grew into a linear pipeline: iterate projects, audit for
vulnerabilities, cut releases, install locally. Each step waited for all
previous steps to complete. Projects couldn't run in parallel. An audit
couldn't start until every project finished updating. A vulnerability
discovered at 2pm had to wait for the 2am maintenance window.

As the scripts accumulated, the concepts started to clarify. Events,
task blocks, throttle control, self-filtering — these patterns kept
emerging from the scripts, so we started extracting them into something
more intentional. Foundry is the result: a strongly typed Rust framework
for building event-driven engineering workflows, replacing the fragile
shell glue with compiler-checked event flows and composable task blocks.

Foundry decouples the work from the scheduling. The same task blocks that
run during nightly maintenance can be triggered individually, at any time,
with throttle controlling how deep the ripple goes.

## Where We Are Today

Foundry has two layers, both defined in Rust:

- **Event library** — the vocabulary of immutable facts that flow through
  the system. Each event type is a variant of the `EventType` enum with
  a well-defined payload structure.
- **Block library** — the catalogue of reusable task blocks. Each block
  implements the `TaskBlock` trait, declaring which events it sinks on,
  what work it performs, and what events it emits.

All blocks are registered into a single engine at startup. **Workflows
are not declared anywhere** — they emerge from the sink/emit relationships
between blocks. The engine routes by event type; blocks self-filter by
inspecting payload fields. This means a different entry event (or a
different payload) activates a different subset of blocks, producing a
different workflow — all from the same block library.

See [Workflow Formations](guide/workflow-formations.md) for the formations
that exist today and the possibilities the current block library opens up.

## Where We're Headed

Events and blocks will remain Rust-defined — they benefit from strong
type safety and compile-time guarantees. The composition layer is what
we intend to extract: allowing teams to declare which blocks participate
in a formation and how events flow between them through configuration
rather than code, enabling situational customisation without
recompilation.

## Key Ideas

- **Events** are immutable facts — something happened. They carry a payload
  but have no opinion about what should happen next.
- **Task blocks** are reusable units of work. Each block sinks on specific
  event types, does work, and emits new events.
- **Throttle** sits on a task block's output side. It controls whether
  downstream events are emitted: `full` (everything propagates),
  `audit_only` (observers emit, mutators suppress), or `dry_run`
  (read-only, no side effects).
- **Workflows** are compositions of task blocks wired together by events.
  They emerge from the emitter/sink relationships — not from a central
  orchestrator.
