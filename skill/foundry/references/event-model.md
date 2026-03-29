# Foundry Event Model Reference

## Event Structure

Every event in Foundry has this shape:

```json
{
  "id": "evt_a1b2c3d4e5f6",
  "event_type": "iteration_requested",
  "project": "my-project",
  "occurred_at": "2026-03-29T12:34:56.789Z",
  "recorded_at": "2026-03-29T12:34:56.789Z",
  "throttle": 0,
  "payload": {}
}
```

- **id** — Deterministic SHA256 hash of (event_type, project, occurred_at, payload), prefixed `evt_`. Same inputs always produce the same ID.
- **throttle** — 0 = Full, 1 = AuditOnly, 2 = DryRun. Propagated through the entire chain.
- **payload** — Event-specific JSON. Completion events use `"success": true/false`.

## Naming Conventions

Events follow four suffix categories:

| Category | Suffix | Meaning |
|----------|--------|---------|
| Command | `*Requested` | Intent — someone or something wants action taken |
| Lifecycle start | `*Started` | A multi-step operation began |
| Lifecycle end | `*Completed` | An operation finished (check payload for success/failure) |
| Domain fact | Specific past participle | A meaningful domain event where the verb adds clarity |

Rules:
- Commands are always `*Requested` — never `*Triggered`.
- `*Completed` is the default for lifecycle endpoints.
- `*Started`/`*Completed` must pair.
- Noun form for compound prefixes (e.g., `ProjectIterationCompleted`, not `ProjectIterateCompleted`).
- Payload boolean results use `success` (not `passed` or other variants).

## Complete Event Type List

### Vulnerability Remediation Workflow
| Event | Snake Case | Category |
|-------|-----------|----------|
| `ScanRequested` | `scan_requested` | Command |
| `VulnerabilityDetected` | `vulnerability_detected` | Domain fact |
| `MainBranchAudited` | `main_branch_audited` | Domain fact |
| `ReleaseTagAudited` | `release_tag_audited` | Domain fact |
| `RemediationStarted` | `remediation_started` | Lifecycle start |
| `RemediationCompleted` | `remediation_completed` | Lifecycle end |
| `ReleaseRequested` | `release_requested` | Command |
| `ReleaseCompleted` | `release_completed` | Lifecycle end |
| `ReleasePipelineCompleted` | `release_pipeline_completed` | Lifecycle end |
| `LocalInstallCompleted` | `local_install_completed` | Lifecycle end |

### Project Lifecycle (Cross-workflow)
| Event | Snake Case | Category |
|-------|-----------|----------|
| `ProjectValidationCompleted` | `project_validation_completed` | Lifecycle end |
| `ProjectIterationCompleted` | `project_iteration_completed` | Lifecycle end |
| `ProjectMaintenanceCompleted` | `project_maintenance_completed` | Lifecycle end |
| `ProjectChangesCommitted` | `project_changes_committed` | Domain fact |
| `ProjectChangesPushed` | `project_changes_pushed` | Domain fact |

### Workflow Triggers
| Event | Snake Case | Category |
|-------|-----------|----------|
| `IterationRequested` | `iteration_requested` | Command |
| `MaintenanceRequested` | `maintenance_requested` | Command |
| `ValidationRequested` | `validation_requested` | Command |
| `DriftAssessmentRequested` | `drift_assessment_requested` | Command |

### Run Lifecycle
| Event | Snake Case | Category |
|-------|-----------|----------|
| `MaintenanceRunStarted` | `maintenance_run_started` | Lifecycle start |
| `MaintenanceRunCompleted` | `maintenance_run_completed` | Lifecycle end |

### Gate Orchestration
| Event | Snake Case | Category |
|-------|-----------|----------|
| `GateResolutionCompleted` | `gate_resolution_completed` | Lifecycle end |
| `PreflightCompleted` | `preflight_completed` | Lifecycle end |
| `ExecutionCompleted` | `execution_completed` | Lifecycle end |
| `GateVerificationCompleted` | `gate_verification_completed` | Lifecycle end |
| `RetryRequested` | `retry_requested` | Command |
| `SummarizeCompleted` | `summarize_completed` | Lifecycle end |

### Iterate Workflow (Phase 3)
| Event | Snake Case | Category |
|-------|-----------|----------|
| `CharterCheckCompleted` | `charter_check_completed` | Lifecycle end |
| `AssessmentCompleted` | `assessment_completed` | Lifecycle end |
| `TriageCompleted` | `triage_completed` | Lifecycle end |
| `PlanCompleted` | `plan_completed` | Lifecycle end |

### Drift Scout
| Event | Snake Case | Category |
|-------|-----------|----------|
| `DriftAssessmentCompleted` | `drift_assessment_completed` | Lifecycle end |

## Key Payload Fields by Event

| Event | Key Payload Fields |
|-------|-------------------|
| `VulnerabilityDetected` | `cve`, `vulnerable`, `dirty`, `package`, `severity` |
| `GateResolutionCompleted` | `project`, `workflow` ("iterate"/"maintain"/"validate"), `gates[]`, `actions` |
| `PreflightCompleted` | `all_passed`, `required_passed`, `results[]`, `workflow` |
| `ExecutionCompleted` | `success`, `workflow`, `summary` |
| `GateVerificationCompleted` | `required_passed`, `all_passed`, `retry_count`, `results[]` |
| `CharterCheckCompleted` | `success`, `sources[]`, `guidance` |
| `AssessmentCompleted` | `success`, `severity`, `principle`, `category`, `assessment` |
| `TriageCompleted` | `accepted`, `reason` |
| `DriftAssessmentCompleted` | `candidate_count`, `high_value_count`, `candidates[]` |
| `ProjectIterationCompleted` | `success`, `project` |
| `ProjectMaintenanceCompleted` | `success`, `project` |
| `MaintenanceRunCompleted` | `project_count`, `skipped_count`, `projects[]`, `root_event_id` (service-level only) |
