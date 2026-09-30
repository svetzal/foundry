# Event Types

All event types are defined in `foundry-sdk/src/event.rs` as the `EventType`
enum. The string representation uses `snake_case`.

Every event carries these common fields:

| Field            | Type               | Description                                       |
| ---------------- | ------------------ | ------------------------------------------------- |
| `id`             | string             | Deterministic SHA-256 derived ID, prefixed `evt_` |
| `event_type`     | string             | Snake-case event type name                        |
| `project`        | string             | Project this event relates to                     |
| `occurred_at`    | RFC 3339 timestamp | When the event happened                           |
| `recorded_at`    | RFC 3339 timestamp | When the event was logged                         |
| `throttle`       | string             | `full` or `dry_run`                               |
| `payload`        | JSON object        | Event-type-specific fields (see below)            |
| `trace_id`       | string or null     | OTel trace identity                               |
| `span_id`        | string or null     | Current workflow span identity                    |
| `parent_span_id` | string or null     | Parent span identity                              |
| `causation_id`   | string or null     | Event that caused this event                      |
| `gather_id`      | string or null     | Scatter/gather group identity                     |

## Hello-World (engine validation)

| Type                 | Description                               |
| -------------------- | ----------------------------------------- |
| `greet_requested`    | Request to compose and deliver a greeting |
| `greeting_composed`  | Greeting message has been composed        |
| `greeting_delivered` | Greeting has been delivered (side effect) |

**`greet_requested` payload**

| Field  | Type   | Description   |
| ------ | ------ | ------------- |
| `name` | string | Name to greet |

**`greeting_composed` payload**

| Field      | Type   | Description            |
| ---------- | ------ | ---------------------- |
| `greeting` | string | Composed greeting text |

## Vulnerability Remediation

| Type                     | Description                                         |
| ------------------------ | --------------------------------------------------- |
| `scan_requested`         | Request to scan a project for known vulnerabilities |
| `vulnerability_detected` | A vulnerability was found (or injected externally)  |
| `release_tag_audited`    | Latest release tag scanned for the vulnerability    |
| `main_branch_audited`    | Main branch checked for the same vulnerability      |
| `remediation_started`    | Automated fix attempt initiated                     |
| `remediation_completed`  | Fix attempt finished (success or failure)           |

**`vulnerability_detected` payload**

| Field        | Type            | Description                                              |
| ------------ | --------------- | -------------------------------------------------------- |
| `cve`        | string          | CVE or advisory ID (e.g. `"CVE-2026-1234"`)              |
| `vulnerable` | bool            | Whether the project is affected                          |
| `dirty`      | bool (optional) | Whether the main branch still contains the vulnerability |

**`release_tag_audited` payload**

| Field        | Type            | Description                                                |
| ------------ | --------------- | ---------------------------------------------------------- |
| `cve`        | string          | CVE from the scan or forwarded from the trigger            |
| `vulnerable` | bool            | Whether the release tag is affected                        |
| `dirty`      | bool (optional) | Forwarded from the upstream trigger for downstream routing |

**`main_branch_audited` payload**

| Field   | Type   | Description                                          |
| ------- | ------ | ---------------------------------------------------- |
| `cve`   | string | CVE identifier                                       |
| `dirty` | bool   | `true` if the vulnerability is still present on main |

**`remediation_completed` payload**

| Field     | Type   | Description                              |
| --------- | ------ | ---------------------------------------- |
| `cve`     | string | CVE that was remediated                  |
| `success` | bool   | Whether the fix was applied successfully |

## Release Lifecycle

| Type                         | Description                                    |
| ---------------------------- | ---------------------------------------------- |
| `release_requested`          | Decision made to cut a patch release           |
| `release_completed`          | Release tag created and pushed                 |
| `release_pipeline_completed` | GitHub Actions build/publish workflow finished |

**`release_completed` payload**

| Field     | Type           | Description                                 |
| --------- | -------------- | ------------------------------------------- |
| `cve`     | string         | CVE that prompted the release               |
| `release` | string         | Release type (e.g. `"patch"`)               |
| `new_tag` | string or null | Semver tag extracted from Claude CLI output |
| `success` | bool           | Whether the Claude CLI invocation succeeded |

**`release_pipeline_completed` payload**

| Field        | Type              | Description                     |
| ------------ | ----------------- | ------------------------------- |
| `status`     | string            | `"success"` or `"failure"`      |
| `conclusion` | string (optional) | GitHub Actions conclusion label |

## Project Lifecycle

| Type                            | Description                             |
| ------------------------------- | --------------------------------------- |
| `project_validation_completed`  | Pre-flight checks for a maintenance run |
| `project_iteration_completed`   | Iterate workflow finished               |
| `project_maintenance_completed` | Maintain workflow finished              |
| `project_changes_committed`     | Git commit created                      |
| `project_changes_pushed`        | Changes pushed to remote                |

**`project_validation_completed` payload**

| Field            | Type              | Description                                                                                                  |
| ---------------- | ----------------- | ------------------------------------------------------------------------------------------------------------ |
| `status`         | string            | `"ok"`, `"error"`, or `"skipped"`                                                                            |
| `reason`         | string (optional) | Human-readable explanation when status is not `"ok"`                                                         |
| `has_gates`      | bool (optional)   | Whether `.hone-gates.json` is present (only on `"ok"`)                                                       |
| `fast_forwarded` | int (optional)    | Commits fast-forwarded from `origin/<branch>` before work began (only on `"ok"`)                             |
| `sync_failure`   | string (optional) | Typed reason the checkout sync was refused: `"dirty_tree"`, `"diverged"`, or `"remote_unavailable"`          |
| `dry_run`        | bool (optional)   | `true` when the sync was simulated; `fast_forwarded` is then the count that would be applied                 |

**`project_iteration_completed` payload**

| Field      | Type            | Description                            |
| ---------- | --------------- | -------------------------------------- |
| `project`  | string          | Project name                           |
| `workflow` | string          | `"iterate"`                            |
| `success`  | bool            | Whether the iterate workflow succeeded |
| `summary`  | string          | Human-readable summary of the result   |
| `changes`  | bool (optional) | Whether code changes were made         |

**`project_maintenance_completed` payload**

| Field      | Type            | Description                             |
| ---------- | --------------- | --------------------------------------- |
| `project`  | string          | Project name                            |
| `workflow` | string          | `"maintain"`                            |
| `success`  | bool            | Whether the maintain workflow succeeded |
| `summary`  | string          | Human-readable summary of the result    |
| `changes`  | bool (optional) | Whether code changes were made          |

**`project_changes_committed` payload**

| Field          | Type              | Description                                                                                                                   |
| -------------- | ----------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| `cve`          | string            | CVE or `"unknown"` (from remediation path)                                                                                    |
| `message`      | string            | Git commit message used                                                                                                       |
| `push_failure` | string (optional) | Why the commit was not pushed: `"push_rejected_diverged"` (rebase onto the moved remote conflicted) or `"remote_unavailable"` |

**`project_changes_pushed` payload**

| Field | Type   | Description                                |
| ----- | ------ | ------------------------------------------ |
| `cve` | string | CVE or `"unknown"` (from remediation path) |

## Local Install

| Type                      | Description                        |
| ------------------------- | ---------------------------------- |
| `local_install_completed` | Local tool reinstallation finished |

## Maintenance Workflow

| Type                    | Payload       | Description                                                |
| ----------------------- | ------------- | ---------------------------------------------------------- |
| `iteration_requested`   | `{ project }` | Triggers the iterate sub-workflow for a validated project  |
| `maintenance_requested` | `{ project }` | Triggers the maintain sub-workflow for a validated project |

## Task Lifecycle

| Type                 | Description                                                                    |
| -------------------- | ------------------------------------------------------------------------------ |
| `task_run_started`   | Isolated one-shot task execution began                                         |
| `task_reviewed`      | Skeptical review produced a structural verdict                                 |
| `task_run_completed` | Task work was landed, durably preserved, or completed with no landing required |

**`task_run_completed` payload**

| Field              | Type               | Description                                                                                |
| ------------------ | ------------------ | ------------------------------------------------------------------------------------------ |
| `project`          | string             | Registered project name                                                                    |
| `success`          | bool               | `true` only for a `complete` verdict whose landing was not blocked                         |
| `landed`           | bool               | Whether complete or safe converging remainder work reached trunk                           |
| `summary`          | string             | Human-readable terminal summary                                                            |
| `verdict`          | string             | `complete`, `remainder`, `defect`, `blocked_on_decision`, or `runner_error`                |
| `preservation_ref` | string (optional)  | Continuation ref: landed commit SHA, or remote branch / `bundle:<path>` for preserved work |
| `land_blocked`     | string (optional)  | Why landing-eligible work did not land: `trunk_moved_conflict`, `trunk_moved_gates_failed`, `trunk_moved_repeatedly`, `checkout_not_ready`, or `git_failed`. The verdict is unchanged |
| `trunk_arrivals`   | array (optional)   | Trunk commits (`commit`, `subject`) that arrived during the run, oldest first; absent when trunk did not move |
| `campaign`         | string (optional)  | Campaign that dispatched the task                                                          |
| `campaign_cycle`   | integer (optional) | Campaign cycle that dispatched the task                                                    |

Verdict-specific fields are `gaps[]`, `diagnosis`, `finding` plus `options[]`,
or `detail`.

## Campaign Formation

| Type                         | Description                                                  |
| ---------------------------- | ------------------------------------------------------------ |
| `campaign_advance_requested` | Request to re-evaluate a durable campaign                    |
| `campaign_advance_completed` | Formation chose done, one next objective, or escalation      |
| `campaign_paused`            | Provider unavailability paused a non-terminal campaign       |
| `campaign_escalated`         | Campaign halted for budget, failure, rule, or owner judgment |
| `campaign_completed`         | Required done evidence proved the mission complete           |
| `campaign_cancelled`         | An owner abandoned the mission before its evidence was met   |

**`campaign_cancelled` payload**

Flattens the shared terminal payload (`campaign`, `project`, `reason`,
`cycles_completed`, `cycles_landed`) so generic terminal observers parse it
unchanged, and adds the operator's disposition choices.

| Field               | Type    | Description                                                     |
| ------------------- | ------- | --------------------------------------------------------------- |
| `terminated_now`    | bool    | The in-flight workflow was aborted rather than left to finish   |
| `discard_work`      | bool    | The terminated cycle's uncommitted work was thrown away         |
| `aborted_event_id`  | string? | Root event of the aborted workflow; absent for a graceful stop  |
| `aborted_trace_id`  | string? | Trace of the aborted workflow; absent for a graceful stop       |

An aborted run never reaches trace persistence, so `aborted_event_id` is the
only handle onto its partial events in the JSONL log — `foundry trace` has
nothing to show for it. `aborted_trace_id` is what correlates the cancellation
with the work-item ledger: the killed cycle's item was recorded `running` under
that trace, and a `--now` cancellation settles it `cancelled` (see
[The work queue](../guide/work-queue.md)).

## Work-Item Ledger

The four events of the work-item ledger. They report the same durable record at
different points in its life, so all four share one payload shape; the event type
says which point, and `state` says where the item stands.

| Type                  | Description                                                    |
| --------------------- | -------------------------------------------------------------- |
| `work_item_submitted` | A unit of work entered the ledger                              |
| `work_item_started`   | An agent started on a ledger item                              |
| `work_item_settled`   | A ledger item reached a settled state, with its disposition     |
| `work_item_cancelled` | An operator stopped a ledger item                              |

`work_item_started` pairs with `work_item_settled` rather than a
`work_item_completed`: an item does not *complete*, it settles, into a state
that may still hold an obligation. `work_item_cancelled` is emitted by
`foundry queue close` and `foundry queue cancel` on the exact item's trace,
and by `foundry campaign cancel --now` on the aborted cycle's trace.

**Shared payload**

| Field         | Type              | Description                                                                       |
| ------------- | ----------------- | --------------------------------------------------------------------------------- |
| `item_id`     | string            | The item's stable `wi_…` id                                                       |
| `project`     | string            | Registered project name                                                           |
| `objective`   | string            | Task description or campaign objective the work serves                            |
| `kind`        | string            | `task`, `campaign_cycle`, `maintenance`, `major_upgrade`, `release`, `remediation` |
| `lane`        | string            | `interactive`, `campaign`, or `maintenance`                                        |
| `state`       | string            | `submitted`, `queued`, `running`, `landed`, `preserved`, `needs_decision`, `failed`, `cancelled` |
| `reason`      | string            | Why the item is in that state, in one line                                        |
| `origin`      | string            | Opaque submitter text; Foundry never interprets it                                |
| `disposition` | object (optional) | How the item ended; present only on a settlement                                  |
| `operator_action` | object (optional) | Owner command, hostname/context in `origin`, `previous_state`, `previous_reason`, optional `previous_settled_at`; original submission origin and disposition retained |

**`disposition` fields**

| Field              | Type              | Description                                                      |
| ------------------ | ----------------- | ---------------------------------------------------------------- |
| `verdict`          | string (optional) | The reviewer's typed verdict tag, for a task-shaped settlement    |
| `landed_commit`    | string (optional) | The trunk commit the work landed as                              |
| `preservation_ref` | string (optional) | Branch or `bundle:<path>` holding unlanded work                  |
| `worktree`         | string (optional) | The isolated worktree the work ran in                            |
| `worktree_removed` | bool (optional)   | Whether that worktree was gone by settlement time                |

**`campaign_advance_requested` payload**

| Field          | Type              | Description                                  |
| -------------- | ----------------- | -------------------------------------------- |
| `campaign`     | string            | Campaign name                                |
| `run_event_id` | string (optional) | Typed task result that triggered the advance |
| `run_result`   | object (optional) | Full `task_run_completed` payload            |

**`campaign_advance_completed` payload**

| Field              | Type                  | Description                                                     |
| ------------------ | --------------------- | --------------------------------------------------------------- |
| `campaign`         | string                | Campaign name                                                   |
| `project`          | string                | Registered project name                                         |
| `cycles_completed` | integer               | Tasks dispatched by the campaign                                |
| `cycles_landed`    | integer               | Task results whose `task_run_completed.landed` field was `true` |
| `decision`         | string                | `done`, `advance`, or `escalate`                                |
| `objective`        | string (advance only) | Exactly one next task objective                                 |
| `reason`           | string                | Evidence or gap supporting the decision                         |
| `prompt`           | string (optional)     | Exact prompt shown to the formation agent                       |
| `agent_provider`   | string (optional)     | Provider used for the formation decision                        |
| `gate_results`     | array                 | Results from campaign done-evidence gates                       |

## Gate Orchestration

| Type                          | Description                                               |
| ----------------------------- | --------------------------------------------------------- |
| `gate_resolution_completed`   | Gate definitions loaded from `.hone-gates.json`           |
| `preflight_completed`         | Gates passed/failed on unmodified codebase                |
| `execution_completed`         | Code changes applied (emitted by future execution blocks) |
| `gate_verification_completed` | Gates passed/failed after execution                       |
| `retry_requested`             | Gate failure triggers bounded retry                       |

**`gate_resolution_completed` payload**

| Field      | Type              | Description                                              |
| ---------- | ----------------- | -------------------------------------------------------- |
| `project`  | string            | Project name                                             |
| `workflow` | string            | `"iterate"`, `"maintain"`, or `"validate"`               |
| `gates`    | array             | Gate definitions (name, command, required, timeout_secs) |
| `actions`  | object (optional) | Forwarded actions from the trigger event                 |

**`preflight_completed` payload**

| Field             | Type   | Description                                                                                       |
| ----------------- | ------ | ------------------------------------------------------------------------------------------------- |
| `project`         | string | Project name                                                                                      |
| `workflow`        | string | Workflow that triggered the preflight                                                             |
| `all_passed`      | bool   | Whether every gate passed                                                                         |
| `required_passed` | bool   | Whether all required gates passed                                                                 |
| `results`         | array  | Per-gate results (name, command, passed, required, output, exit_code, duration_ms?, fix_applied?) |

Each `results[]` entry includes an optional `duration_ms` field (unsigned
integer) recording how long the gate command took in milliseconds. This field is
absent when loading results from events persisted before timing instrumentation
was added.

A `results[]` entry also carries an optional `fix_applied` boolean: `true` when
the gate initially failed but its `fix_command` repaired the working tree and
the re-check then passed (a self-healed gate). The field is omitted when false,
so it is absent for gates that passed clean and for events persisted before
self-healing gates were added.

**`gate_verification_completed` payload**

| Field             | Type   | Description                                                                                       |
| ----------------- | ------ | ------------------------------------------------------------------------------------------------- |
| `project`         | string | Project name                                                                                      |
| `workflow`        | string | Originating workflow                                                                              |
| `all_passed`      | bool   | Whether every gate passed                                                                         |
| `required_passed` | bool   | Whether all required gates passed                                                                 |
| `retry_count`     | number | Current retry count (0 on first attempt)                                                          |
| `results`         | array  | Per-gate results (name, command, passed, required, output, exit_code, duration_ms?, fix_applied?) |

Each `results[]` entry includes an optional `duration_ms` field (unsigned
integer) recording how long the gate command took in milliseconds. This field is
absent when loading results from events persisted before timing instrumentation
was added.

A `results[]` entry also carries an optional `fix_applied` boolean: `true` when
the gate initially failed but its `fix_command` repaired the working tree and
the re-check then passed (a self-healed gate). The field is omitted when false,
so it is absent for gates that passed clean and for events persisted before
self-healing gates were added.

**`retry_requested` payload**

| Field             | Type              | Description                              |
| ----------------- | ----------------- | ---------------------------------------- |
| `project`         | string            | Project name                             |
| `workflow`        | string            | Originating workflow                     |
| `retry_count`     | number            | Incremented retry count                  |
| `failure_context` | string            | Gate output from the failed verification |
| `actions`         | object (optional) | Forwarded actions                        |

## Validation

| Type                   | Description                                    |
| ---------------------- | ---------------------------------------------- |
| `validation_requested` | Request to validate a project's gate health    |
| `validation_completed` | Terminal event with per-gate pass/fail results |

**`validation_requested` payload**

| Field     | Type   | Description  |
| --------- | ------ | ------------ |
| `project` | string | Project name |

**`validation_completed` payload**

| Field     | Type   | Description                                               |
| --------- | ------ | --------------------------------------------------------- |
| `project` | string | Project name                                              |
| `success` | bool   | Whether all required gates passed                         |
| `results` | array  | Per-gate results (name, passed, required, output snippet) |

## Maintenance Run Lifecycle

| Type                        | Description                                   |
| --------------------------- | --------------------------------------------- |
| `maintenance_run_started`   | A maintenance run was triggered for a project |
| `maintenance_run_completed` | All projects processed, summary available     |

**`maintenance_run_started` payload**

| Field     | Type   | Description                  |
| --------- | ------ | ---------------------------- |
| `project` | string | Project name this run covers |

**`maintenance_run_completed` payload**

| Field       | Type   | Description                                                |
| ----------- | ------ | ---------------------------------------------------------- |
| `total`     | number | Total number of projects processed                         |
| `succeeded` | number | Projects that completed successfully                       |
| `failed`    | number | Projects that encountered an error                         |
| `skipped`   | number | Projects that were skipped (already active or `skip=true`) |
| `projects`  | array  | Per-project result objects (name, status, duration_secs)   |

## Dependency Update Policy

| Type | Description |
| --- | --- |
| `dependency_review_requested` | Command: classify one project's dependency updates and plan its majors, applying nothing. Optional payload `{ "policy": "major" }` previews another policy |
| `dependency_updates_classified` | A project's direct dependencies were classified and the maintain brief decided |
| `major_upgrades_planned` | The majors lane decided each major upgrade: `dispatch`, `deduped`, `overflow`, `deferred` or `proposed` |

**`dependency_updates_classified` payload**

| Field | Type | Description |
| --- | --- | --- |
| `project` | string | Registered project name |
| `phase` | string | `before` (the maintain brief), `after` (maintenance completed) or `review` |
| `workflow` | string (optional) | `maintain` in the `before` and `after` phases |
| `success` | bool (optional) | In the `after` phase, whether maintenance succeeded |
| `classification` | object | `outdated[]` (per dependency: `ecosystem`, `manifest`, `package`, `current`, `requirement`, `in_range`, `non_major`, `major`, `hold`, `advisories`), `classified[]`, `unclassified[]`, `lapsed_holds[]`, `holds_warning`, `transitive_advisories[]`, `advisory_source` |
| `brief` | object | `policy`, `policy_set`, `apply[]`, `held_by_policy[]`, `held_by_hold[]`, `majors[]` |

In the `before` phase the payload also carries the gate-resolution fields
(`gates`, `actions` and the rest of the chain context) forward to
`Execute Maintain`.

**`major_upgrades_planned` payload**

| Field | Type | Description |
| --- | --- | --- |
| `upgrades` | array | One entry per major: `project`, `ecosystem`, `manifest`, `package`, `from`, `to`, `security`, `objective`, `command`, `status`, `reason` |
| `per_project_cap` | number | `FOUNDRY_MAJOR_TASKS_PER_PROJECT` in force |
| `per_night_cap` | number | `FOUNDRY_MAJOR_TASKS_PER_NIGHT` in force |
| `dispatch_enabled` | bool | `true` only for the nightly run at `full` throttle |
| `review` | bool | `true` for an on-demand review |
| `history_warning` | string (optional) | Set when earlier tasks could not be read for dedupe |

In the nightly run the payload also carries the
`maintenance_summary_requested` fields (`project_trace_ids`,
`skipped_projects`, `total_duration_ms`, `root_event_id`).

## Release Tag Audit

| Type                  | Description                                    |
| --------------------- | ---------------------------------------------- |
| `release_tag_audited` | Latest release tag scanned (see payload above) |

## Agent Session Lifecycle

Emitted by `foundryd` whenever a Foundry-launched Claude Code agent session
begins or ends. Used by visualisation tools (e.g. `ops-visualizer`) to show
in-flight and historical agent activity, and to locate the per-session
stream-json transcript on disk.

| Type                    | Description                                                       |
| ----------------------- | ----------------------------------------------------------------- |
| `agent_session_started` | An agent session has begun; transcript file path is included      |
| `agent_session_ended`   | The agent session has finished (success, failure, unavailable, or interrupted) |

**`agent_session_started` payload**

| Field             | Type               | Description                                                                                        |
| ----------------- | ------------------ | -------------------------------------------------------------------------------------------------- |
| `session_id`      | string             | UUID identifying this session; matches the transcript file basename                                |
| `agent_type`      | string             | Agent runtime (currently always `claude-code`)                                                     |
| `project`         | string             | Project name (may be empty in v1)                                                                  |
| `working_dir`     | string             | Absolute path of the working directory the agent ran in                                            |
| `source_log_path` | string             | Absolute path to the per-session JSONL transcript (`~/.foundry/agent-sessions/<session_id>.jsonl`) |
| `capability`      | string             | Capability label: `reasoning`, `coding`, or `quick`                                                |
| `access`          | string             | Tool access level: `read_only` or `full`                                                           |
| `started_at`      | RFC 3339 timestamp | When the session was launched                                                                      |
| `trace_id`        | string             | Correlating trace ID (may be empty in v1)                                                          |

**`agent_session_ended` payload**

| Field           | Type               | Description                                                     |
| --------------- | ------------------ | --------------------------------------------------------------- |
| `session_id`    | string             | UUID identifying this session (matches `agent_session_started`) |
| `status`        | string             | Outcome: `ok`, `agent_failed`, `unavailable`, or `interrupted` |
| `exit_code`     | number             | Process exit code (omitted when the agent could not be invoked, and for `interrupted`) |
| `ended_at`      | RFC 3339 timestamp | When the session finished; for `interrupted`, when the daemon restarted |
| `bytes_written` | number             | Total bytes streamed to the transcript file (`0` for `interrupted`) |
| `error`         | string             | Error message when `status = unavailable`; `daemon restarted` when `status = interrupted` (omitted otherwise) |

**`interrupted` sessions.** An agent session is a child process of `foundryd`,
so it dies when the daemon stops, and nothing records its end at that moment.
On the next start, before it accepts work, `foundryd` reads the last 7 days of
the event log (the same lookback as the interrupted maintenance-cycle recovery)
for `agent_session_started` events with no `agent_session_ended` for the same
`session_id`. Nothing can be running then, so each such session is dead. For
each one, `foundryd` records an `agent_session_ended` with `status`
`interrupted`, `error` `daemon restarted`, the original session's `project` and
`trace_id`, and `ended_at` set to the daemon start time. The event is persisted
to the event log and published on the Watch stream like any other. Because the
end is in the log, a later start does not end the same session again.
Unparseable lines in the log are skipped and do not stop the daemon from
starting. `usage` and `cost` are absent: nothing measured the session's last
moments.
