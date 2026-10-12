# A morning check

Maya runs a small software company. Overnight maintenance has finished, a
campaign is still underway, and a coding session left a change that did not
land. She has twenty minutes before a meeting. She wants to find the decisions
that need her attention.

## See what is happening

Maya reads the queue:

```bash
foundry queue
foundry pacing show
```

In this example, one task is running in `parcel-cli`. Another task for that
repository is waiting for it to finish. A task in `receipt-reader` is waiting
for a specified earlier item to land.

The wait reasons matter. The second `parcel-cli` task needs no intervention:
Foundry allows one mutation at a time in that repository. Maya leaves it alone.
The dependent task will need attention if its prerequisite ends without landing.

## Find the questions and unfinished obligations

Maya asks for the open list:

```bash
foundry queue open
```

She finds a campaign asking whether an old configuration format must remain
supported. She also finds a preserved parser change. Open means that something
is still owed, even when no agent is running.

She reads each record before acting:

```bash
foundry campaign show safe-import
foundry queue show wi_0123456789abcdef01234567
```

The campaign needs her product policy. The preserved change needs another
coding run. These lead to different actions.

Maya records the policy and continues the campaign:

```bash
foundry campaign decide safe-import \
  --decision "Keep the old configuration readable for this release"
foundry campaign advance safe-import
```

She follows [Finish a change that stopped](recover-work.md) for the parser work.
If she discovers that another change already delivered it, she checks that
evidence before closing the obligation:

```bash
foundry queue close wi_0123456789abcdef01234567 \
  --reason "The replacement parser passed the acceptance cases on trunk"
```

Closing records her reason and retains preserved work. It does not merge a
branch or prove that the original task landed.

## Leave with a smaller set of decisions

Maya reads `queue open` again. The policy is now in the campaign's durable
record. The remaining coding obligation is visible, and ordinary queued work
can continue without her.

Her morning check ends when she understands what waits and has answered the
questions she can answer. An empty queue is not the goal.

This follows Tracker journey `what_needs_me_now`. Read
[The work queue](../guide/work-queue.md) for states, wait reasons and controls.
