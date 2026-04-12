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

Event types use PascalCase in code and snake_case on the wire (e.g., `ReleaseRequested` → `release_requested`).

### Vulnerability Remediation Workflow
| Event | Category |
|-------|----------|
| `ScanRequested` | Command |
| `VulnerabilityDetected` | Domain fact |
| `MainBranchAudited` | Domain fact |
| `ReleaseTagAudited` | Domain fact |
| `RemediationStarted` | Lifecycle start |
| `RemediationCompleted` | Lifecycle end |
| `ReleaseRequested` | Command |
| `ReleaseCompleted` | Lifecycle end |
| `ReleasePipelineCompleted` | Lifecycle end |
| `LocalInstallCompleted` | Lifecycle end |

### Project Lifecycle (Cross-workflow)
| Event | Category |
|-------|----------|
| `ProjectValidationCompleted` | Lifecycle end |
| `ProjectIterationCompleted` | Lifecycle end |
| `ProjectMaintenanceCompleted` | Lifecycle end |
| `ProjectChangesCommitted` | Domain fact |
| `ProjectChangesPushed` | Domain fact |

### Workflow Triggers
| Event | Category |
|-------|----------|
| `IterationRequested` | Command |
| `MaintenanceRequested` | Command |
| `ValidationRequested` | Command |
| `DriftAssessmentRequested` | Command |
| `PipelineCheckRequested` | Command |

### Run Lifecycle
| Event | Category |
|-------|----------|
| `MaintenanceRunStarted` | Lifecycle start |
| `MaintenanceRunCompleted` | Lifecycle end |

### Gate Orchestration
| Event | Category |
|-------|----------|
| `GateResolutionCompleted` | Lifecycle end |
| `PreflightCompleted` | Lifecycle end |
| `ExecutionCompleted` | Lifecycle end |
| `GateVerificationCompleted` | Lifecycle end |
| `RetryRequested` | Command |
| `SummarizeCompleted` | Lifecycle end |

### Iterate Workflow (Phase 3)
| Event | Category |
|-------|----------|
| `CharterCheckCompleted` | Lifecycle end |
| `AssessmentCompleted` | Lifecycle end |
| `TriageCompleted` | Lifecycle end |
| `PlanCompleted` | Lifecycle end |

### Pipeline Health Check
| Event | Category |
|-------|----------|
| `PipelineChecked` | Domain fact |

### Drift Scout
| Event | Category |
|-------|----------|
| `DriftAssessmentCompleted` | Lifecycle end |

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
| `PipelineChecked` | `passing`, `logs` |
| `ReleaseCompleted` | `release` ("patch"/"manual"), `new_tag`, `success`, `cve` (vuln path only) |
| `ReleasePipelineCompleted` | `success`, `new_tag` |
| `LocalInstallCompleted` | `success` |
