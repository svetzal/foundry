# Foundry Workflow Reference

## Iterate Workflow

Triggered by `foundry iterate <project>` or routed from a maintenance run when
`actions.iterate=true`.

```text
ProjectIterationRequested
  └─ CheckCharter (Observer)
       └─ CharterCheckCompleted {success: true}
            └─ ResolveGates (Observer)
                 └─ GateResolutionCompleted {workflow: "iterate", gates: [...]}
                      └─ RunPreflightGates (Observer)
                           └─ PreflightCompleted {all_passed: true}
                                └─ AssessProject (Observer, AI Reasoning)
                                     └─ AssessmentCompleted
                                          └─ TriageAssessment (Observer, AI Reasoning)
                                               └─ TriageCompleted {accepted: true}
                                                    └─ CreatePlan (Observer, AI Reasoning)
                                                         └─ PlanCompleted
                                                              └─ ExecutePlan (Mutator, AI Coding)
                                                                   └─ ExecutionCompleted
                                                                        └─ RunVerifyGates (Observer)
                                                                             └─ GateVerificationCompleted
                                                                                  └─ RouteGateResult (Observer)
                                                                                       ├─ [passed] ProjectIterationCompleted
                                                                                       │    └─ SummarizeResult → CommitAndPush
                                                                                       └─ [failed, retries < 3] RetryRequested
                                                                                            └─ RetryExecution (Mutator, AI Coding)
                                                                                                 └─ ExecutionCompleted (loops back)
```

**Stop conditions:**

- Charter check fails (`success: false`) — chain stops at CharterCheckCompleted
- Preflight gates fail — AssessProject self-filters, chain stops
- Triage rejects (`accepted: false`) — CreatePlan self-filters, chain stops
- Retries exhausted (3 failures) — emits ProjectIterationCompleted with
  `success: false`

**Maintain chaining:** If the trigger payload has `actions.maintain=true` and
iterate succeeds, RouteGateResult also emits `ProjectMaintenanceRequested` to
chain into the maintain workflow.

## Maintain Workflow

Triggered by `foundry emit project_maintenance_requested` or chained from
iterate.

```text
ProjectMaintenanceRequested
  └─ ResolveGates (Observer)
       └─ GateResolutionCompleted {workflow: "maintain", gates: [...]}
            ├─ RunPreflightGates (Observer) — skips for maintain, emits PreflightCompleted {skipped: true}
            └─ ClassifyDependencyUpdates (Observer) — classifies deps, applies the update policy
                 └─ DependencyUpdatesClassified {phase: "before", brief, gates}
                      └─ ExecuteMaintain (Mutator, AI Coding) — prompt = the brief: exact updates, nothing more
                           └─ ExecutionCompleted {workflow: "maintain"}
                                └─ RunVerifyGates (Observer)
                                     └─ GateVerificationCompleted
                                          └─ RouteGateResult (Observer)
                                               ├─ [passed] ProjectMaintenanceCompleted
                                               │    ├─ SummarizeResult → CommitAndPush
                                               │    └─ ClassifyDependencyUpdates → DependencyUpdatesClassified {phase: "after"}
                                               └─ [failed, retries < 3] RetryRequested
                                                    └─ RetryExecution → loops back (no further dependency moves)
```

The update policy (`patch` / `minor` / `major`, registry field
`update_policy`, unset behaves as `minor`) decides what the brief lists. Majors
never run in the maintain session: after the nightly summary, the majors lane
(`PlanMajorUpgrades`, on `MaintenanceSummaryRequested`) dispatches each one as
its own `foundry task` for `major` projects (caps
`FOUNDRY_MAJOR_TASKS_PER_PROJECT`=2, `FOUNDRY_MAJOR_TASKS_PER_NIGHT`=6) and
proposes them with a command for `minor`/`patch` projects. Security fixes
override the ceiling. `.dependency-holds.json` holds a package at or below a
version until an expiry. `foundry deps <project> [--policy X]` shows all of this
for one project without changing anything.

When an otherwise eligible nightly major upgrade matches a `preserved` ledger
item by exact registered project, package and target version, the majors lane
resumes that obligation from its preservation ref and original objective. Its
child records `kind: major_upgrade`, `lane: maintenance`, origin `nightly majors
lane` and the exact `resumes` id; automation records no operator action. The
parent retains its submission identity and evidence. Only an actual child
landing settles the preserved parent landed; other outcomes leave it open,
and owner cancellation remains terminal. A selected resume admission failure
is reported and never falls back to a fresh task.

Continuations count against the existing nightly caps and run sequentially
with fresh upgrades in the existing order. Update policies, holds, security
eligibility, successful maintenance and in-flight suppression still apply.
Reviews, dry runs and interrupted-cycle summaries dispatch nothing. Without a
matching preserved ledger item, the existing fresh-task and history-suppression
rules apply. Owner `queue resume` remains an interactive, asynchronous task.

## Validate Workflow

Triggered by `foundry validate <project>`. Read-only — no mutations.

```text
ValidationRequested
  └─ ResolveGates (Observer)
       └─ GateResolutionCompleted {workflow: "validate", gates: [...]}
            └─ RunPreflightGates (Observer) — runs gates for validate workflow
                 └─ PreflightCompleted
                      └─ RouteValidationResult (Observer)
                           └─ ValidationCompleted {success: bool, results: [...]}
```

## Task Workflow

Triggered by `foundry task <project> "<description>"`. Mutating, isolated, and
intentionally not retried. The user-provided description is the single
objective.

The daemon admits the submitter's `ExecutionRequested` at the engine routing
boundary. It records the item `queued` and retains the root event. The shared
pacing scheduler starts it by emitting a fresh root on the same trace.
The daemon checks that root against the complete persisted start record.
Campaign formation, campaign cycles, nightly maintenance, releases, remediation,
digest agents, and raw emitted roots use the same boundary. Downstream steps
retain their root's slot; independently derived tasks queue before it releases.

```text
ExecutionRequested {workflow: "task", prompt: "..."}            — admitted root
  └─ Durable admission → WorkItemSubmitted {state: queued, reason}
       … pacing scheduler: WorkItemStarted …
ExecutionRequested {workflow: "task", admitted_work_item_id}   — started root
  └─ CheckCharter (Observer)
       └─ CharterCheckCompleted {success: true, workflow: "task"}
            └─ ResolveGates (Observer)
                 └─ GateResolutionCompleted {workflow: "task", gates: [...]}
                      └─ RunPreflightGates (Observer) — skips for task
                           └─ PreflightCompleted {workflow: "task", skipped: true}
                                └─ DirectPrompt (Observer)
                                     ├─ TaskRunStarted
                                     └─ PlanCompleted {workflow: "task", plan: prompt}
                                          └─ ExecutePlan (Mutator, isolated worktree)
                                               └─ ExecutionCompleted {workflow: "task"}
                                                    └─ RunVerifyGates (Observer, same worktree)
                                                         └─ GateVerificationCompleted
                                                              └─ ReviewTask (Observer, AI Reasoning)
                                                                   └─ TaskReviewed {verdict, gate_results}
                                                                        └─ FinalizeTask (Mutator)
                                                                             └─ TaskRunCompleted {landed, verdict, preservation_ref?}
```

`ReviewTask` emits one structural verdict: `complete`, `remainder`, `defect`,
`blocked_on_decision`, or `runner_error`. A reviewer cannot override a failed
required mechanical gate with `complete`. When `ExecutionCompleted` carries a
provider failure kind (`provider_unavailable` after `ExecutePlan`'s retries at
30 s, 2 min and 5 min, or a terminal account kind), `ReviewTask` emits
`runner_error` naming it without a review session or proof check, and
`FinalizeTask` carries `failure_kind` onto `TaskRunCompleted`. Any other failed
session reaches `ReviewTask` as `session_failure` (`message`, `left_work`) on
`GateVerificationCompleted`; with `left_work: false` and no early evidence in
the worktree it also settles `runner_error` with that message. `FinalizeTask` commits all work. It
lands `complete`, and also lands a converging `remainder` when at least one
required gate ran and every required gate passed. Other results are preserved on
a named remote branch or Git bundle. `RouteGateResult` rejects task workflows,
so the generic retry loop cannot claim them.

## Campaign Formation

Triggered manually by `foundry campaign advance <name>` or automatically by a
campaign task result. Online campaign control flows through the daemon-owned
campaign store; explicit `--offline` is the only direct-file recovery path.
Online reads and mutations render daemon-owned responses directly rather than
re-reading `FOUNDRY_CAMPAIGNS_PATH`. Each decision reloads the durable campaign
record under the store lock, releases the lock while the gates and agent run,
and applies its outcome only if the record is unchanged when it locks again.
A campaign paused, cancelled, completed or escalated meanwhile keeps that
status and the advance stands down (`decision: "stood_down"`, item settles
`landed`). A campaign edited in any other way, including an owner decision
recorded on the active campaign, keeps the edit and forms again from it, up to
three formations; past that the advance fails. One campaign
forms at a time; different campaigns form concurrently.

```text
CampaignAdvanceRequested {campaign, run_event_id?, run_result?}
  └─ AdvanceCampaign (Mutator, AI Reasoning)
       ├─ [done] CampaignAdvanceCompleted {decision: "done"}
       │    └─ CampaignCompleted
       │         └─ SurfaceCampaignTerminal → OpsDigestStarted {forced_event}
       ├─ [advance] CampaignAdvanceCompleted {decision: "advance", objective}
       │    └─ ExecutionRequested {workflow: "task", campaign, base_ref?}
       │         └─ Task Workflow
       │              └─ TaskRunCompleted
       │                   └─ RequestCampaignAdvance
       │                        └─ CampaignAdvanceRequested
       ├─ [escalate] CampaignAdvanceCompleted {decision: "escalate"}
       │    └─ CampaignEscalated
       │         └─ SurfaceCampaignTerminal → OpsDigestStarted {forced_event}
       └─ [stopped] CampaignAdvanceCompleted {decision: "stood_down", campaign_status}
```

The decision block checks live repository state, neutral context files, typed
task results, and required done evidence. It cuts exactly one next objective.
Campaign budget is spent only on dispatched tasks. A final budgeted task still
receives completion evaluation; only an attempted additional dispatch escalates
for budget exhaustion. Paused **and cancelled** campaigns record an in-flight
result without advancing and the advance stands down — that is the whole
graceful-stop mechanism for `campaign cancel`: the running cycle finishes, its
work is preserved as usual, and no successor is dispatched. A stand-down
settles its advance work item `landed`; `success: false` on
`CampaignAdvanceCompleted` marks a fault and settles it `failed`. `campaign cancel --now` instead aborts the
campaign's daemon task outright, which kills the running agent and leaves an
orphaned worktree for `DisposeCampaignWork` to preserve or discard.
Provider-unavailable failures pause without consuming a cycle;
`blocked_on_decision`, genuine runner faults, and owner-judgment findings
escalate. Every advance mints trace and cycle-span identity,
`CampaignAdvanceCompleted` retains the formation prompt, provider, and
done-evidence results, and task-side events carry `campaign_cycle`. Store
mutations are serialized across CLI and daemon processes. Dry-run formation
executes one simulated task to terminal state and suppresses recursive
auto-advance.

## Drift Scout Workflow

Triggered by `foundry scout <project>`. Read-only observation.

```text
DriftAssessmentRequested
  └─ ScoutDrift (Observer, AI Reasoning)
       └─ DriftAssessmentCompleted {candidate_count, high_value_count, candidates: [...]}
```

## Full Maintenance Run

Triggered automatically every night at 02:00 local time by the in-daemon
`nightly-maintenance` sentinel (see `~/.foundry/sentinels.json`), or on demand
by `foundry run`. Fan-out across all active projects.

```text
MaintenanceCycleStarted {project: "system"}
  └─ FanOutMaintenance (Observer)
       ├─ ProjectRunStarted {project: "alpha"}
       │    └─ ValidateProject → ProjectValidationCompleted
       │         └─ RouteProjectWorkflow
       │              └─ ProjectIterationRequested or ProjectMaintenanceRequested (per project flags)
       │                   └─ ... (iterate or maintain chain)
       ├─ ProjectRunStarted {project: "beta"}
       │    └─ ... (same pattern)
       └─ MaintenanceCycleCompleted {project_count, skipped_count}
            └─ GenerateSummary → writes audit report
```

For single-project runs (`foundry run --project alpha`), there's no fan-out —
the project chain runs directly.

## Commit Digest Formation

Fired every day at 17:00 local time by the in-daemon `daily-commit-digest`
sentinel, or on demand by `foundry emit commit_digest_started --project system`.
Linear chain — no fan-out — across all active registered projects.

```text
CommitDigestStarted {project: "system"}
  └─ ObserveCommits (Observer)
       └─ CommitsObserved {window_hours: 24, projects: [{name, branch, commits, error?}, ...]}
            └─ SummarizeCommits (Observer, AI Reasoning)
                 └─ CommitSummaryCompleted {markdown, project_count, total_commits}
                      └─ WriteCommitDigest (Observer)
                           └─ CommitDigestCompleted {success, digest_path?, ...}
                                └─ writes {FOUNDRY_DIGESTS_DIR}/{YYYY-MM-DD}.md
```

Per-project `git log` failures are captured inline on the `ProjectCommits.error`
field and the chain continues. Empty-day firings short-circuit the agent call
and write a "No commits across N projects in the last 24 hours" file (absence is
a fact too). Dry-run firings run the full chain but skip the final file write.

## Ops Digest Formation

Fired every three hours by the in-daemon `ops-digest` sentinel (`0 */3 * * *`),
or on demand by `foundry emit ops_digest_started --project system`. Linear chain
— reads MBOS JSONL events from the intake directory, applies a pressure gate,
then summarises.

```text
OpsDigestStarted {project: "system"}
  └─ ObserveEvents (Observer)
       ├─ [gate not satisfied] OpsDigestCompleted {success: true, skipped: true}
       └─ [gate satisfied] OpsObserved {proceed: true, new_event_count, anomaly_present, events: [{...}]}
            └─ SummarizeEvents (Observer, AI Reasoning)
                 └─ OpsSummaryCompleted {markdown, event_count, new_watermark?}
                      └─ WriteOpsDigest (Observer)
                           └─ OpsDigestCompleted {success, digest_path?, event_count}
                                └─ writes {FOUNDRY_OPS_DIGESTS_DIR}/{YYYY-MM-DD}.md
                                   advances ~/.foundry/ops-digest.watermark
```

**Pressure gate:** The chain short-circuits (`skipped: true`) if fewer than 25
new events have arrived and none are anomalies. Anomalies include P0-urgency
events, `ci_pipeline_failure`, `maintenance_intervention_recorded` with
`outcome=unresolved`, `dependency_vulnerability_detected` with severity
`high`/`critical`, and `maintenance_run_completed` with `reposFailed > 0`.

**Watermark:** The newest event's `occurredAt` timestamp is written to disk
after a successful digest so the next run only processes newer events. First-run
lookback is 24 hours (no watermark). Dry-run firings do not advance the
watermark.

## Vulnerability Remediation Workflow

Triggered by `foundry emit scan_requested --project <name>`.

```text
ScanRequested
  └─ ScanDependencies (Observer)
       └─ VulnerabilityDetected (one per CVE)
            ├─ AuditReleaseTag (Observer)
            │    └─ ReleaseTagAudited
            └─ AuditMainBranch (Observer)
                 └─ MainBranchAudited
                      └─ RemediateVulnerability (Mutator, AI Coding)
                           └─ RemediationCompleted
                                └─ CommitAndPush (Mutator)
                                     ├─ ProjectChangesCommitted
                                     └─ ProjectChangesPushed
                                          └─ AuditReleaseTag (post-push)
                                               └─ ReleaseTagAudited
```

If the main branch is clean (not dirty), the automatic release path fires — see
**Release Workflow > Automatic Release** below.

## Pipeline Health Check Workflow

Triggered by `foundry pipeline <project>`. Checks GitHub Actions CI status and
auto-remediates failures.

```text
PipelineCheckRequested
  └─ CheckPipeline (Observer)
       └─ PipelineChecked {passing: bool, logs: Option<String>}
            └─ RemediatePipeline (Mutator, AI Coding) — self-filters when passing
                 └─ RemediationCompleted
                      └─ CommitAndPush (Mutator)
                           ├─ ProjectChangesCommitted
                           └─ ProjectChangesPushed
```

**CheckPipeline** looks up the project repo and branch from the registry, runs
`gh run list` to check status, and if failing fetches failure logs with
`gh run view --log-failed`.

**RemediatePipeline** self-filters (skips when pipeline is passing). When
failing, invokes Claude with Coding capability and Full access to diagnose and
fix CI failures.

## Release Workflow

Triggered by `foundry release <project>` (manual) or automatically after
vulnerability remediation (CutRelease).

### Manual Release (via CLI)

```text
ReleaseRequested
  └─ ExecuteRelease (Mutator, AI Coding)
       └─ ReleaseCompleted {success, new_tag, release: "manual"}
            └─ WatchPipeline (Observer)
                 └─ ReleasePipelineCompleted
                      └─ InstallLocally (Mutator)
                           └─ LocalInstallCompleted
```

**ExecuteRelease** checks that `actions.release=true` in the registry. Invokes
Claude agent with the project's AGENTS.md to run quality gates, update
changelog, bump version, commit, tag, and push. If `--bump` is provided, passes
it to the agent; otherwise the agent determines the bump from changelog.

### Automatic Release (vulnerability path)

```text
MainBranchAudited {dirty: false}
  └─ CutRelease (Mutator, AI Coding)
       └─ ReleaseCompleted {success, new_tag, release: "patch", cve: "..."}
            └─ WatchPipeline (Observer)
                 └─ ReleasePipelineCompleted
                      └─ InstallLocally (Mutator)
                           └─ LocalInstallCompleted
```

**CutRelease** self-filters when `dirty=true`. Invokes Claude agent to cut a
patch release for the specific CVE.

Both paths share the same `AgentRelease` work block (ComposedStep architecture)
— only the event adapter and output mapper differ.

## Task Block Types

| Kind         | Throttle: Full     | Throttle: DryRun                         |
| ------------ | ------------------ | ---------------------------------------- |
| **Observer** | Executes and emits | Executes and emits                       |
| **Mutator**  | Executes and emits | Simulates success via `dry_run_events()` |

## File Dependencies

- **`.hone-gates.json`** — Quality gate definitions, read by ResolveGates.
  Created by `foundry gates --init`.
- **`CHARTER.md`** (or README.md, CLAUDE.md with `## Project Charter`) —
  Required by CheckCharter for iterate workflow.
- **`~/.claude/agents/{agent}.md`** — Agent instruction file, resolved from
  registry's `agent` field.

## Owner Work-Item Controls

Use `foundry queue close <id> --reason "reviewed; obligation discharged"`
for `preserved`, `needs_decision` and `failed` items, or
`foundry queue cancel <id>` for `submitted`, `queued` and `held` items. Optional
`--origin "owner review"` records context beside the CLI hostname.
Both require the daemon and reject `--offline`. They settle exactly that id
`cancelled`, recording an owner action separately from submission origin and
retaining prior settlement evidence and disposition. The cancellation appears
on Watch and in the durable event history; earlier events remain intact.
Neither command starts agents, stops running workflows or removes preserved
work. Use campaign cancellation for an in-flight campaign, as described above;
`campaign cancel` also cancels the campaign's queued cycles.

`foundry queue hold <id>` takes a `queued` item out of the scheduler's hands
(`held`, reason `held by operator`); `foundry queue release <id>` returns a held
item, or one in `needs_decision` over a dependency that settled some way other
than landed, to `queued` with the scheduler's current verdict as its reason.
They emit `work_item_held` / `work_item_released`. The scheduler also returns
such an item to the queue by itself once every dependency is landed, waiting
or running, and emits `work_item_released`. `release` refuses a `follow_up`. `foundry pacing show |
pause | resume | drain` read and steer the stage itself; see the skill's
"Pace the work" section.

Owner-directed preserved-work continuation uses `foundry queue resume <id>
[--origin <text>] [--after <id>]... [--not-before <when>]`. It requires a live
daemon and refuses `--offline`. Only a `preserved` item with a usable
preservation ref and a registered project can be resumed. Foundry records a
new task with the original objective, `queued` for the pacing scheduler, that
starts from the preserved local branch, remote ref or `bundle:<path>` through
the existing continuation path. When the preserved work does not contain current trunk, Foundry rebases it onto freshly fetched trunk before the agent starts (the preservation ref itself is not rewritten). If that rebase conflicts, the child is refused before any agent session: it settles `failed` with `land_blocked: continuation_conflict` and a reason that names the conflicting files, and the original stays `preserved`. The new record and its lifecycle payloads expose
`resumes`, the exact original id. Queue reads show this link in human and JSON
output. Submission identity and the parent's prior evidence remain intact;
the child records the hostname and optional origin in its `resume` operator
action.

The original obligation stays preserved until the linked task actually lands.
Then Foundry records its landing commit and appends `work_item_settled` for the
original and, transitively, for everything it resumed or continued. Failed,
blocked, preserved and no-landing results leave it open.

`queue resume` also accepts an open `follow_up` item: a standalone landing's
remaining gaps (`obligation: remainder`, `needs_decision`) or red trunk CI
after a landing (`obligation: trunk_ci`, `failed`, with failing jobs and the
failed-log tail). It admits a task from current trunk whose `continues` names
the follow-up, so its landing settles the follow-up. A later landing whose CI
passes also settles a trunk CI follow-up.
An original cancelled by its owner stays cancelled even if its child lands.
Unknown ids return `NOT_FOUND`, blank inputs `INVALID_ARGUMENT`, ineligible
states or unusable evidence `FAILED_PRECONDITION`, and persistence failures
`INTERNAL`. A rejected admission dispatches no execution and invokes no agent.

`foundry task <project> "<description>" --continues <id>` (repeatable) is the
lighter link for a follow-up with its own objective. Each id must be an open
item of the same project. The new item records them in `continues`; when it
lands, each one still open settles `landed` with reason `superseded by <commit>
via <new item id>`, and when it does not land they stay open.

If the initial ledger save fails, no child or lifecycle events are created.
Otherwise admission stages a child in `failed` with reason `resume admission incomplete;
execution not dispatched`, no `started_at`, a `settled_at`, no disposition, and
`resumes` set to the exact preserved parent's id. The child becomes `queued`
only after the `work_item_submitted` lifecycle root persists and the final
ledger save succeeds; the scheduler then starts it and emits
`work_item_started`. A subsequent failure leaves the staged failed child and
preserves the parent and unrelated records.

Successful lifecycle appends remain in durable history and are delivered on
Watch, even when admission later fails. Failed writes are never advertised on
Watch. Earlier bytes, including partial failed appends, are never truncated or
rewritten. Lifecycle events alone are not proof of execution: consult the ledger
and RPC result, and continue from the preserved parent's id.

After a task actually lands, Foundry also checks that registered project's
other open ledger items (`preserved`, `needs_decision` or `failed`) that
recorded a preservation ref against the registered trunk branch, using the
registered checkout path. A preserved commit reachable from trunk, a nonempty
set of commits whose patches are all matched by `git cherry`, or a commit that
`git merge-tree --write-tree` merges into trunk cleanly with trunk's own tree
as the result (content containment, Git 2.38 or later), settles `landed`. A
`needs_decision` or `failed` item settles only when its preserved tip was
committed after the item started; a run that made no commits keeps its
question or fault open. Its `landed_commit` is the verified trunk commit and
its reason is exactly `superseded by <commit>`, whichever proof succeeded. The appended `work_item_settled` event carries
that item's exact id, project and original trace. Objective, submission origin,
`resumes` and prior preservation evidence remain intact; no preserved ref,
bundle or worktree is deleted.

This check runs only after actual task landing. Failed, blocked, preserved and
no-landing task results do not trigger it. It checks only open items with a
preservation ref in the same registered project; owner-cancelled items remain
cancelled. A repeated
terminal does not append another settlement. The ledger is reloaded under the
shared write gate before saving, so concurrent owner actions and unrelated
updates are retained. A failed save emits no settlement.

Missing refs or objects, failed Git commands, preserved changes that trunk
does not hold (unmatched patches whose merge into trunk conflicts or would
still change trunk's tree), and ambiguous evidence leave the item open with an
explicit supersession diagnostic in the settlement block's result and
structured log. When patch equivalence and content containment both fail, the
diagnostic gives each reason. Empty Git output is never proof. Local branch refs and
commit ids are checked directly. Branch names are also checked by exact name
on `origin` when configured, without fetching; conflicting local and remote
heads remain unresolved. A remote-only head must already have locally available
objects. A bundle must advertise exactly one head and that commit must already be
locally available. Patch comparison and content containment require complete,
shared history; patch comparison rejects merge commits that `git cherry` cannot
account for. Queue show and JSON
expose the persisted commit and reason through the daemon's read path.
Scheduled invocation and the reporting reconciler are not delivered by this
landing-triggered check.

### Work reconciliation

`foundry queue reconcile` requires the daemon and prints that invocation's
report; `--offline` is rejected. The canonical `work-reconciler` sentinel uses
`30 */3 * * *` and pairs `work_reconcile_started` with `work_reconcile_completed`.
Inventory reports exact IDs, paths, branch refs and registered-trunk status.
Conservative supersession may settle open work with a preservation ref without a task landing;
concurrent owner changes win, and preservation evidence is retained. No cleanup
or agent invocation occurs. Errors surface as INTERNAL, and anomaly findings
force ops observation below its normal volume threshold. Reports use
`FOUNDRY_RECONCILE_DIR` (default `~/.foundry/reconcile`).

## Initiative members

Initiatives supply the shared outcome, brief, decisions and findings to
campaign formation. Explicit `foundry initiative advance <name>` enables
steering. Passive members retain `foundry campaign advance <member>`. The
campaign store guard enforces the
initiative's total cycle budget, active member limit and owner questions.

Formation snapshots the initiative context under that guard, releases it
while the agent runs, and compares current context before applying output.
New decisions, findings or brief identity invalidate stale formation. Task
payloads carry `initiative`, `initiative_brief`, `initiative_findings`,
`finding_ids` and `providers`. Cycles use `initiative:<name>` as their source
while retaining `campaign` and `campaign_cycle` in the payload.

A landed result consumes the exact findings its formation saw for that member.
Landed reviewer gaps and defect diagnoses become new findings. Preserved work
consumes nothing. Campaign continuation and `queue resume` receive current
findings and the brief identity.

Each block selects its stage provider. Member choices override initiative
choices; omitted stages inherit `agent_provider`. Read-only formation and
review use scratch-only writes. Claude execution uses scoped edits, `dontAsk`
and the OS sandbox with unsandboxed retries disabled. Worktrees stay in their
existing home directory.

Read-only Tracker derivation stages candidates from goals or journeys. It
reads committed projections before the et CLI and changes no Tracker state
during derivation or steering. Slice 2 checks integration evidence, stages
new campaigns and proposes completion for owner confirmation. Derivation can
attach a JSON array of gate definitions with `--integration-gates <file>`; it
keeps the outcome reviews supplied by Tracker.

`foundry initiative advance <name>` submits one outcome assessment and enables
steering after subsequent member settlements. `--member <name>` pins the next
advance and releases that member's owner hold. Steering chooses one typed
`advance`, `dispatch`, `hold`, `relay`, `ask`, `stage`, `done`, `escalate` or
`wait` action. The retained
`initiative_steered` fact includes the prompt, provider and decision.
Foundry computes the eligible members first and skips the agent when no action
can apply (outcome `skipped`). `wait` needs a reason and opens no question; use
`ask` only for an owner question. A hold or relay already in force is a
`no_op`. A guard's refusal is `rejected` and settles the steer `cancelled`, not
`failed`, so only genuine faults appear in `queue open`.

Queued formation, reserved admissions, running cycles and standalone tasks
count toward `max_active_members`; a completed or cancelled member counts only
while one of its cycles still runs.

Initiative decisions answer only explicit question IDs. Use
`foundry initiative decide <name> --question <id> --decision "<answer>"`.
Repeat `--question` for several questions. Without `--member`, answers reach
only the selected questions' members; `--member` can select a subset.
Other questions remain open, including questions for those same members.
Without `--question`, `decide` records binding policy and answers no questions.
Use `foundry initiative note <name> --text "<status>"` for observations.
Notes are durable, nonbinding context. They do not answer questions, resume
campaigns, release holds, change budgets, or request steering.
`initiative show` exposes open and deferred question IDs.
RPC clients must use `RecordInitiativeDecision` with explicit `question_ids`
to answer questions. Updated CLI decisions refuse older daemons without mutation.

Operational blockers use typed recovery, not owner questions. Campaign and
initiative `show` expose `recovery` with its ID, cause and exact detail.
Use `campaign recover <name> --recovery <id> --evidence "<what changed>"` or
`initiative recover <name> --recovery <id> --evidence "<what changed>"`.
An agent can request a recheck after an authorized operational correction.
The daemon reassesses under existing gates, questions, holds and budgets;
recovery records no owner decision and adds no cycles. A campaign pause stays.
A provider outage needs no recovery while the campaign repeats the refused
cycle by itself; the recheck of a `runner_unavailable` recovery left after
those repeats dispatches the same objective without forming.
New RPCs refuse older daemons without a decision or resume fallback.
Standalone operational blockers preserve work; use `queue resume <id>` after
correcting their prerequisite. `.foundry/blocker.json` can set `recovery_kind`
to `capability_unavailable`, `access_unavailable`, `environment_unavailable`,
`missing_evidence`, `unmet_precondition` or `runner_unavailable`.
Without that field, legacy blockers still represent owner decisions.

`initiative show` exposes computed `readiness` with typed blockers and exact
dependency evidence. Online reads check the current ledger and registry;
offline reads say `live: false`. This is admission readiness, not release or
deployment proof. A bare `after: ["producer"]` now requires the producer's
completed outcome. Use `producer:landed` for an intentional first-landing
prerequisite, or `producer:<milestone>` for exact recorded evidence. Partial
landings and notes never satisfy completion. Conditional `hold --until`
ends when all declared prerequisites have proof; indefinite owner holds stay.
Dependency waits require no owner decision, spend no cycle and leave unrelated
members eligible. Admission rechecks controls before starting work.

Shared cycles include initiative tasks. Use
`initiative decide --add-cycles N` to authorize more shared cycles. Once the
shared budget is spent, `initiative decide` without `--add-cycles` is refused
and records nothing. Member budget grants cannot exceed that total. Owner holds and shared questions block
ineligible work; milestone holds require explicit evidence.

The first steer audits Tracker candidates against registered codebases. Audit
findings retain checked paths and classify intent as `realised`, `partial`,
`absent` or `misplaced`. Each steer refreshes intent from the supplying repo;
that repo need not be a registered writable target. Withdrawn intent stops its
owned work gracefully. Realised intent records a close proposal without an ET
write. Slice 2 checks integration evidence, stages campaigns for uncovered work
and proposes completion for owner confirmation.

Only one assessment runs per initiative. Changed store or trunk evidence
invalidates output. Durable pending actions recover on restart without double
spending or dispatch. A steering hold, relay, ask, escalate or stage that
changes state but dispatches nothing requests exactly one follow-up assessment
when the recomputed precheck still lists an eligible member. The follow-up
carries `follow_up_of` (the earlier steer id and step) on its request, steering
record and `initiative_steered` fact, and never requests another. Wait, no-op,
skipped, rejected, stale and failed passes request none, and a queued or
running assessment for the initiative absorbs the request. Existing initiatives remain passive until explicitly advanced.

Optional `provider_pairings` explicitly alternates stage maps. Member stage
overrides remain binding, and continuations retain their admitted pairing.
Without that setting, Foundry keeps the configured stage providers.

### Initiative outcome evidence

Initiative integration gates use `done_evidence` entries of kind `gate`, with
`command`, optional `host`, `required` and repository-relative `artifacts`.
The default host is the daemon's host. Named remote hosts use SSH and their
online registry API. Foundry fetches and fast-forwards registered trunks,
then creates disposable copies named after each registered project.
Commands and artifact paths start at the shared parent directory.
`foundry capture` retains stdout and stderr. Results retain host, trunk commits
and log references. Missing artifacts or failed updates cannot pass a gate.
Failures become initiative findings.

Steering can `stage` a validated campaign within the initiative's projects.
It becomes a member under the existing shared authorization and budget.
Staging does not start an execution cycle.
A `done` action supplies evidence for every outcome review.
Passing required integration gates and current intent are enforced in code.
The initiative stays open until the owner confirms with
`initiative complete <name> --reason <text>`.
Changed trunks, intent or owner policy require a fresh assessment.

For derived initiatives, confirmed completion invokes `et close` from the
supplying repository for verified interactions, then the goal when its
children and success criteria are satisfied. Closure receipts survive partial
failure and restart. Failed closure leaves the initiative open.
Offline recovery cannot bypass outcome evidence or write Tracker state.
Cancellation never closes Tracker. Development stays on the Mac; promotion
still requires Stacey's go-ahead.
