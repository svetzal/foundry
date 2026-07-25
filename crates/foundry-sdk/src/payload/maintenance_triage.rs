use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Post-maintenance failure triage formation — propose-only
// ---------------------------------------------------------------------------

/// Payload for `MaintenanceTriageCompleted` — the triage formation's terminal event.
///
/// Carries the classified verdicts, correlated infra incidents, and summary
/// counts so downstream consumers (dashboards, digests) can act on the
/// triage results without re-processing the raw failure data.
///
/// All fields default to zero/empty for backward compatibility with consumers
/// that may encounter events from before this payload was defined.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MaintenanceTriageCompletedPayload {
    #[serde(default)]
    pub success: bool,
    /// `true` when the triage skipped writing the digest (dry-run or no failures).
    #[serde(default)]
    pub skipped: bool,
    /// Path to the written triage digest file, when written successfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest_path: Option<String>,
    /// Per-gate failure verdicts after classification and de-duplication.
    #[serde(default)]
    pub verdicts: Vec<crate::triage::FailureVerdict>,
    /// Correlated infra incidents (N≥3 projects, same signature → one incident).
    #[serde(default)]
    pub infra_incidents: Vec<crate::triage::InfraIncident>,
    /// Total gate failures before suppression.
    #[serde(default)]
    pub total_failures: u64,
    /// Count of verdicts / incidents whose decision is `SuppressInfra`.
    #[serde(default)]
    pub suppressed_count: u64,
    /// Count of verdicts whose decision is `AutoFixable`.
    #[serde(default)]
    pub auto_fixable_count: u64,
    /// Count of verdicts whose decision is `PolicyCall`.
    #[serde(default)]
    pub policy_count: u64,
    /// Count of verdicts whose decision is `NeedsInvestigation`.
    #[serde(default)]
    pub investigation_count: u64,
    /// Count of verdicts whose decision is `Escalate`.
    #[serde(default)]
    pub escalation_count: u64,
    /// Count of `PreflightCompleted` events encountered in the run window or
    /// streak lookback window whose payload could not be parsed and were
    /// therefore skipped rather than classified.
    #[serde(default)]
    pub unparsed_events: u64,
}
