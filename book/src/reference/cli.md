# CLI Commands

The `foundry` CLI communicates with a running `foundryd` daemon over gRPC.

## Global Options

| Option | Default | Description |
|--------|---------|-------------|
| `--addr <url>` | `http://127.0.0.1:50051` | Daemon address |

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

Show status of active workflows. Queries the daemon for workflows that are
currently being processed in the background. The daemon tracks these via an
in-memory `WorkflowTracker` that is populated when each `Emit` request spawns
a background task and cleared on completion.

```bash
foundry status [workflow_id]
```

Without an argument, shows all active workflows. With a workflow ID, shows
details for that specific workflow.

**Output example:**

```text
evt_47fcb603e1b18c8435b8cc3b [iteration_requested] foundry — running
```

If no workflows are currently running:

```text
No active workflows.
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
project_iteration_completed evt_ghi project=my-tool
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

The command streams progress events in real time and **exits automatically**
when the daemon broadcasts a `maintenance_run_completed` event at the end of
the processing chain. This differs from `foundry watch`, which streams
indefinitely.

When `--project` is omitted, the project name sent to the daemon is `"system"`,
which causes all active (non-skipped) projects to be processed.

**Output:**

```text
Triggered maintenance run for my-tool
Event: evt_47fcb603e1b18c8435b8cc3b

[my-tool] maintenance_run_started
[my-tool] project_validation_completed (ok)
[my-tool] maintenance_run_completed (ok)
```

Use `foundry trace <event_id>` to inspect the full trace after the run completes.

## `foundry validate`

Validate quality gates for one or more projects without running iterate or
maintain workflows. This is a read-only operation — no code changes are made.

```bash
foundry validate <project>...
foundry validate --all
```

| Argument | Required | Description |
|----------|----------|-------------|
| `project` | Yes (unless `--all`) | One or more project names (positional) |
| `--all` | No | Validate all active projects in the registry |

For each project, emits a `validation_requested` event which triggers:
`Resolve Gates` → `Run Preflight Gates` → `Route Validation Result` →
`validation_completed`. No Mutator blocks are involved, so throttle level
is irrelevant.

**Output example:**

```text
Validating mojentic-ts...
  mojentic-ts: PASS
    lint: ok (required)
    format: ok (required)
    test: ok (required)
    build: ok (required)
    security: ok (optional)
validation_requested (evt_007572156d627d7b1211d76f) project=mojentic-ts
  → Resolve Gates (0ms): ok — mojentic-ts: resolved 5 gates for validate workflow
    gate_resolution_completed (evt_92531a666649d6464e569dc2) project=mojentic-ts
      → Run Preflight Gates (6931ms): ok — mojentic-ts: preflight gates passed
        preflight_completed (evt_08b0f626599a23ee8c648a8c) project=mojentic-ts
          → Route Validation Result (3ms): ok — mojentic-ts: validation passed
            validation_completed (evt_e60a246dfa9072414890fa24) project=mojentic-ts
---
```

Exits with code 0 if all projects pass, non-zero if any required gate fails.
Optional gate failures are reported but do not affect the exit code.

## `foundry trace`

View the trace of a completed event chain.

```bash
foundry trace <event_id> [--verbose]
```

| Argument | Required | Description |
|----------|----------|-------------|
| `event_id` | Yes | Root event ID returned by `foundry emit` (positional) |
| `--verbose` | No | Show trigger payloads, emitted payloads, raw shell output, and audit artifact paths |

Displays the full event propagation tree with block execution results.
Traces are stored on disk indefinitely under `~/.foundry/traces/YYYY-MM-DD/`
and survive daemon restarts.

**Output (default):**

```text
greet_requested (evt_47fcb603e1b18c8435b8cc3b) project=hello
  → ComposeGreeting: ok — composed greeting for Stacey
    greeting_composed (evt_a1b2c3d4e5f6) project=hello
      → DeliverGreeting: ok — delivered greeting: Hello, Stacey!
        greeting_delivered (evt_f6e5d4c3b2a1) project=hello
---
Total: 2ms (blocks: 1ms)
```

**Output (with `--verbose`):**

```text
greet_requested (evt_47fcb603e1b18c8435b8cc3b) project=hello
  → ComposeGreeting (1ms): ok — composed greeting for Stacey
    trigger: {"name":"Stacey"}
    emitted[0]: {"greeting":"Hello, Stacey!"}
    greeting_composed (evt_a1b2c3d4e5f6) project=hello
      → DeliverGreeting (0ms): ok — delivered greeting: Hello, Stacey!
---
Total: 2ms (blocks: 1ms)
```

If the trace is unknown:

```text
No trace found for evt_unknown (expired or unknown).
```

## `foundry history`

Browse completed traces stored on disk.

```bash
foundry history [<date>] [--project <project>]
```

| Argument | Required | Description |
|----------|----------|-------------|
| `date` | No | Date in `YYYY-MM-DD` format; omit to show the last 7 days |
| `--project` | No | Filter results by project name |

Traces are read from `~/.foundry/traces/` (or `FOUNDRY_TRACES_DIR`). Each
row shows the event ID, success status, duration, event type, and project.
Dates with no traces are omitted.

**Output example:**

```text
2026-03-22
┌──────────────────────────────┬────────┬──────────┬──────────────────────────┬───────────┐
│ Event ID                     │ Status │ Duration │ Type                     │ Project   │
╞══════════════════════════════╪════════╪══════════╪══════════════════════════╪═══════════╡
│ evt_47fcb603e1b18c8435b8cc3b │ ok     │ 312ms    │ maintenance_run_started  │ my-tool   │
│ evt_a1b2c3d4e5f6789012345678 │ ok     │ 48ms     │ greet_requested          │ hello     │
└──────────────────────────────┴────────┴──────────┴──────────────────────────┴───────────┘
```

If no traces are found:

```text
No traces found in the last 7 days.
```

## `foundry registry`

Manage the project registry without editing the JSON file directly.

```bash
foundry registry <subcommand>
```

### `foundry registry init`

Create an empty registry file at the default path (`~/.foundry/registry.json`).
Does nothing if the file already exists.

```bash
foundry registry init
```

### `foundry registry list`

List all projects in the registry as a table.

```bash
foundry registry list
```

**Output example:**

```text
┌──────────┬────────────┬──────┬──────────────────────────┬───────┐
│ Name     │ Stack      │ Skip │ Actions                  │ Skill │
╞══════════╪════════════╪══════╪══════════════════════════╪═══════╡
│ my-tool  │ rust       │ no   │ iterate, maintain, push  │ auto  │
│ frontend │ typescript │ yes  │ maintain, push           │       │
└──────────┴────────────┴──────┴──────────────────────────┴───────┘
```

The `Skill` column shows `auto` (default derived command), `cmd` (custom command), `off` (explicitly disabled), or blank (not configured).

### `foundry registry show <name>`

Show all details for a single project.

```bash
foundry registry show my-tool
```

**Output example:**

```text
Name:      my-tool
Path:      /Users/alice/projects/my-tool
Stack:     rust
Agent:     claude
Repo:      alice/my-tool
Branch:    main
Skip:      no
Actions:   iterate, maintain, push
Install:   brew: my-tool
Installs skill: yes (default -- runs my-tool init --global --force)
Timeout:   3600s (default)
```

### `foundry registry add`

Add a new project to the registry. If the registry file does not exist, it
is created automatically.

```bash
foundry registry add \
  --name my-tool \
  --path /Users/alice/projects/my-tool \
  --stack rust \
  --agent claude \
  --repo alice/my-tool \
  --branch main \
  [--iterate] [--maintain] [--push] [--audit] [--release] \
  [--install-command "cargo install --path ."] \
  [--install-brew my-formula] \
  [--notes "Human-readable notes about the project"] \
  [--timeout-secs 3600]
```

| Option | Required | Description |
|--------|----------|-------------|
| `--name` | Yes | Unique project name |
| `--path` | Yes | Absolute path to the project |
| `--stack` | Yes | Technology stack: `rust`, `python`, `typescript`, `elixir`, `cpp` |
| `--agent` | Yes | AI agent name (e.g. `claude`) |
| `--repo` | Yes | GitHub slug (`owner/repo`) |
| `--branch` | No | Default branch (default: `main`) |
| `--iterate` | No | Enable iterate action |
| `--maintain` | No | Enable maintain action |
| `--push` | No | Enable push action |
| `--audit` | No | Enable audit action |
| `--release` | No | Enable release action |
| `--install-command` | No | Shell command to run for local install |
| `--install-brew` | No | Homebrew formula name |
| `--notes` | No | Human-readable notes |
| `--timeout-secs` | No | Command timeout in seconds (default: 1800) |

### `foundry registry remove <name>`

Remove a project from the registry. Errors if the project is not found.

```bash
foundry registry remove my-tool
```

### `foundry registry edit <name>`

Update settings for an existing project. Only the fields you pass are changed;
all others are left as-is.

```bash
foundry registry edit my-tool \
  --skip "Waiting for CI to stabilise" \
  --timeout-secs 3600
```

| Option | Description |
|--------|-------------|
| `--path` | Update the project path |
| `--stack` | Update the technology stack |
| `--agent` | Update the agent name |
| `--repo` | Update the GitHub slug |
| `--branch` | Update the default branch |
| `--skip` | Set a skip reason (pass empty string `""` to clear the skip) |
| `--iterate` | Set iterate action (`true`/`false`) |
| `--maintain` | Set maintain action |
| `--push` | Set push action |
| `--audit` | Set audit action |
| `--release` | Set release action |
| `--install-command` | Set install command |
| `--install-brew` | Set Homebrew formula |
| `--notes` | Set notes (pass empty string `""` to clear) |
| `--timeout-secs` | Set command timeout in seconds |
