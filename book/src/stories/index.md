# Using Foundry

You have a few repositories, several things underway, and a limited amount of
attention. These stories follow the person doing that work. Start with the
situation closest to yours.

The people, companies, projects and work-item IDs are fictional. The situations
combine patterns seen in real runs: preserved work, exhausted budgets, questions
that affect several repositories, and intent that changes during implementation.
They describe possible runs, not transcripts of specific customer work.

| Your situation | Start here |
| --- | --- |
| You want to know what needs your attention | [A morning check](morning-check.md) |
| A useful change did not land | [Finish a change that stopped](recover-work.md) |
| You know the outcome, but cannot yet name every change | [Make a tool safe to adopt](campaign.md) |
| Several repositories must change together | [Deliver one outcome across repositories](initiative.md) |
| You need to replace the engine that runs everyone else's work | [Change Foundry while work continues](promote.md) |

Each story follows a need, a decision, an action and an observable ending.
Commands show the important moments. The linked guides contain setup details
and the full command options.

## Before you follow a story

Use [Getting started](../guide/getting-started.md) to connect to a daemon and
register a project. The coding stories assume a clean registered checkout, a
reachable origin, a project charter and meaningful required gates.
Replace the fictional project names and IDs with your own.

Commands use your configured daemon. Check that destination before starting
work. In Foundry's own development setup, real projects belong to the production
daemon on ops-01. The Mac daemon accepts synthetic sandbox projects only.

The initiative story describes slice 1, introduced in Foundry 0.48.0.
Its ending includes an owner check. Automatic outcome evaluation and Tracker
closure belong to slice 2.

## The journeys behind the stories

These stories follow Foundry's Epilogue Tracker journeys. Tracker describes
product intent. It can include work that Foundry does not implement yet.
Each story limits its instructions to the behaviour available today.

| Tracker journey | Story |
| --- | --- |
| `what_needs_me_now` | A morning check |
| `recover_unlanded_work` | Finish a change that stopped |
| `mission_to_done`, `answer_an_escalation` | Make a tool safe to adopt |
| `outcome_across_repos` | Deliver one outcome across repositories |
| `promote_foundry` | Change Foundry while work continues |

For a detailed examination of an actual run, read the
[formation audit](../guide/campaign-formation-audit.md). It records how new
evidence changed a campaign's objectives and where its accounting had limits.
