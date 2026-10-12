# Initiatives

An initiative holds one outcome shared by several campaign missions. It keeps
owner decisions, findings and a binding brief in one place. Each campaign
still derives its own next objective from the repository and the last result.

Slice 0 has no steering agent. Add existing campaigns as members, record the
initiative, and advance the members with `foundry campaign advance`.
Foundry enforces the shared cycle budget and member limits at dispatch.

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
foundry campaign advance publish-contract
```

`show` prints the outcome, member status, cycles used and landed, the latest
verdict and reviewer gaps. It lists each open question once, with its
applicable members and an answer command. `list` and `show` accept `--json`.

`max_cycles` counts dispatched cycles across all members. The initiative owns
member budgets. `max_active_members` defaults to two. A member with an open
owner question cannot advance. A member's `after` prerequisite must have
landed a cycle or completed before that member can advance.

Initiatives and findings use the campaign store API and its existing lock.
Online commands use typed gRPC and daemon-owned state. An unavailable daemon
is an error. Use `--offline` only for recovery with the daemon stopped.
The current storage file is `~/.foundry/campaigns.json`.
Integrations must use the store API rather than depend on its file shape.

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

Candidates do not dispatch campaigns in slice 0. Project assignment, the
codebase audit, steering and Tracker completion belong to slice 1. Derivation
never writes Tracker state or runs `et close`.

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

Slice 1 will add automatic steering, audit-based project assignment and
integration gates on the production daemon's host. Slice 0 does not promote
Foundry to production. Promotion to ops-01 needs Stacey's go-ahead.

See [campaigns](campaigns.md), [the work queue](work-queue.md) and
[initiative events](../reference/event-types.md#initiatives-and-findings).
