# Emitting Events

## From the CLI

```bash
foundry emit <event_type> --project <project> [--throttle <level>] [--payload <json>]
```

### Arguments

| Argument | Required | Description |
|----------|----------|-------------|
| `event_type` | Yes | Event type name (e.g., `greet_requested`, `vulnerability_detected`) |
| `--project` | Yes | Target project name |
| `--throttle` | No | `full` (default), `audit_only`, or `dry_run` |
| `--payload` | No | JSON string with event-specific data |
| `--addr` | No | Daemon address (default: `http://[::1]:50051`) |

### Examples

```bash
# Simple event, default throttle
foundry emit greet_requested --project hello

# With payload
foundry emit greet_requested --project hello --payload '{"name": "Stacey"}'

# Audit only — observe without mutating
foundry emit vulnerability_detected \
  --project hone-cli \
  --throttle audit_only \
  --payload '{"cve": "CVE-2026-1234", "severity": "high"}'

# Dry run — no side effects at all
foundry emit maintenance_run_started \
  --project evt-cli \
  --throttle dry_run
```

## What Happens When You Emit

1. The CLI sends the event to `foundryd` via gRPC
2. The engine creates an `Event` with a deterministic ID
3. The engine finds all task blocks that sink on the event type
4. For each matching block:
   - Check throttle: should this block execute? Should it emit?
   - Execute the block's work
   - Collect emitted events
5. Feed emitted events back into step 3 (the chain continues)
6. Return the initial event ID to the CLI

The chain continues until no more events are produced.

## Inspecting a Completed Chain

After emitting an event, use `foundry trace` with the returned event ID to
see the full propagation tree and what each block did:

```bash
$ foundry emit greet_requested --project hello --payload '{"name": "Stacey"}'
Event emitted: evt_47fcb603e1b18c8435b8cc3b

$ foundry trace evt_47fcb603e1b18c8435b8cc3b
greet_requested (evt_47fcb603e1b18c8435b8cc3b) project=hello
  → ComposeGreeting: ok — composed greeting for Stacey
    greeting_composed (evt_a1b2c3d4e5f6) project=hello
      → DeliverGreeting: ok — delivered greeting: Hello, Stacey!
        greeting_delivered (evt_f6e5d4c3b2a1) project=hello
```

Traces are kept in memory for 1 hour. After that, queries return an empty
result.
