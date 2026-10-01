//! Durable evidence reported by the work reconciler, not an authoritative store.
use serde::{Deserialize, Serialize};

/// One exact inventory identity and its classification.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReconcileFinding {
    pub project: String,
    /// `orphan_worktree`, `orphan_branch`, `broken_item`, unresolved, informational or `dirty_checkout`.
    pub category: String,
    /// Exact item id, ref name or filesystem path; never inferred from item ids.
    pub identity: String,
    pub detail: String,
}

/// Completion shared by the CLI RPC and the ops observation boundary.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkReconcileCompletedPayload {
    pub success: bool,
    pub digest_path: Option<String>,
    /// This invocation's report, even if a concurrent invocation replaces the daily file.
    pub markdown: String,
    pub settled_ids: Vec<String>,
    pub orphan_worktrees: usize,
    pub orphan_branches: usize,
    pub broken_items: usize,
    pub unresolved: usize,
    pub findings: Vec<ReconcileFinding>,
    pub errors: Vec<String>,
}

impl WorkReconcileCompletedPayload {
    /// Findings that require operational attention, regardless of MBOS volume.
    pub fn has_anomaly(&self) -> bool {
        self.orphan_worktrees > 0
            || self.orphan_branches > 0
            || self.broken_items > 0
            || self.unresolved > 0
            || !self.errors.is_empty()
    }
}
