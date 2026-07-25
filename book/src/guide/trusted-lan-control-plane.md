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
or daemon-written path:

| Path | Purpose |
| --- | --- |
| `~/.foundry/registry.json` | Project registry authority |
| `~/.foundry/campaigns.json` | Durable campaign authority |
| `~/.foundry/sentinels.json` | Sentinel authority |
| `~/.foundry/agents.json` | Agent model/provider configuration |
| `~/.foundry/worktrees/` | Isolated task worktrees |
| `~/.foundry/preserved/` | Preserved bundles/refs for non-complete work |
| `~/.foundry/events/YYYY-MM.jsonl` | Durable event log |
| `~/.foundry/traces/YYYY-MM-DD/` | Durable trace files |
| `~/.foundry/audits/{project}/` | Audit artifacts |
| `~/.foundry/digests/YYYY-MM-DD.md` | Commit digests |
| `~/.foundry/ops-digests/YYYY-MM-DD.md` | Ops digests |
| `~/.foundry/ops-digest.watermark` | Ops digest intake watermark |
| `~/.foundry/triage/YYYY-MM-DD.md` | Maintenance triage digests |
| `~/.foundry/supply-chain/YYYY-MM-DD.md` | Supply-chain digests |
| `~/.foundry/agent-sessions/` | Agent transcript JSONL output |

If the current machine overrides any of these with environment variables, the
override file itself is part of the authority boundary too:

- macOS launchd plist environment entries
- Linux `~/.config/foundry/foundryd.env`

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

### 4. Perform the One-Time Copy

Copy the full authority tree once from the Mac to `mojility-ops-01`:

```bash
rsync -a --delete ~/.foundry/ mojility-ops-01:~/.foundry/
rsync -a ~/.config/foundry/foundryd.env mojility-ops-01:~/.config/foundry/foundryd.env
```

If the Mac launchd plist carries environment-only overrides that are not in a
file, reproduce them in `~/.config/foundry/foundryd.env` before starting the
remote daemon.

### 5. Start `mojility-ops-01` as the New Sole Authority

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

### 6. Validate Remote Command Parity

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

Do not retire the Mac daemon until these remote reads match expectations.

### 7. Roll Back if Parity Fails

If parity checks fail, stop the remote daemon before re-enabling the Mac:

```bash
ssh mojility-ops-01 'systemctl --user stop foundryd'
unset FOUNDRY_DAEMON_ADDR
launchctl load ~/Library/LaunchAgents/com.mojility.foundryd.plist
```

If `mojility-ops-01` accepted any writes before failure, copy `~/.foundry/`
back to the Mac while both daemons are stopped, then restart only one daemon.

### 8. Retire the Mac Daemon Only After Success

Once remote parity is confirmed, leave the Mac daemon unloaded and keep the CLI
pointed at `http://mojility-ops-01:50051`. From that point forward,
`mojility-ops-01` is the only writable Foundry authority.
