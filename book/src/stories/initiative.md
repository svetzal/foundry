# Deliver one outcome across repositories

Maya's customer wants to see why a delivery is late. `dispatch-api` owns the
delivery status. `customer-portal` displays it. Changing either repository alone
will not give the customer a useful answer.

In Tracker, Maya has an approved goal, `explain-delays`, and a journey,
`understand-delay`. Its interactions cover publishing a reason and displaying it
to the customer. The journey puts the API interaction before the portal one.

This story includes initiative outcome evidence, owner attention in the
dashboard and owner-confirmed Tracker closure.

## Bring the intent into Foundry

Maya has registered both execution projects with charters and required gates.
She saves the shared check as `delay-gates.json`:

```json
[
  {
    "kind": "gate",
    "command": "python3 dispatch-api/scripts/check-delay.py customer-portal",
    "required": true,
    "artifacts": [
      "dispatch-api/scripts/check-delay.py",
      "dispatch-api/contracts/delay-v2.json",
      "customer-portal/package.json"
    ]
  }
]
```

Both projects verify the same `delay-v2` executable contract in their required gates.
The contract includes older responses that omit the optional reason field.
Provider verification checks API behavior; consumer tests check the portal's handling
of the same cases. Test doubles come from that contract.
The results identify the contract hash and the tested commits on both sides.

The shared check starts both applications with their deployment configuration.
It tests whether a customer can read a useful explanation for a delayed delivery.
Maya runs derivation from the repository that supplies the Tracker intent:

```bash
foundry initiative derive --from et:journey:understand-delay \
  --projects dispatch-api,customer-portal --authorized-by Maya \
  --integration-gates delay-gates.json
foundry initiative show understand-delay
```

The initiative takes its name from the selected journey ID. Approved
interactions become candidates, with ordering from the journey. Derivation
records those candidates without starting campaigns or changing Tracker state.

Maya reads the outcome and candidates. She imports a brief that explains the
customer's language and compatibility requirements:

```bash
foundry initiative brief set understand-delay delay-brief.md
```

Foundry keeps the authoritative brief and its content hash. Members receive
that identity along with the shared decisions and applicable findings.

## Check what already exists

Maya explicitly enables steering:

```bash
foundry initiative advance understand-delay
```

The first assessment audits the registered codebases. In this example, the API
already publishes a reason, but the portal does not display it. The audit
records checked paths and classifies the two candidates as `realised` and
`absent`.

The realised interaction gets a close proposal. Foundry leaves Tracker
unchanged. Maya inspects the proposal before treating the interaction as done.
The portal candidate still needs work, and its prerequisite needs evidence.

Each assessment chooses one action. It may relay the API finding or ask Maya
to confirm that the existing response satisfies the prerequisite. An answer or
a later explicit advance gives steering another assessment. A relay does not
create an idle loop.

## Give both sides the same answer

The existing response uses an internal status code. The members need to know
whether customers should see that code. Both campaigns ask the same question.
Maya opens Sources in ops-visualizer. The initiative appears above its campaigns.
She sees one question with both members and the time it has waited.

Maya selects both members and writes one policy in the decision form.
The form retains her draft if a new member result arrives while she types.
She submits the answer. Both campaigns receive it, and the shared question closes.
The equivalent CLI command is:

```bash
foundry initiative show understand-delay
foundry initiative decide understand-delay \
  --decision "Show a plain-language reason; keep internal codes out of the portal"
```

That decision reaches applicable campaign members, including active ones.
Formation that used the older policy must reassess before applying its result.

Another question needs the customer's input. Maya leaves it open overnight.
After it has waited more than a day, the ops digest names the initiative
and question.
She can return to Sources and answer the affected members.

The initiative allows three open questions at once.
If another campaign escalates, its question remains on that campaign
until a slot opens.
The dashboard shows the waiting count. Unblocked members can still make progress.

Foundry can now advance eligible member work. Each campaign still chooses its
own next coding objective. After Foundry records a member's result and finishes
repository cleanup, the initiative chooses what should happen next.

Maya inspects the coordinated work:

```bash
foundry initiative show understand-delay
foundry finding list understand-delay
foundry queue --source initiative:understand-delay
```

The shared cycle budget limits accepted work across members. Queued formation,
reserved admissions and tasks count toward the active member limit. A budget
extension needs an owner decision with `--add-cycles`.

## Keep following current intent

During implementation, the customer withdraws a proposed tooltip. Maya updates
the approved intent in its supplying repository. The next steer reads that
intent again. It stops work owned by the withdrawn interaction gracefully and
keeps its findings, evidence and cycle history.

If Maya uses a committed projection, she commits that update before steering.
When the source uses the read-only Tracker CLI instead, steering reads its
current intent. Editing an uncommitted projection does not change the input.

## Discover a gap between the two applications

The portal change lands. Both projects pass verification of the same contract,
including the fallback for older responses.
The initiative's shared check runs the current API and portal trunks together.
It finds that the portal's deployment configuration still names a retired API address.
The contracts agree, but the customer cannot reach the explanation.

Foundry records a regression finding.
Steering stages a configuration repair campaign in `customer-portal` because the
existing mission did not cover this deployment setting.
Maya can see the new member before its next advance starts work.
It uses the initiative's existing authorization, providers and remaining cycle budget.

The campaign corrects the API address and lands it.
The next integration check passes against both current trunks.
Steering reviews the customer's journey and proposes completion.
The proposal contains the tested commits and the evidence for each review statement.

## Check the customer's ending

Maya reads the proposal:

```bash
foundry initiative show understand-delay
```

She confirms that the customer can see a useful explanation and that older API
clients still work.
She records completion:

```bash
foundry initiative complete understand-delay \
  --reason "The portal explained a delayed delivery using the API response; old clients still worked"
```

Foundry checks that the tested trunks and intent are still current.
It closes the verified Tracker interactions, then the goal when its success
criteria and children are satisfied.
The initiative becomes complete after those changes succeed.
A failed Tracker call leaves it open with receipts for the changes that did succeed.

This follows Tracker journey `outcome_across_repos`.
Read [Initiatives](../guide/initiatives.md) for gate definitions, milestones,
holds and findings.
