# Throttle Control

Throttle controls how far an event ripples through the task block chain.
It's set at invocation time and propagated through every event in the chain.

## Levels

| Level | Observers | Mutators | Use case |
|-------|-----------|----------|----------|
| `full` | Execute + emit | Execute + emit | Automated runs, nightly maintenance |
| `audit_only` | Execute + emit | Execute + suppress emission | "Just check" — audit without releasing |
| `dry_run` | Execute + emit | Skip execution entirely | Preview what would happen |

## How It Works

The throttle is a property of the **event**, not the task block. When a
block emits downstream events, those events carry the same throttle as
the triggering event. This means the throttle decision is made once (at
invocation) and respected throughout the chain.

```text
foundry emit vulnerability_detected --project my-tool --throttle audit_only

  vulnerability_detected (throttle: audit_only)
    → Audit Main Branch (Observer) → executes, emits main_branch_audited
      → Cut Release (Mutator) → executes, but SUPPRESSES release_completed
        (chain stops here — no further propagation)
```

## Observer vs Mutator

The key design question for every task block: is it an Observer or a Mutator?

- **Observer**: reads state, runs scans, checks conditions. Never changes
  the world. Always runs, always emits, regardless of throttle.
- **Mutator**: writes files, pushes commits, cuts releases, installs tools.
  Changes the world. Throttle controls whether it runs and emits.

At `audit_only`, a Mutator still **executes** (so you see what it would
do in the logs) but **suppresses emission** (so downstream blocks don't
fire). At `dry_run`, Mutators don't execute at all.

## CLI Usage

```bash
# Default: full
foundry emit greet_requested --project hello

# Explicit throttle
foundry emit greet_requested --project hello --throttle audit_only
foundry emit greet_requested --project hello --throttle dry_run
```
