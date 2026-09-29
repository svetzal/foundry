use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Maintenance run lifecycle — cycle (system-level) and per-project pair
// ---------------------------------------------------------------------------

/// Payload for `MaintenanceCycleStarted` (cycle-root, emitted by the scheduler / `foundry run`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceCycleStartedPayload {
    pub project_count: u64,
}

/// Payload for `ProjectRunStarted` (per-project, emitted by `FanOutMaintenance`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[allow(clippy::empty_structs_with_brackets)]
pub struct ProjectRunStartedPayload {
    // currently empty — the project name lives on the Event itself.
}

/// Payload for `MaintenanceSummaryRequested`.
///
/// Emitted by `finalise_system_maintenance` once a maintenance cycle's
/// per-project sub-traces are persisted to disk. Carries the locations of
/// those traces so `GenerateSummary` can read them and render the report.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MaintenanceSummaryRequestedPayload {
    /// Map of project name → on-disk trace event ID.
    #[serde(default)]
    pub project_trace_ids: std::collections::HashMap<String, String>,
    #[serde(default)]
    pub skipped_projects: Vec<String>,
    #[serde(default)]
    pub total_duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_event_id: Option<String>,
    /// Set when this summary is for a cycle foundryd was stopped in the
    /// middle of, and closed on its next start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<InterruptedCycle>,
}

/// A maintenance cycle that never finished because foundryd stopped during
/// it (killed, crashed, host restarted). Recorded on the next start so the
/// cycle still gets a summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptedCycle {
    /// When the cycle started.
    pub started_at: chrono::DateTime<chrono::Utc>,
    /// The last event foundryd recorded for the cycle before it stopped.
    pub last_event_at: chrono::DateTime<chrono::Utc>,
    /// Projects whose run had not completed.
    pub unfinished: Vec<String>,
}

impl InterruptedCycle {
    /// The status line for a project that did not finish.
    pub fn unfinished_reason(&self) -> String {
        format!(
            "interrupted: foundryd stopped (last event {}) before this run finished",
            self.last_event_at.format("%Y-%m-%d %H:%M UTC")
        )
    }
}

/// Payload for `ProjectRunCompleted`.
///
/// Within a scattered maintenance cycle this is emitted by task blocks
/// (`CompleteProjectRun` for runs that did work, `RouteProjectWorkflow` for
/// runs that reached no work) as the uniform per-project terminal. For a
/// standalone single-project run it is synthesized by the service layer.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectRunCompletedPayload {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_event_id: Option<String>,
}
