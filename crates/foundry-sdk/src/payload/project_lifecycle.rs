use serde::{Deserialize, Serialize};

use crate::gateway::AgentFailureMetadata;

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
    #[serde(default, flatten)]
    pub failure: AgentFailureMetadata,
}

/// Typed reason a project's checkout could not be kept in step with its remote.
///
/// Recorded on `ProjectValidationCompleted` (the pre-work sync that opens every
/// per-project maintenance run) and on `ProjectChangesCommitted` (the pre-push
/// sync in `CommitAndPush`). Serialized as a `snake_case` string so consumers
/// can match on it without parsing the human-readable `reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitSyncFailure {
    /// `git status --porcelain` was non-empty before any work began; nothing was touched.
    DirtyTree,
    /// The local branch and `origin/<branch>` have both moved, so a
    /// fast-forward is impossible; nothing was touched.
    Diverged,
    /// `git fetch origin <branch>` failed, or `origin/<branch>` could not be
    /// resolved, so the checkout's position relative to the remote is unknown.
    RemoteUnavailable,
    /// The remote moved during the run and rebasing the local commit onto it
    /// did not apply cleanly. The rebase was aborted and the commit was left on
    /// the local branch for a human; nothing was pushed.
    PushRejectedDiverged,
    /// The local branch was rebased onto a remote that moved during the run,
    /// and the project's required gates failed on the rebased commits (or the
    /// project has no gates to verify them with). Nothing was pushed; the
    /// rebased commits stay on the local branch.
    GatesFailedAfterRebase,
    /// `git push` itself was rejected (for example a non-fast-forward race or
    /// an authentication failure). Nothing was pushed. Never retried with force.
    PushFailed,
    /// The run reported failure, so commits ahead of the remote were kept on
    /// the local branch instead of being pushed.
    RunFailed,
}

impl GitSyncFailure {
    /// The stable wire form (`"dirty_tree"`, `"diverged"`, …).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DirtyTree => "dirty_tree",
            Self::Diverged => "diverged",
            Self::RemoteUnavailable => "remote_unavailable",
            Self::PushRejectedDiverged => "push_rejected_diverged",
            Self::GatesFailedAfterRebase => "gates_failed_after_rebase",
            Self::PushFailed => "push_failed",
            Self::RunFailed => "run_failed",
        }
    }
}

impl std::fmt::Display for GitSyncFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Payload for `ProjectChangesCommitted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectChangesCommittedPayload {
    pub project: String,
    pub cve: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// Set when the commit landed locally but could not be pushed (see
    /// [`GitSyncFailure`] for the reasons).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_failure: Option<GitSyncFailure>,
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
    /// Typed reason when `status` is `"error"` because the checkout could not
    /// be synced with its remote before work began.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_failure: Option<GitSyncFailure>,
    /// Commits fast-forwarded from `origin/<branch>` before work began. Under
    /// `dry_run` this is the count that *would* be fast-forwarded, measured
    /// against the last-fetched remote-tracking ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast_forwarded: Option<u32>,
    /// `true` when the checkout sync was simulated (no fetch, no merge).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::GitSyncFailure;

    #[test]
    fn git_sync_failure_wire_form_matches_as_str() {
        for failure in [
            GitSyncFailure::DirtyTree,
            GitSyncFailure::Diverged,
            GitSyncFailure::RemoteUnavailable,
            GitSyncFailure::PushRejectedDiverged,
            GitSyncFailure::GatesFailedAfterRebase,
            GitSyncFailure::PushFailed,
            GitSyncFailure::RunFailed,
        ] {
            let json = serde_json::to_string(&failure).unwrap();
            assert_eq!(json, format!("\"{}\"", failure.as_str()));
        }
    }
}
