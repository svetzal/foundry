# Foundry

Foundry is an event-driven workflow engine for engineering automation. It runs
quality gates, focused coding tasks, durable multi-cycle campaigns, maintenance,
dependency remediation, releases, and scheduled operational workflows across a
registry of Git repositories.

Foundry has two processes:

- `foundryd` is the long-running daemon that owns workflow state, routes events,
  runs task blocks, and records traces.
- `foundry` is the command-line client used by people, scripts, and agents.

The two communicate over gRPC. Every workflow is observable as an event chain,
and mutating work can be simulated with dry-run throttle.

## Why Foundry

Traditional automation expresses a whole workflow as one script. Foundry
expresses reusable steps as task blocks connected by typed events. This gives
the system a few useful properties:

- the same workflow can be triggered manually, on a schedule, or by another
  workflow;
- independent projects and event branches can run concurrently;
- every block execution and emitted event is retained in a trace;
- observer blocks still run in dry-run mode while mutators simulate success;
- coding work is checked against repository intent and mechanical gates before
  it can land;
- broader missions can continue across multiple evidence-reviewed tasks without
  relying on a stale, pre-cut backlog.

## Install

### Homebrew

```bash
brew tap svetzal/tap
brew install foundry
```

### From source

Building from source requires Rust 1.85 or newer and `protoc`.

```bash
git clone https://github.com/svetzal/foundry.git
cd foundry
./install.sh
```

The installer builds both binaries and installs them to `~/.cargo/bin`. On macOS
it also applies stable ad-hoc signing identifiers so privacy grants survive
future upgrades.

## Quick Start

Start the daemon:

```bash
foundryd
```

Register a Git repository in another terminal:

```bash
foundry registry add \
  --name my-project \
  --path /absolute/path/to/my-project \
  --stack rust \
  --agent codex \
  --repo owner/my-project \
  --branch main \
  --iterate \
  --maintain \
  --push
```

Derive and save quality gates:

```bash
foundry gates --init my-project
foundry validate my-project
```

Run one concrete coding task:

```bash
foundry task my-project \
  "Add a --quiet flag and prove it suppresses progress output"
```

Inspect the result:

```bash
foundry history --project my-project
foundry trace <event-id> --verbose
```

## Choose the Right Workflow

| Need                                                                   | Command                                |
| ---------------------------------------------------------------------- | -------------------------------------- |
| Execute one concrete, immediately actionable objective                 | `foundry task <project> "<objective>"` |
| Complete a broader mission whose next step depends on current evidence | `foundry campaign …`                   |
| Improve a project against its charter                                  | `foundry iterate <project>`            |
| Detect intent drift without changing code                              | `foundry scout <project>`              |
| Check quality gates without changing code                              | `foundry validate <project>`           |
| Run registered maintenance work                                        | `foundry run [--project <name>]`       |
| Diagnose and remediate a failing GitHub Actions pipeline               | `foundry pipeline <project>`           |
| Run an agent-driven release                                            | `foundry release <project>`            |

Prefer these convenience commands to raw `foundry emit`; they set up event
streaming, progress display, and trace rendering for the workflow.

## Tasks

A task is one concrete coding objective. Foundry:

1. creates an isolated Git worktree;
2. checks repository intent and resolves project gates;
3. runs the coding agent;
4. executes required gates in the worktree;
5. asks a separate reviewer for a typed verdict; and
6. lands or preserves the result.

The verdict is one of `complete`, `remainder`, `defect`, `blocked_on_decision`,
or `runner_error`. Complete work lands when its required gates pass. Converging
`remainder` work may also land when at least one required gate ran and all
required gates passed; its finite gaps become the next campaign objective.
Defects, decisions, and runner failures never land.

Any work that does not land is preserved on a named remote branch or, when a
push is unavailable, in a Git bundle under `~/.foundry/preserved/`.

## Campaigns

A campaign is a durable mission, not a queue of pre-written tasks. On every
cycle, Foundry inspects the delivered trunk, any accumulated preserved work, the
last typed task result, owner decisions, recent objective history, and the
campaign's done evidence. It then chooses exactly one outcome:

- **done** — evidence proves the mission complete;
- **advance** — dispatch one next objective; or
- **escalate** — stop for budget, policy, or human judgement.

Create a definition:

```json
{
  "name": "retrieval-identity-v1",
  "project": "my-project",
  "mission": "Prove both retrieval entrypoints preserve raw response identity.",
  "context_paths": ["docs/retrieval-contract.md"],
  "done_evidence": [
    {
      "kind": "gate",
      "command": "cargo test retrieval_identity",
      "required": true,
      "artifacts": ["tests/retrieval_identity.rs"]
    },
    {
      "kind": "review",
      "statement": "Both real entrypoints are covered without masking response IDs."
    }
  ],
  "budget": { "max_cycles": 8 },
  "escalation": ["A public compatibility decision requires an owner."],
  "authorized_by": "Owner Name",
  "agent_provider": "codex"
}
```

Then add and advance it:

```bash
foundry campaign add ./retrieval-identity-v1.json
foundry campaign show retrieval-identity-v1
foundry campaign advance retrieval-identity-v1
```

Task results request the next campaign advance automatically. Use the control
commands when intervention is required:

```bash
foundry campaign pause retrieval-identity-v1
foundry campaign resume retrieval-identity-v1
foundry campaign decide retrieval-identity-v1 \
  --decision "Preserve the existing public response type."
foundry campaign resume retrieval-identity-v1 --add-cycles 2
foundry campaign complete retrieval-identity-v1 \
  --reason "Owner-reviewed production evidence confirms the mission shipped."
foundry campaign cancel retrieval-identity-v1 \
  --reason "Superseded by the streaming rewrite." --now
```

Campaign done-evidence gates run during formation against delivered trunk. They
are distinct from the project's task gates, which run inside each isolated task
worktree. Required campaign gates must therefore be runnable from the repository
checkout. Put deployment or remote-host assertions in an owner-reviewed `review`
statement, or make the remote probe optional.

See [Tasks and Campaigns](book/src/guide/campaigns.md) for the lifecycle,
evidence model, landing rules, provider pauses, budget extensions, recovery, and
cycle-level observability.

## Scheduled Work

`foundryd` includes declarative cron-like sentinels. The canonical schedules run
nightly maintenance, a daily commit digest, a periodic operations digest, and a
nightly supply-chain scan.

```bash
foundry sentinel list
foundry sentinel show nightly-maintenance
foundry sentinel disable nightly-maintenance
foundry sentinel enable nightly-maintenance
```

Sentinel definitions live in `~/.foundry/sentinels.json`. Enable and disable
commands notify the running daemon immediately.

## Observability

Every event carries trace, span, causation, and scatter/gather context. Campaign
task-side events also carry the campaign name and cycle number, so a cycle can
be reconstructed from the event stream without inferring boundaries from
timestamps.

```bash
foundry status
foundry watch --project my-project
foundry history --project my-project
foundry trace <event-id> --verbose
```

Persistent data is stored below `~/.foundry/` by default:

| Path                   | Contents                                        |
| ---------------------- | ----------------------------------------------- |
| `registry.json`        | Registered projects and enabled actions         |
| `campaigns.json`       | Durable campaign definitions and state          |
| `events/YYYY-MM.jsonl` | Append-only workflow events                     |
| `traces/YYYY-MM-DD/`   | Renderable workflow traces                      |
| `worktrees/`           | Disposable task worktrees                       |
| `preserved/`           | Git bundle fallbacks for work that did not land |
| `sentinels.json`       | Scheduled workflow triggers                     |

Online registry and campaign commands are daemon-authoritative. Use `--offline`
only for deliberate direct-file recovery while `foundryd` is stopped.

## Dry Runs

Throttle controls mutating task blocks without removing the rest of the
workflow:

```bash
foundry run --project my-project --throttle dry_run
```

Observers execute normally. Mutators produce their simulated success events
without changing repositories or external systems.

## Documentation

The complete documentation is an mdBook under [`book/`](book/):

- [Getting Started](book/src/guide/getting-started.md)
- [Project Registry](book/src/guide/registry.md)
- [Tasks and Campaigns](book/src/guide/campaigns.md)
- [Workflow Formations](book/src/guide/workflow-formations.md)
- [CLI Reference](book/src/reference/cli.md)
- [Architecture](book/src/architecture/concepts.md)

Build it locally with:

```bash
mdbook build book
```

## Development

Foundry is a Rust workspace containing:

- `foundry-sdk` — event, payload, gate, campaign, and task-block contracts;
- `foundry-engine` — event routing, propagation, retries, and gathering;
- `foundry-blocks` — production workflow blocks;
- `foundryd` — the daemon and gRPC service; and
- `foundry-cli` — the command-line client and renderers.

Run the required quality gates:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
mdbook build book
```

See [AGENTS.md](AGENTS.md) for repository conventions and release procedure.

## Licence

Foundry is released under the [MIT Licence](LICENSE).
