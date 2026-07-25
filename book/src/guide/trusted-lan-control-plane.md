# Trusted-LAN Control Plane

Foundry's control plane is plaintext gRPC. It is intended only for loopback or
for a trusted LAN or VPN segment that you already control with host firewalls
and network policy. Foundry does not add TLS, authentication, clustering, or
multi-writer coordination in this slice.

## Address Configuration

`foundryd` still defaults to `127.0.0.1:50051`.

Set `FOUNDRYD_LISTEN_ADDR` to bind somewhere else at daemon startup:

```bash
FOUNDRYD_LISTEN_ADDR=0.0.0.0:50051 foundryd
FOUNDRYD_LISTEN_ADDR=192.168.10.24:50051 foundryd
```

`foundry` resolves the daemon URL in this order:

1. Explicit `--addr`
2. `FOUNDRY_DAEMON_ADDR`
3. `http://127.0.0.1:50051`

Examples:

```bash
foundry --addr http://mojility-ops-01:50051 status

export FOUNDRY_DAEMON_ADDR=http://mojility-ops-01:50051
foundry status
```

Only expose the daemon on a network you already trust. If the host is not on a
trusted segment, keep the default loopback bind.

## Authoritative State Inventory

Before moving daemon authority to another host, account for every daemon-owned
or daemon-written path. Inventory the actual resolved value for every override
below on the current Mac before you stop it, because any path moved outside
`~/.foundry/` must be copied directly and is part of the single authority set.

| Env var | Default path | Purpose |
| --- | --- |
| `FOUNDRY_REGISTRY_PATH` | `~/.foundry/registry.json` | Project registry authority |
| `FOUNDRY_CAMPAIGNS_PATH` | `~/.foundry/campaigns.json` | Durable campaign authority |
| `FOUNDRY_SENTINELS_PATH` | `~/.foundry/sentinels.json` | Sentinel authority |
| `FOUNDRY_AGENT_CONFIG_PATH` | `~/.foundry/agents.json` | Agent model/provider configuration |
| `FOUNDRY_WORKTREES_DIR` | `~/.foundry/worktrees/` | Isolated task worktrees |
| `FOUNDRY_PRESERVED_DIR` | `~/.foundry/preserved/` | Preserved bundles/refs for non-complete work |
| `FOUNDRY_EVENTS_DIR` | `~/.foundry/events/` | Durable event log root |
| `FOUNDRY_TRACES_DIR` | `~/.foundry/traces/` | Durable trace storage root |
| `FOUNDRY_AUDITS_DIR` | `~/.foundry/audits/` | Audit artifact root |
| `FOUNDRY_DIGESTS_DIR` | `~/.foundry/digests/` | Commit digest output root |
| `FOUNDRY_OPS_DIGESTS_DIR` | `~/.foundry/ops-digests/` | Ops digest output root |
| `FOUNDRY_TRIAGE_DIR` | `~/.foundry/triage/` | Maintenance triage digest root |
| `FOUNDRY_SUPPLY_CHAIN_DIR` | `~/.foundry/supply-chain/` | Supply-chain digest output root |
| `FOUNDRY_OPS_EVENTS_DIR` | `~/Work/Operations/Events/intake/` | MBOS JSONL intake root consumed by ops digests |

These non-overridable paths are still part of the authority copy set:

- `~/.foundry/ops-digest.watermark`
- `~/.foundry/agent-sessions/`

The environment source itself is also authoritative because it defines where
the daemon reads and writes:

- macOS launchd plist environment entries on the current Mac
- Linux `~/.config/foundry/foundryd.env` on `mojility-ops-01`

## Mac to `mojility-ops-01` Migration Runbook

This is a single-authority cutover. Do not allow both daemons to accept writes
at the same time.

### 1. Prepare `mojility-ops-01`

Install the release binaries and service unit, but do not start the daemon yet:

```bash
mkdir -p ~/.config/foundry ~/.config/systemd/user
cp systemd/foundryd.service ~/.config/systemd/user/foundryd.service
```

Create `~/.config/foundry/foundryd.env` on `mojility-ops-01`:

```text
FOUNDRYD_LISTEN_ADDR=0.0.0.0:50051
FOUNDRY_DIGESTS_DIR=/home/svetzal/Work/Operations/Automation/commit-digests
FOUNDRY_OPS_DIGESTS_DIR=/home/svetzal/Work/Operations/Automation/ops-digests
FOUNDRY_OPS_EVENTS_DIR=/home/svetzal/Work/Operations/Events/intake
FOUNDRY_SUPPLY_CHAIN_DIR=/home/svetzal/Work/Operations/Automation/supply-chain-audits
```

Adjust host firewalls so only trusted LAN or VPN clients can reach TCP 50051.

Before cutover day, prepare the destination parents for any override that will
live outside `~/.foundry/`, especially `FOUNDRY_OPS_EVENTS_DIR`.

### 2. Confirm Authority Campaigns Are Complete

Before cutover, the following authority campaigns must all be complete:

- Registry authority campaign
- Campaign authority campaign
- Sentinel authority campaign
- Observability authority campaign

The point is to freeze topology and state semantics before the copy, not to
migrate live concurrent writes.

### 3. Stop the Mac Daemon

Freeze the current writer before copying any state:

```bash
launchctl unload ~/Library/LaunchAgents/com.mojility.foundryd.plist
```

Do not start `mojility-ops-01` yet. At this moment there should be zero active
writable daemons.

### 4. Inventory the Mac's Actual Authoritative Paths

Record the real source path for every override, including paths that still
happen to point inside `~/.foundry/`. The migration copy must use these
resolved locations, not assumptions:

```bash
plutil -p ~/Library/LaunchAgents/com.mojility.foundryd.plist
```

Build a cutover checklist that names the exact current Mac source and the exact
`mojility-ops-01` destination for each of:

- `FOUNDRY_REGISTRY_PATH`
- `FOUNDRY_CAMPAIGNS_PATH`
- `FOUNDRY_SENTINELS_PATH`
- `FOUNDRY_AGENT_CONFIG_PATH`
- `FOUNDRY_WORKTREES_DIR`
- `FOUNDRY_PRESERVED_DIR`
- `FOUNDRY_EVENTS_DIR`
- `FOUNDRY_TRACES_DIR`
- `FOUNDRY_AUDITS_DIR`
- `FOUNDRY_DIGESTS_DIR`
- `FOUNDRY_OPS_DIGESTS_DIR`
- `FOUNDRY_TRIAGE_DIR`
- `FOUNDRY_SUPPLY_CHAIN_DIR`
- `FOUNDRY_OPS_EVENTS_DIR`
- `~/.foundry/ops-digest.watermark`
- `~/.foundry/agent-sessions/`
- `~/Library/LaunchAgents/com.mojility.foundryd.plist`

### 5. Perform the One-Time Copy

Copy the default Foundry home once, then copy every actual override source from
the checklist once while both daemons remain stopped:

```bash
rsync -a --delete ~/.foundry/ mojility-ops-01:~/.foundry/
rsync -a ~/.config/foundry/foundryd.env mojility-ops-01:~/.config/foundry/foundryd.env
rsync -a ~/Work/Operations/Events/intake/ mojility-ops-01:/home/svetzal/Work/Operations/Events/intake/
```

Then run one `rsync -a` per overridden path whose authoritative source is
outside `~/.foundry/` or differs from the destination in
`~/.config/foundry/foundryd.env`. Do not skip directories that are only
outputs; they are still part of the authoritative state expected by the new
sole writer.

If the Mac launchd plist carries environment-only overrides that are not in a
file, reproduce them in `~/.config/foundry/foundryd.env` before starting
`mojility-ops-01`.

### 6. Start `mojility-ops-01` as the New Sole Authority

```bash
ssh mojility-ops-01 '
  systemctl --user daemon-reload &&
  systemctl --user enable --now foundryd &&
  systemctl --user status foundryd --no-pager
'
```

Point the Mac CLI at the new authority:

```bash
export FOUNDRY_DAEMON_ADDR=http://mojility-ops-01:50051
```

### 7. Validate Remote Command Parity

Run these from the Mac against `mojility-ops-01`:

```bash
foundry status
foundry registry list
foundry campaign list
foundry sentinel list
foundry history
```

Then validate a representative detail read from each authority surface:

```bash
foundry registry show <project>
foundry campaign show <campaign>
foundry sentinel show nightly-maintenance
```

Also confirm that each copied output root and the MBOS intake root exist at the
authoritative destination paths declared in `~/.config/foundry/foundryd.env`.

Do not retire the Mac daemon until these remote reads and path checks match
expectations.

### 8. Roll Back if Parity Fails

If parity checks fail, stop the remote daemon before re-enabling the Mac:

```bash
ssh mojility-ops-01 'systemctl --user stop foundryd'
unset FOUNDRY_DAEMON_ADDR
launchctl load ~/Library/LaunchAgents/com.mojility.foundryd.plist
```

If `mojility-ops-01` accepted any writes before failure, copy `~/.foundry/`
back to the Mac while both daemons are stopped, then copy back every
env-overridden authoritative path from the checklist before restarting only one
daemon.

### 9. Retire the Mac Daemon Only After Success

Once remote parity is confirmed, leave the Mac daemon unloaded and keep the CLI
pointed at `http://mojility-ops-01:50051`. From that point forward,
`mojility-ops-01` is the only writable Foundry authority.
