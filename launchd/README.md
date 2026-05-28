# launchd LaunchAgent Configuration

This directory contains the macOS LaunchAgent plist used to keep `foundryd`
running. With sentinels internalised inside the daemon (see
`book/src/guide/sentinels.md`), launchd's job is now only "keep the daemon
alive" — proactive workflows like the nightly maintenance run live in
`~/.foundry/sentinels.json` instead of in a separate scheduled plist.

## Files

- `com.mojility.foundryd.plist` — Persistent daemon: keeps `foundryd` running
  at all times (KeepAlive), started on login.

## Prerequisites

The plist uses `YOUR_USERNAME` as a placeholder for your macOS username. You
must replace it before loading.

## Installation

### Step 1: Replace the username placeholder

```bash
sed -i "" "s/YOUR_USERNAME/$USER/g" launchd/*.plist
```

Verify:

```bash
grep -n "YOUR_USERNAME\|$USER" launchd/*.plist
```

### Step 2: Copy the plist to your LaunchAgents directory

```bash
cp launchd/*.plist ~/Library/LaunchAgents/
```

### Step 3: Load the daemon

```bash
launchctl load ~/Library/LaunchAgents/com.mojility.foundryd.plist
```

## Unloading

```bash
launchctl unload ~/Library/LaunchAgents/com.mojility.foundryd.plist
```

To pick up a freshly built `foundryd` binary, unload and reload — this is
the canonical post-`./install.sh` step.

## Logs

`~/Library/Logs/foundryd.log` collects stdout and stderr from the daemon.
Tail it in real time:

```bash
tail -f ~/Library/Logs/foundryd.log
```

## Migration: removing the legacy maintenance plist

Older installs ran the nightly maintenance cycle via a second LaunchAgent,
`com.mojility.foundry-maintenance.plist`. That plist has been removed from
the repo as of Slice 1 of the Sentinel work — the same schedule now lives in
the in-daemon `nightly-maintenance` sentinel.

If you previously installed it, unload and delete it:

```bash
launchctl unload ~/Library/LaunchAgents/com.mojility.foundry-maintenance.plist
rm ~/Library/LaunchAgents/com.mojility.foundry-maintenance.plist
```

After the next `foundryd` restart, confirm the sentinel is active:

```bash
foundry sentinel list
```

You should see `nightly-maintenance` with `Enabled: yes` and schedule
`cron: 0 2 * * *`. Without this cleanup step the daemon **and** the legacy
plist would both emit `maintenance_cycle_started` each night, doubling the
work.

## Notes

- This is a **LaunchAgent** (not a LaunchDaemon), so it runs in the
  logged-in user's context and has access to user environment variables
  and the home directory.
- The binary is expected at `~/.cargo/bin/foundryd` (or the Homebrew path
  if installed via `brew install svetzal/tap/foundry`). Update the
  `ProgramArguments` path in the plist before loading if your install
  layout differs.
