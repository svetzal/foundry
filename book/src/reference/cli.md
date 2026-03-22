# CLI Commands

The `foundry` CLI communicates with a running `foundryd` daemon over gRPC.

## Global Options

| Option | Default | Description |
|--------|---------|-------------|
| `--addr <url>` | `http://[::1]:50051` | Daemon address |

## `foundry emit`

Emit an event into the system. May trigger a workflow chain.

```bash
foundry emit <event_type> --project <project> [--throttle <level>] [--payload <json>] [--wait]
```

| Argument | Required | Description |
|----------|----------|-------------|
| `event_type` | Yes | Event type name (positional) |
| `--project` | Yes | Target project |
| `--throttle` | No | `full`, `audit_only`, or `dry_run` (default: `full`) |
| `--payload` | No | JSON string with event-specific data |
| `--wait` | No | Block until processing completes, then display the trace |

By default, `emit` returns immediately after the daemon accepts the event.
Use `--wait` to block until the full event chain finishes and then display
the trace output (equivalent to running `foundry trace` after completion).

**Output (default):**

```text
Event emitted: evt_47fcb603e1b18c8435b8cc3b
```

**Output (with `--wait`):**

```text
Event emitted: evt_47fcb603e1b18c8435b8cc3b
Waiting for processing to complete...
greet_requested (evt_47fcb603e1b18c8435b8cc3b) project=hello
  → Compose Greeting (1ms): ok — composed greeting for Stacey
    greeting_composed (evt_a1b2c3d4e5f6) project=hello
      → Deliver Greeting (0ms): ok — delivered greeting: Hello, Stacey!
---
Total: 2ms (blocks: 1ms)
```

## `foundry status`

Show status of active workflows. Queries the daemon for workflows currently
being processed in the background.

```bash
foundry status [workflow_id]
```

Without an argument, shows all active workflows. With a workflow ID, shows
details for that specific workflow.

**Output example:**

```text
evt_47fcb603e1b18c8435b8cc3b [iteration_requested] foundry — running
```

## `foundry watch`

Stream live events as they are emitted in real time.

```bash
foundry watch [--project <project>]
```

| Option | Required | Description |
|--------|----------|-------------|
| `--project` | No | Filter by project name; omit to see all projects |

Server-side streaming — stays open until interrupted (`Ctrl-C`). Each line shows
the event type, event ID, project, and payload (when non-empty).

**Output example:**

```text
maintenance_run_started evt_abc project=my-tool
project_validation_completed evt_def project=my-tool
  payload: {"status":"ok","has_gates":true}
project_iterate_completed evt_ghi project=my-tool
```

## `foundry run`

Trigger a maintenance run for all active projects or a single named project.

```bash
foundry run [--project <project>] [--throttle <level>]
```

| Option | Required | Description |
|--------|----------|-------------|
| `--project` | No | Limit run to a single project by name; omit to run all projects |
| `--throttle` | No | `full`, `audit_only`, or `dry_run` (default: `full`) |

`foundry run` emits a `maintenance_run_started` event which triggers the
maintenance workflow chain: validate → iterate (if enabled) → maintain (if
enabled) → commit and push → post-push audit.

When `--project` is omitted, the project name sent to the daemon is `"system"`,
which causes all active (non-skipped) projects to be processed.

**Output:**

```text
Triggered maintenance run for my-tool
Event: evt_47fcb603e1b18c8435b8cc3b
```

Use `foundry watch` or `foundry trace <event_id>` to follow the resulting chain.

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
