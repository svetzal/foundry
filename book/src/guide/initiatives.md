# Initiatives

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
owner question cannot advance. A member's `after` prerequisite must have
landed a cycle or completed before that member can advance. Queued formation,
reserved dispatches and standalone initiative tasks count toward the active cap.
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
| `dispatch` | Start one concrete initiative-owned task in a registered project |
| `hold` | Wait for explicit member milestones |
| `relay` | Record a finding for the selected members |
| `ask` | Open one shared owner question |
| `escalate` | Stop for owner attention with a reason |

The assessment receives the outcome, brief identity, member results, findings,
questions, owner decisions and recent trunk evidence. Its provider is the
initiative's formation provider, at balanced tier and medium effort.
`initiative_steered` retains the exact prompt, provider and typed decision.

Only one assessment can run per initiative. Store changes or a changed trunk
invalidate stale output. Foundry releases the store guard during repository
reads and agent execution. Durable action receipts prevent replay from starting
work or spending cycles twice. Pending actions recover after daemon restart.
Hold, relay, ask and escalate do not repeatedly wake an idle initiative.

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

## Record an owner decision

A decision applies to all campaign members unless `--member` selects a subset.
It reaches active members as well as escalated members.

```bash
foundry initiative decide shared-contract --decision "Keep port 5001"
foundry initiative decide shared-contract \
  --decision "Retain the legacy adapter" --member adopt-contract
```

Foundry records the decision on the initiative and each selected campaign.
It closes the questions that decision answers for those members. Formation
that was running when the decision arrived discards its stale outcome and
forms again from the current record. It does not overwrite the decision.

Use `--add-cycles N` with `initiative decide` to authorize more shared cycles.
Member grants stay within that total. Raising an individual campaign budget
cannot authorize an initiative to exceed its shared budget.

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
their intent references. Foundry never runs `et close` in slice 1.

## Complete or cancel

Complete an initiative after the owner affirms its outcome.

Completion stops unfinished member campaigns gracefully and clears their owner
questions. Those campaigns become cancelled because the overall outcome is
verified; their individual missions are not recorded as complete. Members that
are already complete or cancelled keep their status. Running cycles can settle
and preserve their work, while queued cycles stop.

```bash
foundry initiative complete shared-contract \
  --reason "Both sides passed the contract review"
```

Cancel an initiative when the owner stops the work:

```bash
foundry initiative cancel shared-contract --reason "The shared contract was withdrawn"
```

Cancellation stops its member campaigns gracefully. Running cycles can settle
and preserve their work. It does not discard preserved branches.

Slice 2 adds integration gates, general campaign staging and outcome evaluation,
including Tracker closure. Slice 3 adds question aging and dashboard controls.
Development stays on the Mac's synthetic daemon. Promotion to ops-01 requires
Stacey's go-ahead.

See [campaigns](campaigns.md), [the work queue](work-queue.md) and
[initiative events](../reference/event-types.md#initiatives-and-findings).
