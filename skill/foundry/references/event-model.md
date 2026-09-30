# Foundry Event Model Reference

## Event Structure

Every event in Foundry has this shape:

```json
{
  "id": "evt_a1b2c3d4e5f6",
  "event_type": "project_iteration_requested",
  "project": "my-project",
  "occurred_at": "2026-03-29T12:34:56.789Z",
  "recorded_at": "2026-03-29T12:34:56.789Z",
  "throttle": 0,
  "payload": {}
}
```

- **id** — Deterministic SHA256 hash of (event_type, project, occurred_at,
  payload), prefixed `evt_`. Same inputs always produce the same ID.
- **throttle** — 0 = Full, 1 = DryRun. Propagated through the entire chain.
- **payload** — Event-specific JSON. Completion events use
  `"success": true/false`.

> **Tracing:** Every event also carries `trace_id`, `span_id`, `parent_span_id`,
> and `causation_id` fields used for OTel-shaped nested tracing and domain
> causality. Campaign task-side events also carry `campaign_cycle`, so cycles
> remain distinguishable when campaigns run concurrently in the same project.
> See `book/src/architecture/tracing.md` for details.

## Naming Conventions

Events follow four suffix categories:

| Category        | Suffix                   | Meaning                                                   |
| --------------- | ------------------------ | --------------------------------------------------------- |
| Command         | `*Requested`             | Intent — someone or something wants action taken          |
| Lifecycle start | `*Started`               | A multi-step operation began                              |
| Lifecycle end   | `*Completed`             | An operation finished (check payload for success/failure) |
| Domain fact     | Specific past participle | A meaningful domain event where the verb adds clarity     |

Rules:

- Commands are always `*Requested` — never `*Triggered`.
- `*Completed` is the default for lifecycle endpoints.
- `*Started`/`*Completed` must pair.
- Noun form for compound prefixes (e.g., `ProjectIterationCompleted`, not
  `ProjectIterateCompleted`).
- Payload boolean results use `success` (not `passed` or other variants).

## Complete Event Type List

Event types use PascalCase in code and snake_case on the wire (e.g.,
`ReleaseRequested` → `release_requested`).

### Vulnerability Remediation Workflow

| Event                      | Category        |
| -------------------------- | --------------- |
| `ScanRequested`            | Command         |
| `VulnerabilityDetected`    | Domain fact     |
| `MainBranchAudited`        | Domain fact     |
| `ReleaseTagAudited`        | Domain fact     |
| `RemediationStarted`       | Lifecycle start |
| `RemediationCompleted`     | Lifecycle end   |
| `ReleaseRequested`         | Command         |
| `ReleaseCompleted`         | Lifecycle end   |
| `ReleasePipelineCompleted` | Lifecycle end   |
| `LocalInstallCompleted`    | Lifecycle end   |

### Project Lifecycle (Cross-workflow)

| Event                         | Category      |
| ----------------------------- | ------------- |
| `ProjectValidationCompleted`  | Lifecycle end |
| `ProjectIterationCompleted`   | Lifecycle end |
| `ProjectMaintenanceCompleted` | Lifecycle end |
| `ProjectChangesCommitted`     | Domain fact   |
| `ProjectChangesPushed`        | Domain fact   |

### Workflow Triggers

| Event                         | Category |
| ----------------------------- | -------- |
| `ProjectIterationRequested`   | Command  |
| `ProjectMaintenanceRequested` | Command  |
| `ExecutionRequested`          | Command  |
| `ValidationRequested`         | Command  |
| `DriftAssessmentRequested`    | Command  |
| `PipelineCheckRequested`      | Command  |

### Task Lifecycle

| Event              | Category        |
| ------------------ | --------------- |
| `TaskRunStarted`   | Lifecycle start |
| `TaskReviewed`     | Domain fact     |
| `TaskRunCompleted` | Lifecycle end   |

### Agent Sessions

| Event                 | Category        |
| --------------------- | --------------- |
| `AgentSessionStarted` | Lifecycle start |
| `AgentSessionEnded`   | Lifecycle end   |

Both carry `session_id`. `AgentSessionEnded.status` is `ok`, `agent_failed`,
`unavailable`, or `interrupted`. An agent session dies when `foundryd` stops,
and nothing records its end then. On the next start, before it accepts work,
`foundryd` looks back 7 days in the event log for sessions with a start and no
end. It records an `AgentSessionEnded` for each, with status `interrupted`,
`error` `daemon restarted`, the original session's project and trace, and
`ended_at` set to the daemon start time. These ends are in the log, so a later
start does not end the same session again.

### Work-Item Ledger

One durable record per unit of work Foundry dispatched, in
`~/.foundry/work-items.json`. `WorkItemStarted` pairs with `WorkItemSettled`
rather than a `*Completed`: an item does not complete, it settles into a state
that may still hold an obligation (`preserved`, `needs_decision`, `failed`).

| Event               | Category        |
| ------------------- | --------------- |
| `WorkItemSubmitted` | Domain fact     |
| `WorkItemStarted`   | Lifecycle start |
| `WorkItemSettled`   | Domain fact     |
| `WorkItemCancelled` | Domain fact     |

Each carries `item_id`, `project`, `objective`, `kind` (`task`,
`campaign_cycle`, `maintenance`, `major_upgrade`, `release`, `remediation`),
`lane` (`interactive`, `campaign`, `maintenance`), `state`, `reason` and
`origin`; a settlement also carries a `disposition` with the verdict, the
landed commit or preservation ref, the worktree path and whether it was
removed.

`WorkItemCancelled` is emitted only by `foundry campaign cancel --now`, which
kills the in-flight cycle so no `TaskRunCompleted` can settle its item. That
item settles `cancelled` with the operator's `--reason` as its reason, and the
event rides the aborted cycle's trace rather than the cancellation's. A graceful
cancel emits nothing here — its cycle finishes and settles the usual way.

Read the ledger with `foundry queue` (running, queued, open and the newest 20
settled items), `foundry queue open` (only the three open states) and
`foundry queue show <item_id>` (one item's full record, followed by that item's
own `work_item_*` events from the durable event log — selected by payload
`item_id`, never by trace or project, oldest first). All three take `--json`
and `--offline`, and all three are read-only.

### Campaign Formation

| Event                      | Category      |
| -------------------------- | ------------- |
| `CampaignAdvanceRequested` | Command       |
| `CampaignAdvanceCompleted` | Lifecycle end |
| `CampaignPaused`           | Domain fact   |
| `CampaignEscalated`        | Domain fact   |
| `CampaignCompleted`        | Lifecycle end |

### Run Lifecycle

| Event                       | Category                               |
| --------------------------- | -------------------------------------- |
| `MaintenanceCycleStarted`   | Lifecycle start (system-level fan-out) |
| `MaintenanceCycleCompleted` | Lifecycle end (system-level fan-out)   |
| `ProjectRunStarted`         | Lifecycle start (per-project)          |
| `ProjectRunCompleted`       | Lifecycle end (per-project)            |

### Gate Orchestration

| Event                       | Category      |
| --------------------------- | ------------- |
| `GateResolutionCompleted`   | Lifecycle end |
| `PreflightCompleted`        | Lifecycle end |
| `ExecutionCompleted`        | Lifecycle end |
| `GateVerificationCompleted` | Lifecycle end |
| `RetryRequested`            | Command       |
| `SummarizeCompleted`        | Lifecycle end |

### Iterate Workflow (Phase 3)

| Event                   | Category      |
| ----------------------- | ------------- |
| `CharterCheckCompleted` | Lifecycle end |
| `AssessmentCompleted`   | Lifecycle end |
| `TriageCompleted`       | Lifecycle end |
| `PlanCompleted`         | Lifecycle end |

### Pipeline Health Check

| Event             | Category    |
| ----------------- | ----------- |
| `PipelineChecked` | Domain fact |

### Drift Scout

| Event                      | Category      |
| -------------------------- | ------------- |
| `DriftAssessmentCompleted` | Lifecycle end |

### Dependency Update Policy

| Event                         | Category                                      |
| ----------------------------- | --------------------------------------------- |
| `DependencyReviewRequested`   | Command (span opener) — `foundry deps`        |
| `DependencyUpdatesClassified` | Domain fact — phase `before`, `after`, `review` |
| `MajorUpgradesPlanned`        | Domain fact — the majors lane's decisions     |

### Ops Digest Formation

| Event                 | Category                      |
| --------------------- | ----------------------------- |
| `OpsDigestStarted`    | Lifecycle start (span opener) |
| `OpsObserved`         | Domain fact                   |
| `OpsSummaryCompleted` | Lifecycle end                 |
| `OpsDigestCompleted`  | Lifecycle end                 |

## Key Payload Fields by Event

| Event                         | Key Payload Fields                                                                                                                              |
| ----------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| `VulnerabilityDetected`       | `cve`, `vulnerable`, `dirty`, `package`, `severity`                                                                                             |
| `GateResolutionCompleted`     | `project`, `workflow` ("iterate"/"maintain"/"validate"), `gates[]`, `actions`                                                                   |
| `PreflightCompleted`          | `all_passed`, `required_passed`, `results[]`, `workflow`                                                                                        |
| `ExecutionCompleted`          | `success`, `workflow`, `summary`                                                                                                                |
| `GateVerificationCompleted`   | `required_passed`, `all_passed`, `retry_count`, `results[]`                                                                                     |
| `CharterCheckCompleted`       | `success`, `sources[]`, `guidance`                                                                                                              |
| `AssessmentCompleted`         | `success`, `severity`, `principle`, `category`, `assessment`                                                                                    |
| `TriageCompleted`             | `accepted`, `reason`                                                                                                                            |
| `DriftAssessmentCompleted`    | `candidate_count`, `high_value_count`, `candidates[]`                                                                                           |
| `ProjectIterationCompleted`   | `success`, `project`                                                                                                                            |
| `ProjectMaintenanceCompleted` | `success`, `project`                                                                                                                            |
| `MaintenanceCycleCompleted`   | `project_count`, `skipped_count`, `projects[]`, `root_event_id` (service-level fan-out only)                                                    |
| `PipelineChecked`             | `passing`, `logs`                                                                                                                               |
| `ReleaseCompleted`            | `release` ("patch"/"manual"), `new_tag`, `success`, `cve` (vuln path only)                                                                      |
| `ReleasePipelineCompleted`    | `success`, `new_tag`                                                                                                                            |
| `LocalInstallCompleted`       | `success`                                                                                                                                       |
| `OpsObserved`                 | `proceed`, `new_event_count`, `anomaly_present`, `new_watermark?`, `events[{id, event_type, occurred_at, domain, urgency?, summary?, client?}]` |
| `OpsSummaryCompleted`         | `markdown`, `event_count`, `new_watermark?`                                                                                                     |
| `OpsDigestCompleted`          | `success`, `skipped`, `digest_path?`, `event_count`                                                                                             |
| `TaskReviewed`                | `objective`, `review`, `gate_results[]`, structural verdict fields, `campaign?`                                                                 |
| `TaskRunCompleted`            | `success`, `landed`, `summary`, `preservation_ref?`, `land_blocked?`, `trunk_arrivals?`, structural verdict fields, `campaign?`                                                  |
| `CampaignAdvanceRequested`    | `campaign`, `run_event_id?`, `run_result?`                                                                                                      |
| `CampaignAdvanceCompleted`    | `campaign`, `cycles_completed`, `cycles_landed`, `decision`, `objective?`, `reason`                                                             |
| `CampaignEscalated`           | `campaign`, `reason`, `cycles_completed`, `cycles_landed`                                                                                       |
| `CampaignCompleted`           | `campaign`, `reason`, `cycles_completed`, `cycles_landed`                                                                                       |
