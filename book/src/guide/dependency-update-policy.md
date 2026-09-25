# Dependency Update Policy

Nightly maintenance moves a project's dependencies only as far as the
project's `update_policy` allows. Foundry decides what to move in code, before
the maintain agent starts. The agent gets a list of the exact updates to apply,
and it must not go beyond that list.

## The policy

Each registered project has an `update_policy`. The policy is a ceiling. Each
level includes the levels below it.

| Policy | What maintenance may do |
| --- | --- |
| `patch` | Move the lockfile inside the existing manifest constraints. No manifest constraint changes. |
| `minor` | Also widen a constraint to the newest release that is not a breaking (major) release. |
| `major` | Also take major releases. A major upgrade never happens inside the nightly maintain session. Each one becomes its own `foundry task`. |

When a project has no policy, maintenance behaves as `minor`. The maintenance
summary lists the project under "No update policy set", so that you choose one.

Set the policy with the registry commands:

```bash
foundry registry edit my-tool --update-policy major
foundry registry add --name my-tool ... --update-policy patch
```

`foundry registry show my-tool` shows the policy on the `Updates:` line.

### What counts as a major

A move is a major when it changes the first version component. Semver treats
`0.x` versions differently, and so does Foundry:

- In a `0.x` version, a change to the second component is a major (`0.7.4` to
  `0.8.0`).
- In a `0.0.x` version, every change is a major (`0.0.3` to `0.0.4`).

Calendar versions follow the same rule, so `2025.2` to `2026.1` is a major.

## Classification

The `Classify Dependency Updates` step runs after the gates resolve and before
`Execute Maintain`. For each direct dependency it finds the locked version, the
manifest constraint, and the published releases. Then it records three
candidate versions:

| Field | Meaning |
| --- | --- |
| `in_range` | The newest non-major release that the current constraint admits. A lockfile-only move. |
| `non_major` | The newest release that is not a major from the current version. |
| `major` | The newest release, when it is a major. |

Foundry reads each ecosystem's own files and asks each ecosystem's own
registry. It does not need the project's toolchain to be installed.

| Ecosystem | Direct dependencies | Locked version | Releases |
| --- | --- | --- | --- |
| Cargo | `Cargo.toml` and its workspace members | `Cargo.lock` | crates.io sparse index |
| Hex | `mix.exs` of each Mix project the audit covers | `mix.lock` | hex.pm API |
| npm | `package.json` | `package-lock.json` or `bun.lock` | npm registry |
| PyPI | `pyproject.toml` (project, optional, dependency groups) | `uv.lock` | PyPI JSON API |
| Maven | `gradle/libs.versions.toml` | the catalog itself | Maven Central, Google Maven, Gradle plugin portal |
| SwiftPM | `Package.swift` | `Package.resolved` | the package repository's git tags |

Each ecosystem's constraint operators are read correctly: Cargo caret and
tilde, Elixir `~>`, npm `^`, `~`, x-ranges and hyphen ranges, PEP 440
specifiers, Gradle catalog versions and Maven ranges, and SwiftPM `from:`,
`upToNextMinor`, `exact:` and ranges. Pre-releases are never candidates. A
Maven qualifier is a flavor, not an upgrade: `33.1.0-jre` follows
`33.0.0-jre`, and `0.8.0-0.6.x-compat` is not an upgrade of `0.8.0`.

A Gradle `[versions]` key that several libraries share (for example `ktor`)
moves as one, so it is classified once, under the key's name.

### Not classified

Anything Foundry cannot classify is reported as "not classified", with the
reason. It is never reported as up to date. Examples:

- a stack with no classifier (C++),
- a missing or unsupported lockfile (`bun.lockb`, `yarn.lock`, `poetry.lock`),
- a Kotlin project without a version catalog,
- a dependency that is declared but not in the lockfile,
- a registry that did not answer.

## The maintain brief

From the classification and the policy, Foundry builds the brief. For each
outdated dependency it decides one of these:

- **Apply** the move. `patch` projects get the `in_range` version. `minor` and
  `major` projects get the `non_major` version. The brief says whether the move
  is lockfile-only or needs a constraint edit.
- **Held back by policy**: a `patch` project's move that needs a constraint
  edit.
- **Held by a hold**: see [Holds](#holds).
- **Major**: never applied in the maintain session. See
  [The majors lane](#the-majors-lane).

The maintain prompt lists the updates to apply, then the held and major
updates for information. It tells the agent to use targeted commands (for
example `cargo update -p <pkg> --precise <version>` or
`uv lock --upgrade-package <pkg>==<version>`), never a blanket upgrade, and not
to change any other dependency. A retry of a failed maintain run is told not to
move any dependency beyond what the first attempt applied.

To see the brief for a project without running maintenance:

```bash
foundry deps my-tool
foundry deps my-tool --policy major    # preview another policy; the registry does not change
```

`foundry deps` emits `dependency_review_requested`. It prints the outdated
dependencies, the brief, and what the majors lane would do. It changes nothing.

## Security fixes override the ceiling

The brief uses the advisories from the most recent nightly supply-chain scan
(at most eight days old). When the policy's target does not reach a fixed
release:

- A fix that is a patch or a minor move is applied anyway. The brief marks it
  "beyond the policy ceiling".
- A fix that is a major move goes to the majors lane, also for `minor` and
  `patch` projects.
- A vulnerable package that is not a direct dependency becomes a lockfile move
  to at least the fixed version, or a major task when that move is a major.

The supply-chain auto-fix engine follows the same rule. See
[Supply-chain scan](supply-chain.md#the-auto-fix-engine-gated-dark).

## Holds

A hold keeps a package at or below a version. Commit a
`.dependency-holds.json` file in the repository root:

```json
{
  "version": 1,
  "holds": [
    {
      "package": "phoenix_live_view",
      "max": "1.1",
      "reason": "vendored Roost requires ~> 1.1",
      "expires": "2026-12-24"
    }
  ]
}
```

- `max` is a version prefix. `"1.1"` admits every `1.1.x` and lower. `"1"`
  admits every `1.x`.
- `expires` is a date. After that date the hold lapses: the held update comes
  back, and the summary lists the hold under "Lapsed holds — re-decide". A date
  that does not parse also lapses the hold.
- `ecosystem` (optional) limits the hold to one ecosystem, for a package name
  that two ecosystems share.

An active hold caps the brief at the newest release inside the hold. A major
that the hold blocks does not go to the majors lane.

A security fix respects a hold when a fixed release exists inside it. When no
fixed release exists inside the hold, the fix overrides the hold, and the brief
and the summary say so.

If the file is not valid JSON, no holds apply, and the classification carries
a warning that names the file. This mirrors `.supply-chain-allow.json`.

## The majors lane

After the nightly maintenance run, the `Plan Major Upgrades` step decides each
major:

| Status | When |
| --- | --- |
| `dispatch` | The project's policy is `major` (or the major is a security fix), and nothing below applies. |
| `proposed` | The project's policy is `minor` or `patch`. The summary prints the `foundry task` command to run it. |
| `deferred` | Maintenance for the project did not succeed. |
| `deduped` | A task for the same project, package and target version is in flight (started in the last 24 hours), or an earlier task left a preserved remainder, defect or blocked decision whose branch or bundle still exists. |
| `overflow` | Over the per-project or per-night cap. The summary prints the command. |

Each dispatched major becomes one `foundry task` with the objective "Upgrade
`<pkg>` from `<a>` to `<b>` in `<project>`: adapt call sites, keep all gates
green". The task runs through the normal task formation: an isolated worktree,
the project's gates, and a landing on trunk only when the task is complete and
green. Otherwise its work is preserved.

The daemon starts the dispatched tasks after the maintenance summary is
written, one after another, each as its own workflow. Under `dry_run` nothing
is dispatched; the summary says "would dispatch".

Caps bound the cost:

| Variable | Default | Purpose |
| --- | --- | --- |
| `FOUNDRY_MAJOR_TASKS_PER_PROJECT` | `2` | Most major-upgrade tasks dispatched for one project in one night |
| `FOUNDRY_MAJOR_TASKS_PER_NIGHT` | `6` | Most major-upgrade tasks dispatched in one night, across every project |

Projects are taken in name order, security majors first, so the caps fall the
same way each night.

## The summary

The maintenance summary has a "Dependency drift" table near the top, beside
"Unpushed commits" and "Scanner failures". It has one row per project: the
policy, the updates applied by class, the counts held by policy and by holds,
the majors lane's decisions, and the scopes not classified. Below the table it
lists:

- **Applied beyond the brief**: moves the agent made that the brief did not
  list, and any major. Foundry finds these by comparing the classification
  before the agent ran with the one after maintenance completed.
- **No update policy set**.
- **Lapsed holds — re-decide**.

A "Dependencies" section after the status table gives each project's detail,
with the `foundry task` command for every major that was not dispatched.
