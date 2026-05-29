# Foundry Idempotency — Design Spec

A contract for making **mutator block execution safe to redeliver** — so that a
crash-resumed workflow, a retried remote-worker dispatch (Phase 2), or a
duplicated event never repeats an irreversible side effect (`git push`, cut a
release, install a binary).

Companion to `SDK.md`. This spec defines the contract and the first
implementable slice; it does **not** mandate that the enforcement path ship
before there is a redelivery source to defend against. Where an effect's state
cannot be confirmed, the design escalates into **self-healing reconciliation**
(see below) rather than failing silently — the longer-term goal is a system that
notices and repairs its own interrupted work.

## Scope and honest framing

Idempotency dedup is a **`BlockKind::Mutator`-only concern**. `Observer` blocks
are replay-safe by definition (they read state, run scans, check conditions) and
are never gated. The engine already knows `kind()`, so the existing taxonomy is
the gate — no new classification.

Two layers, increasing robustness:

| Layer | Mechanism | Covers | Cost |
| --- | --- | --- | --- |
| **L1 — engine ledger** | At-most-once *dispatch* for completed-and-recorded effects, keyed on `(block, trigger)` | The 90% case: a block that *fully completed* before the crash/redelivery | Cheap, generic, uniform |
| **L2 — semantic probe** | Block observes the external world to resolve the ambiguous *in-progress* window | The block that crashed *mid-effect* | Opt-in, per critical mutator |

**Payoff is gated on redelivery existing.** In a single live `process()` call
an event is popped once and offered to each matching block once — the ledger
never fires. Its value is realized exactly at the boundaries Phase 2 and
durability introduce: remote-worker dispatch retry, and crash-resume from the
durable event log. The contract additions below are non-breaking (all
defaulted), so they can land now to let block authors start declaring intent;
the *enforcement* in `run_block` activates when the first redelivery source is
built.

## Contract additions (`foundry-sdk`)

### 1. `effect_key` on `TaskBlock`

```rust
pub trait TaskBlock: Send + Sync {
    // ...existing methods...

    /// Identity of the *effect* this execution produces, stable across
    /// redeliveries. Returning `Some(key)` lets the engine suppress a second
    /// execution that would repeat the same side effect.
    ///
    /// Default `None`: the engine uses its derived key `(block_name, trigger.id)`,
    /// which dedups redeliveries of the *same trigger*. Override only when the
    /// effect identity is broader than one trigger — and only when that identity
    /// is computable from the trigger ALONE, with no I/O, before execution.
    fn effect_key(&self, _trigger: &Event) -> Option<String> {
        None
    }
}
```

The "no I/O, computable from the trigger alone" constraint is load-bearing — see
the release worked example for why agent-driven mutators usually cannot widen
the key and must rely on L2 instead.

### 2. `EffectProbe` + `probe_effect` (Layer 2, optional)

```rust
/// Whether a mutator's effect has already landed in the external world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectProbe {
    /// The effect is confirmed already applied — skip re-execution.
    Applied,
    /// The effect is confirmed NOT applied — safe to re-execute.
    NotApplied,
    /// Cannot determine — the engine resolves conservatively.
    Unknown,
}

pub trait TaskBlock: Send + Sync {
    /// Resolve an ambiguous *in-progress* record after a crash or redelivery:
    /// observe the external world and report whether this execution's effect
    /// already landed.
    ///
    /// Called by the engine ONLY when the ledger shows `InProgress` for this
    /// block's effect key. Default `Unknown` → the engine is conservative: it
    /// does not re-run a mutator whose effect might have landed, and surfaces
    /// the ambiguity instead of guessing.
    fn probe_effect(
        &self,
        _trigger: &Event,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<EffectProbe>> + Send + '_>> {
        Box::pin(async { Ok(EffectProbe::Unknown) })
    }
}
```

## The ledger (`foundry-engine`)

The dedup *evidence* is shaped like the `BlockExecution` records the engine
already produces (`block_name`, `trigger_event_id`, `success`). But those live
in the **TTL'd trace store** (`TraceStore::new(ttl)`) — a cache, not a durable
ledger — and are indexed by `event_id`/`span`, not by effect key. So the ledger
is a small, dedicated, **non-expiring** keyed store, mirroring `EventWriter`'s
append-only, flush-and-close, crash-safe pattern.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectStatus {
    /// No record — first time this effect key is seen.
    Fresh,
    /// A prior attempt began but never recorded completion
    /// (crash or worker loss inside the effect window).
    InProgress,
    /// A prior attempt completed and was recorded.
    Completed,
}

/// Durable, keyed record of mutator effects, consulted by the engine before
/// dispatching a mutator. Sync (small local journal writes, like `EventWriter`);
/// can move async if ever backed by a remote store.
pub trait IdempotencyLedger: Send + Sync {
    fn status(&self, key: &str) -> EffectStatus;
    /// Durably mark an effect in-progress BEFORE its side effect runs (WAL).
    fn mark_in_progress(&self, key: &str) -> anyhow::Result<()>;
    /// Durably mark an effect completed AFTER its side effect succeeds.
    fn mark_completed(&self, key: &str) -> anyhow::Result<()>;
    /// Clear an in-progress marker after a failed attempt, so retry is allowed.
    fn clear(&self, key: &str) -> anyhow::Result<()>;
}
```

`EffectStatus` and `IdempotencyLedger` live in `foundry-engine` (only the engine
and host reference them). `EffectProbe` lives in `foundry-sdk` (referenced by
`TaskBlock::probe_effect`, which blocks implement).

### Default file-backed impl

`FileIdempotencyLedger` — append-only JSONL journal, one line per transition
`{ key, status, at }`. On construction, fold the journal into an in-memory
`RwLock<HashMap<String, EffectStatus>>` (last-write-wins per key); serve
`status()` from the map, append-and-flush on every mutation — the same
`Mutex` plus flush-and-close discipline as `EventWriter`. The engine holds
`Option<Arc<dyn IdempotencyLedger>>`; the host constructs and injects it,
exactly as it does `EventWriter` today.

## Engine integration — the single gate in `run_block`

Key derivation:

```rust
fn dedup_key(block: &dyn TaskBlock, trigger: &Event) -> String {
    block
        .effect_key(trigger)
        .unwrap_or_else(|| format!("{}::{}", block.name(), trigger.id))
}
```

The gate, inserted in `run_block` after the dry-run / throttle checks and before
`execute_with_retry`:

```rust
// Idempotency gate — mutators only; observers are replay-safe and never gated.
let dedup_key = (block.kind() == BlockKind::Mutator).then(|| dedup_key(block, current));

if let (Some(key), Some(ledger)) = (&dedup_key, &self.ledger) {
    match ledger.status(key) {
        EffectStatus::Completed => {
            tracing::info!(key = %key, "skipped (idempotent: effect already applied)");
            return make_block_execution(
                block, current, block_span_id, workflow_span_id,
                block_start, true, "skipped (idempotent)".into(),
            );
        }
        EffectStatus::InProgress => match block.probe_effect(current).await {
            Ok(EffectProbe::Applied) => {
                let _ = ledger.mark_completed(key);
                return make_block_execution(/* ... */ "skipped (probe: effect applied)".into());
            }
            Ok(EffectProbe::NotApplied) => { /* fall through to re-execute */ }
            Ok(EffectProbe::Unknown) | Err(_) => {
                // Cannot confirm the effect's state. Do NOT re-run (that risks a
                // duplicate); hand off to self-healing reconciliation instead.
                tracing::warn!(key = %key, "effect in-progress, unresolvable — requesting reconciliation");
                let recon = current.with_payload(
                    EventType::ReconciliationRequested,
                    &ReconciliationRequestedPayload {
                        effect_key: key.clone(),
                        block_name: block.name().to_string(),
                        trigger_event_id: current.id.clone(),
                    },
                )?;
                self.persist_and_broadcast_events(vec![recon], current, &block_span_id, state, true);
                return make_block_execution(/* ... */ false, "handed off to reconciliation".into());
            }
        },
        EffectStatus::Fresh => {
            let _ = ledger.mark_in_progress(key); // WAL write before the effect
        }
    }
}

// ...existing execute_with_retry...

// On Ok(result) where result.success:
if let (Some(key), Some(ledger)) = (&dedup_key, &self.ledger) {
    let _ = ledger.mark_completed(key);
}
// On failure / Err: ledger.clear(key) so a legitimate retry is not blocked.
```

### Skip semantics: no-op, not replay

A skipped (`Completed`) block does **not** re-emit its events. This is correct
under the **event-log-replay resume model**: crash-resume reconstructs the
processing queue from the durable event log, so a completed block's downstream
events are recovered independently — the ledger only prevents *re-executing* the
block, while the logged events drive propagation. Re-emitting on skip would
double-deliver downstream. (The alternative "root-replay" model would require
replay; we explicitly choose log-replay because it makes the log the source of
truth, aligning with the durability direction in `SDK.md`.)

## The irreducible in-progress window

`mark_in_progress` and the side effect are **not atomic** — `git push` and a
local journal write cannot share a transaction. A crash between them leaves an
`InProgress` record with unknown real-world state. This residual risk cannot be
eliminated generically; it can only be *resolved*, and it lands precisely on
mutators whose effect is not internally idempotent. L2's `probe_effect` is the
only thing that closes it inline, and only for blocks that can observe their own
effect. For the rest, the engine does not guess — it hands the ambiguity to
**self-healing reconciliation** rather than fail silently.

## Self-healing reconciliation

When the inline probe returns `Unknown` (or errors), the engine emits a
`ReconciliationRequested` event instead of just failing the block. This turns an
unresolvable in-progress record from a dead-end into a *workflow* — an escalation
ladder that resolves automatically where it can and escalates to a human only
where it genuinely must.

### Event family (fits the AGENTS.md taxonomy)

| Event | Category | Meaning |
| --- | --- | --- |
| `ReconciliationRequested` | Command (`*Requested`) | The engine wants an ambiguous effect resolved. Carries `effect_key`, `block_name`, `trigger_event_id`. |
| `ReconciliationStarted` / `ReconciliationCompleted` | Lifecycle | A reconciliation attempt began / finished (`success` on the payload). |
| `EffectReconciled` | Domain fact | The terminal resolution: `applied` (effect was already done) or `reverted` (world confirmed clean, safe to re-run). |

These are **built-in** `EventType` variants, not `Custom` — reconciliation is a
core engine concern. `ReconciliationRequested` opens a workflow span, so it must
be registered via `EventType::is_span_opener` (the AGENTS.md rule for any new
`*Requested` event). `RemediationStarted` already exists in the taxonomy, so the
lifecycle pairing has precedent.

### The escalation ladder

```text
mutator crashes mid-effect
  → restart; ledger shows InProgress; inline probe → Unknown
  → engine emits ReconciliationRequested { effect_key, block, trigger }
  → ReconcileEffect block sinks on it:
        ReconciliationStarted
        1. RE-PROBE      — retry the block's probe after a backoff (handles a
                           transient probe failure: network, locked repo, etc.)
        2. INVESTIGATE   — hand the ambiguity to an agent via AgentGateway:
                           "release of project X was in flight at crash; inspect
                            git tags, origin, and the pipeline — APPLIED or not?"
        3. RESOLVE:
             confirmed applied  → EffectReconciled{applied};  ledger → Completed
             confirmed clean    → EffectReconciled{reverted}; ledger → cleared,
                                   original trigger re-emitted to re-run safely
             still uncertain    → ReconciliationCompleted{success:false}
                                   → escalates to a human (the true dead-end)
```

Steps 1–2 are where self-healing lives: most crash-mid-effect ambiguities are a
transient probe failure or an answerable "did it land?" question, and Foundry
already has the machinery — agent-driven remediation blocks
(`RemediateVulnerability`, `RemediatePipeline`) are the existing pattern this
follows.

### The safety invariant — asymmetric harm

The harms are not symmetric. A false **"applied"** (skip work that didn't
actually happen) is *recoverable* — the next scheduled run notices and redoes
it. A false **"clean → re-run"** (repeat an effect that did happen) is
*irreversible* — a double release, a double push. Therefore:

> **Reconciliation may only resolve to `reverted` (re-run) when the world is
> *positively confirmed* clean. Every uncertain outcome resolves to `applied`
> (skip) or escalates. Uncertainty never re-runs.**

This biases self-healing toward the safe failure mode by construction, which is
what makes auto-resolution trustworthy enough to leave unattended.

### Ledger-write authority

The reconciliation block does **not** hold a ledger handle. It emits
`EffectReconciled`, and the **engine** folds that fact into the ledger — keeping
the engine the single ledger writer. This makes the ledger itself event-sourced:
its state is a projection over the in-progress markers and `EffectReconciled`
facts in the durable stream, rebuildable on restart. That is the same
"log is the source of truth" move the durability direction in `SDK.md` is
heading toward, and it sets up the longer-term **agent-efficacy retrospectives**
(AGENTS.md) — every reconciliation is a recorded, queryable instance of the
system noticing and healing its own interrupted work.

### What rides on the resume model

Continuing the *original* workflow after reconciliation (so the mutator's normal
downstream events still flow) depends on the event-log-replay resume model and
therefore lands with the durability work, not before. Slice D below delivers the
reconciliation event family and the `ReconcileEffect` block; full
resume-and-continue is sequenced with crash-resume itself.

## Worked example — `release.rs` (Layer 2, and its limits)

The release step's effect is "tag `vX.Y.Z` created at HEAD and pushed." It
already owns the world-observing primitive: `verify_tag_at_head`
(`release.rs:514`), today used to confirm the agent tagged the right commit.

This example is instructive because it shows **both the shape and the limits**
of semantic idempotency:

+ **`effect_key` cannot widen.** An agent-driven release decides the bump
  (`patch`/`minor`/`major`) and the resulting version *during* execution, by
  reading the project's current version — I/O that `effect_key(trigger)` is
  forbidden from doing. So release returns `None` and dedups on the default
  `(block_name, trigger.id)`: redelivering the *same* `ReleaseRequested` event
  is suppressed, which is exactly the remote-retry / resume case.

+ **`probe_effect` is best-effort, often `Unknown`.** Given only the trigger,
  the block does not know which tag a prior crashed attempt would have created.
  It can run a heuristic — "is HEAD tagged with a `vX.Y.Z` that points at HEAD
  and is pushed to origin?" (reusing `verify_tag_at_head`'s git inspection) — and
  upgrade `Unknown → Applied` when that clearly holds. When it cannot tell, it
  returns `Unknown`, and the engine surfaces the release for human/agent
  reconciliation rather than risk a double release.

The takeaway for the spec: **do not force a clean probe where one doesn't
exist.** For agent-driven, non-deterministic-output mutators, conservative
surfacing is the correct L2 resolution. Blocks whose effect identity *is* known
from the trigger (e.g. a fixed tag, a deterministic install path) get a real
`Applied`/`NotApplied` probe.

## Implementation slices

1. **Slice A — contract (lands now, additive, dormant).**
   `effect_key`, `EffectProbe`, `probe_effect` on `TaskBlock` (all defaulted);
   `EffectStatus` + `IdempotencyLedger` trait + `FileIdempotencyLedger` in
   `foundry-engine`, unit-tested in isolation. **Not yet wired into dispatch** —
   no behavior change, quality gates stay green. Block authors may begin
   declaring `effect_key`.
2. **Slice B — activate the gate.** Wire the `run_block` mutator gate and inject
   the ledger from the host. Lands together with (or just ahead of) the first
   redelivery source — remote-worker dispatch retry (Phase 2) or crash-resume.
3. **Slice C — L2 on `release.rs`.** Implement the best-effort `probe_effect`
   reusing `verify_tag_at_head`; hand off to reconciliation on `Unknown`.
4. **Slice D — self-healing reconciliation.** The `ReconciliationRequested` /
   `ReconciliationStarted` / `ReconciliationCompleted` / `EffectReconciled` event
   family (built-in variants; `ReconciliationRequested` registered as a span
   opener), plus a `ReconcileEffect` block implementing the re-probe → agent
   investigation → escalate ladder under the asymmetric-harm invariant. Engine
   folds `EffectReconciled` into the ledger. Full resume-and-continue is
   sequenced with crash-resume (durability work), not here.

## Decided

1. **In-progress default — conservative.** Never re-run a mutator whose effect
   might have landed; resolve via reconciliation, escalate when uncertain.
2. **Reconciliation surface — typed event.** An unresolvable `InProgress` emits
   `ReconciliationRequested` (observable in traces, actionable by a block),
   moving toward self-healing rather than a silent failure.

## Open decisions

1. **Ledger retention** — the append-only journal grows unbounded. Compact on
   startup (fold to latest-per-key and rewrite). A periodic rewrite of the folded
   map is enough; nothing fancier.

## Test plan

+ **Ledger unit tests** (mirror `event_writer` tests): fresh→in-progress→
  completed transitions survive reconstruction from the journal; `clear` allows
  re-entry; concurrent writes never interleave.
+ **Engine gate tests** (in-memory fake ledger): a completed mutator is skipped
  with a `success` `BlockExecution` and emits nothing; an observer is never
  gated; a fresh mutator runs and is marked completed; a failed mutator is
  cleared so retry proceeds; an `InProgress` + `Unknown` probe surfaces rather
  than re-runs.
+ **Release L2 test**: with a fake `ShellGateway` reporting "tag at HEAD,
  pushed," `probe_effect` returns `Applied`; with ambiguous git state, `Unknown`.
+ **Reconciliation tests**: an `Unknown` probe emits `ReconciliationRequested`
  with the effect key/block/trigger; a `ReconcileEffect` re-probe that confirms
  "applied" emits `EffectReconciled{applied}` and the engine folds the ledger to
  `Completed`; an uncertain investigation emits `ReconciliationCompleted{success:
  false}` and never re-emits the original trigger (the asymmetric-harm invariant).
