# Tasks and Campaigns

Foundry has two engineering dispatch primitives:

- A **task** executes one concrete objective immediately.
- A **campaign** holds a broader mission and derives one task at a time from
  current repository state until evidence proves the mission complete.

Campaigns do not contain a pre-cut queue. The next objective is created only
when the preceding task has a typed result, so stale downstream inventory and
per-item retry loops are unnecessary.

The daemon exposes the durable inventory through two read-only gRPC queries:

- **`ListCampaigns`** — returns summary/status records sorted by campaign name,
  with an optional exact `project` filter. Missing or empty stores return an
  empty list; malformed or unreadable stores return a gRPC error rather than an
  implicit empty inventory.
- **`GetCampaign`** — retrieves the complete definition of one campaign by exact
  name, including `intent_refs`, `context_paths`, all `done_evidence` entries
  with the `Gate`/`Review` type distinction preserved, and escalation rules.
  Returns `NOT_FOUND` (not an implicit empty) when the name is absent from the
  store.

## One-Shot Tasks

```bash
foundry task parite-cli "Add a --quiet flag and prove it suppresses progress output"
foundry task parite-cli "Fix the parser regression" --agent codex
```

The task formation:

1. Creates a disposable Git worktree under `~/.foundry/worktrees/`.
2. Runs the coding agent and quality gates inside that worktree.
3. Performs a read-only skeptical review against the objective and evidence.
4. Emits one typed verdict: `complete`, `remainder`, `defect`,
   `blocked_on_decision`, or `runner_error`.
5. Commits all task work before returning a terminal result.

Only a `complete` verdict with passing required gates may fast-forward the
registered trunk branch. Every non-complete result is pushed to a named
preservation branch; if no remote push is possible, Foundry writes a Git bundle
under `~/.foundry/preserved/`. Tasks do not retry. A campaign decides whether
the preserved result should seed another objective.

## Campaign Definitions

Create a JSON definition file:

```json
{
  "name": "parite-phase-2d",
  "project": "parite-cli",
  "mission": "Prove both retrieval entrypoints preserve raw response identity.",
  "intent_refs": ["parite.intent.raw-retrieval-evidence"],
  "context_paths": [".alloy/projections/AGENTS.generated.md"],
  "done_evidence": [
    {
      "kind": "gate",
      "command": "cargo test -p parite-core retrieval_parity",
      "required": true,
      "artifacts": ["crates/parite-core/tests/retrieval_parity.rs"]
    },
    {
      "kind": "review",
      "statement": "The parity suite compares unmasked response IDs across both real entrypoints."
    }
  ],
  "budget": { "max_cycles": 12 },
  "escalation": ["A behavior choice requires an owner decision."],
  "authorized_by": "Stacey",
  "agent_provider": "codex"
}
```

`intent_refs` are opaque trace references. `context_paths` must be
repository-relative files and cannot escape the registered checkout. Foundry
reads these neutral artifacts but does not invoke the system that produced
them. A gate may declare repository-relative `artifacts`; every declared path
must exist before its command is eligible to pass. This prevents test runners
that silently ignore missing file arguments from producing false-green campaign
evidence. At least one `done_evidence` item is required. The default cycle
budget is 20 when `budget` is omitted.

## Managing a Campaign

```bash
foundry campaign add ./parite-phase-2d.json
foundry campaign list
foundry campaign show parite-phase-2d
foundry campaign advance parite-phase-2d
foundry campaign pause parite-phase-2d
foundry campaign resume parite-phase-2d
# When the cycle budget was exhausted, explicitly authorize more work:
foundry campaign resume parite-phase-2d --add-cycles 1
```

New definitions start as `staged`. An authorized staged campaign becomes
`active` on its first advance. Each advance re-runs mechanical done-evidence,
reviews the repository and context artifacts, then makes exactly one decision:

- `done` — all required gate and review evidence is satisfied.
- `advance` — dispatch exactly one next objective from mission minus current
  state.
- `escalate` — stop because the budget, an escalation rule, runner failure, or
  owner judgment requires attention.

Task results auto-request the next advance. A `remainder` or `defect` carries
its preserved branch into the next task, so the campaign resumes warm. A
`blocked_on_decision` or `runner_error` escalates immediately. `cycles_completed`
counts dispatched tasks, while `cycles_landed` counts only task results whose
work actually landed on trunk.
When the final budgeted task lands, Foundry still evaluates the repository for
completion. Only a decision to dispatch another task is converted to a budget
escalation.

Pausing prevents automatic or manual advancement. If an already-running task
finishes after the pause, Foundry records its result without changing the
paused state. The typed result and preservation ref remain pending in the
campaign store; the next manual advance after resume consumes them, so formation
sees the exact reviewer gaps and the task continues from preserved work. Resume
is allowed only when `authorized_by` is present. A campaign
that exhausted its budget also requires `--add-cycles N`; this makes the owner's
additional work authorization explicit. Completion
and escalation are terminal events and are forced into the next ops digest as
an anomaly. Campaign-store mutations are serialized across the CLI and daemon,
so a control command cannot overwrite an in-flight formation decision.

## Dry Run

Campaign advancement honors event throttle. A dry-run
`campaign_advance_requested` simulates the next objective without mutating the
campaign store or repository. It executes exactly one simulated task through
review and terminal result, then stops without recursively auto-advancing:

```bash
foundry emit campaign_advance_requested \
  --project parite-cli \
  --throttle dry_run \
  --payload '{"campaign":"parite-phase-2d"}' \
  --wait
```
