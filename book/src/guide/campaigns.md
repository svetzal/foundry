# Tasks and Campaigns

Foundry has two engineering dispatch primitives:

- A **task** executes one concrete objective immediately.
- A **campaign** holds a broader mission and derives one task at a time from
  current repository state until evidence proves the mission complete.

Campaigns do not contain a pre-cut queue. The next objective is created only
when the preceding task has a typed result, so stale downstream inventory and
per-item retry loops are unnecessary.

The daemon owns the durable campaign inventory. Online
`foundry campaign add/list/show/advance/pause/resume/decide/complete` all go
through typed gRPC and do not read, create, or mutate
`FOUNDRY_CAMPAIGNS_PATH`. Successful online reads and mutations render the
daemon's typed response directly, so stale client-side campaign files cannot
mask the live daemon-owned state. Pass `--offline` only for direct-file
recovery while the daemon is stopped. If the daemon is unreachable, the online
command fails and leaves any absent or pre-existing client-side
`FOUNDRY_CAMPAIGNS_PATH` byte-identical.

The read-only inventory surface starts with two gRPC queries:

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

Two verdicts may fast-forward the registered trunk branch. A `complete` verdict
with passing required gates lands, as does a `remainder` — the reviewer's term
for a finite list of missing work on a *converging* implementation — provided at
least one required gate ran and every required gate passed. Converging work
integrates rather than accumulating a long-lived divergent branch, and the green
required gates are what keep trunk from going red; a `remainder` with no
required gate to vouch for it does not land. Its reviewer gaps travel forward in
the typed result and become the campaign's next objective.

`defect`, `blocked_on_decision`, and `runner_error` never land. Every result
that does not land is pushed to a named preservation branch; if no remote push
is possible, Foundry writes a Git bundle under `~/.foundry/preserved/`. Either
way the next cycle resumes from the preserved work; a bundle also carries `HEAD`,
so you can fetch or clone it by hand to recover the work yourself. Tasks do
not retry. A campaign decides whether the preserved result should seed another
objective.

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

`intent_refs` are opaque trace references. `context_paths` must be existing
repository-relative files under the registered checkout: absolute paths,
parent-directory traversal, missing files, and paths that resolve outside the
checkout are rejected by `foundry campaign add` before the definition is saved.
Foundry reads these neutral artifacts but does not invoke the system that
produced them. This means an accepted campaign definition will not later fail
formation solely because a declared context file never existed. A gate may
declare repository-relative `artifacts`; every declared path must exist before
its command is eligible to pass. This prevents test runners that silently
ignore missing file arguments from producing false-green campaign evidence. At
least one `done_evidence` item is required. The default cycle budget is 20 when
`budget` is omitted.

## Managing a Campaign

```bash
foundry campaign add ./parite-phase-2d.json
foundry campaign list
foundry campaign show parite-phase-2d
foundry campaign advance parite-phase-2d
foundry campaign pause parite-phase-2d
foundry campaign decide parite-phase-2d --decision "Use the generated tonic client path."
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

Task results auto-request the next advance. A result that did not land carries
its preserved branch into the next task, so the campaign resumes warm. A
`blocked_on_decision` or `runner_error` escalates immediately. `cycles_completed`
counts dispatched tasks, while `cycles_landed` counts only task results whose
work actually landed on trunk.

Formation reasons about two trees, because they can differ. The live repository
snapshot is the delivered trunk state and is what a `done` decision is judged
against. Separately, when the previous cycle did not land, its preserved branch
becomes the next execution's base ref — so formation is also shown an
`ACCUMULATED UNMERGED WORK` section listing the commits and changed files
reachable from that ref but absent from trunk. An `advance` objective is cut
from mission minus trunk-plus-accumulated. Without this the agent inspects only
trunk and re-cuts objectives the preserved branch already satisfied.
When the final budgeted task lands, Foundry still evaluates the repository for
completion. Only a decision to dispatch another task is converted to a budget
escalation.

Pausing prevents automatic or manual advancement. If an already-running task
finishes after the pause, Foundry records its result without changing the
paused state. The typed result and preservation ref remain pending in the
campaign store; the next manual advance after resume consumes them, so formation
sees the exact reviewer gaps and the task continues from preserved work.

Resume is valid for both `paused` and `escalated` campaigns. When an escalation
is budget-only (the engine stopped because the cycle limit was reached but no
human judgment question was recorded), `resume` is the right command — it
returns the campaign to `active` without requiring an owner-decision record:

```bash
foundry campaign resume parite-phase-2d
```

`resume` requires `authorized_by` to be set and will not silently reactivate
an exhausted campaign. When `cycles_completed >= max_cycles`, pass
`--add-cycles N` to explicitly authorize more work; the engine rejects `resume`
without an extension on an exhausted budget:

```bash
foundry campaign resume parite-phase-2d --add-cycles 1
```

Completion and escalation are terminal events and are forced into the next ops
digest as an anomaly. Campaign-store mutations are serialized across the CLI and
daemon, so a control command cannot overwrite an in-flight formation decision.
If a daemon-side save fails during `pause`, `resume`, `decide`, or `complete`,
the RPC returns `INTERNAL`, leaves the persisted daemon-owned store
byte-identical, and `complete` does not emit `CampaignCompleted`.

When a task escalates with a human judgment question, record the owner's policy
before the next advance:

```bash
foundry campaign decide parite-phase-2d \
  --decision "Keep the generated tonic client boundary; do not add raw JSON shims."
```

`decide` is valid only for an `escalated` campaign. It appends an owner
decision record with the decision text, the campaign's `authorized_by` value,
and a timestamp, then returns the campaign to `active`. Subsequent formation
prompts include every recorded owner decision as binding context, so the next
advance can continue under explicit policy instead of re-escalating on the same
question.

By default, `foundry campaign decide` is an online mutation: it requires a
reachable `foundryd` daemon and sends the decision through the
`DecideCampaign` RPC. If the daemon is unreachable, the command fails and does
not touch the client-side `FOUNDRY_CAMPAIGNS_PATH`.

If you need to update the file while the daemon is stopped, opt into the direct
store path explicitly:

```bash
foundry campaign decide parite-phase-2d \
  --decision "Keep the generated tonic client boundary; do not add raw JSON shims." \
  --offline
```

### External completion

When production or other owner-reviewed evidence proves the mission shipped
without another formation cycle, close the campaign explicitly:

```bash
foundry campaign complete parite-phase-2d \
  --reason "Production verification confirms every required outcome shipped."
```

This is an owner-authorized terminal transition. Foundry retains the reason and
timestamp, clears any stale pending result, and emits the same completion event
used by an internally completed campaign. Use `--offline` only while the daemon
is stopped; the direct-file path cannot emit the terminal event.

### Online and offline control

By default, every campaign control command is daemon-authoritative:

- `add` sends the JSON definition through `AddCampaign`; the daemon validates
  the referenced project and context paths against daemon-owned registry state,
  persists atomically, and returns the durable `CampaignDetail` that the CLI
  renders directly.
- `list` renders `ListCampaigns` directly.
- `show` renders `GetCampaign` directly.
- `advance` dispatches `AdvanceCampaign`, prints the returned root event ID,
  watches the workflow, and then renders the trace from that daemon-owned
  event.
- `pause`, `resume`, `decide`, and `complete` render the typed
  `CampaignDetail` returned by their respective RPCs.

Without `--offline`, an unreachable daemon is always an error. The CLI does
not warn and fall back to direct file mutation automatically.

If `foundryd` is not running, pass `--offline` to opt into direct-file
recovery:

```bash
foundry campaign add ./parite-phase-2d.json --offline
foundry campaign list --offline
foundry campaign show parite-phase-2d --offline
foundry campaign pause parite-phase-2d --offline
foundry campaign resume parite-phase-2d --offline
foundry campaign resume parite-phase-2d --add-cycles 1 --offline
foundry campaign decide parite-phase-2d --decision "Keep the daemon boundary." --offline
foundry campaign complete parite-phase-2d --reason "Production verification confirms every required outcome shipped." --offline
```

The offline path reads or mutates the file directly and cannot emit workflow
events. Restart `foundryd` afterward so its in-memory state is refreshed from
disk before you resume normal online control.

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
