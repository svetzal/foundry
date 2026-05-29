# Foundry Core Platform / SDK Split

A plan to separate Foundry into a **core platform** and a **contributor SDK**,
so that others can build custom task blocks and workflows and plug them into a
Foundry installation.

## Decisions

- **Delivery model:** Both — SDK first (in-process, type-safe crate split),
  gRPC block workers later (additive). The in-process split proves the contract
  before any wire protocol is committed.
- **Trust model:** Trusted / in-org contributors for now. Blocks run in-process
  with full host privileges; isolation/sandboxing is deferred. The SDK boundary
  is designed so an isolated (out-of-process) model can be added later without
  reshaping the contract.

## Guiding principle

Design the **SDK boundary** (Phase 1) so the gRPC worker protocol (Phase 2) is
*additive* — a worker is just a `TaskBlock` whose `execute()` forwards over the
wire. If the trait stays the contract, Phase 2 never reshapes Phase 1.

## Gross components today

| Layer | Crate | Role |
|---|---|---|
| Contract | `foundry-core` (~6k LOC) | Domain vocabulary: `Event`, `EventType`, `TaskBlock`, `TaskBlockResult`, `Throttle`, `Scatter`/`GatherSpec`, `Registry`, `Sentinel`, payloads. Already mostly an SDK. |
| Mechanism + batteries + host | `foundryd` (~29k LOC) | Three fused things: the **engine** (`engine.rs`, dispatch/scatter/gather/retry/tracing), the **38 concrete blocks** (`blocks/`, `orchestrator.rs`), and the **host** (gRPC `service.rs`, scheduler, persistence, `register_blocks()` in `main.rs`). |
| Client | `foundry-cli` (~3.6k LOC) | Pure gRPC client. Already decoupled — not part of this work. |

Key strength: the engine is **already generic over blocks**. Dispatch is
`block.sinks_on().contains(&event.event_type)` (`engine.rs:452`); there is **no
exhaustive `match` on `EventType` in the engine**. Workflows are emergent —
blocks chained by which events they sink and source.

## The three coupling points blocking third-party contribution

1. **`EventType` is a closed enum** (`event.rs:310`, 56 variants,
   `#[serde(rename_all="snake_case")]` + strum). Custom workflows need custom
   events, but today every variant must be added in core and recompiled. The
   wire format is *already* a snake_case string, so opening this up is
   backward-compatible. **#1 blocker.**
2. **Block registration is hardcoded** in `register_blocks()` (`main.rs:224`) —
   a static list of `engine.register(Box::new(...))`. No block can be added
   without editing that function. No dynamic loading.
3. **Two hidden couplings blocks depend on:** the hardcoded `is_span_opener()`
   `matches!` list (`event.rs:406`) — a custom `*Requested` event silently
   won't open a trace span — and the gateway traits (`ShellGateway`,
   `ScannerGateway`, `AgentGateway` in `gateway.rs`), the I/O shell blocks need
   but which lives inside `foundryd`, not in the published contract.

## Proposed layering

```
foundry-sdk      ← THE CONTRACT (published, semver'd). What a block author imports.
  ├─ TaskBlock trait, TaskBlockResult, BlockKind, RetryPolicy
  ├─ Event, EventType (made OPEN), Throttle, Scatter/GatherSpec
  ├─ gateway traits (ShellGateway, AgentGateway, ScannerGateway) — moved up from foundryd
  ├─ payload helpers + the task_block_meta!/parse_payload! macros
  └─ a BlockRegistration descriptor (name, sinks_on, opens_span, factory)

foundry-engine   ← THE MECHANISM (depends on sdk). Dispatch loop, scatter/gather,
                    retry, tracing stamper. Knows nothing about specific blocks.

foundry-blocks   ← THE BATTERIES (depends on sdk). Today's 38 blocks. A consumer of
                    the SDK like any third party — proves the SDK is sufficient.

foundryd         ← THE HOST. gRPC server, scheduler, persistence, and the wiring
                    that assembles engine + whichever block sets are enabled.

foundry-cli      ← unchanged.
```

`foundry-core` effectively becomes `foundry-sdk` (rename + re-export shim for
one release). The load-bearing moves: gateways and the registration descriptor
migrate *up* into the contract, and the 38 blocks become an *ordinary downstream
consumer* — the moment they compile against only `foundry-sdk`, a third party
provably can too.

## Progress log

Commits land directly on `main` (trunk-based), each green through
`fmt` / `clippy --all-targets -D warnings` / full test suite.

- ✅ **Step 2 — open `EventType`.** `Custom(String)` variant via strum
  `#[strum(default)]`; hand-rolled serde keeps the wire format a bare
  snake_case string (byte-identical); unknown strings → `Custom`. `as_str()`
  now returns `String`. Round-trip + wire-stability tests added.
- ✅ **Step 1 — rename `foundry-core` → `foundry-sdk`** with a `foundry-core`
  shim crate (`pub use foundry_sdk::*;`) so existing `use foundry_core::…`
  paths keep compiling.
- ✅ **Coupling point #3 — gateway contract into the SDK.** `ShellGateway`,
  `ScannerGateway`, `AgentGateway` traits + their data types
  (`CommandResult`, `AuditResult`, `Vulnerability`, `Agent*`) now live in
  `foundry_sdk::gateway`. In-memory fakes ship in
  `foundry_sdk::gateway::fakes` behind a `test-support` feature. Production
  impls stay in the host; old paths preserved via re-exports.
- ✅ **`span_context` → SDK.** Shared by both engine and block layers; moved
  to `foundry_sdk::span_context` (SDK gained a `tokio` dependency).
- ✅ **Step 4 — `foundry-engine` extracted.** `engine` + `event_writer` +
  `gather_store` now in their own crate depending only on `foundry-sdk`.
  Engine mechanics tests (local stubs) stay in-crate; the real-block
  remediation tests relocated to `foundryd::engine_remediation_tests`
  (`#[cfg(test)]` module — avoids the engine↔blocks dev-dep cycle).

### Remaining

- ⏳ **Step 5 — extract `foundry-blocks`.** Runbook below.
- ⏳ **Step 3 — declarative span-openers** (fold into step 6 wiring).
- ⏳ **Step 6 — `BlockRegistration` descriptor** replacing the hand-edited
  `register_blocks()`.

### Step 5 runbook (foundry-blocks)

Modules to move into `crates/foundry-blocks/src/`: `blocks/` (all ~38 block
files), `shell.rs`, `scanner.rs`, `agent_stream.rs`, `gateway.rs` (the
production impls), `gate_runner.rs`, `gate_file.rs`, `summary.rs`,
`charter.rs`, `trace_writer.rs`. Leave in `foundryd`: `main`, `service`,
`scheduler`, `trace_store`, `workflow_tracker`, `legacy_event_check`,
`orchestrator` (host-level fan-out; only uses `foundry_core`), `proto`.

**Churn-minimizing trick:** have `foundry-blocks` depend on the `foundry-core`
*shim* and keep the same `crate::`-relative module layout. Then
`foundry_core::…` paths and `crate::shell` / `crate::scanner` references need
no rewrite. Provide a `crate::gateway` module that re-exports the SDK traits
*and* houses the moved impls:
`pub use foundry_core::gateway::*;` + the `ProcessShellGateway` /
`ClaudeAgentGateway` definitions. Add `foundry-sdk` (test-support) as a
dev-dependency and `#[cfg(test)] pub use foundry_sdk::gateway::fakes;` so block
tests keep `crate::gateway::fakes`.

**Cross-crate test relocation:** the chain test files
(`iterate_chain_test`, `maintain_chain_test`, `prompt_chain_test`,
`release_chain_test`, `strategic_chain_test`) and `test_helpers` use
`foundry_engine::Engine` *and* real blocks → relocate to `foundryd` as
`#[cfg(test)]` modules (same pattern as `engine_remediation_tests`), importing
`foundry_blocks::…` + `foundry_engine::engine::Engine`.

**Host rewiring:** `foundryd` depends on `foundry-blocks`; repoint
`crate::blocks` → `foundry_blocks::blocks` (etc.), drop the moved `mod`
declarations, and update `register_blocks()` in `main.rs`. Acceptance test:
`foundry-blocks` compiles with only `foundry-core`/`foundry-sdk` +
`foundry-engine` (dev) in scope — proving a third party can build blocks
against the SDK alone.

## Phase 1 — Carve out the SDK, prove it with our own blocks

1. **Birth `foundry-sdk`.** Rename/re-export `foundry-core` → `foundry-sdk`
   (keep `foundry-core` as a thin re-export for one release to avoid a flag-day).
   Move the gateway traits and a `BlockRegistration` descriptor up into it.
   Host-only types (registry persistence, trace store) stay out.
2. **Open `EventType`.** Add `Custom(String)` backed by the existing snake_case
   wire format (or a string newtype with `const` built-ins). Non-breaking. Add a
   round-trip test: `Custom("foo_happened")` ⇄ `"foo_happened"`.
3. **Make span-openers declarative.** Delete the hardcoded `is_span_opener()`
   `matches!`. The host builds the span-opener set at startup from each
   registered block's `opens_span` flag. Custom `*Requested` events open spans
   for free.
4. **Extract `foundry-engine`.** Move `engine.rs` + scatter/gather/retry/tracing
   stamper into its own crate depending only on `foundry-sdk`. Mostly a file
   move — the engine already has no `EventType` match.
5. **Demote the 38 blocks to a downstream consumer.** Move `blocks/` +
   `orchestrator.rs` into `foundry-blocks` (later splittable into `-quality`,
   `-release`, `-git`). Depends on `foundry-sdk` only. **When this compiles, the
   SDK is provably sufficient — that's the Phase 1 acceptance test.**
6. **Registration becomes data, not a hand-edited function.** Replace
   `register_blocks()`'s static list with collection of `BlockRegistration`
   descriptors (explicit builder per block-crate, or the `inventory` crate for
   auto-collection). Adding a block = adding a crate + a feature, never editing
   `main.rs`.

**Result:** `foundry-sdk` / `foundry-engine` / `foundry-blocks` / `foundryd` /
`foundry-cli`. Contributors write a crate against `foundry-sdk`, enable a
feature, rebuild. In-process, type-safe, no protocol yet. Quality gates stay
green at every step (mostly mechanical relocation).

## Phase 2 — gRPC block workers (additive, later)

- Extend `proto/foundry.proto` with a block-worker protocol: a worker opens a
  bidirectional stream, sends `RegisterBlock { name, sinks_on, opens_span }`,
  then the daemon dispatches `BlockDispatch { trigger_event }` and the worker
  replies `BlockResult` (wire form of `TaskBlockResult`, scatter included).
- In the host, implement a `RemoteBlock` that *is* a `TaskBlock` — its
  `execute()` forwards the trigger over the stream and awaits the reply. The
  engine, scatter/gather, and retry are unchanged because `RemoteBlock`
  satisfies the same trait. This is why the Phase 1 boundary matters.
- Scatter/gather already crosses async boundaries via events, so it survives the
  process hop. New concern: dispatch timeout / worker liveness, which maps onto
  the existing `RetryPolicy`.
- Capability-scoping the gateways is deferred (trusted contributors). If Foundry
  ever opens to a public ecosystem, workers are where sandboxing lands — they
  already give crash isolation and a natural place to scope gateway access.

## Watch-outs

- **`Custom(String)` ergonomics:** keep built-in events strongly typed; only
  custom ones go stringly. A newtype with `const` associated values for built-ins
  gives both. Spike this before committing.
- **Docs + skill sync:** `book/` and `skill/foundry/` encode the "closed enum,
  edit core" mental model — update in lockstep (AGENTS.md doc-sync rule).
- **Don't over-split block crates up front (YAGNI).** One `foundry-blocks` crate
  proves the contract; split by domain only when a second consumer or build-time
  reason appears.
</content>
</invoke>
