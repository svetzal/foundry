# Event Types

All event types are defined in `foundry-core/src/event.rs` as the `EventType`
enum. The string representation uses `snake_case`.

Every event carries these common fields:

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Deterministic SHA-256 derived ID, prefixed `evt_` |
| `event_type` | string | Snake-case event type name |
| `project` | string | Project this event relates to |
| `occurred_at` | RFC 3339 timestamp | When the event happened |
| `recorded_at` | RFC 3339 timestamp | When the event was logged |
| `throttle` | string | `full`, `audit_only`, or `dry_run` |
| `payload` | JSON object | Event-type-specific fields (see below) |

## Hello-World (engine validation)

| Type | Description |
|------|-------------|
| `greet_requested` | Request to compose and deliver a greeting |
| `greeting_composed` | Greeting message has been composed |
| `greeting_delivered` | Greeting has been delivered (side effect) |

**`greet_requested` payload**

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Name to greet |

**`greeting_composed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `greeting` | string | Composed greeting text |

## Vulnerability Remediation

| Type | Description |
|------|-------------|
| `scan_requested` | Request to scan a project for known vulnerabilities |
| `vulnerability_detected` | A vulnerability was found (or injected externally) |
| `release_tag_audited` | Latest release tag scanned for the vulnerability |
| `main_branch_audited` | Main branch checked for the same vulnerability |
| `remediation_started` | Automated fix attempt initiated |
| `remediation_completed` | Fix attempt finished (success or failure) |

**`vulnerability_detected` payload**

| Field | Type | Description |
|-------|------|-------------|
| `cve` | string | CVE or advisory ID (e.g. `"CVE-2026-1234"`) |
| `vulnerable` | bool | Whether the project is affected |
| `dirty` | bool (optional) | Whether the main branch still contains the vulnerability |

**`release_tag_audited` payload**

| Field | Type | Description |
|-------|------|-------------|
| `cve` | string | CVE from the scan or forwarded from the trigger |
| `vulnerable` | bool | Whether the release tag is affected |
| `dirty` | bool (optional) | Forwarded from the upstream trigger for downstream routing |

**`main_branch_audited` payload**

| Field | Type | Description |
|-------|------|-------------|
| `cve` | string | CVE identifier |
| `dirty` | bool | `true` if the vulnerability is still present on main |

**`remediation_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `cve` | string | CVE that was remediated |
| `success` | bool | Whether the fix was applied successfully |

## Release Lifecycle

| Type | Description |
|------|-------------|
| `auto_release_triggered` | Decision made to cut a patch release |
| `auto_release_completed` | Release tag created and pushed |
| `release_pipeline_completed` | GitHub Actions build/publish workflow finished |

**`auto_release_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `cve` | string | CVE that prompted the release |
| `release` | string | Release type (e.g. `"patch"`) |
| `new_tag` | string or null | Semver tag extracted from Claude CLI output |
| `success` | bool | Whether the Claude CLI invocation succeeded |

**`release_pipeline_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `status` | string | `"success"` or `"failure"` |
| `conclusion` | string (optional) | GitHub Actions conclusion label |

## Project Lifecycle

| Type | Description |
|------|-------------|
| `project_validation_completed` | Pre-flight checks for a maintenance run |
| `project_iterate_completed` | `hone iterate` finished |
| `project_maintain_completed` | `hone maintain` finished |
| `project_changes_committed` | Git commit created |
| `project_changes_pushed` | Changes pushed to remote |

**`project_validation_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `status` | string | `"ok"`, `"error"`, or `"skipped"` |
| `reason` | string (optional) | Human-readable explanation when status is not `"ok"` |
| `has_gates` | bool (optional) | Whether `.hone-gates.json` is present (only on `"ok"`) |

**`project_iterate_completed` payload**

The payload is the parsed JSON output of `hone iterate --json`. When `hone`
output is not valid JSON, the raw text is captured instead:

| Field | Type | Description |
|-------|------|-------------|
| `raw` | string | Raw stdout from `hone iterate` (when not valid JSON) |
| `exit_code` | number | Process exit code (when not valid JSON) |

When `hone iterate` returns valid JSON, all fields from that JSON object are
present directly in the payload.

**`project_maintain_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `agent` | string | Agent name passed to `hone maintain` |
| `path` | string | Project path |
| `audit_dir` | string | Audit directory (may be empty) |
| `success` | bool | Whether the step succeeded |

**`project_changes_committed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `cve` | string | CVE or `"unknown"` (from remediation path) |
| `message` | string | Git commit message used |

**`project_changes_pushed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `cve` | string | CVE or `"unknown"` (from remediation path) |

## Local Install

| Type | Description |
|------|-------------|
| `local_install_completed` | Local tool reinstallation finished |

## Maintenance Workflow

| Type | Payload | Description |
|------|---------|-------------|
| `iteration_requested` | `{ project }` | Triggers the iterate sub-workflow for a validated project |
| `maintenance_requested` | `{ project }` | Triggers the maintain sub-workflow for a validated project |

## Gate Orchestration

| Type | Description |
|------|-------------|
| `gates_resolved` | Gate definitions loaded from `.hone-gates.json` |
| `preflight_completed` | Gates passed/failed on unmodified codebase |
| `execution_completed` | Code changes applied (emitted by future execution blocks) |
| `gate_verification_completed` | Gates passed/failed after execution |
| `retry_requested` | Gate failure triggers bounded retry |

**`gates_resolved` payload**

| Field | Type | Description |
|-------|------|-------------|
| `project` | string | Project name |
| `workflow` | string | `"iterate"`, `"maintain"`, or `"validate"` |
| `gates` | array | Gate definitions (name, command, required, timeout_secs) |
| `actions` | object (optional) | Forwarded actions from the trigger event |

**`preflight_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `project` | string | Project name |
| `workflow` | string | Workflow that triggered the preflight |
| `all_passed` | bool | Whether every gate passed |
| `required_passed` | bool | Whether all required gates passed |
| `results` | array | Per-gate results (name, passed, required, output, exit_code) |

**`gate_verification_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `project` | string | Project name |
| `workflow` | string | Originating workflow |
| `all_passed` | bool | Whether every gate passed |
| `required_passed` | bool | Whether all required gates passed |
| `retry_count` | number | Current retry count (0 on first attempt) |
| `results` | array | Per-gate results |

**`retry_requested` payload**

| Field | Type | Description |
|-------|------|-------------|
| `project` | string | Project name |
| `workflow` | string | Originating workflow |
| `retry_count` | number | Incremented retry count |
| `failure_context` | string | Gate output from the failed verification |
| `actions` | object (optional) | Forwarded actions |

## Validation

| Type | Description |
|------|-------------|
| `validation_requested` | Request to validate a project's gate health |
| `validation_completed` | Terminal event with per-gate pass/fail results |

**`validation_requested` payload**

| Field | Type | Description |
|-------|------|-------------|
| `project` | string | Project name |

**`validation_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `project` | string | Project name |
| `success` | bool | Whether all required gates passed |
| `results` | array | Per-gate results (name, passed, required, output snippet) |

## Maintenance Run Lifecycle

| Type | Description |
|------|-------------|
| `maintenance_run_started` | A maintenance run was triggered for a project |
| `maintenance_run_completed` | All projects processed, summary available |

**`maintenance_run_started` payload**

| Field | Type | Description |
|-------|------|-------------|
| `project` | string | Project name this run covers |

**`maintenance_run_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `total` | number | Total number of projects processed |
| `succeeded` | number | Projects that completed successfully |
| `failed` | number | Projects that encountered an error |
| `skipped` | number | Projects that were skipped (already active or `skip=true`) |
| `projects` | array | Per-project result objects (name, status, duration_secs) |

## Release Tag Audit

| Type | Description |
|------|-------------|
| `release_tag_audited` | Latest release tag scanned (see payload above) |
