use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Commit-digest workflow — daily proactive summary of registered projects
// ---------------------------------------------------------------------------

/// Payload for `CommitDigestStarted` (cycle-root, emitted by the sentinel).
///
/// Mirrors `MaintenanceCycleStartedPayload` for symmetry — the project count
/// is filled in by `ObserveCommits` once the active registry is known. The
/// sentinel itself emits an empty payload (`{}`), and the count defaults to
/// zero on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommitDigestStartedPayload {
    #[serde(default)]
    pub project_count: u64,
}

/// A single commit row inside a `CommitsObserved` payload.
///
/// Captures only the fields the downstream summariser actually needs. We
/// deliberately omit the patch body — the digest is a high-level scan,
/// not a code review.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CommitInfo {
    /// Full SHA-1 hash. Display callers truncate to 7 chars themselves.
    pub sha: String,
    /// Commit author display name (`%an` in `git log`).
    pub author: String,
    /// Author timestamp in RFC 3339 (`%aI` in `git log`).
    pub timestamp: String,
    /// Commit subject — the first line of the message (`%s` in `git log`).
    pub subject: String,
}

/// One project's slice of a `CommitsObserved` payload. Carries an `error`
/// when the `git log` invocation failed, so downstream blocks can surface
/// the failure inline without aborting the chain.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectCommits {
    pub name: String,
    pub branch: String,
    #[serde(default)]
    pub commits: Vec<CommitInfo>,
    /// When `Some`, the `git log` call for this project failed; `commits` is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Payload for `CommitsObserved` — the raw evidence the summariser will turn
/// into prose. Always emitted, even on empty days.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommitsObservedPayload {
    /// Width of the wall-clock window the observer used (hours).
    #[serde(default)]
    pub window_hours: u32,
    /// One entry per active registry project.
    #[serde(default)]
    pub projects: Vec<ProjectCommits>,
}

impl CommitsObservedPayload {
    /// Sum of `commits.len()` across all projects (errored projects
    /// contribute zero).
    pub fn total_commits(&self) -> u64 {
        self.projects.iter().map(|p| p.commits.len() as u64).sum()
    }

    /// Count of projects in the payload — successful or errored.
    pub fn project_count(&self) -> u64 {
        self.projects.len() as u64
    }
}

/// Payload for `CommitSummaryCompleted` — the agent's rendered digest body
/// plus the bookkeeping totals needed for the final write step's header.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommitSummaryCompletedPayload {
    pub markdown: String,
    #[serde(default)]
    pub project_count: u64,
    #[serde(default)]
    pub total_commits: u64,
}

/// Payload for `CommitDigestCompleted` — the chain's terminal event.
///
/// `digest_path` is `None` on a dry-run firing (chain ran, file was not
/// written) and on any persistence failure (`success: false`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CommitDigestCompletedPayload {
    pub success: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest_path: Option<String>,
    #[serde(default)]
    pub project_count: u64,
    #[serde(default)]
    pub total_commits: u64,
}
