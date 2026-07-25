# gRPC API

The Foundry service is defined in `proto/foundry.proto`.

## Service: `Foundry`

### `Emit(EmitRequest) → EmitResponse`

Fire an event into the system. The engine spawns processing as a background task
and returns the event ID immediately. Use `Trace` to check for completion,
`Status` to see in-flight workflows, or `Watch` for real-time event streaming.

**Request:**

| Field          | Type          | Description                                                |
| -------------- | ------------- | ---------------------------------------------------------- |
| `event_type`   | string        | Event type name                                            |
| `project`      | string        | Target project                                             |
| `throttle`     | Throttle enum | `THROTTLE_FULL`, `THROTTLE_AUDIT_ONLY`, `THROTTLE_DRY_RUN` |
| `payload_json` | string        | Optional JSON payload                                      |

**Response:**

| Field         | Type   | Description                           |
| ------------- | ------ | ------------------------------------- |
| `event_id`    | string | Deterministic ID of the created event |
| `workflow_id` | string | ID of the triggered workflow (if any) |

### `Status(StatusRequest) → StatusResponse`

Query active workflow states.

**Request:**

| Field         | Type   | Description                              |
| ------------- | ------ | ---------------------------------------- |
| `workflow_id` | string | Specific workflow (empty for all active) |

**Response:**

| Field       | Type                    | Description            |
| ----------- | ----------------------- | ---------------------- |
| `workflows` | repeated WorkflowStatus | Active workflow states |

### `Watch(WatchRequest) → stream WatchResponse`

Server-side streaming of live events as they are processed by the engine.
Optionally filtered by project name.

**Request:**

| Field     | Type   | Description                                              |
| --------- | ------ | -------------------------------------------------------- |
| `project` | string | Project name to filter by; empty string for all projects |

**Response (stream):**

| Field          | Type   | Description           |
| -------------- | ------ | --------------------- |
| `event_id`     | string | Event identifier      |
| `event_type`   | string | Event type name       |
| `project`      | string | Target project        |
| `payload_json` | string | Event payload as JSON |

### `RegistryAdd(RegistryAddRequest) → RegistryAddResponse`

Add a project to the daemon's in-memory registry and persist the change to
`registry.json`. The daemon is the single source of truth for registry state.

**Request:**

| Field             | Type   | Description                                                                    |
| ----------------- | ------ | ------------------------------------------------------------------------------ |
| `name`            | string | Unique project name                                                            |
| `path`            | string | Absolute path on the local filesystem                                          |
| `stack`           | string | Technology stack: `rust`, `python`, `typescript`, `elixir`, `cpp`              |
| `agent`           | string | AI agent name                                                                  |
| `repo`            | string | GitHub repo slug (`owner/repo`)                                                |
| `branch`          | string | Default branch (empty → `main`)                                                |
| `iterate`         | bool   | Enable iterate action                                                          |
| `maintain`        | bool   | Enable maintain action                                                         |
| `push`            | bool   | Enable push action                                                             |
| `audit`           | bool   | Enable audit action                                                            |
| `release`         | bool   | Enable release action                                                          |
| `install_command` | string | Shell command for local install (mutually exclusive with `install_brew`)       |
| `install_brew`    | string | Homebrew formula for local install (mutually exclusive with `install_command`) |
| `notes`           | string | Human-readable notes (empty → none)                                            |
| `timeout_secs`    | uint64 | Per-project timeout (0 → use default 3600 s)                                   |

**Response:**

| Field     | Type    | Description                     |
| --------- | ------- | ------------------------------- |
| `project` | Project | The newly created project entry |

**Errors:** `ALREADY_EXISTS` if the name is already in the registry;
`INVALID_ARGUMENT` for an unknown stack or conflicting install fields;
`INTERNAL` with the stable message `failed to persist registry state` when
saving fails.

**CLI:** `foundry registry add ...` routes through this RPC by default and
therefore requires a reachable daemon. Pass `--offline` to bypass the daemon and
mutate the registry file directly. Without `--offline`, an unreachable daemon
returns an error and leaves the client-side registry file unchanged. The online
path mutates daemon-owned state only and does not create
`FOUNDRY_REGISTRY_PATH`. If daemon persistence fails, the RPC returns `INTERNAL`
with a stable `failed to persist registry state` message and leaves both the
daemon's in-memory registry and its on-disk registry bytes unchanged.

### `RegistryList(RegistryListRequest) → RegistryListResponse`

List the daemon-owned registry inventory from the in-memory state held by
`foundryd`. The online CLI path renders this response directly and does not read
`FOUNDRY_REGISTRY_PATH`.

**Request:**

This message has no fields.

**Response:**

| Field      | Type             | Description                                                 |
| ---------- | ---------------- | ----------------------------------------------------------- |
| `projects` | repeated Project | Every project currently loaded in the daemon-owned registry |

Each `Project` carries the full registry data required by online clients:
`name`, `path`, `stack`, `agent`, `repo`, `branch`, `skip`, action flags,
install config, notes, timeout, installs-skill state, and audit exceptions.

**Errors:** None at the RPC layer; the daemon answers from already-loaded
registry state.

**CLI:** `foundry registry list` routes through this RPC by default and
therefore requires a reachable daemon. Pass `--offline` to read the registry
file directly. Without `--offline`, an unreachable daemon returns an error and
leaves the client-side registry file unchanged. The online path renders the RPC
response directly and does not create `FOUNDRY_REGISTRY_PATH`; if that path is
absent, the online path leaves it absent. `foundry registry init` is not part of
this RPC surface and remains an offline-only recovery command.

### `RegistryShow(RegistryShowRequest) → RegistryShowResponse`

Retrieve one exact-name project from the daemon-owned registry state. The match
is exact and does not perform prefix or substring lookup.

**Request:**

| Field  | Type   | Description                    |
| ------ | ------ | ------------------------------ |
| `name` | string | Exact project name to retrieve |

**Response:**

| Field     | Type    | Description                                              |
| --------- | ------- | -------------------------------------------------------- |
| `project` | Project | The full daemon-owned project record for that exact name |

**Errors:** `NOT_FOUND` if no exact-name project exists.

**CLI:** `foundry registry show <name>` routes through this RPC by default and
therefore requires a reachable daemon. Pass `--offline` to read the registry
file directly. Without `--offline`, an unreachable daemon returns an error and
leaves the client-side registry file unchanged. The online path renders the RPC
response directly and does not create `FOUNDRY_REGISTRY_PATH`; if that path is
absent, the online path leaves it absent.

### `RegistryRemove(RegistryRemoveRequest) → RegistryRemoveResponse`

Remove a project from the registry by name.

**Request:**

| Field  | Type   | Description            |
| ------ | ------ | ---------------------- |
| `name` | string | Project name to remove |

**Errors:** `NOT_FOUND` if no project with that name exists; `INTERNAL` with the
stable message `failed to persist registry state` when saving fails.

**CLI:** `foundry registry remove <name>` routes through this RPC by default and
therefore requires a reachable daemon. Pass `--offline` to bypass the daemon and
mutate the registry file directly. Without `--offline`, an unreachable daemon
returns an error and leaves the client-side registry file unchanged. The online
path mutates daemon-owned state only and does not create
`FOUNDRY_REGISTRY_PATH`. If daemon persistence fails, the RPC returns `INTERNAL`
with a stable `failed to persist registry state` message and leaves both the
daemon's in-memory registry and its on-disk registry bytes unchanged.

### `RegistryEdit(RegistryEditRequest) → RegistryEditResponse`

Apply partial edits to an existing project. Only fields that are non-empty /
non-zero are applied. Use `clear_*` booleans to explicitly clear optional fields
(e.g. `clear_skip = true` to un-skip a project).

**Request:**

| Field             | Type   | Description                                                               |
| ----------------- | ------ | ------------------------------------------------------------------------- |
| `name`            | string | Project to edit (required)                                                |
| `path`            | string | New path (empty → no change)                                              |
| `stack`           | string | New stack (empty → no change)                                             |
| `agent`           | string | New agent (empty → no change)                                             |
| `repo`            | string | New repo slug (empty → no change)                                         |
| `branch`          | string | New branch (empty → no change)                                            |
| `skip`            | string | New skip reason; non-empty sets it; empty → no change unless `clear_skip` |
| `clear_skip`      | bool   | Remove the skip flag                                                      |
| `iterate`         | bool   | Set iterate to true (use `clear_iterate` to set false)                    |
| `clear_iterate`   | bool   | Set iterate to false                                                      |
| `maintain`        | bool   | Set maintain to true                                                      |
| `clear_maintain`  | bool   | Set maintain to false                                                     |
| `push`            | bool   | Set push to true                                                          |
| `clear_push`      | bool   | Set push to false                                                         |
| `audit`           | bool   | Set audit to true                                                         |
| `clear_audit`     | bool   | Set audit to false                                                        |
| `release`         | bool   | Set release to true                                                       |
| `clear_release`   | bool   | Set release to false                                                      |
| `install_command` | string | Set a shell-command install                                               |
| `install_brew`    | string | Set a Homebrew formula install                                            |
| `clear_install`   | bool   | Remove the install config                                                 |
| `notes`           | string | Set notes (empty string + `clear_notes = false` → no change)              |
| `clear_notes`     | bool   | Remove notes                                                              |
| `timeout_secs`    | uint64 | Set timeout (0 → no change unless `clear_timeout`)                        |
| `clear_timeout`   | bool   | Revert timeout to the daemon default                                      |

**Response:**

| Field     | Type    | Description               |
| --------- | ------- | ------------------------- |
| `project` | Project | The updated project entry |

**Errors:** `NOT_FOUND`; `INVALID_ARGUMENT` for conflicting install fields or
unknown stack; `INTERNAL` with the stable message
`failed to persist registry state` when saving fails.

**CLI:** `foundry registry edit <name> ...` routes through this RPC by default
and therefore requires a reachable daemon. Pass `--offline` to bypass the daemon
and mutate the registry file directly. Without `--offline`, an unreachable
daemon returns an error and leaves the client-side registry file unchanged. The
online path mutates daemon-owned state only and does not create
`FOUNDRY_REGISTRY_PATH`. If daemon persistence fails, the RPC returns `INTERNAL`
with a stable `failed to persist registry state` message and leaves both the
daemon's in-memory registry and its on-disk registry bytes unchanged.

### `SentinelList(SentinelListRequest) → SentinelListResponse`

List every daemon-owned sentinel from the in-memory scheduler control-plane
state held by `foundryd`.

**Request:**

This message has no fields.

**Response:**

| Field       | Type              | Description                                                    |
| ----------- | ----------------- | -------------------------------------------------------------- |
| `sentinels` | repeated Sentinel | Every daemon-owned sentinel entry in scheduler evaluation order |

Each `Sentinel` carries the exact scheduler contract the CLI needs to render:
`name`, `cron`, `emit_event_type`, `emit_project`, `emit_throttle`,
`emit_payload_json`, and `enabled`.

**Errors:** None at the RPC layer; the daemon answers from already-loaded
sentinel state.

**CLI:** `foundry sentinel list` routes through this RPC by default and
therefore requires a reachable daemon. Pass `--offline` to read the sentinel
file directly. Without `--offline`, an unreachable daemon returns an error and
leaves the client-side sentinel file unchanged. The online path renders the RPC
response directly and does not create `FOUNDRY_SENTINELS_PATH`; if that path is
absent, the online path leaves it absent.

### `SentinelShow(SentinelShowRequest) → SentinelShowResponse`

Retrieve one exact-name sentinel from the daemon-owned scheduler control-plane
state. The name match is exact and does not perform prefix or substring lookup.

**Request:**

| Field  | Type   | Description                     |
| ------ | ------ | ------------------------------- |
| `name` | string | Exact sentinel name to retrieve |

**Response:**

| Field      | Type     | Description                                         |
| ---------- | -------- | --------------------------------------------------- |
| `sentinel` | Sentinel | The full daemon-owned sentinel record for that name |

**Errors:** `NOT_FOUND` if no exact-name sentinel exists.

**CLI:** `foundry sentinel show <name>` routes through this RPC by default and
therefore requires a reachable daemon. Pass `--offline` to read the sentinel
file directly. Without `--offline`, an unreachable daemon returns an error and
leaves the client-side sentinel file unchanged. The online path renders the RPC
response directly and does not create `FOUNDRY_SENTINELS_PATH`; if that path is
absent, the online path leaves it absent.

### `SentinelEnable(SentinelEnableRequest) → SentinelEnableResponse`

Mark one daemon-owned sentinel as enabled, persist the updated sentinel store,
and wake the in-process scheduler so the next firing is recomputed immediately.

**Request:**

| Field  | Type   | Description                   |
| ------ | ------ | ----------------------------- |
| `name` | string | Exact sentinel name to enable |

**Response:**

| Field      | Type     | Description                                             |
| ---------- | -------- | ------------------------------------------------------- |
| `sentinel` | Sentinel | The committed daemon-owned sentinel record after enable |

**Errors:** `NOT_FOUND` if no exact-name sentinel exists; `INTERNAL` if the
daemon cannot persist the sentinel store.

**CLI:** `foundry sentinel enable <name>` routes through this RPC by default
and therefore requires a reachable daemon. Pass `--offline` to bypass the
daemon and mutate the sentinel file directly. Without `--offline`, an
unreachable daemon returns an error and leaves the client-side sentinel file
unchanged. The online path mutates daemon-owned state only and does not create
`FOUNDRY_SENTINELS_PATH`. If daemon persistence fails, the RPC returns
`INTERNAL`, leaves the daemon-owned in-memory sentinel store unchanged, does
not wake the scheduler, and leaves the on-disk sentinel bytes unchanged.

### `SentinelDisable(SentinelDisableRequest) → SentinelDisableResponse`

Mark one daemon-owned sentinel as disabled, persist the updated sentinel store,
and wake the in-process scheduler so any pending firing is cancelled
immediately.

**Request:**

| Field  | Type   | Description                    |
| ------ | ------ | ------------------------------ |
| `name` | string | Exact sentinel name to disable |

**Response:**

| Field      | Type     | Description                                              |
| ---------- | -------- | -------------------------------------------------------- |
| `sentinel` | Sentinel | The committed daemon-owned sentinel record after disable |

**Errors:** `NOT_FOUND` if no exact-name sentinel exists; `INTERNAL` if the
daemon cannot persist the sentinel store.

**CLI:** `foundry sentinel disable <name>` routes through this RPC by default
and therefore requires a reachable daemon. Pass `--offline` to bypass the
daemon and mutate the sentinel file directly. Without `--offline`, an
unreachable daemon returns an error and leaves the client-side sentinel file
unchanged. The online path mutates daemon-owned state only and does not create
`FOUNDRY_SENTINELS_PATH`. If daemon persistence fails, the RPC returns
`INTERNAL`, leaves the daemon-owned in-memory sentinel store unchanged, does
not wake the scheduler, and leaves the on-disk sentinel bytes unchanged.

### `AddCampaign(AddCampaignRequest) → AddCampaignResponse`

Add one campaign definition to the daemon-owned campaign store. The daemon
parses the JSON definition, validates the referenced project and context paths
against daemon-owned registry state, acquires the exclusive campaign-store lock,
and persists the new definition atomically.

**Request:**

| Field             | Type   | Description                                                                 |
| ----------------- | ------ | --------------------------------------------------------------------------- |
| `definition_json` | string | Full campaign definition JSON exactly as accepted by `foundry campaign add` |

**Response:**

| Field      | Type           | Description                                |
| ---------- | -------------- | ------------------------------------------ |
| `campaign` | CampaignDetail | Full durable definition that was persisted |

**Errors:** `INVALID_ARGUMENT` when the JSON is invalid or the definition is
structurally invalid; `FAILED_PRECONDITION` when the definition references an
unknown registered project or invalid context artifact; `ALREADY_EXISTS` when a
campaign with the same name already exists; `INTERNAL` when the store cannot be
saved. On `INTERNAL`, the daemon leaves the on-disk campaign store unchanged.

**CLI:** `foundry campaign add <definition.json>` routes through this RPC by
default. The online CLI renders the returned `CampaignDetail` directly and does
not re-read `FOUNDRY_CAMPAIGNS_PATH`. Pass `--offline` only when you
intentionally need direct-file recovery while the daemon is stopped.

### `ListCampaigns(ListCampaignsRequest) → ListCampaignsResponse`

List the durable campaign inventory from the daemon's configured campaign store.
This is a read-only query: it loads the store at request time, returns records
in deterministic campaign-name order, and exposes summary/status fields only.

**Request:**

| Field     | Type   | Description                                                            |
| --------- | ------ | ---------------------------------------------------------------------- |
| `project` | string | Optional exact project-name filter; empty string returns all campaigns |

**Response:**

| Field       | Type              | Description                                       |
| ----------- | ----------------- | ------------------------------------------------- |
| `campaigns` | repeated Campaign | Durable inventory records sorted by campaign name |

**Errors:** `FAILED_PRECONDITION` when the campaign store is malformed;
`INTERNAL` when the campaign store is unreadable. Missing or empty stores return
an empty list.

### `PauseCampaign(PauseCampaignRequest) → PauseCampaignResponse`

Pause a campaign. The operation is idempotent on the `status` field — pausing an
already-paused campaign is not an error. The daemon holds an exclusive lock on
the campaign store for the duration of the write, so a concurrent advance
formation cannot interleave with a pause.

Any `pending_run_result` that was recorded before the pause is explicitly
preserved: the operation never clears or overwrites it. The result remains
available for the next manual advance after a subsequent resume.

**Request:**

| Field  | Type   | Description                  |
| ------ | ------ | ---------------------------- |
| `name` | string | Exact campaign name to pause |

**Response:**

| Field      | Type           | Description                                                          |
| ---------- | -------------- | -------------------------------------------------------------------- |
| `campaign` | CampaignDetail | Full campaign detail reflecting the state after the pause is applied |

**Errors:** `NOT_FOUND` when no campaign with the given name exists;
`FAILED_PRECONDITION` when the campaign store is malformed; `INTERNAL` when the
campaign store is unreadable or when persistence fails. On save failure, the
daemon leaves the persisted campaign store unchanged.

**CLI:** `foundry campaign pause <name>` routes through this RPC when the daemon
is reachable. Pass `--offline` to bypass the daemon and mutate the store file
directly (useful when `foundryd` is not running). Without `--offline`, an
unreachable daemon is an error and the client-side campaigns path is left
untouched.

### `ResumeCampaign(ResumeCampaignRequest) → ResumeCampaignResponse`

Resume a `paused` or `escalated` campaign, optionally extending its cycle
budget. The daemon holds an exclusive lock on the campaign store for the
duration of the write.

`pending_run_result` is explicitly preserved: the operation never clears or
overwrites it. The result remains available for the next manual advance after
resume.

**Accepted statuses:** `paused` and `escalated`. Budget-only escalations (where
the engine stopped because the cycle limit was reached) may be resumed with this
RPC without recording an owner-decision entry. Use `DecideCampaign` when the
escalation contains a human judgment question that requires a policy record.

**Exhausted budget guard:** when `add_cycles == 0` and
`cycles_completed >= max_cycles`, the RPC returns `FAILED_PRECONDITION`. Pass a
positive `add_cycles` to explicitly authorize more work; the engine never
silently reactivates an exhausted campaign.

**Request:**

| Field        | Type   | Description                                                                 |
| ------------ | ------ | --------------------------------------------------------------------------- |
| `name`       | string | Exact campaign name to resume                                               |
| `add_cycles` | uint64 | Additional cycles to add to `max_cycles` before resuming (0 = no extension) |

**Response:**

| Field      | Type           | Description                                                           |
| ---------- | -------------- | --------------------------------------------------------------------- |
| `campaign` | CampaignDetail | Full campaign detail reflecting the state after the resume is applied |

**Errors:** `NOT_FOUND` when no campaign with the given name exists;
`FAILED_PRECONDITION` when the campaign is not `paused` or `escalated`, lacks
`authorized_by`, the budget is exhausted and `add_cycles == 0`, `add_cycles`
would overflow `max_cycles`, or the campaign store is malformed; `INTERNAL` when
the campaign store is unreadable or persistence fails. On save failure, the
daemon leaves the persisted campaign store unchanged.

**CLI:** `foundry campaign resume <name>` routes through this RPC when the
daemon is reachable. Pass `--offline` to bypass the daemon and mutate the store
file directly (useful when `foundryd` is not running). Without `--offline`, an
unreachable daemon is an error and the client-side campaigns path is left
untouched. The rendered output is built from the
`ResumeCampaignResponse.campaign` detail — the CLI never re-reads the store file
on the online path.

### `DecideCampaign(DecideCampaignRequest) → DecideCampaignResponse`

Record an owner decision on an escalated campaign. The daemon holds the
exclusive campaign-store lock for the full mutation, appends one durable owner
decision record, and returns the campaign to `active` so the next advance can
proceed with that policy in context.

The persisted owner decision carries the decision text, the campaign's current
`authorized_by` identity, and the daemon timestamp. Existing counters and any
stored `pending_run_result` are preserved.

**Request:**

| Field      | Type   | Description                             |
| ---------- | ------ | --------------------------------------- |
| `name`     | string | Exact campaign name to update           |
| `decision` | string | Non-empty owner decision text to record |

**Response:**

| Field      | Type           | Description                                                             |
| ---------- | -------------- | ----------------------------------------------------------------------- |
| `campaign` | CampaignDetail | Full campaign detail reflecting the state after the decision is applied |

**Errors:** `NOT_FOUND` when no campaign with the given name exists;
`INVALID_ARGUMENT` when `decision` is empty after trimming;
`FAILED_PRECONDITION` when the campaign is not `escalated`, lacks
`authorized_by`, or the campaign store is malformed; `INTERNAL` when the
campaign store is unreadable or persistence fails. On save failure, the daemon
leaves the persisted campaign store unchanged.

**CLI:** `foundry campaign decide <name> --decision "<text>"` routes through
this RPC by default and therefore requires a reachable daemon. Pass `--offline`
to bypass the daemon and mutate the store file directly. Without `--offline`, an
unreachable daemon returns an error and leaves the client-side campaigns path
unchanged.

### `CompleteCampaign(CompleteCampaignRequest) → CompleteCampaignResponse`

Mark an authorized campaign complete from outside the formation loop. The
request requires a non-empty reason and an existing `authorized_by` owner. The
daemon stores the reason as an append-only owner record, clears any pending run
result, changes the status to `completed`, and emits the normal
`CampaignCompleted` event for terminal observers. Calling it on an already
completed campaign is idempotent.

**Request:**

| Field    | Type   | Description                                          |
| -------- | ------ | ---------------------------------------------------- |
| `name`   | string | Exact campaign name to complete                      |
| `reason` | string | Evidence-backed owner reason for external completion |

**Response:**

| Field      | Type           | Description                                                           |
| ---------- | -------------- | --------------------------------------------------------------------- |
| `campaign` | CampaignDetail | Full campaign detail reflecting the state after completion is applied |

**Errors:** `INVALID_ARGUMENT` for a blank reason, `NOT_FOUND` for an unknown
campaign, and `FAILED_PRECONDITION` when the campaign has no authorizing owner
or the campaign store is malformed; `INTERNAL` when the campaign store is
unreadable or persistence fails. On save failure, the daemon leaves the
persisted campaign store unchanged and does not emit `CampaignCompleted`.

**CLI:** `foundry campaign complete <name> --reason "<text>"` routes through
this RPC by default. The online CLI renders the returned `CampaignDetail`
directly, never re-reads `FOUNDRY_CAMPAIGNS_PATH`, and leaves the client-side
path untouched if the daemon is unreachable. Pass `--offline` only for
direct-file recovery while the daemon is stopped.

### `AdvanceCampaign(AdvanceCampaignRequest) → AdvanceCampaignResponse`

Dispatch one manual campaign-advance workflow for an `active` or `staged`
campaign. The daemon validates the current status under the exclusive lock,
releases the lock, emits `CampaignAdvanceRequested`, and returns immediately.

**Request:**

| Field  | Type   | Description                    |
| ------ | ------ | ------------------------------ |
| `name` | string | Exact campaign name to advance |

**Response:**

| Field      | Type           | Description                                                         |
| ---------- | -------------- | ------------------------------------------------------------------- |
| `campaign` | CampaignDetail | Current campaign detail at dispatch time                            |
| `event_id` | string         | Root event ID of the dispatched `CampaignAdvanceRequested` workflow |

**Errors:** `NOT_FOUND` when the name is absent; `FAILED_PRECONDITION` when the
campaign is `paused`, `escalated`, or `completed`; `INTERNAL` when the store is
unreadable. Rejected advances do not mutate the store.

**CLI:** `foundry campaign advance <name>` routes through this RPC by default.
The online CLI prints the returned root `event_id`, watches the daemon-owned
workflow stream, and renders the daemon trace for that event. It does not
re-read `FOUNDRY_CAMPAIGNS_PATH`.

### `GetCampaign(GetCampaignRequest) → GetCampaignResponse`

Retrieve the complete durable definition of one campaign by exact name. Unlike
`ListCampaigns`, this RPC returns the full detail record including
`intent_refs`, `context_paths`, `done_evidence` (with the `Gate`/`Review` type
distinction preserved), escalation rules, and all runtime status fields.

**Request:**

| Field  | Type   | Description                    |
| ------ | ------ | ------------------------------ |
| `name` | string | Exact campaign name to look up |

**Response:**

| Field      | Type           | Description                             |
| ---------- | -------------- | --------------------------------------- |
| `campaign` | CampaignDetail | Full durable definition of the campaign |

**Errors:** `NOT_FOUND` when no campaign with the given name exists in the store
(even when other campaigns are present); `FAILED_PRECONDITION` when the campaign
store is malformed; `INTERNAL` when the campaign store is unreadable.

**CLI:** `foundry campaign show <name>` routes through this RPC by default and
renders the returned `CampaignDetail` directly, without re-reading
`FOUNDRY_CAMPAIGNS_PATH`.

### `History(HistoryRequest) → HistoryResponse`

List durable trace history from the daemon-owned trace store.

**Request:**

| Field         | Type   | Description                                                                |
| ------------- | ------ | -------------------------------------------------------------------------- |
| `date`        | string | Exact day in `YYYY-MM-DD` format, or empty to request recent history       |
| `project`     | string | Exact project filter, or empty for all projects                            |
| `recent_days` | uint32 | Number of recent days to return when `date` is empty; online CLI uses `7`  |

**Response:**

| Field  | Type                | Description                                  |
| ------ | ------------------- | -------------------------------------------- |
| `days` | repeated HistoryDay | Matching days, newest day first              |

Each `HistoryDay` carries a `date` plus `repeated HistoryTrace`. `HistoryTrace`
contains `event_id`, `event_type`, `project`, `success`, `total_duration_ms`,
and `trace_id`.

Completed traces are persisted to disk under `~/.foundry/traces/YYYY-MM-DD/`
and survive daemon restarts. The daemon reads that store directly when serving
history, preserving durable retention semantics even after in-memory cache
entries expire.

**CLI:** `foundry history` routes through this RPC by default, renders the
daemon response directly, and does not read or create a client-side
`FOUNDRY_TRACES_DIR` unless `--offline` is explicit.

### `Trace(TraceRequest) → TraceResponse`

Retrieve the trace of a completed event chain. Returns all events produced
during processing and a record of each block execution.

**Request:**

| Field      | Type   | Description              |
| ---------- | ------ | ------------------------ |
| `event_id` | string | Root event ID to look up |

**Response:**

| Field              | Type                         | Description                                      |
| ------------------ | ---------------------------- | ------------------------------------------------ |
| `found`            | bool                         | Whether a trace was found for the given event ID |
| `events`           | repeated TraceEvent          | All events in the chain                          |
| `block_executions` | repeated TraceBlockExecution | Record of each block execution                   |

Completed traces are persisted to disk under `~/.foundry/traces/YYYY-MM-DD/` and
survive daemon restarts. The `Trace` RPC checks the in-memory store first (for
recently completed chains) and falls back to disk for older traces.

**CLI:** `foundry trace <event-id>` routes through this RPC by default. When no
trace is found, the CLI prints `No trace found for <event-id> (expired or unknown).`

### `Span(SpanRequest) → SpanResponse`

Retrieve every event and block execution that belongs to one span.

**Request:**

| Field     | Type   | Description              |
| --------- | ------ | ------------------------ |
| `span_id` | string | Exact span ID to look up |

**Response:**

| Field              | Type                         | Description                                      |
| ------------------ | ---------------------------- | ------------------------------------------------ |
| `found`            | bool                         | Whether a span was found for the given span ID   |
| `events`           | repeated TraceEvent          | Events whose `span_id` matches the requested span |
| `block_executions` | repeated TraceBlockExecution | Blocks whose own span or parent span matches     |
| `trace_id`         | string                       | Owning trace ID, including for block-only spans  |
| `total_duration_ms`| uint64                       | Sum of returned block durations                  |

`Span` is an in-memory lookup keyed by the daemon's span index. It is intended
for live drill-down and status filtering rather than durable offline browsing.
If the span is unknown, the response is `found = false` with empty collections
and an empty `trace_id`.

## Messages

### `WorkflowStatus`

| Field           | Type                     | Description                           |
| --------------- | ------------------------ | ------------------------------------- |
| `workflow_id`   | string                   | Workflow identifier                   |
| `workflow_type` | string                   | Workflow type name                    |
| `project`       | string                   | Target project                        |
| `state`         | string                   | pending, running, completed, failed   |
| `started_at`    | string                   | ISO 8601 timestamp                    |
| `completed_at`  | string                   | ISO 8601 timestamp (empty if running) |
| `task_blocks`   | repeated TaskBlockStatus | Per-block status                      |

### `TaskBlockStatus`

| Field          | Type   | Description                                  |
| -------------- | ------ | -------------------------------------------- |
| `name`         | string | Block name                                   |
| `state`        | string | pending, running, completed, skipped, failed |
| `started_at`   | string | ISO 8601 timestamp                           |
| `completed_at` | string | ISO 8601 timestamp                           |
| `throttled`    | bool   | True if emission was suppressed by throttle  |

### `TraceEvent`

| Field         | Type          | Description                    |
| ------------- | ------------- | ------------------------------ |
| `event_id`    | string        | Deterministic event identifier |
| `event_type`  | string        | Event type name                |
| `project`     | string        | Target project                 |
| `occurred_at` | string        | ISO 8601 timestamp             |
| `throttle`    | Throttle enum | Throttle level for this event  |

### `Campaign`

Summary-only wire form returned by `ListCampaigns`. Intentionally omits
`intent_refs`, `context_paths`, `done_evidence`, and `escalation` — use
`GetCampaign` to retrieve those fields.

| Field               | Type   | Description                                                               |
| ------------------- | ------ | ------------------------------------------------------------------------- |
| `name`              | string | Campaign name                                                             |
| `project`           | string | Registered project name                                                   |
| `mission`           | string | Campaign mission statement                                                |
| `status`            | string | Durable status: `staged`, `active`, `paused`, `escalated`, or `completed` |
| `cycles_completed`  | uint64 | Number of dispatched task cycles                                          |
| `cycles_landed`     | uint64 | Number of task results whose work reached trunk                           |
| `max_cycles`        | uint64 | Configured campaign cycle budget                                          |
| `authorized_by`     | string | Owner authorization identity, or empty when absent                        |
| `agent_provider`    | string | Preferred agent provider, or empty when absent                            |
| `last_run_event_id` | string | Most recent campaign-run event ID, or empty when absent                   |

### `CampaignDetail`

Full durable definition of a campaign, as returned by `GetCampaign`. Carries all
fields in `Campaign` plus the definition-time fields that the summary form
omits.

| Field               | Type                   | Description                                                               |
| ------------------- | ---------------------- | ------------------------------------------------------------------------- |
| `name`              | string                 | Campaign name                                                             |
| `project`           | string                 | Registered project name                                                   |
| `mission`           | string                 | Campaign mission statement                                                |
| `status`            | string                 | Durable status: `staged`, `active`, `paused`, `escalated`, or `completed` |
| `cycles_completed`  | uint64                 | Number of dispatched task cycles                                          |
| `cycles_landed`     | uint64                 | Number of task results whose work reached trunk                           |
| `max_cycles`        | uint64                 | Configured campaign cycle budget                                          |
| `authorized_by`     | string                 | Owner authorization identity, or empty when absent                        |
| `agent_provider`    | string                 | Preferred agent provider, or empty when absent                            |
| `last_run_event_id` | string                 | Most recent campaign-run event ID, or empty when absent                   |
| `intent_refs`       | repeated string        | Intent reference labels that anchor the mission                           |
| `context_paths`     | repeated string        | Paths to context documents for the campaign                               |
| `done_evidence`     | repeated DoneEvidence  | Completion criteria with `Gate`/`Review` type distinction                 |
| `escalation`        | repeated string        | Human-readable escalation instructions                                    |
| `owner_decisions`   | repeated OwnerDecision | Append-only owner policy decisions recorded after escalations             |

### `OwnerDecision`

One recorded owner decision attached to a campaign after an escalation. These
records are append-only and are threaded back into future campaign formation
prompts as binding context.

| Field           | Type   | Description                                            |
| --------------- | ------ | ------------------------------------------------------ |
| `decision`      | string | Owner-authored decision text                           |
| `authorized_by` | string | Owner identity copied from the campaign at record time |
| `decided_at`    | string | RFC 3339 timestamp recorded by the daemon              |

### `DoneEvidence`

One completion-evidence item. The `kind` field distinguishes the two variants.

**Gate variant** (`kind = "gate"`):

| Field       | Type            | Description                                                        |
| ----------- | --------------- | ------------------------------------------------------------------ |
| `kind`      | string          | `"gate"`                                                           |
| `command`   | string          | Shell command to evaluate                                          |
| `required`  | bool            | Whether this gate is required for completion                       |
| `statement` | string          | Empty for Gate items                                               |
| `artifacts` | repeated string | Repository-relative paths that must exist before the gate can pass |

**Review variant** (`kind = "review"`):

| Field       | Type            | Description                         |
| ----------- | --------------- | ----------------------------------- |
| `kind`      | string          | `"review"`                          |
| `command`   | string          | Empty for Review items              |
| `required`  | bool            | `false` for Review items            |
| `statement` | string          | Human-readable completion statement |
| `artifacts` | repeated string | Empty for Review items              |

### `TraceBlockExecution`

| Field                   | Type            | Description                                                          |
| ----------------------- | --------------- | -------------------------------------------------------------------- |
| `block_name`            | string          | Name of the block that executed                                      |
| `trigger_event_id`      | string          | Event ID that triggered this block                                   |
| `success`               | bool            | Whether the block succeeded                                          |
| `summary`               | string          | Human-readable summary of the result                                 |
| `emitted_event_ids`     | repeated string | IDs of events emitted by this block                                  |
| `duration_ms`           | uint64          | Wall-clock milliseconds for this block execution (including retries) |
| `raw_output`            | string          | Combined stdout+stderr from any shell command run by this block      |
| `exit_code`             | int32           | Exit code from any shell command run by this block                   |
| `trigger_payload_json`  | string          | JSON payload of the event that triggered this block                  |
| `emitted_payload_jsons` | repeated string | JSON payloads of events emitted by this block                        |
| `audit_artifacts`       | repeated string | Paths to audit artefact files produced by this block                 |

### `Throttle` (enum)

| Value                 | Number | Description                       |
| --------------------- | ------ | --------------------------------- |
| `THROTTLE_FULL`       | 0      | All blocks emit                   |
| `THROTTLE_AUDIT_ONLY` | 1      | Observers emit, mutators suppress |
| `THROTTLE_DRY_RUN`    | 2      | Read-only, no side effects        |
