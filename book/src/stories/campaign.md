# Make a tool safe to adopt

Maya wants another team to adopt `config-kit`, a tool that reads configuration
files. She knows what success looks like: old files remain readable, unsupported
formats produce useful errors, and the adoption checks run in CI. She does not
yet know which changes the repository needs.

A campaign suits this work. Maya states the mission and the evidence that will
prove it. Formation will derive one objective at a time from that evidence.

## Authorise a bounded mission

Maya saves `safe-import.json`:

```json
{
  "name": "safe-import",
  "project": "config-kit",
  "mission": "A new team can adopt the tool without losing old configuration compatibility.",
  "done_evidence": [
    {
      "kind": "review",
      "statement": "Old files remain readable; unsupported formats have useful errors; CI runs the adoption checks."
    }
  ],
  "budget": { "max_cycles": 3 },
  "escalation": ["Ask the owner before removing compatibility."],
  "authorized_by": "Maya",
  "agent_provider": "codex"
}
```

The project already has required gates. The review statement describes the
mission beyond those mechanical checks. For stronger completion evidence, Maya
can also name existing acceptance tests as
[campaign done gates](../guide/campaigns.md#campaign-definitions).

She adds the campaign and starts it:

```bash
foundry campaign add safe-import.json
foundry campaign advance safe-import
```

## Let implementation change the next objective

The first assessment finds missing format checks. It dispatches a task.
Review then finds a diagnostic defect, and Foundry preserves the implementation.
The next assessment uses that result and the live repository to choose a repair.
The repaired implementation lands after gates and review.

On reassessment, formation finds that compatibility tests already exist under
an unexpected name. The next objective can connect those tests to the adoption
checks. There is no need to implement the same behaviour again.

Maya reads the result, rather than counting landed tasks as proof of completion:

```bash
foundry campaign show safe-import
foundry campaign report safe-import
```

## Answer the question that belongs to the owner

Suppose the campaign has used its three cycles and still needs a compatibility
decision. `queue open` exposes that escalation. `campaign show` includes the
question, evidence and commands available to Maya.

She checks the adoption team's requirement. They need the old format for one
more release. She records that policy and authorises two more cycles:

```bash
foundry campaign decide safe-import \
  --decision "Keep the old format readable for this release" --add-cycles 2
foundry campaign advance safe-import
```

The decision becomes binding context. The extra cycles are explicit permission
to continue. Without that extension, a spent budget cannot fund more tasks.

## Prove the whole mission

After the remaining work lands, formation checks the whole mission against
trunk and done evidence. It can complete the campaign when that evidence holds.
A complete verdict for one task proves only that task's objective.

If completion needs evidence outside the repository, Maya checks that evidence
herself. She then records an owner completion with a specific reason:

```bash
foundry campaign complete safe-import \
  --reason "The adoption team loaded its old files and checked the CI results"
```

These are two possible endings. Maya uses owner completion only when she has
checked the mission's outcome, rather than to remove an inconvenient escalation.

This follows Tracker journeys `mission_to_done` and `answer_an_escalation`.
Read [Tasks and campaigns](../guide/campaigns.md) for definitions and controls.
