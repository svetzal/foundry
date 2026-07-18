# Event Types

All event types are defined in `foundry-sdk/src/event.rs` as the `EventType`
enum. The string representation uses `snake_case`.

Every event carries these common fields:

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Deterministic SHA-256 derived ID, prefixed `evt_` |
| `event_type` | string | Snake-case event type name |
| `project` | string | Project this event relates to |
| `occurred_at` | RFC 3339 timestamp | When the event happened |
| `recorded_at` | RFC 3339 timestamp | When the event was logged |
| `throttle` | string | `full` or `dry_run` |
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
| `release_requested` | Decision made to cut a patch release |
| `release_completed` | Release tag created and pushed |
| `release_pipeline_completed` | GitHub Actions build/publish workflow finished |

**`release_completed` payload**

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
| `project_iteration_completed` | Iterate workflow finished |
| `project_maintenance_completed` | Maintain workflow finished |
| `project_changes_committed` | Git commit created |
| `project_changes_pushed` | Changes pushed to remote |

**`project_validation_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `status` | string | `"ok"`, `"error"`, or `"skipped"` |
| `reason` | string (optional) | Human-readable explanation when status is not `"ok"` |
| `has_gates` | bool (optional) | Whether `.hone-gates.json` is present (only on `"ok"`) |

**`project_iteration_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `project` | string | Project name |
| `workflow` | string | `"iterate"` |
| `success` | bool | Whether the iterate workflow succeeded |
| `summary` | string | Human-readable summary of the result |
| `changes` | bool (optional) | Whether code changes were made |

**`project_maintenance_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `project` | string | Project name |
| `workflow` | string | `"maintain"` |
| `success` | bool | Whether the maintain workflow succeeded |
| `summary` | string | Human-readable summary of the result |
| `changes` | bool (optional) | Whether code changes were made |

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

## Task Lifecycle

| Type | Description |
|------|-------------|
| `task_run_started` | Isolated one-shot task execution began |
| `task_reviewed` | Skeptical review produced a structural verdict |
| `task_run_completed` | Task work was landed, durably preserved, or completed with no landing required |

**`task_run_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `project` | string | Registered project name |
| `success` | bool | `true` only for a `complete` verdict |
| `landed` | bool | `true` only when complete work was actually fast-forwarded onto trunk |
| `summary` | string | Human-readable terminal summary |
| `verdict` | string | `complete`, `remainder`, `defect`, `blocked_on_decision`, or `runner_error` |
| `preservation_ref` | string (optional) | Continuation ref: landed commit SHA, or remote branch / `bundle:<path>` for preserved work |
| `campaign` | string (optional) | Campaign that dispatched the task |

Verdict-specific fields are `gaps[]`, `diagnosis`, `finding` plus `options[]`,
or `detail`.

## Campaign Formation

| Type | Description |
|------|-------------|
| `campaign_advance_requested` | Request to re-evaluate a durable campaign |
| `campaign_advance_completed` | Formation chose done, one next objective, or escalation |
| `campaign_escalated` | Campaign halted for budget, failure, rule, or owner judgment |
| `campaign_completed` | Required done evidence proved the mission complete |

**`campaign_advance_requested` payload**

| Field | Type | Description |
|-------|------|-------------|
| `campaign` | string | Campaign name |
| `run_event_id` | string (optional) | Typed task result that triggered the advance |
| `run_result` | object (optional) | Full `task_run_completed` payload |

**`campaign_advance_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `campaign` | string | Campaign name |
| `project` | string | Registered project name |
| `cycles_completed` | integer | Tasks dispatched by the campaign |
| `cycles_landed` | integer | Task results whose `task_run_completed.landed` field was `true` |
| `decision` | string | `done`, `advance`, or `escalate` |
| `objective` | string (advance only) | Exactly one next task objective |
| `reason` | string | Evidence or gap supporting the decision |

## Gate Orchestration

| Type | Description |
|------|-------------|
| `gate_resolution_completed` | Gate definitions loaded from `.hone-gates.json` |
| `preflight_completed` | Gates passed/failed on unmodified codebase |
| `execution_completed` | Code changes applied (emitted by future execution blocks) |
| `gate_verification_completed` | Gates passed/failed after execution |
| `retry_requested` | Gate failure triggers bounded retry |

**`gate_resolution_completed` payload**

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
| `results` | array | Per-gate results (name, command, passed, required, output, exit_code, duration_ms?, fix_applied?) |

Each `results[]` entry includes an optional `duration_ms` field (unsigned integer) recording how long the gate command took in milliseconds. This field is absent when loading results from events persisted before timing instrumentation was added.

A `results[]` entry also carries an optional `fix_applied` boolean: `true` when the gate initially failed but its `fix_command` repaired the working tree and the re-check then passed (a self-healed gate). The field is omitted when false, so it is absent for gates that passed clean and for events persisted before self-healing gates were added.

**`gate_verification_completed` payload**

| Field | Type | Description |
|-------|------|-------------|
| `project` | string | Project name |
| `workflow` | string | Originating workflow |
| `all_passed` | bool | Whether every gate passed |
| `required_passed` | bool | Whether all required gates passed |
| `retry_count` | number | Current retry count (0 on first attempt) |
| `results` | array | Per-gate results (name, command, passed, required, output, exit_code, duration_ms?, fix_applied?) |

Each `results[]` entry includes an optional `duration_ms` field (unsigned integer) recording how long the gate command took in milliseconds. This field is absent when loading results from events persisted before timing instrumentation was added.

A `results[]` entry also carries an optional `fix_applied` boolean: `true` when the gate initially failed but its `fix_command` repaired the working tree and the re-check then passed (a self-healed gate). The field is omitted when false, so it is absent for gates that passed clean and for events persisted before self-healing gates were added.

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

## Agent Session Lifecycle

Emitted by `foundryd` whenever a Foundry-launched Claude Code agent session
begins or ends. Used by visualisation tools (e.g. `ops-visualizer`) to show
in-flight and historical agent activity, and to locate the per-session
stream-json transcript on disk.

| Type | Description |
|------|-------------|
| `agent_session_started` | An agent session has begun; transcript file path is included |
| `agent_session_ended` | The agent session has finished (success, failure, or unavailable) |

**`agent_session_started` payload**

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | UUID identifying this session; matches the transcript file basename |
| `agent_type` | string | Agent runtime (currently always `claude-code`) |
| `project` | string | Project name (may be empty in v1) |
| `working_dir` | string | Absolute path of the working directory the agent ran in |
| `source_log_path` | string | Absolute path to the per-session JSONL transcript (`~/.foundry/agent-sessions/<session_id>.jsonl`) |
| `capability` | string | Capability label: `reasoning`, `coding`, or `quick` |
| `access` | string | Tool access level: `read_only` or `full` |
| `started_at` | RFC 3339 timestamp | When the session was launched |
| `trace_id` | string | Correlating trace ID (may be empty in v1) |

**`agent_session_ended` payload**

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | string | UUID identifying this session (matches `agent_session_started`) |
| `status` | string | Outcome: `ok`, `agent_failed`, or `unavailable` |
| `exit_code` | number | Process exit code (omitted when the agent could not be invoked) |
| `ended_at` | RFC 3339 timestamp | When the session finished |
| `bytes_written` | number | Total bytes streamed to the transcript file |
| `error` | string | Error message when `status = unavailable` (omitted otherwise) |
