# Maintenance Workflow

The maintenance workflow runs iterate and maintain automation against each
registered project, committing and pushing any changes they produce. It is
designed to be triggered nightly via launchd or manually via `foundry emit`.

## How It Works

Each project goes through its own independent chain. The chain is driven
entirely by events — no mutable state is shared between projects.

### Per-Project Chain

```mermaid
flowchart TD
    A([maintenance_run_started]) --> B[[Validate Project]]
    B --> C([project_validation_completed])
    C --> D[[Route Project Workflow]]
    D -->|iterate=true| E([iteration_requested])
    D -->|iterate=false, maintain=true| F([maintenance_requested])
    D -->|no actions enabled| G([end])
    E --> H[[Resolve Gates]]
    H --> I[[Run Preflight Gates]]
    I --> J[[Check Charter]]
    J --> K[[Assess Project]]
    K --> L[[Triage Assessment]]
    L --> M[[Create Plan]]
    M --> N[[Execute Plan]]
    N --> O[[Run Verify Gates]]
    O --> P[[Route Gate Result]]
    P -->|pass| Q([project_iteration_completed])
    P -->|fail, retries left| R[[Retry Execution]]
    Q -->|maintain=true| F
    F --> S[[Resolve Gates]]
    S --> T[[Execute Maintain]]
    T --> U[[Run Verify Gates]]
    U --> V[[Route Gate Result]]
    V -->|pass| W([project_maintenance_completed])
    V -->|fail, retries left| X[[Retry Execution]]
```

### Routing Logic

`Route Project Workflow` reads the `actions` flags forwarded in the
`project_validation_completed` payload and makes a single decision:

| Condition | Emits |
|-----------|-------|
| `status != "ok"` | nothing — chain stops |
| `actions.iterate = true` | `iteration_requested` |
| `actions.iterate = false`, `actions.maintain = true` | `maintenance_requested` |
| both false | nothing — no automation enabled |

When `iterate = true`, the `actions.maintain` flag is forwarded inside the
`iteration_requested` payload. After a successful iteration, the gate routing
emits `maintenance_requested` automatically when that flag is `true`,
so the maintain sub-workflow starts without an extra routing step.

## Triggering a Maintenance Run

To run maintenance for a single project:

```bash
foundry emit project_validation_completed my-project \
  --payload '{"status":"ok","actions":{"iterate":true,"maintain":true}}'
```

To trigger the full nightly cycle:

```bash
foundry emit maintenance_run_started my-project
```

## Throttle Behaviour

| Throttle | Effect |
|----------|--------|
| `full` | All blocks execute and emit events |
| `audit_only` | Observers emit; mutators suppress output |
| `dry_run` | Observers emit; mutators are skipped entirely |

Under `dry_run`, only `iteration_requested` or `maintenance_requested` are
emitted (by the Observer router). No execution blocks run.

## Agent Capabilities

The maintenance workflow uses a single agent invocation in `Execute Maintain`:

| Phase | Capability | Model | Access | Purpose |
|-------|-----------|-------|--------|---------|
| Execute Maintain | Coding | `claude-sonnet-4-6` | Full | Update dependencies, fix vulnerabilities, resolve gate failures |

Gate definitions are passed as context so the agent knows what must pass
after its changes. If the project has an agent file registered, it is
supplied via `--agent`.

See the [Iteration Workflow](iteration-workflow.md#agent-capabilities) for
the full model-to-capability mapping and CLI parameters used across all
agent invocations.
