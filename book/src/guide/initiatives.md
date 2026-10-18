# Initiatives

For a concrete example, start with
[Deliver one outcome across repositories](../stories/initiative.md).

An initiative holds one outcome shared by several campaign missions. It keeps
owner decisions, findings and a binding brief in one place. Each campaign
still derives its own next objective from the repository and the last result.

Add existing campaigns as members, or derive staged candidates from Tracker.
An explicit `foundry initiative advance` enables steering. The steering agent
reads the current evidence and chooses one next action. Foundry enforces the
shared cycle budget, active member limit and questions before admitting work.

Existing definitions remain passive until you enable steering. Passive members
still advance with `foundry campaign advance`.

## Add an initiative

Add the member campaigns first. Register each campaign's project.
Include that project in the initiative's `projects` list.
A campaign can belong to only one initiative.

Save this definition as `initiative.json`:

```json
{
  "name": "shared-contract",
  "outcome": "The producer and consumer use the same contract",
  "intent_refs": ["et:goal:shared-contract"],
  "projects": ["producer", "consumer"],
  "members": [
    { "kind": "campaign", "name": "publish-contract" },
    {
      "kind": "campaign",
      "name": "adopt-contract",
      "after": ["publish-contract"]
    }
  ],
  "done_evidence": [
    {
      "kind": "review",
      "statement": "The consumer accepts the producer's contract"
    }
  ],
  "budget": {
    "max_cycles": 20,
    "max_active_members": 2,
    "max_open_questions": 3
  },
  "authorized_by": "Stacey",
  "agent_provider": "codex",
  "providers": {
    "formation": "claude",
    "execution": "codex",
    "review": "claude"
  }
}
```

Run these commands:

```bash
foundry initiative add initiative.json
foundry initiative list
foundry initiative show shared-contract
foundry initiative advance shared-contract
```

`show` prints the outcome, member status, cycles used and landed, the latest
verdict and reviewer gaps. It lists each open question once, with its
applicable members and an answer command. `list` and `show` accept `--json`.
The response includes the latest steering record and a total history count.
The store and typed event history retain all assessment prompts and decisions.

`max_cycles` counts dispatched cycles across all members. The initiative owns
member budgets. `max_active_members` defaults to two. A member with an open
owner question cannot advance. A bare member in `after` must have completed
its campaign outcome before the dependent can advance. A partial landing does
not satisfy it. Queued formation,
reserved dispatches, running cycles and standalone initiative tasks count
toward the active cap. A completed or cancelled member counts only while one of
its cycles still runs, for example after a graceful cancellation. Its last
cycle without a recorded result, a reservation left behind, or a queued or held
item never counts. Cancelling a queued cycle or formation with `queue cancel` or
`campaign cancel` releases the slot it reserved.
The engine checks repository ownership before starting another mutation.

Initiatives and findings use the campaign store API and its existing lock.
Online commands use typed gRPC and daemon-owned state. An unavailable daemon
is an error. Use `--offline` only for recovery with the daemon stopped.
The current storage file is `~/.foundry/campaigns.json`.
Integrations must use the store API rather than depend on its file shape.

## Steer the initiative

Request one assessment, or pin it to an eligible member:

```bash
foundry initiative advance shared-contract
foundry initiative advance shared-contract --member adopt-contract
```

The command returns the submitted event identity. Inspect the initiative and
queue while work proceeds. Once enabled, steering runs again after member
settlement and repository cleanup. It can select another member or pause for
an owner decision. A passive initiative keeps its existing campaign behaviour.

The read-only agent chooses exactly one typed action:

| Action | Effect |
| --- | --- |
| `advance` | Advance an eligible member, assigning a project to a candidate |
| `dispatch` | Start one initiative-owned task in a registered project |
| `hold` | Wait for explicit member milestones |
| `relay` | Record a finding for the selected members |
| `ask` | Open one shared owner question |
| `stage` | Add a campaign for work the existing members do not cover |
| `done` | Propose completion after the outcome checks pass |
| `escalate` | Stop for owner attention with a reason |
| `wait` | Record why nothing should move until running work settles |

A `wait` requires a reason. Foundry records it and changes nothing else:
it opens no question and does not change the initiative status. Use `ask`
only for a question the owner must answer. Waiting for workers to finish is a
`wait`, not a question.

Some decisions are already in force. A `hold` on a member that has an owner
hold keeps the owner hold. A `relay` to members that a finding already reaches
adds no recipient. Foundry records these decisions as no-ops and changes
nothing else.

The assessment receives the outcome, brief identity, member results, findings,
questions, owner decisions, integration results and recent trunk evidence. Its
provider is the
initiative's formation provider, at balanced tier and medium effort.
`initiative_steered` retains the exact prompt, provider and typed decision.

The steering prompt has a limit of 65,536 bytes. Foundry puts the packet in
the prompt when the complete prompt, fixed text included, fits that limit.
When it does not fit, Foundry writes the packet to a file under
`initiatives/steering-evidence/` beside the campaign store. The prompt then
gives the file's path and SHA-256 hash. Foundry refuses the steer only when the
prompt does not fit even with the file reference. It never truncates owner
policy.

### Check eligibility before the agent runs

Foundry computes the steering options in code before it asks the agent.
The packet's `eligibility` section lists the members that can advance now.
It lists every other member with its reason: owner hold, unmet milestone or
prerequisite, open question, no free slot, spent budget or busy repository.
It also lists the members that use the active-member slots, and it says
whether a new task can start.

An automatic assessment does not start an agent when no action can apply:

- No member can advance.
- No task can start.
- At least one member uses a slot, so its settlement requests the next assessment.
- No finding that can reach more members, and no owner decision, arrived after
  the previous steering decision.

Foundry records `initiative_steered` with outcome `skipped` and the reason.
An owner's `initiative advance` request always starts an assessment.

The agent can choose `wait` while members are eligible, when the mission
requires it. Foundry records its reason beside the eligible list.

### How each assessment settles

Each assessment is a work item. Its settled state shows whether a person must act:

| Outcome | Work item | Meaning |
| --- | --- | --- |
| `applied` | `landed` | Foundry recorded the decision and requested its action |
| `waited` | `landed` | The agent chose `wait`; the reason is on the fact |
| `no_op` | `landed` | The decision was already in force |
| `skipped` | `landed` | No action could apply, so no agent started |
| `rejected` | `cancelled` | A guard refused the decision |
| `stale` | `landed` | Evidence changed; a fresh assessment was requested |
| `failed` | `failed` | The agent failed, its output was unusable, or Foundry state could not be read or written |

A guard refusal is not work for a person. Foundry refuses a held member, a full
active-member limit, a busy repository or a project outside the initiative.
It also refuses a member other than the one the owner pinned, and invalid audit
evidence. The refused decision has no effect.

The work item settles `cancelled` with the reason
`steering rejected: <guard message>`. The fact keeps the guard's message in
`error`. `foundry queue open` lists only `failed` assessments.

Only one assessment can run per initiative. Store changes or a changed trunk
invalidate stale output. Foundry releases the store guard during repository
reads and agent execution. Durable action receipts prevent replay from starting
work or spending cycles twice. Pending actions recover after daemon restart.
A wait does not wake the initiative again.

### Wake steering after a member settles

A member cycle result and an initiative task request the next assessment when
they settle. Repository cleanup and ledger accounting occur first.

A member advance can also settle without a cycle. The member can escalate,
stand down or finish. An advance that a steer started can also start a cycle.
Then Foundry calculates the eligibility again. The work item of the advance and
its slot reservation are not in this calculation. Foundry requests an
assessment when these two conditions are true:

- At least one member can advance.
- The member of the advance cannot advance at this time.

A member with a cycle in the queue or in progress uses a slot, so it cannot
advance. Thus each advance from a steer requests the next assessment, until all
slots are in use or no member can advance. Then the members advance in
parallel, up to `max_active_members`. When another path starts a cycle, the
member waits for the result of that cycle.

If the member of an advance can still advance, the advance changed nothing.
Foundry does not request an assessment for it, because steering could choose
the same advance again and again. A failed advance does not request an
assessment. If an assessment of the initiative waits in the queue or runs, the
request joins it.

The `escalated` status does not stop steering. It shows that the owner has an
open question. A member that the question applies to cannot advance. Other
members can advance. A full set of open questions does not stop steering either.
Steering can advance a member that has no open question.

### Follow up a step that dispatches nothing

A steering `hold`, `relay`, `ask`, `escalate` or `stage` changes state but
starts no work. A `hold` or `relay` that is already in force (`no_op`) starts no
work either. After one of these steps, Foundry calculates the eligibility again.
If at least one member can advance, Foundry requests exactly one follow-up
assessment. Without it, the eligible member waits until the next settled fact.

The follow-up is an ordinary `initiative_advance_requested` through the work
ledger. Pacing, the precheck and the one-assessment rule apply to it. If an
assessment of the initiative is already queued or running, Foundry does not add
another. A follow-up requested while its own steer still runs starts after that
steer ends.

The request carries `follow_up_of`, with the earlier steer's id and its step.
The follow-up's steering record and its `initiative_steered` fact keep the same
cause, so `initiative show` and the events show the chain. A follow-up pass
never requests another follow-up, whatever it decides.

These outcomes never request a follow-up: `waited`, `skipped`, `rejected`,
`stale` and `failed`. A `stale` assessment requests a fresh assessment, which
keeps the follow-up cause. A `done` proposal waits for the owner and does
not request one either. An owner's direct hold or relay records its own fact,
and that fact already requests the next assessment.

An indefinite owner hold remains until an owner releases it. Pinning an
`initiative advance --member` releases that member's owner hold. A steering
agent cannot release it. Holds with milestones require explicit evidence;
an arbitrary milestone label is not satisfied by any landed cycle.

Owner overrides use the same admission checks:

```bash
foundry initiative hold shared-contract --member adopt-contract \
  --reason "Review the contract first"
foundry initiative hold shared-contract --member adopt-contract \
  --until publish-contract:contract-landed --reason "Wait for the contract"
foundry initiative milestone shared-contract --member publish-contract \
  --milestone contract-landed --evidence producer@a1b2c3d --evidence contract.md
foundry initiative relay shared-contract --finding fnd_contract_port \
  --member adopt-contract
foundry initiative ask shared-contract --question "Which port is binding?" \
  --member publish-contract --member adopt-contract
foundry initiative dispatch shared-contract consumer "Verify the contract example"
```

Record custom milestones only after checking their evidence. Built-in settlement
milestones retain actual landing or completion evidence. A meaningful owner
override during a running assessment is refused with a retry instruction;
it is never silently dropped.

## Readiness and dependencies

`initiative show <name>` reports readiness for each member. The JSON response
contains `readiness.members`, typed `blockers`, and each dependency's required
milestone, satisfaction and evidence. This is a read-only snapshot. Admission
checks the current state again before starting work.

Readiness means the next assessment or task is allowed under the current
controls. It does not mean the product is released, deployed or usable on a
particular host. Express those requirements as named evidence milestones.

Use the dependency that matches what the consumer actually needs:

| `after` entry | Required fact |
| --- | --- |
| `"producer"` or `"producer:completed"` | The producer campaign's whole outcome is completed |
| `"producer:landed"` | At least one producer cycle landed |
| `"producer:contract-published"` | Evidence was recorded for that exact named milestone |

A bare dependency no longer means the first landed cycle. Existing definitions
that intentionally start after the first landing must use `member:landed`.
Definitions keep the same JSON shape; Foundry does not rewrite their intent.
A custom milestone called `completed` cannot complete an active campaign.
Completed task receipts and current realised-intent audits also provide outcome
evidence for task and candidate members.

For example, a consumer that needs the released contract can declare:

```json
{
  "kind": "campaign",
  "name": "adopt-contract",
  "after": ["publish-contract:contract-published"]
}
```

Record that exact milestone with checked publication evidence. A different
milestone, a status note, or another landed cycle cannot satisfy it. Milestones
are recorded evidence declarations; Foundry does not infer publication or
refresh their external evidence automatically.

A hold with `until` ends when every declared prerequisite is satisfied. This
also applies when the owner set the conditional hold. An indefinite owner hold
still needs an explicit owner release. Agents cannot replace it. Unknown,
malformed, self-referencing and cyclic prerequisites are refused.

Dependency waits open no owner question, spend no cycle and require no decision
to clear. Existing settlement and milestone events wake enabled steering.
Unrelated eligible members can continue while a dependency waits. Questions,
indefinite holds, pauses, recovery and budgets still apply after it is satisfied.

Online readiness uses the daemon's current store, work ledger and registry.
It includes running and cleanup owners and unavailable projects. Offline
inspection reports `live: false`: it checks the store but does not claim the
live ledger or registry was checked. An older daemon that does not report
readiness is shown as unavailable, never as ready.

## Record an owner decision

Open the Sources page in ops-visualizer to review initiatives above campaigns.
Each initiative shows its outcome, member results and integration evidence.
Each shared question shows its age and applicable members.
Use the CLI below to answer an exact question.
Dashboard clients must use `RecordInitiativeDecision` with its ID in `question_ids`.
Older clients can still record policy, but cannot implicitly answer questions.

The dashboard keeps your draft and member selection when live results arrive.
If the daemon refuses a decision, the form shows its reason and retains your draft.

A decision applies to every member that can still act, unless `--member`
selects a subset. It reaches staged, active, paused and escalated members.
It skips completed and cancelled members, as `campaign decide` refuses them.

If `--member` names a completed or cancelled member, Foundry refuses the whole
decision and records nothing. The refusal names each such member.
If no member can still act, Foundry records the decision on the initiative only.
It then reaches no campaign and answers no member question.

```bash
foundry initiative decide shared-contract --decision "Keep port 5001"
foundry initiative decide shared-contract \
  --decision "Retain the legacy adapter" --member adopt-contract
```

Foundry records the decision on the initiative and each applicable campaign.
Without `--question`, the decision answers no questions. Formation
that was running when the decision arrived discards its stale outcome and
forms again from the current record. It does not overwrite the decision.

### Answer a specific question

Read `initiative show` to find the question ID.
Use `--question` to select it explicitly:

```bash
foundry initiative show shared-contract
foundry initiative decide shared-contract --question question_abc123 \
  --decision "Use port 5001"
```

Repeat `--question` to answer several questions with one decision.
Without `--member`, the answer applies to the selected questions' members.
Use `--member` to answer for only some of those members.
Other questions remain open, including questions for the same member.
Their IDs and original ages stay unchanged.

You can also answer deferred questions directly by ID.
Foundry refuses unknown IDs or unrelated member selections without changing state.

A member with another unanswered question stays blocked.
A policy decision or budget grant alone does not answer its questions.
Existing callers must supply question IDs when they intend to answer.
Updated CLI decisions use `RecordInitiativeDecision` for both policy and answers.
An older daemon refuses that RPC without changing state.
The CLI never falls back to the older decision RPC.

### Record a status note

Use a note for an observation about work already done:

```bash
foundry initiative note shared-contract --text "Mac verification landed; CI passed"
foundry initiative note shared-contract --text "Waiting for the current build" --json
```

Notes are durable observations, separate from binding owner decisions.
They appear in `initiative show` and later assessment context.
Recording a note does not answer questions, resume campaigns, release holds,
change budgets, or request a steering pass.
Notes do not invalidate an outcome proposal's policy evidence.
Use the appropriate control command to change authority or work state.

Use `--add-cycles N` with `initiative decide` to authorize more shared cycles.
When the shared budget is spent, `initiative decide` without `--add-cycles` is
refused and records nothing. A decision alone would leave a member that
escalated on budget still escalated, and the same question would open again.
`campaign decide` without cycles on an escalated, spent member is also refused,
and its message names the initiative that owns the budget. An active or paused
member records a `campaign decide` without cycles, as any campaign does. Member grants stay within that total. Raising an individual campaign budget
cannot authorize an initiative to exceed its shared budget.

### Recover an operational blocker

Operational recovery is separate from an owner decision.
A recovery record has an ID, a typed cause, the exact blocker and its time.
It does not grant authority or create an owner question.
Campaign and initiative reads expose the record in human and JSON output.

| Cause | What needs attention |
| --- | --- |
| `capability_unavailable` | A required platform or capability is missing |
| `access_unavailable` | Required access is unavailable |
| `environment_unavailable` | The execution environment cannot support the work |
| `missing_evidence` | Required proof is absent or not current |
| `unmet_precondition` | A named prerequisite has not been met |
| `runner_unavailable` | The runner or provider failed before useful assessment |

Inspect the blocker before requesting a recheck:

```bash
foundry campaign show port-upgrade --json
foundry campaign recover port-upgrade --recovery recovery_123 \
  --evidence "Required toolchain installed; version check passed"
foundry initiative recover shared-contract --recovery recovery_456 \
  --evidence "Integration fixture repaired; request fresh verification" --json
```

Use the exact current recovery ID.
Repeat `--evidence` to name several observations.
A recheck records evidence and requests fresh assessment under the existing
limits.
It does not mark the mission complete or turn failed gates green.
The same missing prerequisite can produce another recovery record.
A stale ID, an empty observation, or a terminal target changes nothing.
Recovery requires the daemon; it has no offline or decision fallback.
Older daemons refuse the new RPCs without mutation.

An agent can request this recheck after correcting an operational problem
within its existing authority. No new owner decision is needed for that step.
Recovery preserves owner questions, publication holds and cycle budgets.
It also preserves an explicit campaign pause.
A paused campaign records the evidence without dispatching an assessment.

A recovering campaign member cannot start another cycle until rechecked.
Other eligible initiative members can continue.
A completion attempt with unmet prerequisites records their exact reasons.
It does not open an "Outcome completion lacks evidence" question.
New settlement evidence can trigger another assessment.
Only fresh successful completion checks resolve missing-evidence or
precondition recovery automatically.
Capability and access blockers need evidence-backed rechecks.

Standalone task blockers retain their typed verdict and preserved work.
After correcting the prerequisite, use `queue resume <id>` to continue
that task.
Each task keeps its own blocker; concurrent task blockers do not overwrite
an initiative's outcome-recovery record.

### Keep owner attention bounded

`max_open_questions` limits the shared question list. The default is three.
Matching questions appear once with the union of their applicable members.
Their age starts at the earliest unanswered occurrence.
Answering one member does not reset the age for the others.

Extra distinct member questions remain on their campaigns.
The initiative reports how many questions are waiting for a slot.
After an answer frees a slot, the next waiting question appears.
A manual request for another distinct question is refused at the limit.

At the limit, steering can advance an eligible member that needs no owner answer.
If no member can advance, it waits for the owner.
The ops digest reports shared questions older than one day.
It also reports an initiative with no member activity for thirty days.
The inactivity check uses recorded creation and member activity times.
An older initiative with no known time stays unclassified
until member activity provides one.

`foundry queue open` shows an escalated initiative once and suppresses its
members' escalation rows beneath it. Open work items retain their own rows.
Use this source filter to see work the initiative caused:

```bash
foundry queue --source initiative:shared-contract
```

## Store the binding brief

Foundry owns the authoritative brief.

Omit `brief` from the initiative definition. Foundry rejects an external
brief object. Import its content with `brief set` after adding the initiative.

Run these commands:

```bash
foundry initiative brief set shared-contract BRIEF.md
foundry initiative brief show shared-contract
```

The stored path is `~/.foundry/initiatives/shared-contract/BRIEF.md`.
The record includes its SHA-256 hash. Formation, execution and continuation
receive the path and hash. Formation reads the brief as binding context.
`brief show` refuses content that differs from the recorded hash.

## Relay findings

A finding records information a member must see before its next objective.
Save this definition as `finding.json`:

```json
{
  "id": "fnd_contract_port",
  "initiative": "shared-contract",
  "from": { "kind": "owner", "id": "Stacey" },
  "to": ["adopt-contract"],
  "kind": "contract_changed",
  "text": "The producer now listens on port 5001",
  "refs": ["producer@a1b2c3d", "contract.md"],
  "consumed_by": []
}
```

Record and inspect it:

```bash
foundry finding add finding.json
foundry finding list shared-contract
foundry finding list shared-contract --member adopt-contract --json
```

The kinds are `contract_changed`, `regression`, `blocker`, `evidence` and
`owner_note`. An empty `to` list applies to every campaign member. Findings
remain in the record after consumption so their origin remains visible.

A landed cycle's reviewer gaps create findings automatically. A `defect`
verdict creates a finding with its diagnosis. Formation receives a bounded
"Findings from the initiative" section, newest first. Its task payload carries
the exact finding IDs and the bounded text. Campaign continuations and
`queue resume` also receive current findings.

Foundry consumes a finding for a member only when a cycle that saw it lands.
Formation alone, a preserved run and an unlanded verdict do not consume it.
Other applicable members keep seeing it until their own cycle lands.

## Choose providers by stage

Campaigns and initiatives accept this map:

```json
"providers": { "formation": "claude", "execution": "codex", "review": "claude" }
```

A member's explicit stage takes precedence over the initiative's stage.
An omitted stage uses `agent_provider`. Existing definitions keep working.
The task chain carries the map. Each block selects its own stage provider.
`foundry campaign report` keeps stage tokens separate for each provider.

Campaign execution accepts a provider only when it declares that it enforces
a writable root. Codex and Claude implement that boundary. Worktrees remain
under `~/.foundry/worktrees`.

For Claude, each invocation uses a private settings file, `dontAsk` permission
mode and the OS sandbox. The policy allows file edits only inside the selected
worktree. The invocation excludes user and project settings, hooks and MCP
servers. It disables permission bypass and unsandboxed subprocess retries.
Read-only formation and review allow writes only to temporary scratch space.
Sibling repositories and Foundry's state remain readable.

An initiative can explicitly alternate stage pairings:

```json
"provider_pairings": [
  { "formation": "claude", "execution": "codex", "review": "claude" },
  { "formation": "codex", "execution": "claude", "review": "codex" }
]
```

Explicit member stages still take precedence. A continuation keeps its admitted
pairing. Foundry does not alternate providers unless the definition requests it.

## Derive from Epilogue Tracker

Run derivation in the repository that supplies the Tracker intent:

```bash
foundry initiative derive --from et:goal:shared-contract --projects producer,consumer
foundry initiative derive --from et:journey:adopt-contract --projects producer,consumer
```

Add `--authorized-by <owner>` to record the authorizing owner. If omitted,
it defaults to the invoking user. Derivation records the initiative in
Foundry with staged candidates. It does not start member campaigns.
Add `--integration-gates <file>` to include shared checks during derivation.
The file contains a JSON array of gate definitions, as shown in
[Check the outcome across repositories](#check-the-outcome-across-repositories).
Foundry adds these gates to the reviews supplied by Tracker.
The file accepts only entries of kind `gate`.

Foundry reads a committed projection first. Supported paths are
`.et/projection.json`, `.alloy/projections/et.json` and
`.alloy/projections/epilogue.json`. Each projection has `goals`, `interactions`
and `journeys` arrays. Without a projection, Foundry invokes the read-only
`et list ... --all --json` commands from the repository root. The `et` CLI
loads its own repository configuration. Foundry does not read credentials.

The goal description and success criteria become the outcome. Interactions
in `create`, `update` or `delete` state become staged candidates in priority
order. Journey steps supply `after` order. Success criteria and interaction
descriptions become review statements. `intent_refs` retain the Tracker IDs.

Derivation retains the supplying repository and intent reference. The supplying
repository need not be a registered execution target. Audit and dispatch can
use only the initiative's registered projects.

The first advance audits each candidate against the registered codebases before
starting work. Findings retain checked paths and classify intent as `realised`,
`partial`, `absent` or `misplaced`. Realised intent produces a close proposal.
It does not start redundant work or write Tracker state.

Each later steer reads intent again. New approved intent becomes a candidate.
Withdrawn intent stops its owned work gracefully. Completed intent retains its
evidence and history. Existing findings and cycle counts remain attached to
their intent references. Tracker stays unchanged until the owner confirms completion.

## Check the outcome across repositories

For a changed boundary, both sides must verify the same versioned executable
contract set. Consumer tests state the behavior needed by the consumer.
Provider verification checks that same contract against the implementation.
Cover relevant errors, optional fields and supported old and new versions.
Matching schemas do not show that two services agree. Mocks that each side
writes independently of the other do not show it either. Derive cross-service
test doubles from the verified contract set.
Inside one repository, a test double at the gateway or transport boundary is
still valid proof (see [Test design](campaigns.md#test-design)).

Run contract verification in each project's required gates.
Include the contract version or hash, tested commits and both verification results
in the initiative's outcome evidence.
Record missing verification as unfinished work within the authorized repositories.
Use the brief, shared decisions and findings to coordinate the contract
and missing work.

Contract tests establish compatibility for their declared cases and versions.
Integration gates also check wiring and the user's full journey.
Foundry repeats a short contract reminder in initiative agent prompts,
beside the shared test-design rule.
This guidance does not add a contract verifier or a required evidence field.
Declared gates and outcome reviews provide the completion checks.

Add integration gates to the initiative's `done_evidence`:

```json
{
  "kind": "gate",
  "command": "python3 producer/scripts/check-contract.py consumer",
  "host": null,
  "required": true,
  "artifacts": ["producer/scripts/check-contract.py", "consumer/contract.json"]
}
```

Omit `host`, or set it to `null`, to use the daemon's host.
A named remote host uses SSH and that host's registered checkouts.
The daemon's host is ops-01 in production and the Mac during development.
The gate host needs Git, Python 3 and the Foundry CLI on its command path.
Named remote hosts also need a running Foundry daemon for registry and pacing checks.

Foundry fetches each registered trunk and permits only a fast-forward update.
It creates disposable copies beneath a shared directory.
Each copy uses its registered project name, such as `producer` or `consumer`.
The command runs from that shared directory.
Use paths relative to that directory for the command and required artifacts.

Foundry checks the artifacts before starting the command.
A missing artifact, failed checkout update or failed command cannot pass the gate.
Commands run through `foundry capture`, with stdout and stderr retained in
Foundry evidence.
The result records the host and exact trunk commits.
Gate failures create findings for the next assessment.

Foundry integrates only when the assessment holds every initiative trunk.
An assessment reserves every initiative repository through pacing, but only if
all of them are free.
It does not wait for these repositories.
Pacing decides when the assessment starts.
At that time, if all initiative repositories are free, the assessment holds all of them.

If other work uses one of these repositories when the assessment starts, the
assessment holds no initiative repository.
It starts next to that work, so another eligible member can start too.
That assessment does not fetch trunks or run gates, and it records no gate
result or finding.
Its steering record gives the reason in `integration_skipped`, and
`foundry initiative show` shows that reason.
Older gate results stay, but they do not show the current trunks.

Steering receives the gate results and reviews each outcome statement against
the trunks.
Passing contract gates on both sides proves compatibility for the tested contract.
Other outcome claims still need their declared evidence.
Changed trunks, intent or owner policy invalidate an earlier completion proposal.

## Add work uncovered by an integration check

An integration check can expose work that no member campaign covers.
Steering can choose `stage` to add a campaign within the initiative's registered
projects.
Foundry checks the campaign definition, ownership, providers and shared budget.
It records the new member without starting an execution cycle.
A later eligible advance starts its work through normal pacing.

Inspect staged work with these commands:

```bash
foundry initiative show shared-contract
foundry campaign list
foundry queue --source initiative:shared-contract
```

## Confirm completion

A `done` decision is a proposal for the owner.
Every required integration gate must pass.
Steering must affirm every review statement with supporting evidence.
Open questions and unfinished intent prevent completion.
The initiative remains open while it waits for the owner.

Read the proposal and its evidence:

```bash
foundry initiative show shared-contract
foundry initiative show shared-contract --json
```

Confirm the outcome with a reason:

```bash
foundry initiative complete shared-contract \
  --reason "Both sides passed the contract review"
```

Foundry checks that the evidence still matches the current trunks, intent and policy.
If it changed, request another assessment with `initiative advance`.
An explicit offline command cannot bypass outcome checks or change Tracker.

For a derived initiative, confirmation closes verified Tracker interactions
through `et close`.
It closes the goal after its children are in reality and its success criteria
are affirmed.
Foundry runs the Tracker CLI from the supplying repository.
It does not read or copy Tracker credentials.

Closure receipts record successful Tracker changes.
A failed closure leaves the initiative open and retains those receipts.
Retry the same completion command after resolving the reported failure.
A retry does not repeat a closure already confirmed by current Tracker state.
After a restart, the daemon also retries completion that the owner has already
confirmed. It checks the evidence again before each Tracker change.
If evidence changes after confirmation, use `initiative decide` to record the
new owner direction. Then request another assessment and confirm its proposal.

## Cancel the initiative

Cancel an initiative when the owner stops the work:

```bash
foundry initiative cancel shared-contract --reason "The shared contract was withdrawn"
```

Cancellation stops its member campaigns gracefully.
Running cycles can settle and preserve their work.
It does not discard preserved branches or close Tracker entities.

See [campaigns](campaigns.md), [the work queue](work-queue.md) and
[initiative events](../reference/event-types.md#initiatives-and-findings).
