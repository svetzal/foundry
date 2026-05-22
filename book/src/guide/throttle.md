# Throttle Control

Throttle controls how far an event ripples through the task block chain.
It's set at invocation time and propagated through every event in the chain.

## Levels

| Level | Observers | Mutators | Use case |
|-------|-----------|----------|----------|
| `full` | Execute + emit | Execute + emit | Automated runs, nightly maintenance |
| `dry_run` | Execute + emit | Skip execution, simulate success via `dry_run_events()` | Preview what would happen |

`full` is the default.

## How It Works

The throttle is a property of the **event**, not the task block. When a
block emits downstream events, those events carry the same throttle as
the triggering event. This means the throttle decision is made once (at
invocation) and respected throughout the chain.

Under `dry_run`, Mutator blocks are not executed at all. Instead they
simulate success via `dry_run_events()`. The simulated events carry
`dry_run: true` and are still delivered downstream, so the full shape of
the chain remains visible even though no Mutator actually ran.

```text
foundry emit vulnerability_detected --project my-tool --throttle dry_run

  vulnerability_detected (throttle: dry_run)
    → Audit Main Branch (Observer) → executes, emits main_branch_audited
      → Cut Release (Mutator) → NOT executed, emits simulated
        release_completed (dry_run: true)
        → downstream blocks still see the chain
```

## Observer vs Mutator

The key design question for every task block: is it an Observer or a Mutator?

- **Observer**: reads state, runs scans, checks conditions. Never changes
  the world. Always runs, always emits, regardless of throttle.
- **Mutator**: writes files, pushes commits, cuts releases, installs tools.
  Changes the world. Throttle controls whether it runs.

At `dry_run`, Mutators don't execute at all — they simulate success via
`dry_run_events()`, and the simulated events carry `dry_run: true`.

## CLI Usage

```bash
# Default: full
foundry emit greet_requested --project hello

# Explicit throttle
foundry emit greet_requested --project hello --throttle dry_run
```
