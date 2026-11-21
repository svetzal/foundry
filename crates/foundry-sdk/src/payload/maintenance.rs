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
    pub started_at: crate::time::UtcDateTime,
    /// The last event foundryd recorded for the cycle before it stopped.
    pub last_event_at: crate::time::UtcDateTime,
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

/// Payload for `MaintenanceSummaryWritten`, the fact that `GenerateSummary`
/// wrote a nightly run's summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct MaintenanceSummaryWrittenPayload {
    /// Where the summary was written.
    pub summary_path: String,
    /// Set when most of the run's projects failed with one agent cause.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet_failure: Option<FleetFailure>,
}

/// Projects in one maintenance run that failed with the same agent cause,
/// when they are most of the run: one fault on the host, not many project
/// faults.
///
/// On 2026-11-16 an expired Claude login failed all 31 projects' nightly
/// maintenance identically for five nights, and the summary listed 31
/// separate failures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FleetFailure {
    /// Projects that ran maintenance (skipped projects excluded).
    pub attempted: u64,
    /// Projects that failed with `cause`, in name order.
    pub projects: Vec<String>,
    /// The shared cause, as the agent sessions' summaries state it.
    pub cause: String,
}

impl FleetFailure {
    /// The smallest run in which a shared cause counts as fleet-wide.
    pub const MIN_PROJECTS: usize = 3;
    /// The share of attempted projects, in percent, that must share the cause.
    pub const MIN_SHARE_PERCENT: usize = 80;

    /// The fleet-wide failure among `attempted` projects, given the agent
    /// cause of each project that failed with one, or `None` when no cause is
    /// shared by at least [`Self::MIN_SHARE_PERCENT`] of them (and by at least
    /// [`Self::MIN_PROJECTS`]).
    #[must_use]
    pub fn detect<'a>(
        attempted: usize,
        causes: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Option<Self> {
        let mut by_cause: std::collections::BTreeMap<&str, Vec<&str>> =
            std::collections::BTreeMap::new();
        for (project, cause) in causes {
            by_cause.entry(cause).or_default().push(project);
        }
        let (cause, mut projects) =
            by_cause.into_iter().max_by_key(|(_, projects)| projects.len())?;
        let shared = projects.len();
        if attempted < Self::MIN_PROJECTS
            || shared < Self::MIN_PROJECTS
            || shared * 100 < attempted * Self::MIN_SHARE_PERCENT
        {
            return None;
        }
        projects.sort_unstable();
        Some(Self {
            attempted: attempted as u64,
            projects: projects.into_iter().map(str::to_string).collect(),
            cause: cause.to_string(),
        })
    }

    /// One line naming the share and the cause.
    #[must_use]
    pub fn headline(&self) -> String {
        format!(
            "{} of {} projects failed with the same cause: {}",
            self.projects.len(),
            self.attempted,
            self.cause
        )
    }
}

#[cfg(test)]
mod fleet_failure_tests {
    use super::FleetFailure;

    const EXPIRED: &str =
        "agent authentication failed: Failed to authenticate: OAuth session expired";

    #[test]
    fn every_project_failing_alike_is_one_fleet_failure() {
        let names: Vec<String> = (0..31).map(|n| format!("p{n:02}")).collect();
        let fleet = FleetFailure::detect(31, names.iter().map(|n| (n.as_str(), EXPIRED))).unwrap();
        assert_eq!(fleet.projects.len(), 31);
        assert_eq!(fleet.cause, EXPIRED);
        assert_eq!(
            fleet.headline(),
            format!("31 of 31 projects failed with the same cause: {EXPIRED}")
        );
    }

    #[test]
    fn eighty_percent_is_the_threshold() {
        let four = [
            ("a", EXPIRED),
            ("b", EXPIRED),
            ("c", EXPIRED),
            ("d", EXPIRED),
        ];
        assert!(FleetFailure::detect(5, four).is_some(), "4 of 5 is 80%");
        assert!(FleetFailure::detect(6, four).is_none(), "4 of 6 is under 80%");
    }

    #[test]
    fn small_runs_and_mixed_causes_are_not_fleet_failures() {
        assert!(FleetFailure::detect(2, [("a", EXPIRED), ("b", EXPIRED)]).is_none());
        let mixed = [("a", EXPIRED), ("b", "tests failed"), ("c", "lint failed")];
        assert!(FleetFailure::detect(3, mixed).is_none());
        assert!(FleetFailure::detect(10, []).is_none());
    }
}
