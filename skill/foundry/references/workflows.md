# Foundry Workflow Reference

## Iterate Workflow

Triggered by `foundry iterate <project>` or routed from a maintenance run when `actions.iterate=true`.

```
IterationRequested
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

**Maintain chaining:** If the trigger payload has `actions.maintain=true` and iterate succeeds, RouteGateResult also emits `MaintenanceRequested` to chain into the maintain workflow.

## Maintain Workflow

Triggered by `foundry emit maintenance_requested` or chained from iterate.

```
MaintenanceRequested
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

Triggered by `foundry run`. Fan-out across all active projects.

```
MaintenanceRunStarted {project: "system"}
  └─ FanOutMaintenance (Observer)
       ├─ MaintenanceRunStarted {project: "alpha"}
       │    └─ ValidateProject → ProjectValidationCompleted
       │         └─ RouteProjectWorkflow
       │              └─ IterationRequested or MaintenanceRequested (per project flags)
       │                   └─ ... (iterate or maintain chain)
       ├─ MaintenanceRunStarted {project: "beta"}
       │    └─ ... (same pattern)
       └─ MaintenanceRunCompleted {project_count, skipped_count}
            └─ GenerateSummary → writes audit report
```

For single-project runs (`foundry run --project alpha`), there's no fan-out — the project chain runs directly.

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

If the main branch is clean (not dirty), the release path fires:
```
RemediationCompleted
  └─ CutRelease (Mutator) → ReleaseRequested
       └─ WatchPipeline (Observer) → ReleasePipelineCompleted
            └─ InstallLocally (Mutator) → LocalInstallCompleted
```

## Task Block Types

| Kind | Throttle: Full | Throttle: AuditOnly | Throttle: DryRun |
|------|---------------|--------------------|--------------------|
| **Observer** | Executes and emits | Executes and emits | Executes and emits |
| **Mutator** | Executes and emits | Logs but doesn't deliver downstream | Simulates success via `dry_run_events()` |

## File Dependencies

- **`.hone-gates.json`** — Quality gate definitions, read by ResolveGates. Created by `foundry gates --init`.
- **`CHARTER.md`** (or README.md, CLAUDE.md with `## Project Charter`) — Required by CheckCharter for iterate workflow.
- **`~/.claude/agents/{agent}.md`** — Agent instruction file, resolved from registry's `agent` field.
