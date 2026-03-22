# CLI Commands

The `foundry` CLI communicates with a running `foundryd` daemon over gRPC.

## Global Options

| Option | Default | Description |
|--------|---------|-------------|
| `--addr <url>` | `http://[::1]:50051` | Daemon address |

## `foundry emit`

Emit an event into the system. May trigger a workflow chain.

```bash
foundry emit <event_type> --project <project> [--throttle <level>] [--payload <json>]
```

| Argument | Required | Description |
|----------|----------|-------------|
| `event_type` | Yes | Event type name (positional) |
| `--project` | Yes | Target project |
| `--throttle` | No | `full`, `audit_only`, or `dry_run` (default: `full`) |
| `--payload` | No | JSON string with event-specific data |

**Output:**

```text
Event emitted: evt_47fcb603e1b18c8435b8cc3b
Workflow started: wf_abc123  (if a workflow was triggered)
```

## `foundry status`

Show status of active workflows.

```bash
foundry status [workflow_id]
```

Without an argument, shows all active workflows. With a workflow ID, shows
details for that specific workflow including task block states.

## `foundry watch`

Stream live workflow updates as they happen.

```bash
foundry watch [workflow_id]
```

Server-side streaming — stays open until interrupted. Each update shows
the workflow state and per-task-block status including throttle suppression.

## `foundry trace`

View the trace of a completed event chain.

```bash
foundry trace <event_id>
```

| Argument | Required | Description |
|----------|----------|-------------|
| `event_id` | Yes | Root event ID returned by `foundry emit` (positional) |

Displays the full event propagation tree with block execution results.
Traces are stored in memory for 1 hour after the event chain completes.

**Output:**

```text
greet_requested (evt_47fcb603e1b18c8435b8cc3b) project=hello
  → ComposeGreeting: ok — composed greeting for Stacey
    greeting_composed (evt_a1b2c3d4e5f6) project=hello
      → DeliverGreeting: ok — delivered greeting: Hello, Stacey!
        greeting_delivered (evt_f6e5d4c3b2a1) project=hello
```

If the trace has expired or the event ID is unknown:

```text
No trace found for evt_unknown (expired or unknown).
```
