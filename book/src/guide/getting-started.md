# Getting Started

This guide takes Foundry from installation to a registered project and a first
evidence-reviewed task. For the underlying event model, continue to
[Concepts](../architecture/concepts.md) afterward.

## Install

### Homebrew

```bash
brew tap svetzal/tap
brew install foundry
```

### From source

Source builds require Rust 1.85 or newer and the Protocol Buffers compiler.

```bash
brew install protobuf
git clone https://github.com/svetzal/foundry.git
cd foundry
./install.sh
```

`install.sh` builds and installs both binaries to `~/.cargo/bin`. On macOS it
also gives them stable ad-hoc signing identifiers. Use the script for subsequent
source upgrades so macOS privacy grants remain attached to the binaries.

The installation contains:

- `foundryd` — the long-running workflow daemon;
- `foundry` — the CLI controller.

Verify the CLI:

```bash
foundry --version
```

## Start the Daemon

Run the daemon in a terminal:

```bash
foundryd
```

By default it listens on `127.0.0.1:50051`, loads project, campaign, and
sentinel state from `~/.foundry/`, and registers the production task-block
library.

To expose the daemon on a trusted LAN, set `FOUNDRYD_LISTEN_ADDR` at startup:

```bash
FOUNDRYD_LISTEN_ADDR=0.0.0.0:50051 foundryd
```

The CLI resolves its daemon URL by precedence: explicit `--addr`, then
`FOUNDRY_DAEMON_ADDR`, then `http://127.0.0.1:50051`. See
[Trusted-LAN Control Plane](trusted-lan-control-plane.md) for the plaintext
networking model and the Mac-to-`mojility-ops-01` migration runbook.

The repository also includes service definitions for unattended operation:

- macOS: [`launchd/README.md`](../../../launchd/README.md)
- Linux: [`systemd/README.md`](../../../systemd/README.md)

Do not run `foundryd` as root. It needs the same repositories, agent
credentials, GitHub credentials, and `~/.foundry` state as the user who owns the
workspaces.

## Register a Project

Open another terminal and add a Git checkout:

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

The online command updates daemon-owned state through gRPC and persists it to
`~/.foundry/registry.json`. Confirm the result:

```bash
foundry registry show my-project
```

See [The Project Registry](registry.md) for every field, action flag, skip
reason, install strategy, and explicit offline recovery.

## Establish Quality Gates

Ask Foundry to inspect the repository and write `.hone-gates.json`:

```bash
foundry gates --init my-project
```

Review that file in the project, then run the gates without changing code:

```bash
foundry validate my-project
```

Required gates are the mechanical safety boundary for task landing. A project
should have at least one meaningful required gate before it accepts autonomous
mutations.

## Run One Task

Use a task for one concrete, immediately executable objective:

```bash
foundry task my-project \
  "Add a --quiet flag and prove it suppresses progress output"
```

Foundry creates an isolated worktree, runs the coding agent, executes the
project gates, performs a separate skeptical review, and returns one typed
verdict. Passing complete work lands on the registered branch. Safe, converging
remainder work can also land when required gates passed; defects and blocked
work are preserved without reaching trunk.

See [Tasks and Campaigns](campaigns.md) for verdicts, preservation, landing
rules, and broader multi-cycle missions.

## Inspect the Workflow

Convenience commands stream block progress and render the completed trace. You
can return to it later:

```bash
foundry history --project my-project
foundry trace <event-id>
foundry trace <event-id> --verbose
```

While work is running:

```bash
foundry status
foundry watch --project my-project
```

Traces survive daemon restarts under `~/.foundry/traces/`.

## Try a Dry Run

Throttle lets a workflow retain its observation and routing behaviour while
simulating mutators:

```bash
foundry run --project my-project --throttle dry_run
```

Observers execute normally. Mutators emit their simulated success events without
changing repositories or external systems. See [Throttle Control](throttle.md)
for the execution rules.

## Next Steps

- Use `foundry campaign` when the mission is broader than one task and the next
  objective should be derived from current evidence.
- Use `foundry iterate` for charter-driven quality improvement.
- Use `foundry scout` for read-only intent-drift discovery.
- Use `foundry run` for registered maintenance actions.
- Inspect [Sentinels](sentinels.md) for scheduled maintenance and digest
  workflows.
- Read [Workflow Formations](workflow-formations.md) to understand how events
  activate the shared task-block library.

## Build Foundry Itself

Contributors can build the workspace directly:

```bash
cargo build --workspace
```

The repository's required gates are:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps
cargo deny check
mdbook build book
```
