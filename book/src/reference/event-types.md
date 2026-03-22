# Event Types

All event types are defined in `foundry-core/src/event.rs` as the
`EventType` enum. The string representation uses `snake_case`.

## Hello-World (engine validation)

| Type | Description |
|------|-------------|
| `greet_requested` | Request to compose and deliver a greeting |
| `greeting_composed` | Greeting message has been composed |
| `greeting_delivered` | Greeting has been delivered (side effect) |

## Vulnerability Remediation

| Type | Description |
|------|-------------|
| `vulnerability_detected` | A vulnerability was found in a project's release |
| `main_branch_audited` | Main branch checked for the same vulnerability |
| `remediation_started` | Automated fix attempt initiated |
| `remediation_completed` | Fix attempt finished (success or failure) |
| `release_tag_audited` | Release tag scanned for vulnerabilities |

## Release Lifecycle

| Type | Description |
|------|-------------|
| `auto_release_triggered` | Decision made to cut a patch release |
| `auto_release_completed` | Release tag pushed to remote |
| `release_pipeline_completed` | GitHub Actions build/publish workflow finished |

## Project Lifecycle

| Type | Description |
|------|-------------|
| `project_validation_completed` | Pre-flight checks passed or failed |
| `project_iterate_completed` | Hone iterate finished |
| `project_maintain_completed` | Hone maintain finished |
| `project_changes_committed` | Git commit created |
| `project_changes_pushed` | Changes pushed to remote |

## Local Install

| Type | Description |
|------|-------------|
| `local_install_completed` | Local tool reinstallation finished |

## Maintenance Workflow

| Type | Payload | Description |
|------|---------|-------------|
| `iteration_requested` | `{ project }` | Triggers the iterate sub-workflow for a validated project |
| `maintenance_requested` | `{ project }` | Triggers the maintain sub-workflow for a validated project |

## Maintenance Run

| Type | Description |
|------|-------------|
| `maintenance_run_started` | Nightly (or manual) maintenance run began |
| `maintenance_run_completed` | All projects processed, summary generated |
