# Event Model

Foundry's event model defines the vocabulary of immutable facts that flow
through the system. Events carry payloads describing what occurred but have
no opinion about what should happen next — task blocks make those decisions.

## Authoritative Source

The canonical event type definitions live in
`foundry-core/src/event.rs` (`EventType` enum). The implementation
roadmap for completing all task blocks and workflows is in
`IMPLEMENTATION_PLAN.md` at the project root.

## Event Categories

### Run Lifecycle

| Event | Emitter | Purpose |
|-------|---------|---------|
| `maintenance_run_started` | Orchestrator / manual | Begins a per-project maintenance chain |
| `maintenance_run_completed` | Orchestrator (fan-in) | All projects finished; carries aggregate results |

### Per-Project: Iterate / Maintain

| Event | Emitter | Purpose |
|-------|---------|---------|
| `project_validation_completed` | ValidateProject | Pre-flight check (dir, branch, gates) |
| `project_iterate_completed` | RunHoneIterate | One structural improvement attempted |
| `project_maintain_completed` | RunHoneMaintain | Dependencies updated, gates verified |
| `project_changes_committed` | CommitAndPush | Git commit created |
| `project_changes_pushed` | CommitAndPush | Pushed to remote |

### Per-Project: Release Audit

| Event | Emitter | Purpose |
|-------|---------|---------|
| `release_tag_audited` | AuditReleaseTag | Latest tag scanned for vulnerabilities |
| `main_branch_audited` | AuditMainBranch | Main branch checked for same vulnerability |
| `auto_release_triggered` | Audit chain | Intent to cut a patch release |
| `auto_release_completed` | CutRelease | Tag pushed |

### Vulnerability Remediation

| Event | Emitter | Purpose |
|-------|---------|---------|
| `vulnerability_detected` | External / nightly audit | Entry point for remediation workflow |
| `remediation_started` | RemediateVulnerability | Fix attempt underway |
| `remediation_completed` | RemediateVulnerability | Fix attempt finished (success or failure) |

### Distribution Pipeline

| Event | Emitter | Purpose |
|-------|---------|---------|
| `release_pipeline_completed` | WatchPipeline | GitHub Actions finished building and publishing |
| `local_install_completed` | InstallLocally | Tool reinstalled on local machine |

## Event Structure

Every event has:

- **id** — Deterministic SHA256 hash of (type + project + occurred_at + payload)
- **event_type** — One of the `EventType` enum variants
- **project** — Which project this event relates to
- **occurred_at / recorded_at** — When it happened vs. when it was logged
- **throttle** — Propagated through the chain to control downstream behaviour
- **payload** — Event-type-specific JSON data

## Payload Conventions

Downstream blocks read payload fields to make routing decisions (self-filtering).
The engine routes by event type only — it cannot inspect payloads.

| Field | Used By | Values |
|-------|---------|--------|
| `vulnerable` | AuditMainBranch | `true`/`false` — whether the tag has known CVEs |
| `dirty` | RemediateVulnerability, CutRelease | `true`/`false` — whether main still has the vulnerability |
| `cve` | All vulnerability blocks | CVE identifier string |
| `status` | Downstream blocks | `"ok"`/`"error"` — validation and completion status |
| `has_changes` | CommitAndPush | Whether there are uncommitted changes to persist |
