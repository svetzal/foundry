# Linux systemd Setup

This directory contains a user-level `systemd` service template for running
`foundryd` on Linux infrastructure.

Use a user service instead of a root system service. Foundry needs the same
user-level state as the agent tools it orchestrates: GitHub auth, repository
checkouts, Claude/Codex credentials, `~/.foundry`, and shell PATH. Running the
daemon as root creates ownership and credential mismatches.

## Prerequisites

- Linux x86_64 host
- `systemd` with user services
- `git`
- Authenticated agent CLI, such as Claude Code or Codex CLI, on the service
  user's PATH
- GitHub access for repositories Foundry will operate on

For unattended servers, enable lingering so the user service runs after logout:

```bash
sudo loginctl enable-linger "$USER"
```

## Install release binaries

Install the latest release tarball into `/usr/local/bin`:

```bash
set -euo pipefail

VERSION=v0.26.1
tmpdir="$(mktemp -d)"
base_url="https://github.com/svetzal/foundry/releases/download/${VERSION}"

curl -L "${base_url}/foundry-linux-x64.tar.gz" \
  -o "${tmpdir}/foundry-linux-x64.tar.gz"
tar -xzf "${tmpdir}/foundry-linux-x64.tar.gz" -C "${tmpdir}"

sudo install -m 0755 "${tmpdir}/foundry" /usr/local/bin/foundry
sudo install -m 0755 "${tmpdir}/foundryd" /usr/local/bin/foundryd

foundry --version
```

If the machine has `gh` authenticated, the download can instead use GitHub CLI:

```bash
VERSION=v0.26.1
tmpdir="$(mktemp -d)"

gh release download "${VERSION}" \
  --repo svetzal/foundry \
  --pattern foundry-linux-x64.tar.gz \
  --dir "${tmpdir}"
tar -xzf "${tmpdir}/foundry-linux-x64.tar.gz" -C "${tmpdir}"

sudo install -m 0755 "${tmpdir}/foundry" /usr/local/bin/foundry
sudo install -m 0755 "${tmpdir}/foundryd" /usr/local/bin/foundryd
```

## Configure service environment

Create an optional environment file for host-specific paths and feature flags:

```bash
mkdir -p ~/.config/foundry
$EDITOR ~/.config/foundry/foundryd.env
```

Example:

```text
FOUNDRYD_LISTEN_ADDR=0.0.0.0:50051
FOUNDRY_DIGESTS_DIR=/home/svetzal/Work/Operations/Automation/commit-digests
FOUNDRY_OPS_DIGESTS_DIR=/home/svetzal/Work/Operations/Automation/ops-digests
FOUNDRY_OPS_EVENTS_DIR=/home/svetzal/Work/Operations/Events/intake
```

Leave the file absent if the default `~/.foundry/*` paths are acceptable.

### Put Foundry state on a data disk

`FOUNDRY_HOME` moves the whole Foundry home: the registry, ledger, events,
traces, worktrees, preserved bundles and evidence. Task worktrees and build
evidence are large, so on a host with a small root filesystem put the home on
the data disk:

```text
FOUNDRY_HOME=/srv/work/foundry
```

Set the same value in the operating user's shell profile (for example
`~/.profile`), so `foundry --offline …` and the scripts read the same files as
the daemon. A per-path variable such as `FOUNDRY_REGISTRY_PATH` still wins over
`FOUNDRY_HOME`.

To move an existing home:

1. Drain the daemon: `foundry pacing drain --timeout 30m`. Do this outside the
   02:00 nightly window.
2. Stop the daemon: `systemctl --user stop foundryd`.
3. Copy the home, then check the copy:
   `rsync -aH ~/.foundry/ /srv/work/foundry/`.
4. Move the old home aside and leave a symbolic link in its place:
   `mv ~/.foundry ~/.foundry.pre-move && ln -s /srv/work/foundry ~/.foundry`.
   The ledger, events and traces record absolute paths (worktrees, preserved
   bundles, evidence) under `~/.foundry`. The link keeps those paths valid.
5. Add `FOUNDRY_HOME=/srv/work/foundry` to `~/.config/foundry/foundryd.env`
   and to the shell profile.
6. Start the daemon and resume: `systemctl --user start foundryd`, then
   `foundry pacing resume --lane all`.
7. Check the daemon: `foundry status`, `foundry queue`. When a nightly has
   run, remove `~/.foundry.pre-move`.

To roll back, stop the daemon, remove `FOUNDRY_HOME` from both files, replace
the link with the directory, and start the daemon.

Only use a non-loopback `FOUNDRYD_LISTEN_ADDR` on a trusted LAN or VPN. The
daemon's gRPC control plane is plaintext in this slice.

## Install and start the user service

From the Foundry repository:

```bash
mkdir -p ~/.config/systemd/user
cp systemd/foundryd.service ~/.config/systemd/user/foundryd.service

systemctl --user daemon-reload
systemctl --user enable --now foundryd
systemctl --user status foundryd
```

Install the Foundry skill and verify the daemon:

```bash
foundry init --global
foundry status
foundry sentinel list
```

## Update procedure

Install the newer release binaries, then restart the user service:

```bash
systemctl --user restart foundryd
foundry --version
foundry status
```

If the new release changes bundled skill guidance, rerun:

```bash
foundry init --global --force
```

## Logs

Use `journalctl` for service logs:

```bash
journalctl --user -u foundryd -f
```

Foundry workflow state is stored under `~/.foundry` (or `$FOUNDRY_HOME`) unless overridden with
environment variables.
