# Concepts

## Events

An event records that something happened. It is an immutable fact appended
to the event log. Events carry:

- **id** — deterministic hash of content (same input always produces same ID)
- **event_type** — what happened (e.g., `vulnerability_detected`)
- **project** — which project this relates to
- **throttle** — propagated through the chain, controls downstream behavior
- **payload** — event-type-specific data as JSON
- **occurred_at** / **recorded_at** — timestamps

Events have no opinion about what should happen next. That's the job of
task blocks.

## Task Blocks

A task block is a unit of work. It has:

- **Name** — human-readable identifier
- **Kind** — `Observer` (reads/scans) or `Mutator` (writes/deploys)
- **Sinks** — which event types trigger this block
- **Work** — what it does when triggered
- **Emits** — which events it produces on completion

Task blocks are registered with the engine at startup. When an event
arrives, the engine finds all blocks that sink on that event type and
executes them.

### Observer vs Mutator

The distinction matters for throttle behavior:

| Kind | Examples | Throttle behavior |
|------|----------|-------------------|
| Observer | Audit tag, audit main, validate project | Always executes, always emits |
| Mutator | Cut release, install locally, commit+push | Execution and emission controlled by throttle |

## Throttle

The throttle sits on a task block's **output side**. After a block
completes its work, it checks the throttle before emitting downstream
events.

| Level | Observers | Mutators |
|-------|-----------|----------|
| `full` | Execute + emit | Execute + emit |
| `audit_only` | Execute + emit | Execute + **suppress emission** |
| `dry_run` | Execute + emit | **Skip execution** + suppress emission |

The throttle is set when an event is first emitted (e.g., via CLI) and
propagated through the entire chain. This means a single command controls
how deep the ripple goes.

## Workflows

A workflow is not a first-class object in Foundry. It emerges from the
emitter/sink wiring between task blocks. When you emit `vulnerability_detected`,
the chain of task blocks that fire in response *is* the vulnerability
remediation workflow.

This means workflows are composable by nature — adding a new task block
that sinks on an existing event type automatically extends every workflow
that produces that event.

## Engine

The engine is the runtime that:

1. Receives an event
2. Finds task blocks that sink on that event type
3. Checks throttle to decide whether to execute and/or emit
4. Executes matching blocks
5. Collects emitted events and feeds them back into step 2
6. Continues until no more events are produced (the chain is complete)

The engine processes events depth-first within a single invocation.
