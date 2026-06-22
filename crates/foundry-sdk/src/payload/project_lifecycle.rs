use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Project lifecycle
// ---------------------------------------------------------------------------

/// Payload for `ProjectIterationCompleted` and `ProjectMaintenanceCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectCompletedPayload {
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub success: bool,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub workflow: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loop_context: Option<serde_json::Value>,
    /// When `false`, downstream blocks such as `CommitAndPush` skip the commit.
    /// Absent (None) is interpreted as "changes may exist".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<bool>,
}

/// Payload for `ProjectChangesCommitted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectChangesCommittedPayload {
    pub project: String,
    pub cve: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stub: Option<bool>,
}

/// Payload for `ProjectChangesPushed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectChangesPushedPayload {
    pub project: String,
    pub cve: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stub: Option<bool>,
}

/// Payload for `ProjectValidationCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectValidationCompletedPayload {
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub has_gates: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<serde_json::Value>,
    /// Human-readable explanation when `status` is `"error"` or `"skipped"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
