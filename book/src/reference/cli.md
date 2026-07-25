# CLI Commands

The `foundry` CLI communicates with a running `foundryd` daemon over gRPC.

## Global Options

| Option         | Default                  | Description    |
| -------------- | ------------------------ | -------------- |
| `--addr <url>` | `http://127.0.0.1:50051` | Daemon address |

## `foundry emit`

Emit an event into the system. May trigger a workflow chain.

```bash
foundry emit <event_type> --project <project> [--throttle <level>] [--payload <json>] [--wait]
```

| Argument     | Required | Description                                              |
| ------------ | -------- | -------------------------------------------------------- |
| `event_type` | Yes      | Event type name (positional)                             |
| `--project`  | Yes      | Target project                                           |
| `--throttle` | No       | `full` or `dry_run` (default: `full`)                    |
| `--payload`  | No       | JSON string with event-specific data                     |
| `--wait`     | No       | Block until processing completes, then display the trace |

By default, `emit` returns immediately after the daemon accepts the event. Use
`--wait` to block until the full event chain finishes and then display the trace
output (equivalent to running `foundry trace` after completion).

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
in-memory `WorkflowTracker` that is populated when each `Emit` request spawns a
background task and cleared on completion. This is a live view only; completed
work moves to durable trace history and no longer appears here.

```bash
foundry status [workflow_id] [--span <span-id>]
```

Without an argument, shows all active workflows. With a workflow ID, shows
details for that specific workflow. `--span` first resolves the span through
the daemon's `Span` RPC and then keeps only active workflows in that span's
trace. If the daemon is unreachable, the command fails; there is no offline
status cache or trace-file fallback.

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

| Option      | Required | Description                                      |
| ----------- | -------- | ------------------------------------------------ |
| `--project` | No       | Filter by project name; omit to see all projects |

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

| Option       | Required | Description                                                     |
| ------------ | -------- | --------------------------------------------------------------- |
| `--project`  | No       | Limit run to a single project by name; omit to run all projects |
| `--throttle` | No       | `full` or `dry_run` (default: `full`)                           |

`foundry run` emits a `maintenance_run_started` event which triggers the
maintenance workflow chain: validate → iterate (if enabled) → maintain (if
enabled) → commit and push → post-push audit.

The command streams progress events in real time and **exits automatically**
when the daemon broadcasts a `maintenance_run_completed` event at the end of the
processing chain. This differs from `foundry watch`, which streams indefinitely.

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

Use `foundry trace <event_id>` to inspect the full trace after the run
completes.

## `foundry task`

Run one concrete coding objective against a registered project.

```bash
foundry task <project> "<description>" [--agent <provider>]
```

| Argument      | Required | Description                                             |
| ------------- | -------- | ------------------------------------------------------- |
| `project`     | Yes      | Registered project name                                 |
| `description` | Yes      | One concrete objective, supplied as a positional string |
| `--agent`     | No       | Override the registered agent provider for this task    |

The command waits for `task_run_completed`, streams block progress, and renders
the full trace. Execution and verification occur in an isolated Git worktree.
The terminal event contains one structural verdict:

| Verdict               | Meaning                                                                                                     |
| --------------------- | ----------------------------------------------------------------------------------------------------------- |
| `complete`            | Required gates and skeptical review passed; deliverable changes landed on trunk, or no landing was required |
| `remainder`           | The objective is incomplete; `gaps[]` names what remains                                                    |
| `defect`              | The implementation or evidence is wrong; `diagnosis` explains why                                           |
| `blocked_on_decision` | A human choice is required; `finding` and `options[]` carry it                                              |
| `runner_error`        | The agent, workspace, Git, or provider failed                                                               |

Task execution has no retry route. All task work is committed before terminal
state. Non-complete work is preserved on a remote branch or, if push fails, in a
Git bundle. For landed work, `preservation_ref` carries the landed commit SHA;
otherwise it identifies the preserved branch or bundle artifact.

## `foundry campaign`

Manage durable, evidence-terminated engineering objectives.

```bash
foundry campaign add <definition.json>
foundry campaign list
foundry campaign show <name>
foundry campaign advance <name>
foundry campaign pause <name>
foundry campaign decide <name> --decision "Use the generated tonic client path."
foundry campaign complete <name> --reason "Production evidence confirms the mission shipped."
foundry campaign resume <name>
foundry campaign resume <name> --add-cycles 2
```

| Subcommand | Daemon required?       | Description                                                                 |
| ---------- | ---------------------- | --------------------------------------------------------------------------- |
| `add`      | Yes unless `--offline` | Validate and atomically add one JSON definition                             |
| `list`     | Yes unless `--offline` | Show campaign status and cycle counts                                       |
| `show`     | Yes unless `--offline` | Show the complete stored campaign record                                    |
| `advance`  | Yes                    | Re-evaluate done evidence and dispatch one next task, complete, or escalate |
| `pause`    | Yes unless `--offline` | Halt future automatic and manual advancement                                |
| `decide`   | Yes unless `--offline` | Record an owner decision on an escalated campaign and return it to active   |
| `complete` | Yes unless `--offline` | Mark an authorized campaign complete with an auditable owner reason         |
| `resume`   | Yes unless `--offline` | Return an authorized paused or escalated campaign to active state           |

The store defaults to `~/.foundry/campaigns.json` and can be overridden with
`FOUNDRY_CAMPAIGNS_PATH`. A definition requires non-empty `name`, `project`, and
`mission` fields plus at least one `done_evidence` item. `authorized_by` is
required before `decide`, `complete`, or `resume`. `decide` is valid only when
the campaign is currently `escalated`; it appends an owner decision record and
makes that policy available to the next formation run. `resume --add-cycles N`
is required when an exhausted budget needs an explicit positive extension.
Without `--offline`, all eight campaign subcommands are daemon-authoritative:
they require a reachable `foundryd` daemon, fail without touching the
client-side `FOUNDRY_CAMPAIGNS_PATH` if the daemon is unreachable, and render
the daemon's typed response or workflow output directly rather than re-reading
the client store. See [Tasks and Campaigns](../guide/campaigns.md) for the
definition schema and lifecycle.

`advance` has no offline formation path. It re-runs campaign done-evidence
against delivered trunk, inspects recent objective history and accumulated
preserved work, and chooses exactly one of done, one next objective, or
escalation. A dispatched task runs the project's resolved gates in its isolated
worktree; campaign gate commands are not copied into task acceptance evidence.

Each advance prints and watches a root event ID. Its events carry trace and span
context, while task-side events also carry `campaign_cycle`.
`campaign_advance_completed` retains the formation prompt, agent provider, and
done-evidence gate results, allowing the cycle to be reconstructed through
`foundry trace <event-id> --verbose`.

For online mutations, daemon-side persistence is atomic at the control-plane
boundary: if a save fails, the command surfaces the typed gRPC `INTERNAL` error,
the daemon-owned campaign store stays byte-identical on disk, and
`campaign complete` does not emit a terminal completion event.

`complete` is the owner-authorized external terminal path. It accepts any
non-completed campaign state, requires a non-empty `--reason`, clears any stale
pending run result, records the reason with the authorizing owner and timestamp,
and emits the normal `campaign_completed` terminal event. Repeating it for an
already-completed campaign is idempotent.

## `foundry sentinel`

Inspect or toggle the daemon-owned scheduled sentinels.

```bash
foundry sentinel list [--offline]
foundry sentinel show <name> [--offline]
foundry sentinel enable <name> [--offline]
foundry sentinel disable <name> [--offline]
```

| Subcommand | Daemon required?       | Description                                                              |
| ---------- | ---------------------- | ------------------------------------------------------------------------ |
| `list`     | Yes unless `--offline` | Render every daemon-owned sentinel entry                                 |
| `show`     | Yes unless `--offline` | Render one exact-name daemon-owned sentinel                              |
| `enable`   | Yes unless `--offline` | Mark one sentinel enabled and wake the in-process scheduler immediately  |
| `disable`  | Yes unless `--offline` | Mark one sentinel disabled and wake the in-process scheduler immediately |

The store defaults to `~/.foundry/sentinels.json` and can be overridden with
`FOUNDRY_SENTINELS_PATH`. Without `--offline`, all four commands are
daemon-authoritative: `list` renders `SentinelList`, `show` renders
`SentinelShow`, `enable` calls `SentinelEnable`, and `disable` calls
`SentinelDisable`. The online path never reads the client-side sentinel file,
so an absent `FOUNDRY_SENTINELS_PATH` stays absent and a malformed trap file is
left byte-identical.

If `foundryd` is unreachable, the command fails with a stable actionable error
that names the matching offline recovery command. There is no silent fallback.
Pass `--offline` only when the daemon is stopped and you intentionally want to
read or mutate the sentinel JSON file directly.

Online `enable` and `disable` are persistence-atomic at the daemon boundary:
the daemon writes a same-directory temporary file and renames it into place only
after the full JSON payload is ready. If that save fails, the RPC returns
`INTERNAL`, the daemon-owned in-memory sentinel store remains unchanged, `list`
and `show` continue to reflect the pre-mutation state, the scheduler is not
notified, and the on-disk `sentinels.json` bytes remain unchanged.

## `foundry validate`

Validate quality gates for one or more projects without running iterate or
maintain workflows. This is a read-only operation — no code changes are made.

```bash
foundry validate <project>...
foundry validate --all
```

| Argument  | Required             | Description                                  |
| --------- | -------------------- | -------------------------------------------- |
| `project` | Yes (unless `--all`) | One or more project names (positional)       |
| `--all`   | No                   | Validate all active projects in the registry |

For each project, emits a `validation_requested` event which triggers:
`Resolve Gates` → `Run Preflight Gates` → `Route Validation Result` →
`validation_completed`. No Mutator blocks are involved, so throttle level is
irrelevant.

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

View one completed event chain from the daemon-owned trace store.

```bash
foundry trace <event_id> [--verbose] [--flat]
```

| Argument    | Required | Description                                                                         |
| ----------- | -------- | ----------------------------------------------------------------------------------- |
| `event_id`  | Yes      | Root event ID returned by `foundry emit` (positional)                               |
| `--verbose` | No       | Show trigger payloads, emitted payloads, raw shell output, and audit artifact paths |
| `--flat`    | No       | Force the legacy chronological event tree instead of the default span tree           |

By default this renders the daemon's span tree view. `--flat` forces the
legacy chronological event tree. The CLI asks the daemon for the trace; it does
not inspect `FOUNDRY_TRACES_DIR` directly unless you explicitly choose offline
history browsing. Traces are persisted under `~/.foundry/traces/YYYY-MM-DD/`,
survive daemon restarts, and are available through the daemon even after the
in-memory cache has expired.

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

Browse durable completed traces from the daemon-owned trace store.

```bash
foundry history [<date>] [--project <project>] [--offline]
```

| Argument    | Required | Description                                               |
| ----------- | -------- | --------------------------------------------------------- |
| `date`      | No       | Date in `YYYY-MM-DD` format; omit to show the last 7 days |
| `--project` | No       | Filter results by project name                            |
| `--offline` | No       | Explicitly read local trace files instead of the daemon   |

Without `--offline`, `history` is daemon-authoritative: it calls the daemon's
typed `History` RPC, renders daemon-owned results directly, and never reads or
creates a client-side `FOUNDRY_TRACES_DIR`. If the daemon is unreachable, the
command fails and suggests rerunning with `--offline`. Use `--offline`
deliberately when you want direct file diagnostics against
`~/.foundry/traces/` (or `FOUNDRY_TRACES_DIR`).

Each row shows the event ID, trace ID, success status, duration, event type,
and project. Dates with no traces are omitted. Within a day, rows are rendered
in deterministic newest-first order.

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
This is an explicit offline recovery command and requires `--offline`. It
rejects runs without `--offline` before contacting the daemon or touching the
registry path. Does nothing if the file already exists. This command never uses
the daemon, even when `foundryd` is running.

```bash
foundry --offline registry init
```

### `foundry registry list`

List all projects in the daemon-owned registry as a table. By default this
requires a reachable `foundryd` daemon. Use `--offline` to read the local
registry file directly for recovery. Without `--offline`, an unreachable daemon
returns an error and leaves the client-side registry file untouched. The online
path renders the daemon response directly and does not create
`FOUNDRY_REGISTRY_PATH`. Typed daemon failures render as
`daemon error: <Code> — <message>`.

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

The `Skill` column shows `auto` (default derived command), `cmd` (custom
command), `off` (explicitly disabled), or blank (not configured).

### `foundry registry show <name>`

Show all details for a single project from the daemon-owned registry. By default
this requires a reachable `foundryd` daemon. Use `--offline` to read the local
registry file directly for recovery. Without `--offline`, an unreachable daemon
returns an error and leaves the client-side registry file untouched. The online
path renders the daemon response directly and does not create
`FOUNDRY_REGISTRY_PATH`. Missing projects surface the daemon's typed `NotFound`
status.

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

Add a new project to the daemon-owned registry. By default this requires a
reachable `foundryd` daemon. Use `--offline` to write the local registry file
directly for recovery. Without `--offline`, an unreachable daemon returns an
error and leaves the client-side registry file untouched. In offline mode, if
the registry file does not exist, it is created automatically. The online path
mutates daemon-owned state only and does not create `FOUNDRY_REGISTRY_PATH`. If
daemon persistence fails, the command surfaces the daemon's stable `INTERNAL`
error and leaves the daemon-owned registry unchanged in memory and on disk.
Duplicate names and invalid inputs surface the daemon's typed `AlreadyExists`
and `InvalidArgument` statuses.

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

| Option              | Required | Description                                                       |
| ------------------- | -------- | ----------------------------------------------------------------- |
| `--name`            | Yes      | Unique project name                                               |
| `--path`            | Yes      | Absolute path to the project                                      |
| `--stack`           | Yes      | Technology stack: `rust`, `python`, `typescript`, `elixir`, `cpp` |
| `--agent`           | Yes      | AI agent name (e.g. `claude`)                                     |
| `--repo`            | Yes      | GitHub slug (`owner/repo`)                                        |
| `--branch`          | No       | Default branch (default: `main`)                                  |
| `--iterate`         | No       | Enable iterate action                                             |
| `--maintain`        | No       | Enable maintain action                                            |
| `--push`            | No       | Enable push action                                                |
| `--audit`           | No       | Enable audit action                                               |
| `--release`         | No       | Enable release action                                             |
| `--install-command` | No       | Shell command to run for local install                            |
| `--install-brew`    | No       | Homebrew formula name                                             |
| `--notes`           | No       | Human-readable notes                                              |
| `--timeout-secs`    | No       | Command timeout in seconds (default: 3600)                        |

### `foundry registry remove <name>`

Remove a project from the daemon-owned registry. By default this requires a
reachable `foundryd` daemon. Use `--offline` to mutate the local registry file
directly for recovery. Without `--offline`, an unreachable daemon returns an
error and leaves the client-side registry file untouched. The online path
mutates daemon-owned state only and does not create `FOUNDRY_REGISTRY_PATH`. If
daemon persistence fails, the command surfaces the daemon's stable `INTERNAL`
error and leaves the daemon-owned registry unchanged in memory and on disk.
Missing projects surface the daemon's typed `NotFound` status.

```bash
foundry registry remove my-tool
```

### `foundry registry edit <name>`

Update settings for an existing project. Only the fields you pass are changed;
all others are left as-is. By default this requires a reachable `foundryd`
daemon. Use `--offline` to mutate the local registry file directly for recovery.
Without `--offline`, an unreachable daemon returns an error and leaves the
client-side registry file untouched. The online path mutates daemon-owned state
only and does not create `FOUNDRY_REGISTRY_PATH`. If daemon persistence fails,
the command surfaces the daemon's stable `INTERNAL` error and leaves the
daemon-owned registry unchanged in memory and on disk. Missing projects surface
the daemon's typed `NotFound` status.

```bash
foundry registry edit my-tool \
  --skip "Waiting for CI to stabilise" \
  --timeout-secs 3600
```

| Option              | Description                                                  |
| ------------------- | ------------------------------------------------------------ |
| `--path`            | Update the project path                                      |
| `--stack`           | Update the technology stack                                  |
| `--agent`           | Update the agent name                                        |
| `--repo`            | Update the GitHub slug                                       |
| `--branch`          | Update the default branch                                    |
| `--skip`            | Set a skip reason (pass empty string `""` to clear the skip) |
| `--iterate`         | Set iterate action (`true`/`false`)                          |
| `--maintain`        | Set maintain action                                          |
| `--push`            | Set push action                                              |
| `--audit`           | Set audit action                                             |
| `--release`         | Set release action                                           |
| `--install-command` | Set install command                                          |
| `--install-brew`    | Set Homebrew formula                                         |
| `--notes`           | Set notes (pass empty string `""` to clear)                  |
| `--timeout-secs`    | Set command timeout in seconds                               |
