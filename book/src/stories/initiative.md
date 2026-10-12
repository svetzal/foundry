# Deliver one outcome across repositories

Maya's customer wants to see why a delivery is late. `dispatch-api` owns the
delivery status. `customer-portal` displays it. Changing either repository alone
will not give the customer a useful answer.

In Tracker, Maya has an approved goal, `explain-delays`, and a journey,
`understand-delay`. Its interactions cover publishing a reason and displaying it
to the customer. The journey puts the API interaction before the portal one.

This story uses initiative slice 1. Development checks passed on the Mac
daemon, and production promotion awaits approval. Slice 2 will add automatic
outcome evaluation and Tracker closure.

## Bring the intent into Foundry

Maya has registered both execution projects with charters and required gates.
Maya runs derivation from the repository that supplies the Tracker intent:

```bash
foundry initiative derive --from et:journey:understand-delay \
  --projects dispatch-api,customer-portal --authorized-by Maya
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
whether customers should see that code. Maya checks the question and records
one policy:

```bash
foundry initiative show understand-delay
foundry initiative decide understand-delay \
  --decision "Show a plain-language reason; keep internal codes out of the portal"
```

That decision reaches applicable campaign members, including active ones.
Formation that used the older policy must reassess before applying its result.

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

## Check the customer's ending

The portal change lands and its gates pass. Maya checks the two applications
together with a representative delayed delivery. She confirms that the
customer can see a useful explanation and that older API clients still work.

She records completion:

```bash
foundry initiative complete understand-delay \
  --reason "The portal explained a delayed delivery using the API response; old clients still worked"
```

In slice 1, this owner check supplies the overall completion decision. Foundry
does not run initiative integration gates or close Tracker entities. Maya
handles Tracker closure through her normal intent review after checking the
evidence.

This follows Tracker journey `outcome_across_repos`, through its currently
implemented steps. Read [Initiatives](../guide/initiatives.md) for definitions,
milestones, holds and findings.
