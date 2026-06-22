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
