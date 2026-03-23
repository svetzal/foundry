# gRPC API

The Foundry service is defined in `proto/foundry.proto`.

## Service: `Foundry`

### `Emit(EmitRequest) → EmitResponse`

Fire an event into the system. The engine spawns processing as a background
task and returns the event ID immediately. Use `Trace` to check for
completion, `Status` to see in-flight workflows, or `Watch` for real-time
event streaming.

**Request:**

| Field | Type | Description |
|-------|------|-------------|
| `event_type` | string | Event type name |
| `project` | string | Target project |
| `throttle` | Throttle enum | `THROTTLE_FULL`, `THROTTLE_AUDIT_ONLY`, `THROTTLE_DRY_RUN` |
| `payload_json` | string | Optional JSON payload |

**Response:**

| Field | Type | Description |
|-------|------|-------------|
| `event_id` | string | Deterministic ID of the created event |
| `workflow_id` | string | ID of the triggered workflow (if any) |

### `Status(StatusRequest) → StatusResponse`

Query active workflow states.

**Request:**

| Field | Type | Description |
|-------|------|-------------|
| `workflow_id` | string | Specific workflow (empty for all active) |

**Response:**

| Field | Type | Description |
|-------|------|-------------|
| `workflows` | repeated WorkflowStatus | Active workflow states |

### `Watch(WatchRequest) → stream WatchResponse`

Server-side streaming of live events as they are processed by the engine.
Optionally filtered by project name.

**Request:**

| Field | Type | Description |
|-------|------|-------------|
| `project` | string | Project name to filter by; empty string for all projects |

**Response (stream):**

| Field | Type | Description |
|-------|------|-------------|
| `event_id` | string | Event identifier |
| `event_type` | string | Event type name |
| `project` | string | Target project |
| `payload_json` | string | Event payload as JSON |

### `Trace(TraceRequest) → TraceResponse`

Retrieve the trace of a completed event chain. Returns all events produced
during processing and a record of each block execution.

**Request:**

| Field | Type | Description |
|-------|------|-------------|
| `event_id` | string | Root event ID to look up |

**Response:**

| Field | Type | Description |
|-------|------|-------------|
| `found` | bool | Whether a trace was found for the given event ID |
| `events` | repeated TraceEvent | All events in the chain |
| `block_executions` | repeated TraceBlockExecution | Record of each block execution |

Completed traces are persisted to disk under `~/.foundry/traces/YYYY-MM-DD/`
and survive daemon restarts. The `Trace` RPC checks the in-memory store first
(for recently completed chains) and falls back to disk for older traces.

## Messages

### `WorkflowStatus`

| Field | Type | Description |
|-------|------|-------------|
| `workflow_id` | string | Workflow identifier |
| `workflow_type` | string | Workflow type name |
| `project` | string | Target project |
| `state` | string | pending, running, completed, failed |
| `started_at` | string | ISO 8601 timestamp |
| `completed_at` | string | ISO 8601 timestamp (empty if running) |
| `task_blocks` | repeated TaskBlockStatus | Per-block status |

### `TaskBlockStatus`

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Block name |
| `state` | string | pending, running, completed, skipped, failed |
| `started_at` | string | ISO 8601 timestamp |
| `completed_at` | string | ISO 8601 timestamp |
| `throttled` | bool | True if emission was suppressed by throttle |

### `TraceEvent`

| Field | Type | Description |
|-------|------|-------------|
| `event_id` | string | Deterministic event identifier |
| `event_type` | string | Event type name |
| `project` | string | Target project |
| `occurred_at` | string | ISO 8601 timestamp |
| `throttle` | Throttle enum | Throttle level for this event |

### `TraceBlockExecution`

| Field | Type | Description |
|-------|------|-------------|
| `block_name` | string | Name of the block that executed |
| `trigger_event_id` | string | Event ID that triggered this block |
| `success` | bool | Whether the block succeeded |
| `summary` | string | Human-readable summary of the result |
| `emitted_event_ids` | repeated string | IDs of events emitted by this block |
| `duration_ms` | uint64 | Wall-clock milliseconds for this block execution (including retries) |
| `raw_output` | string | Combined stdout+stderr from any shell command run by this block |
| `exit_code` | int32 | Exit code from any shell command run by this block |
| `trigger_payload_json` | string | JSON payload of the event that triggered this block |
| `emitted_payload_jsons` | repeated string | JSON payloads of events emitted by this block |
| `audit_artifacts` | repeated string | Paths to audit artefact files produced by this block |

### `Throttle` (enum)

| Value | Number | Description |
|-------|--------|-------------|
| `THROTTLE_FULL` | 0 | All blocks emit |
| `THROTTLE_AUDIT_ONLY` | 1 | Observers emit, mutators suppress |
| `THROTTLE_DRY_RUN` | 2 | Read-only, no side effects |
