# Supply-Chain Scan

The supply-chain formation is a nightly, working-tree dependency-advisory scan
across every managed project. It is **advisory** and never fails a project run.
Its remediation engine is gated off by default; when explicitly enabled, it
may apply verified dependency fixes and commit them locally without pushing.

## Why it is its own formation

A supply-chain advisory is an *external, time-triggered* fact. It appears
because the world changed — a CVE was published against a dependency — not
because the project's own code regressed. It can go red on a repo with **zero
diff landed**.

That makes it categorically different from a quality gate, which answers "is *my
code* correct?" and fails because of *your* change. Cramming a time-triggered,
externally-owned advisory check into the change-triggered, blocking preflight
gate set has three failure modes:

1. it aborts preflight, killing the very maintain step that could bump the dep;
2. it has no memory — it re-discovers and re-fails the same CVE every night;
3. it conflates "my code regressed" with "an advisory dropped" under one red
   checkmark, so a project with perfect code reads as failed.

The release-tag audit action (`ReleaseTagAudited`) already handles supply-chain
*at release time*. This formation is the missing **nightly working-tree** lane:
it scans what is checked out now, on a schedule, independent of whether code
changed.

## The chain

```
nightly-supply-chain sentinel  →  SupplyChainScanStarted
        →  ScanSupplyChain        →  SupplyChainScanned
        →  RemediateSupplyChain    →  SupplyChainRemediated
        →  WriteSupplyChainDigest  →  SupplyChainScanCompleted
```

- **`ScanSupplyChain`** iterates every active registry project, runs the
  stack's audit tool (`cargo audit`, `npm audit`, `mix deps.audit`,
  `osv-scanner` on Swift's `Package.resolved`) against the working-tree
  lockfile, classifies each advisory against that repo's committed
  allowlist, and emits `SupplyChainScanned`. Each finding carries a **fix
  version** when the audit tool reports one and, where npm supplies it, the
  direct **fix package** whose upgrade removes a vulnerable transitive package.
  Python is the exception to "global tool": `pip-audit` is a project
  dependency, so it is run from the project's own `.venv/bin/pip-audit` — a
  repo that hasn't installed it reports cleanly under "Not scanned" rather than
  relying on a global PATH. Kotlin is the other exception: Foundry runs the
  project's own `./gradlew dependencyCheckAggregate` and reads the JSON report
  it writes, so the project's suppression file decides what counts.
  Dependency-Check names no fix version, so every Kotlin finding is a policy
  call.
- **`RemediateSupplyChain`** triages every live finding by fix availability and
  emits `SupplyChainRemediated`, carrying the scan through. A *populated* fix
  version means the advisory is mechanically **auto-fixable**; an *empty* one
  means a **policy call** — an exploitability judgement about our usage that
  stays human. When the auto-fix engine is enabled (see below) it also *applies*
  the fixable ones, behind a verify-and-rollback rail; otherwise it only
  classifies.
- **`WriteSupplyChainDigest`** renders a *deterministic* markdown digest (no
  agent — CVE identifiers must never be paraphrased or hallucinated) and writes
  it atomically to `{FOUNDRY_SUPPLY_CHAIN_DIR}/{YYYY-MM-DD}.md`. Dry-run skips
  the write.

The schedule is `0 6 * * *` (06:00 local), offset past the 02:00 maintenance
run. The sentinel ships **enabled** in the canonical seed; disable it with
`foundry sentinel disable nightly-supply-chain`.

## The allowlist — committed per-repo memory

A gate is stateless: it re-fails the same advisory forever. A function
remembers a decision. Each repo may carry a committed
`.supply-chain-allow.json` at its root — a neutral artifact Foundry *reads* (it
never writes it; acceptances are authored by a human and land through the repo's
normal commit flow, so every decision lives in git history):

```json
{
  "version": 1,
  "allowed": [
    {
      "cve": "GHSA-gv7w-rqvm-qjhr",
      "reason": "transitive dev-only dependency; not reachable in our runtime",
      "expires": "2026-09-01"
    }
  ]
}
```

Each entry classifies one advisory on the day of the scan:

| State | Condition | Effect |
|-------|-----------|--------|
| **live** | not in the allowlist | reported as a finding |
| **accepted** | present, `expires` today-or-later (or absent) | suppressed; noted under "Accepted" |
| **lapsed** | present, `expires` has passed | **resurfaces as a live finding** and is flagged under "Lapsed acceptances — re-decide" |

An entry matches a finding when it names the finding's ID or any alias the
scanner reports for the same advisory. pip-audit, osv-scanner and cargo-audit
report aliases, so one entry for `CVE-2026-45829` also accepts the pip-audit
finding `PYSEC-2026-311`. An active acceptance under any alias wins over a
lapsed one. The post-push auditor and the `scan_requested` scan read the same
file with the same semantics.

The expiry is deliberate: an acceptance is a decision to revisit, not a
permanent mute. A malformed `expires` string fails safe — the advisory
resurfaces rather than hiding.

## The digest

The digest opens with a **triage line** — `N auto-fixable · M policy-call` —
splitting the live findings by fix availability. It then groups findings into
sections: **Live findings** (a per-project CVE / package / severity / version /
**fix** table, where the fix column shows the resolving version or `policy
call`), **Lapsed acceptances** (now live, need a fresh decision), **Accepted**
(active allowlist entries, for transparency), and **Not scanned** (projects
whose audit tool was unavailable or had no lockfile — reported, never failed). A
clean scan reads "No live supply-chain advisories."

## The auto-fix engine (gated dark)

`RemediateSupplyChain` can do more than classify: it can *apply* a fixable
advisory's fix. This is **off by default** and stays inert on every install
until two conditions both hold — the env var `FOUNDRY_SUPPLY_CHAIN_REMEDIATE` is
truthy *and* the run is at `Full` throttle (never under `dry_run`). With the gate
off, the block is byte-for-byte the classifier.

When enabled, each project with fixable findings goes through a mandatory
verify-and-rollback rail, and every change is **reversible — committed locally,
never pushed**:

1. **Refuse a dirty tree, or one with no gates.** A project whose working tree
   carries uncommitted changes is skipped, so a rollback can always return to a
   known-clean `HEAD`. A repo with no `.hone-gates.json` gates is skipped too —
   an unverifiable fix is never applied.
2. **Full compatible update first.** Rather than pinning one package, the engine
   first moves *every* dependency to the newest version the manifest already
   allows. This clears the advisory the same way a routine dependency refresh
   would, and keeps the lockfile close to current instead of accumulating
   one-off pins:
   - Rust: `cargo update` refreshes `Cargo.lock`.
   - TypeScript: `npm update --package-lock-only` or `bun update
     --lockfile-only` refreshes the native lockfile (with `package.json`
     committed or restored alongside it).
   - Python: `uv lock --upgrade` refreshes `uv.lock`.
   - Swift: `swift package update` refreshes `Package.resolved` (never
     `Package.swift`).

   The engine then **re-runs the same scanner** that detected the findings to
   confirm which ones the update cleared, and re-runs the repo's gates. If at
   least one finding cleared and the required gates pass, the lockfile is
   committed as `chore: update dependency lockfile to latest compatible
   versions (fixes <CVE>)`. If the update fails, clears nothing, cannot be
   re-scanned, or fails a required gate, its files are unstaged and restored
   from `HEAD`.
3. **Targeted fallback.** Every finding the full update did not clear — or every
   finding, when the full update was reverted — falls back to a single-package
   fix that follows the project's stack and lockfile:
   - Rust: `cargo update -p <pkg> --precise <fix>` updates `Cargo.lock`.
   - TypeScript: Bun and npm projects update their native lockfile. A matching
     direct dependency or override pin is rewritten in `package.json` first;
     transitive advisories target npm's explicit `fixAvailable.name` package.
   - Python: uv projects rewrite a matching `pyproject.toml` requirement and
     run `uv lock --upgrade-package <pkg>==<fix>`.
   - Swift: no targeted pin. osv-scanner names packages by repository URL and
     SwiftPM pins by package identity, so Foundry does not guess the mapping;
     the finding is reported as `apply_failed`.
   - Kotlin, Elixir, C++: no fixer (`no_fixer`).

   The gates are re-run; on a pass only the files that fixer touched are
   committed (`chore(deps): bump … (supply-chain auto-fix)`), otherwise they are
   restored from `HEAD`. Unsupported stacks or projects without a supported
   lockfile report a visible `apply_failed`/`no_fixer` outcome rather than
   guessing.

Each applied fix commits immediately, so a later finding's rollback can never
clobber an earlier success. Every outcome's `detail` starts with the path taken
— `full_update` or `targeted_pin` — and a targeted outcome names why the full
update was not enough (for example, `targeted_pin (after gate verification
failed after the full update): …`).

The digest gains a **Remediation** section — *Auto-fixed*, *Reverted*, and *Not
auto-fixed (needs attention)* — only when the engine actually ran. Enable it by
adding the env var to the daemon's launch environment; disable by removing it.

## Release-tag audit scan errors

The `ReleaseTagAudited` event carries an optional `scan_error` field
(`Option<String>` in the SDK; absent from the JSON wire format when `None`).

**Invariant:** a scan that could not run is reported as *unknown*, never as a
clean result. When `scan_error` is set:

- `vulnerable` reflects the upstream payload value (the last known state),
  **not** a fresh clean reading.
- The `cve` field is likewise forwarded from upstream unchanged.
- Downstream blocks that branch on `vulnerable: false` should check for a
  non-`null` `scan_error` before treating a result as authoritative.

`scan_error` is populated in three situations:

| Cause | Example message |
|-------|-----------------|
| `git checkout <tag>` returns non-zero | `"git checkout v1.2.3 failed: ..."` |
| Scanner tool returned a tool-level error | `"cargo audit not found"` |
| Scanner gateway itself returned `Err` | `"I/O error spawning audit tool"` |

## Environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `FOUNDRY_SUPPLY_CHAIN_DIR` | `~/.foundry/supply-chain` | Digest output directory |
| `FOUNDRY_SUPPLY_CHAIN_REMEDIATE` | _(unset → off)_ | Set truthy (`1`/`true`/`yes`/`on`) to enable the auto-fix engine. Off by default. |
