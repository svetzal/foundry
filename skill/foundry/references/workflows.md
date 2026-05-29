# Foundry Workflow Reference

## Iterate Workflow

Triggered by `foundry iterate <project>` or routed from a maintenance run when `actions.iterate=true`.

```
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
- Retries exhausted (3 failures) — emits ProjectIterationCompleted with `success: false`

**Maintain chaining:** If the trigger payload has `actions.maintain=true` and iterate succeeds, RouteGateResult also emits `ProjectMaintenanceRequested` to chain into the maintain workflow.

## Maintain Workflow

Triggered by `foundry emit project_maintenance_requested` or chained from iterate.

```
ProjectMaintenanceRequested
  └─ ResolveGates (Observer)
       └─ GateResolutionCompleted {workflow: "maintain", gates: [...]}
            ├─ RunPreflightGates (Observer) — skips for maintain, emits PreflightCompleted {skipped: true}
            └─ ExecuteMaintain (Mutator, AI Coding)
                 └─ ExecutionCompleted {workflow: "maintain"}
                      └─ RunVerifyGates (Observer)
                           └─ GateVerificationCompleted
                                └─ RouteGateResult (Observer)
                                     ├─ [passed] ProjectMaintenanceCompleted
                                     │    └─ SummarizeResult → CommitAndPush
                                     └─ [failed, retries < 3] RetryRequested
                                          └─ RetryExecution → loops back
```

## Validate Workflow

Triggered by `foundry validate <project>`. Read-only — no mutations.

```
ValidationRequested
  └─ ResolveGates (Observer)
       └─ GateResolutionCompleted {workflow: "validate", gates: [...]}
            └─ RunPreflightGates (Observer) — runs gates for validate workflow
                 └─ PreflightCompleted
                      └─ RouteValidationResult (Observer)
                           └─ ValidationCompleted {success: bool, results: [...]}
```

## Drift Scout Workflow

Triggered by `foundry scout <project>`. Read-only observation.

```
DriftAssessmentRequested
  └─ ScoutDrift (Observer, AI Reasoning)
       └─ DriftAssessmentCompleted {candidate_count, high_value_count, candidates: [...]}
```

## Full Maintenance Run

Triggered automatically every night at 02:00 local time by the in-daemon `nightly-maintenance` sentinel (see `~/.foundry/sentinels.json`), or on demand by `foundry run`. Fan-out across all active projects.

```
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

For single-project runs (`foundry run --project alpha`), there's no fan-out — the project chain runs directly.

## Commit Digest Formation

Fired every day at 17:00 local time by the in-daemon `daily-commit-digest` sentinel, or on demand by `foundry emit commit_digest_started --project system`. Linear chain — no fan-out — across all active registered projects.

```
CommitDigestStarted {project: "system"}
  └─ ObserveCommits (Observer)
       └─ CommitsObserved {window_hours: 24, projects: [{name, branch, commits, error?}, ...]}
            └─ SummarizeCommits (Observer, AI Reasoning)
                 └─ CommitSummaryComposed {markdown, project_count, total_commits}
                      └─ WriteCommitDigest (Observer)
                           └─ CommitDigestCompleted {success, digest_path?, ...}
                                └─ writes {FOUNDRY_DIGESTS_DIR}/{YYYY-MM-DD}.md
```

Per-project `git log` failures are captured inline on the `ProjectCommits.error` field and the chain continues. Empty-day firings short-circuit the agent call and write a "No commits across N projects in the last 24 hours" file (absence is a fact too). Dry-run firings run the full chain but skip the final file write.

## Ops Digest Formation

Fired every three hours by the in-daemon `ops-digest` sentinel (`0 */3 * * *`), or on demand by `foundry emit ops_digest_started --project system`. Linear chain — reads MBOS JSONL events from the intake directory, applies a pressure gate, then summarises.

```
OpsDigestStarted {project: "system"}
  └─ ObserveEvents (Observer)
       ├─ [gate not satisfied] OpsDigestCompleted {success: true, skipped: true}
       └─ [gate satisfied] OpsObserved {proceed: true, new_event_count, anomaly_present, events: [{...}]}
            └─ SummarizeEvents (Observer, AI Reasoning)
                 └─ OpsSummaryComposed {markdown, event_count, new_watermark?}
                      └─ WriteOpsDigest (Observer)
                           └─ OpsDigestCompleted {success, digest_path?, event_count}
                                └─ writes {FOUNDRY_OPS_DIGESTS_DIR}/{YYYY-MM-DD}.md
                                   advances ~/.foundry/ops-digest.watermark
```

**Pressure gate:** The chain short-circuits (`skipped: true`) if fewer than 25 new events have arrived and none are anomalies. Anomalies include P0-urgency events, `ci_pipeline_failure`, `maintenance_intervention_recorded` with `outcome=unresolved`, `dependency_vulnerability_detected` with severity `high`/`critical`, and `maintenance_run_completed` with `reposFailed > 0`.

**Watermark:** The newest event's `occurredAt` timestamp is written to disk after a successful digest so the next run only processes newer events. First-run lookback is 24 hours (no watermark). Dry-run firings do not advance the watermark.

## Vulnerability Remediation Workflow

Triggered by `foundry emit scan_requested --project <name>`.

```
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

If the main branch is clean (not dirty), the automatic release path fires — see **Release Workflow > Automatic Release** below.

## Pipeline Health Check Workflow

Triggered by `foundry pipeline <project>`. Checks GitHub Actions CI status and auto-remediates failures.

```
PipelineCheckRequested
  └─ CheckPipeline (Observer)
       └─ PipelineChecked {passing: bool, logs: Option<String>}
            └─ RemediatePipeline (Mutator, AI Coding) — self-filters when passing
                 └─ RemediationCompleted
                      └─ CommitAndPush (Mutator)
                           ├─ ProjectChangesCommitted
                           └─ ProjectChangesPushed
```

**CheckPipeline** looks up the project repo and branch from the registry, runs `gh run list` to check status, and if failing fetches failure logs with `gh run view --log-failed`.

**RemediatePipeline** self-filters (skips when pipeline is passing). When failing, invokes Claude with Coding capability and Full access to diagnose and fix CI failures.

## Release Workflow

Triggered by `foundry release <project>` (manual) or automatically after vulnerability remediation (CutRelease).

### Manual Release (via CLI)

```
ReleaseRequested
  └─ ExecuteRelease (Mutator, AI Coding)
       └─ ReleaseCompleted {success, new_tag, release: "manual"}
            └─ WatchPipeline (Observer)
                 └─ ReleasePipelineCompleted
                      └─ InstallLocally (Mutator)
                           └─ LocalInstallCompleted
```

**ExecuteRelease** checks that `actions.release=true` in the registry. Invokes Claude agent with the project's AGENTS.md to run quality gates, update changelog, bump version, commit, tag, and push. If `--bump` is provided, passes it to the agent; otherwise the agent determines the bump from changelog.

### Automatic Release (vulnerability path)

```
MainBranchAudited {dirty: false}
  └─ CutRelease (Mutator, AI Coding)
       └─ ReleaseCompleted {success, new_tag, release: "patch", cve: "..."}
            └─ WatchPipeline (Observer)
                 └─ ReleasePipelineCompleted
                      └─ InstallLocally (Mutator)
                           └─ LocalInstallCompleted
```

**CutRelease** self-filters when `dirty=true`. Invokes Claude agent to cut a patch release for the specific CVE.

Both paths share the same `AgentRelease` work block (ComposedStep architecture) — only the event adapter and output mapper differ.

## Task Block Types

| Kind | Throttle: Full | Throttle: DryRun |
|------|---------------|--------------------|
| **Observer** | Executes and emits | Executes and emits |
| **Mutator** | Executes and emits | Simulates success via `dry_run_events()` |

## File Dependencies

- **`.hone-gates.json`** — Quality gate definitions, read by ResolveGates. Created by `foundry gates --init`.
- **`CHARTER.md`** (or README.md, CLAUDE.md with `## Project Charter`) — Required by CheckCharter for iterate workflow.
- **`~/.claude/agents/{agent}.md`** — Agent instruction file, resolved from registry's `agent` field.
