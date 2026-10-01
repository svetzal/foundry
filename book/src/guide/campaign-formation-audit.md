# Context-mixer2 formation audit

The full assessment after each implementation cycle is intentional. Implementation
changes the repository and teaches the agent things that can change the remaining
work. Formation earns its place when it revises the next objective using those
changes, preserves accumulated work, or verifies the whole mission before closing.
Command count and token volume alone do not establish waste.

This audit covers the local campaign
`context-mixer2-atlas-contract-v1-20260930`, completed on October 1, 2026.
It dispatched three implementation tasks and landed two. The first task was
preserved after review found defects. The last task received a complete verdict.

## What the six sessions did

| Session prefix | Decision | New evidence and resulting action |
| --- | --- | --- |
| a319731d | Advance | Confirmed both capabilities were absent. Chose atlas preflight first and left manifest compatibility for the next cycle. |
| 880ebfdd | No decision | Inspected preserved first-cycle work, but an obsolete clock deadline killed the session during slow macOS Git probes. It supplied no usable objective. |
| 7e5a347d | Advance | Combined the preserved implementation, reviewer defects, missing manifest compatibility, and a failing adoption gate into the second objective. The daemon continued from the preserved commit. |
| 39d14536 | Escalate | Reassessed landed behavior, found remaining diagnostic gaps and the missing acceptance test target, and asked for authorization to extend the exhausted budget. |
| d802b977 | Advance | After extension, distinguished an absent test target from absent behavior. Existing tests were named manifest_schema. Chose precise diagnostic repair and test-target reconciliation rather than reimplementing the schema guard. |
| 5a323b74 | Done | Checked the delivered trunk against the whole mission, passing binary tests, CI wiring, docs, compatibility, and read-only behavior. Closed the campaign. |

The six sessions were not six implementation plans. Three formed implementation
objectives, two assessed whether the campaign could finish or continue, and one
failed before returning a decision. A separate prompt-size refusal ran no agent
and does not count as a formation session.

The useful change in scope was clearest in the third objective. Formation found
that manifest compatibility existed under the wrong test-target name. It asked
for reconciliation and preserved the implementation. That is the kind of learning
an assessment must use. The final assessment also checked the whole campaign,
while the task reviewer had checked its specific assigned objective.

## How learning travels today

The implementation output is stored in the execution_completed event and its
agent transcript. The reviewer inspects source, tests, and gates, then returns a
typed verdict. The next formation receives that verdict, its diagnosis or gaps,
landing state, preserved reference, recent objective history, live Git evidence,
done-gate results, and paths to the mission's contract files.

It does not automatically receive the implementer's discoveries as a structured
learning record. TaskRunCompleted.summary is a generic landing summary. Formation
therefore reconstructs much of the learning by reading changed source and tests.
That reconstruction is legitimate, but the audit cannot prove that every useful
implementation discovery reached the next assessment.

There is also a continuity defect on escalation. Clearing pending_run_result
removes the convenient typed handoff. This campaign required restoring its
preserved result during recovery. The durable task event and Git reference still
existed. This audit improvement exposes the latest task event ID; it does not
change that continuation behavior.

The next contract improvement should be an evidence-linked implementation learning
record: discoveries, invalidated assumptions, remaining work, and the source/test
or event that supports each item. Formation should reassess those claims against
the live repository, then record what changed in its assessment and why. A small
structured record is a starting point for assessment, not a substitute for it.

## Accounting and measurement limits

The five finished formation sessions reported 202,457 fresh input tokens,
1,748,992 cached input tokens, and 15,316 output tokens. The failed session has
no terminal usage record. Its native transcript contains a last observed
cumulative count of 355,009 input tokens including 322,688 cached tokens, plus
2,522 output tokens. That is a partial observation, not final usage. It remains
separate from campaign totals.

Each session's first model request contained about 35,000 to 37,000 input tokens.
The Foundry prompt was 12,347 to 15,700 bytes. Automatically loaded instructions,
skills, and other provider context contribute to the difference. The evidence
supports a large fixed context cost, but does not attribute its exact tokens to
individual instruction sources.

The six sessions completed 153 commands and captured 1,276,634 output bytes.
Native tool-output text includes wrappers and truncation. Neither byte count is a
token count or a reliable measure of the text retained in every subsequent model
request. The aggregate report has no price for the models in this run. Do not
interpret its known-list estimate as total spend.

These quantities describe the work; they do not judge whether assessment was
worthwhile. The stronger test is whether the assessment used new evidence to
choose a better objective and retained the learning required by the next cycle.

## Audit improvements

Campaign reports now include chronological formation sessions and decision events.
Session records include trace and transcript links, terminal usage, exact request
bytes for new sessions, completed and repeated commands, failed commands, and
captured output bytes. Codex native observations include the first request's input,
the last cumulative counters, and a matching native transcript path. Missing or
unsupported transcripts remain explicit. Partial observations never inflate or
replace terminal usage totals.

New decision events identify the latest task result known to the campaign. Full
prompts and gate results remain in their original durable events. This makes it
possible to inspect the assessment alongside the preceding implementation and
review rather than judging it from a stage total.

These changes introduce no deadline, token threshold, command quota, or shortcut
around the full reassessment.
