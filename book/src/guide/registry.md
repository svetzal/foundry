# The Project Registry

The registry is Foundry's source of truth for which projects exist on your
machine and what automation applies to each one. Without a populated registry,
the daemon starts successfully but skips all project-specific work.

## Where the Registry Lives

By default: `~/.foundry/registry.json`

Override the path with the environment variable:

```bash
export FOUNDRY_REGISTRY_PATH=/path/to/my-registry.json
```

The daemon reads the registry on startup. If the file is missing it logs a
warning and continues with an empty registry (no projects will be processed).

## Registry Format (v2)

```json
{
  "version": 2,
  "projects": [
    {
      "name": "my-tool",
      "path": "/Users/alice/projects/my-tool",
      "stack": "rust",
      "agent": "claude",
      "repo": "alice/my-tool",
      "branch": "main",
      "skip": false,
      "actions": {
        "iterate": true,
        "maintain": true,
        "push": true,
        "audit": true,
        "release": false
      },
      "install": {
        "command": "cargo install --path ."
      }
    }
  ]
}
```

### Top-level fields

| Field | Type | Description |
|-------|------|-------------|
| `version` | number | Must be `2` |
| `projects` | array | List of `ProjectEntry` objects |

### ProjectEntry fields

| Field | Required | Type | Description |
|-------|----------|------|-------------|
| `name` | Yes | string | Unique human-readable identifier used in events and logs |
| `path` | Yes | string | Absolute path to the project on your local filesystem |
| `stack` | Yes | string | Technology stack — see [Stack values](#stack-values) |
| `agent` | Yes | string | AI agent name passed to `hone` commands (e.g. `"claude"`) |
| `repo` | Yes | string | GitHub repository slug (`owner/repo`) used by `Watch Pipeline` |
| `branch` | Yes | string | Default branch (e.g. `"main"`) — validation checks out this branch |
| `skip` | No | bool | Set to `true` to exclude this project from all runs; default `false` |
| `actions` | No | object | Which automation steps are enabled; all default to `false` |
| `install` | No | object | How to reinstall locally after automation — see [InstallConfig](#installconfig) |

### Stack values

The `stack` field tells Foundry which audit tool to use and how to run
stack-specific commands.

| Value | Audit tool | Notes |
|-------|------------|-------|
| `"rust"` | `cargo audit --json` | Requires `cargo-audit` to be installed |
| `"typescript"` | `npm audit --json` | Exit code 1 = vulnerabilities found (not a tool failure) |
| `"python"` | `pip-audit --format=json` | Requires `pip-audit` to be installed |
| `"elixir"` | `mix deps.audit --format=json` | — |

### ActionFlags

The `actions` object controls which steps run during a maintenance run. All
flags default to `false` when the `actions` key is absent.

| Flag | What it enables |
|------|-----------------|
| `iterate` | Runs `hone iterate <agent> --json` after project validation passes |
| `maintain` | Runs `hone maintain` — either after `iterate` completes (when both are enabled) or directly after validation (when only `maintain` is enabled) |
| `push` | Pushes commits to the remote via `git push` after a commit is made |
| `audit` | (Reserved for future use — currently informational only) |
| `release` | (Reserved for future use — currently informational only) |

### InstallConfig

The `install` field configures how the project is reinstalled locally after
automation completes. Exactly one variant is used per entry:

**Command** — runs an arbitrary shell command in the project directory:

```json
"install": { "command": "cargo install --path ." }
```

**Brew** — installs via a Homebrew formula:

```json
"install": { "brew": "my-formula" }
```

## Minimal Project Entry

Only the six required fields are needed. All optional fields default to safe
values (no actions enabled, no install step, not skipped):

```json
{
  "version": 2,
  "projects": [
    {
      "name": "minimal-project",
      "path": "/Users/alice/projects/minimal-project",
      "stack": "rust",
      "agent": "claude",
      "repo": "alice/minimal-project",
      "branch": "main"
    }
  ]
}
```

## Excluding a Project Temporarily

Set `"skip": true` to pause automation without removing the entry:

```json
{
  "name": "on-hold",
  "path": "/Users/alice/projects/on-hold",
  "stack": "typescript",
  "agent": "claude",
  "repo": "alice/on-hold",
  "branch": "main",
  "skip": true
}
```

The `Validate Project` block silently acknowledges skipped projects (emits
`project_validation_completed` with `status: "skipped"`) so the engine trace
remains complete.

## Multiple Projects

A single registry file can declare any number of projects. They are processed
concurrently during a maintenance run (up to `max_concurrent` at a time, which
defaults to the number of active projects unless the orchestrator is configured
otherwise):

```json
{
  "version": 2,
  "projects": [
    {
      "name": "api-server",
      "path": "/Users/alice/projects/api-server",
      "stack": "rust",
      "agent": "claude",
      "repo": "alice/api-server",
      "branch": "main",
      "actions": { "iterate": true, "maintain": true, "push": true }
    },
    {
      "name": "frontend",
      "path": "/Users/alice/projects/frontend",
      "stack": "typescript",
      "agent": "claude",
      "repo": "alice/frontend",
      "branch": "main",
      "actions": { "maintain": true, "push": true }
    }
  ]
}
```
