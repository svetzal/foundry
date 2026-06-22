use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Release workflow
// ---------------------------------------------------------------------------

/// Payload for `MainBranchAudited`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MainBranchAuditedPayload {
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub cve: String,
    #[serde(default)]
    pub vulnerable: bool,
    #[serde(default)]
    pub dirty: bool,
}

/// Payload for `ReleaseTagAudited`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseTagAuditedPayload {
    #[serde(default)]
    pub project: String,
    pub cve: String,
    #[serde(default)]
    pub tag: String,
    pub vulnerable: bool,
    /// Fallback dirty flag forwarded from upstream payloads when the scanner
    /// cannot run (project not in registry, no lockfile, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
}

/// Payload for `ReleaseRequested`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseRequestedPayload {
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub cve: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Optional version bump type (`patch`, `minor`, `major`). When absent
    /// the release agent determines the bump from the changelog.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bump: Option<String>,
}

/// Payload for `ReleaseCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseCompletedPayload {
    #[serde(default)]
    pub cve: String,
    #[serde(default)]
    pub release: String,
    #[serde(default)]
    pub new_tag: Option<String>,
    pub success: bool,
}

/// Payload for `ReleasePipelineCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleasePipelineCompletedPayload {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conclusion: Option<String>,
}

/// Payload for `LocalInstallCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LocalInstallCompletedPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
    /// Set to `"skipped"` when no install was performed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Human-readable explanation when `status` is `"skipped"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
}

/// Payload for `LocalSkillInstallCompleted`.
///
/// Emitted after `LocalInstallCompleted` when the project registry has an
/// `installs_skill` entry. Failure is soft: a failed skill install does not
/// fail the parent block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalSkillInstallCompletedPayload {
    pub project: String,
    pub command: String,
    pub success: bool,
    /// Last few lines of stdout, for display in traces.
    pub stdout_tail: String,
    /// Last few lines of stderr, for display in traces.
    pub stderr_tail: String,
}
