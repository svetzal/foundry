# Supply-Chain Scan

The supply-chain formation is a nightly, working-tree dependency-advisory scan
across every managed project. It is **detection-only and advisory**: it never
mutates a working tree and never fails a project run.

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
  stack's audit tool (`cargo audit`, `npm audit`, `pip-audit`, `mix
  deps.audit`) against the working-tree lockfile, classifies each advisory
  against that repo's committed allowlist, and emits `SupplyChainScanned`.
  Each finding carries a **fix version** when the audit tool reports one.
- **`RemediateSupplyChain`** triages every live finding by fix availability and
  emits `SupplyChainRemediated`, carrying the scan through. A *populated* fix
  version means the advisory is mechanically **auto-fixable**; an *empty* one
  means a **policy call** — an exploitability judgement about our usage that
  stays human. (The actual auto-fix engine — in-range bump and override-pin
  manifest rewrite with gate-verify-and-rollback — ships dark behind an env gate
  in a later increment; today this block only classifies and never mutates.)
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

## Environment variables

| Variable | Default | Purpose |
|----------|---------|---------|
| `FOUNDRY_SUPPLY_CHAIN_DIR` | `~/.foundry/supply-chain` | Digest output directory |
