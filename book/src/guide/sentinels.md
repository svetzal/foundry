# Sentinels (Scheduled Triggers)

A **sentinel** is a declarative, named, scheduled trigger that lives inside
`foundryd` and emits an event into the engine when its schedule fires. They
internalise the kinds of proactive workflows that previously required an
external scheduler (a launchd plist, a cron entry, a systemd timer).

The shipping example is `nightly-maintenance`: at 02:00 every day it emits
`maintenance_cycle_started` with `project = "system"`, which is exactly the
event the `foundry run` command used to emit from outside the daemon.

## Why have them at all?

Before sentinels, the nightly maintenance run lived in
`launchd/com.mojility.foundry-maintenance.plist`. That meant:

- The trace store and event history had no record of *why* a cycle started.
  It just appeared, as if conjured from outside the daemon.
- Adding a second proactive workflow meant editing a per-machine system file.
- Pausing or rescheduling required editing that system file too.

Sentinels move all of that inside `foundryd`. The launchd plist's only job
becomes "keep `foundryd` running"; everything proactive happens through the
event bus, the scheduler, and the registry the daemon already owns.

## How they fire

```mermaid
flowchart LR
    A[sentinels.json] -->|loaded at startup| B(Scheduler)
    B -->|tokio::time::sleep until next firing| C{deadline reached?}
    C -->|yes| D[build Event from EmitSpec]
    D --> E[engine.process]
    C -->|reload signal| B
    F[foundry sentinel<br/>enable/disable] -->|gRPC mutation| G[Notify::notify_one]
    G --> B
```

The scheduler always knows the soonest upcoming firing across every enabled
sentinel. It blocks on `tokio::select!` between that deadline and a reload
`Notify`; when the reload fires (because a CLI command toggled a sentinel's
`enabled` state), the loop recomputes deadlines.

## The default seed

On first start the daemon writes `~/.foundry/sentinels.json` with the
canonical seed set — currently two entries:

```json
{
  "version": 1,
  "sentinels": [
    {
      "name": "nightly-maintenance",
      "schedule": { "cron": "0 2 * * *" },
      "emit": {
        "event_type": "maintenance_cycle_started",
        "project": "system",
        "throttle": "full",
        "payload": {}
      },
      "enabled": true
    },
    {
      "name": "daily-commit-digest",
      "schedule": { "cron": "0 17 * * *" },
      "emit": {
        "event_type": "commit_digest_started",
        "project": "system",
        "throttle": "full",
        "payload": {}
      },
      "enabled": true
    }
  ]
}
```

The `daily-commit-digest` sentinel drives the [Commit Digest](commit-digest.md)
formation.

Subsequent starts read the existing file and **additively merge** any seed
entries whose names are not yet present — so new Foundry releases that
ship additional canonical sentinels reach existing installs automatically
on the next restart, without manual JSON edits and without overwriting
user toggles or hand-edited cron on entries already in the file. The path
is overridable via `FOUNDRY_SENTINELS_PATH`.

## Schedule format

Slice 1 supports a single schedule kind, `cron`, taking a standard 5-field
expression evaluated in **local time**:

```
minute hour day-of-month month day-of-week
```

Examples:

| Cron | Means |
|---|---|
| `0 2 * * *` | Every day at 02:00 local |
| `*/30 * * * *` | Every 30 minutes |
| `0 9 * * 1` | Every Monday at 09:00 local |
| `0 17 * * 1-5` | 17:00 on weekdays |

Six-field expressions (`second minute hour dom month dow`) are also accepted
verbatim; five-field expressions are auto-padded with a leading `0` for
seconds. The schedule shape is an externally-tagged enum
(`{"cron": "..."}`) so future kinds (`interval`, `event_silence`) can be
added without breaking the existing wire format.

## Defaults you should know about

- **Time zone is local.** A cron expression of `0 2 * * *` means 02:00
  wherever the daemon is running, not 02:00 UTC. This matches how the
  launchd plist behaved.
- **Catch-up policy is "skip missed firings."** If the daemon was down at
  02:00 and starts at 05:00, the next firing is the *next* 02:00. Sentinels
  intentionally never play catch-up on miss — workflows like maintenance
  should not run twice in quick succession because the daemon was offline.
- **Auto-seeding only happens once.** Subsequent restarts read the existing
  file even if you have deleted every entry. To restore the seed: remove
  `~/.foundry/sentinels.json` and restart.

## CLI

`foundry sentinel list | show | enable | disable` mirrors `foundry registry`
exactly:

```bash
foundry sentinel list
foundry sentinel show nightly-maintenance

# Toggle through gRPC (preferred — the daemon's scheduler wakes immediately):
foundry sentinel disable nightly-maintenance
foundry sentinel enable nightly-maintenance

# Toggle the file directly when the daemon is not running:
foundry sentinel disable --offline nightly-maintenance
```

`list` and `show` always read the file directly. `enable` and `disable` try
the gRPC `SentinelEnable` / `SentinelDisable` RPCs first; if the daemon is
unreachable they fall back to direct file mutation (and print a warning so
you know to restart the daemon to pick up the change).

## Adding a new sentinel

There are two paths:

- **Canonical sentinels** (the ones every Foundry install should have) are
  added to `SentinelStore::default_seed()` in `foundry-sdk`. The additive
  seed-merge that runs on every daemon start will append them to existing
  `sentinels.json` files automatically — no JSON editing, no migration
  step. This is how `daily-commit-digest` reached installs that were
  already running Slice 1.
- **One-off, machine-local sentinels** still need a manual edit. Open
  `~/.foundry/sentinels.json` in your editor, append an entry following
  the schema shown above, and restart the daemon. `foundry sentinel add
  | remove | edit` is deferred to a later slice.

Future slices will also bring non-cron schedule kinds (`interval`,
`event_silence`) so sentinels can be event-driven, not just timer-driven.

## Relationship to launchd

After Slice 1, `launchd/com.mojility.foundry-maintenance.plist` is removed.
Only `com.mojility.foundryd.plist` remains, and its job shrinks to "keep the
daemon alive." If you are upgrading from an older Foundry install that had
the maintenance plist loaded, unload and delete it:

```bash
launchctl unload ~/Library/LaunchAgents/com.mojility.foundry-maintenance.plist
rm ~/Library/LaunchAgents/com.mojility.foundry-maintenance.plist
```

After the next `foundryd` restart, `foundry sentinel list` should show
`nightly-maintenance` as `enabled`. Without this cleanup step the daemon
*and* the legacy plist would both emit `maintenance_cycle_started` each
night, doubling the work.
