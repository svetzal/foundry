# launchd LaunchAgent Configuration

This directory contains macOS LaunchAgent plist files for running Foundry components automatically.

## Files

- `com.mojility.foundryd.plist` — Persistent daemon: keeps `foundryd` running at all times (KeepAlive), started on login.
- `com.mojility.foundry-maintenance.plist` — Scheduled job: runs `foundry run` nightly at 2:00 AM.

## Prerequisites

Both plist files use `YOUR_USERNAME` as a placeholder for your macOS username. You must replace this before loading them.

## Installation

### Step 1: Replace the username placeholder

Run the following command from the project root to substitute your actual username into both plists:

```bash
sed -i "" "s/YOUR_USERNAME/$USER/g" launchd/*.plist
```

Verify the substitution looks correct:

```bash
grep -n "YOUR_USERNAME\|$USER" launchd/*.plist
```

### Step 2: Copy the plists to your LaunchAgents directory

```bash
cp launchd/*.plist ~/Library/LaunchAgents/
```

### Step 3: Load the agents

Load the foundryd daemon (starts immediately and on every subsequent login):

```bash
launchctl load ~/Library/LaunchAgents/com.mojility.foundryd.plist
```

Load the maintenance schedule (triggers `foundry run` at 2:00 AM daily):

```bash
launchctl load ~/Library/LaunchAgents/com.mojility.foundry-maintenance.plist
```

## Unloading

To stop and unload an agent:

```bash
launchctl unload ~/Library/LaunchAgents/com.mojility.foundryd.plist
launchctl unload ~/Library/LaunchAgents/com.mojility.foundry-maintenance.plist
```

## Logs

Both agents write stdout and stderr to `~/Library/Logs/`:

- `~/Library/Logs/foundryd.log` — daemon output
- `~/Library/Logs/foundry-maintenance.log` — maintenance run output

You can tail the daemon log in real time:

```bash
tail -f ~/Library/Logs/foundryd.log
```

## Notes

- These are **LaunchAgents** (not LaunchDaemons), so they run in the logged-in user's context and have access to user environment variables and the user's home directory.
- The binaries are expected at `~/.cargo/bin/foundryd` and `~/.cargo/bin/foundry`. Build and install them with `cargo install --path crates/foundryd` and `cargo install --path crates/foundry-cli` respectively.
- If you move your Cargo installation (e.g., change `CARGO_HOME`), update the `ProgramArguments` paths in the plist files before loading.
