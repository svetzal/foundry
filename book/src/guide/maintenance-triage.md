# Post-Maintenance Failure Triage

After each nightly maintenance run, Foundry automatically classifies every gate
failure and writes a dated triage digest. The formation is **propose-only**: it
reads the Foundry event log, analyses the failures, and writes a markdown report.
It applies nothing to any project.

## How It Works

The triage formation consists of two task blocks:

1. **`TriageMaintenance`** — sinks on `MaintenanceSummaryRequested`. Reads the
   Foundry JSONL event log for the maintenance run window, extracts
   `PreflightCompleted` failures, classifies each into one of twelve domain
   classes, correlates infra flakes, detects chronic deadlocks, and emits
   `MaintenanceTriageCompleted` with a typed payload.

2. **`WriteTriageDigest`** — sinks on `MaintenanceTriageCompleted`. Renders the
   classified verdicts into a structured markdown file and writes it atomically
   to `~/.foundry/triage/YYYY-MM-DD.md`.

## Failure Classes

| Class | Meaning | Default Decision |
|-------|---------|-----------------|
| `agent_runner_fault` | Agent runner crashed, timed out, or produced a silent no-op | `suppress_infra` (correlate first) |
| `ci_infra_flake` | CI/infra ephemeral failure: filesystem error, OOM, network hiccup | `suppress_infra` (correlate first) |
| `format_and_lint_drift` | Formatting or lint drift — mechanically fixable | `auto_fixable` |
| `vuln_with_fix` | Security advisory with a known fix version | `auto_fixable` |
| `routine_dependency_bump` | Patch/minor dependency bump (no constraints broken) | `auto_fixable` |
| `vuln_no_fix` | Security advisory with no available fix | `policy_call` |
| `dependency_major_bump_or_constraint_relax` | Major version bump or constraint relaxation required | `policy_call` |
| `gate_infra_misconfig` | Gate toolchain or configuration problem | `policy_call` |
| `compile_and_static_analysis_code_error` | Compile error or static-analysis failure | `needs_investigation` |
| `test_breakage` | Test suite failure | `needs_investigation` |
| `chronic_deadlock` | N≥3 consecutive failures on the same gate | `escalate` |
| `triage_rejected_noise` | Reclassified as benign (e.g. `git push --dry-run` exit 1) | `reclassify_benign` |

## Infra Correlation

When N≥3 distinct projects share the same normalised failure signature and all
fall into the infra class (`agent_runner_fault` or `ci_infra_flake`), their
individual verdicts are collapsed into a single `InfraIncident` with decision
`suppress_infra`. This avoids noise from widespread infrastructure events
flooding the digest.

## Streak Detection

If the same `(project, gate)` pair has failed in N≥3 consecutive
`PreflightCompleted` events within the lookback window (default 14 days), the
failure is reclassified as `ChronicDeadlock` with decision `Escalate`. These
appear at the top of the digest as **Deadlock Escalations**.

## Digest Format

The digest at `~/.foundry/triage/YYYY-MM-DD.md` contains:

- **Summary table** — counts by category (total, suppressed, auto-fixable, policy, investigation, escalation)
- **Deadlock Escalations** — chronic failures needing immediate attention
- **Auto-fixable Proposals** — failures with a suggested fix command
- **Infra-suppressed (Correlated)** — collapsed infra incidents
- **Policy Calls** — failures requiring a human judgement
- **Needs Investigation** — failures without a clear mechanical fix
- **Reclassified as Benign** — outcomes accepted as noise

## Configuration

Override the triage output directory with the `FOUNDRY_TRIAGE_DIR` environment
variable. The default is `~/.foundry/triage/`.

| Variable | Default | Purpose |
|----------|---------|---------|
| `FOUNDRY_TRIAGE_DIR` | `~/.foundry/triage` | Triage digest output directory |

## Dry-run Behaviour

When triggered with `Throttle::DryRun` (e.g. `foundry run --dry-run`), the
`WriteTriageDigest` block skips the file write but still runs the full render.
The `MaintenanceTriageCompleted` event is re-emitted with `digest_path: null`.

## Event Chain

```
MaintenanceSummaryRequested
  └─► TriageMaintenance
        └─► MaintenanceTriageCompleted (verdicts, infra_incidents, counts)
              └─► WriteTriageDigest
                    └─► MaintenanceTriageCompleted (with digest_path populated)
```
