# gRPC API

The Foundry service is defined in `proto/foundry.proto`.

## Service: `Foundry`

### `Emit(EmitRequest) → EmitResponse`

Fire an event into the system. The engine processes the event synchronously
and returns after the entire chain completes.

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

### `Watch(StatusRequest) → stream WorkflowStatus`

Server-side streaming of workflow status updates.

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

Traces are stored in memory with a 1-hour TTL. Queries for expired or
unknown event IDs return `found: false` with empty lists.

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

### `Throttle` (enum)

| Value | Number | Description |
|-------|--------|-------------|
| `THROTTLE_FULL` | 0 | All blocks emit |
| `THROTTLE_AUDIT_ONLY` | 1 | Observers emit, mutators suppress |
| `THROTTLE_DRY_RUN` | 2 | Read-only, no side effects |
