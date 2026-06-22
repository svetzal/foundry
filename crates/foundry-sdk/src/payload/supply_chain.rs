use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Supply-chain scan formation — nightly working-tree dependency advisory scan
// ---------------------------------------------------------------------------

/// One live advisory finding for a single project's dependency tree.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SupplyChainFinding {
    /// Advisory identifier (CVE / GHSA / RUSTSEC).
    pub cve: String,
    /// The vulnerable package name as reported by the scanner.
    pub package: String,
    /// Severity label (`"critical"`, `"high"`, `"medium"`, `"low"`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    /// Installed version of the vulnerable package, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Earliest version that resolves the advisory, when the scanner reports
    /// one. `Some` → mechanically fixable (the remediation block can act);
    /// `None` → no fix available, a human policy call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_version: Option<String>,
}

/// An advisory that the repo's `.supply-chain-allow.json` spoke to — either an
/// active acceptance (suppressed) or a lapsed one (resurfaced, needs a fresh
/// decision; the advisory also appears in `findings`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SuppressedFinding {
    /// Advisory identifier the allowlist entry addresses.
    pub cve: String,
    /// The acceptance rationale recorded in the allowlist.
    pub reason: String,
    /// `"allowlisted"` (active acceptance) or `"expired"` (acceptance lapsed).
    pub status: String,
    /// The expiry date that has passed — present only when `status == "expired"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expired_on: Option<String>,
}

/// Per-project supply-chain scan outcome.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectSupplyChainScan {
    /// Registry project name.
    pub project: String,
    /// Technology stack (drives which audit tool ran).
    pub stack: String,
    /// Live advisories (not actively allowlisted). Includes resurfaced ones
    /// whose acceptance lapsed.
    #[serde(default)]
    pub findings: Vec<SupplyChainFinding>,
    /// Advisories the allowlist spoke to (active acceptances and lapses).
    #[serde(default)]
    pub suppressed: Vec<SuppressedFinding>,
    /// Tool-level error (scanner not installed, no lockfile, parse failure).
    /// A scan error is reported, not failed — the project run stays green.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scan_error: Option<String>,
}

/// Payload for `SupplyChainScanned` — the formation's mid-chain evidence event,
/// carrying every project's classified findings for the digest writer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SupplyChainScannedPayload {
    /// Per-project scan outcomes.
    #[serde(default)]
    pub projects: Vec<ProjectSupplyChainScan>,
    /// Number of projects scanned.
    #[serde(default)]
    pub project_count: u64,
    /// Total live findings across all projects (excludes active allowlisted).
    #[serde(default)]
    pub finding_count: u64,
    /// Number of projects with at least one live finding.
    #[serde(default)]
    pub affected_project_count: u64,
}

/// The outcome of attempting to remediate one live finding.
///
/// `status` is an open string so new mechanisms can extend it without a wire
/// break: `"applied"` (fix verified by gates and committed), `"rolled_back"`
/// (applied but gates failed, reverted), `"apply_failed"` (the fix command
/// itself failed — e.g. the fixed version is out of the manifest's range, the
/// override-rewrite case), `"no_fixer"` (fixable but no mechanism for this stack
/// yet), or `"skipped"` (project-level: not in registry, dirty tree, no gates to
/// verify against — see `detail`).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct RemediationOutcome {
    /// Registry project the finding belongs to.
    pub project: String,
    /// Advisory identifier.
    pub cve: String,
    /// Affected package.
    pub package: String,
    /// The version the fix targeted, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_version: Option<String>,
    /// Outcome status (see type docs).
    pub status: String,
    /// Human-readable detail (failure reason, skip reason).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Payload for `SupplyChainRemediated` — the formation's mid-chain remediation
/// event, sitting between the scan and the digest.
///
/// In the current (non-mutating) increment the remediation block is a *triage
/// classifier*: it carries every project's scan through verbatim and adds a
/// fixable-vs-policy-call split derived from each finding's `fix_version`.
/// `remediated_count` is the number of findings actually auto-fixed and is `0`
/// until the mutating half lands behind its env gate.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SupplyChainRemediatedPayload {
    /// Per-project scan outcomes, carried through from `SupplyChainScanned` so
    /// the digest can render its findings/lapsed/accepted/not-scanned sections.
    #[serde(default)]
    pub projects: Vec<ProjectSupplyChainScan>,
    /// Number of projects scanned.
    #[serde(default)]
    pub project_count: u64,
    /// Total live findings across all projects.
    #[serde(default)]
    pub finding_count: u64,
    /// Number of projects with at least one live finding.
    #[serde(default)]
    pub affected_project_count: u64,
    /// Live findings carrying a fix version (mechanically auto-fixable).
    #[serde(default)]
    pub fixable_count: u64,
    /// Live findings with no fix version (a human policy call).
    #[serde(default)]
    pub no_fix_count: u64,
    /// Findings actually auto-fixed (verified + committed) this run. `0` when
    /// remediation is gated off (the default) or nothing was fixable.
    #[serde(default)]
    pub remediated_count: u64,
    /// Per-finding remediation outcomes. Empty when remediation is gated off
    /// (the classifier-only path) — the digest then renders no remediation
    /// section, matching the pre-remediation behaviour.
    #[serde(default)]
    pub outcomes: Vec<RemediationOutcome>,
}

/// Payload for `SupplyChainScanCompleted` — the formation's terminal event.
///
/// `digest_path` is `None` on a dry-run firing (chain ran, file not written)
/// and on any persistence failure (`success: false`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SupplyChainScanCompletedPayload {
    pub success: bool,
    #[serde(default)]
    pub skipped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest_path: Option<String>,
    #[serde(default)]
    pub project_count: u64,
    #[serde(default)]
    pub finding_count: u64,
}
