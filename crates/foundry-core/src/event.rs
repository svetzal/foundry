use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::throttle::Throttle;

/// A Foundry event — an immutable fact that something happened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Deterministic ID derived from content (excluding `recorded_at`).
    pub id: String,
    /// The event type name (e.g., `vulnerability_detected`).
    pub event_type: EventType,
    /// The project this event relates to.
    pub project: String,
    /// When the event occurred.
    pub occurred_at: DateTime<Utc>,
    /// When the event was recorded in the log.
    pub recorded_at: DateTime<Utc>,
    /// The throttle level propagated through this event chain.
    pub throttle: Throttle,
    /// Event-type-specific payload.
    pub payload: serde_json::Value,
}

impl Event {
    /// Create a new event with a deterministic ID.
    pub fn new(
        event_type: EventType,
        project: String,
        throttle: Throttle,
        payload: serde_json::Value,
    ) -> Self {
        let occurred_at = Utc::now();
        let recorded_at = occurred_at;
        let id = Self::compute_id(&event_type, &project, &occurred_at, &payload);

        Self {
            id,
            event_type,
            project,
            occurred_at,
            recorded_at,
            throttle,
            payload,
        }
    }

    fn compute_id(
        event_type: &EventType,
        project: &str,
        occurred_at: &DateTime<Utc>,
        payload: &serde_json::Value,
    ) -> String {
        let mut hasher = Sha256::new();
        hasher.update(event_type.as_str().as_bytes());
        hasher.update(project.as_bytes());
        hasher.update(occurred_at.to_rfc3339().as_bytes());
        hasher.update(payload.to_string().as_bytes());
        let hash = hasher.finalize();
        format!("evt_{}", hex::encode(&hash[..12]))
    }
}

/// Known event types in the system.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    // Vulnerability remediation workflow
    ScanRequested,
    VulnerabilityDetected,
    MainBranchAudited,
    RemediationStarted,
    RemediationCompleted,
    AutoReleaseTriggered,
    AutoReleaseCompleted,
    ReleasePipelineCompleted,
    LocalInstallCompleted,

    // Project lifecycle (used across workflows)
    ProjectValidationCompleted,
    ProjectIterateCompleted,
    ProjectMaintainCompleted,
    ProjectChangesCommitted,
    ProjectChangesPushed,

    // Run lifecycle
    MaintenanceRunStarted,
    MaintenanceRunCompleted,

    // Release audit
    ReleaseTagAudited,

    // Hello-world workflow (validates engine mechanics)
    GreetRequested,
    GreetingComposed,
    GreetingDelivered,
}

impl EventType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ScanRequested => "scan_requested",
            Self::VulnerabilityDetected => "vulnerability_detected",
            Self::MainBranchAudited => "main_branch_audited",
            Self::RemediationStarted => "remediation_started",
            Self::RemediationCompleted => "remediation_completed",
            Self::AutoReleaseTriggered => "auto_release_triggered",
            Self::AutoReleaseCompleted => "auto_release_completed",
            Self::ReleasePipelineCompleted => "release_pipeline_completed",
            Self::LocalInstallCompleted => "local_install_completed",
            Self::ProjectValidationCompleted => "project_validation_completed",
            Self::ProjectIterateCompleted => "project_iterate_completed",
            Self::ProjectMaintainCompleted => "project_maintain_completed",
            Self::ProjectChangesCommitted => "project_changes_committed",
            Self::ProjectChangesPushed => "project_changes_pushed",
            Self::MaintenanceRunStarted => "maintenance_run_started",
            Self::MaintenanceRunCompleted => "maintenance_run_completed",
            Self::ReleaseTagAudited => "release_tag_audited",
            Self::GreetRequested => "greet_requested",
            Self::GreetingComposed => "greeting_composed",
            Self::GreetingDelivered => "greeting_delivered",
        }
    }
}

impl std::str::FromStr for EventType {
    type Err = EventTypeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "scan_requested" => Ok(Self::ScanRequested),
            "vulnerability_detected" => Ok(Self::VulnerabilityDetected),
            "main_branch_audited" => Ok(Self::MainBranchAudited),
            "remediation_started" => Ok(Self::RemediationStarted),
            "remediation_completed" => Ok(Self::RemediationCompleted),
            "auto_release_triggered" => Ok(Self::AutoReleaseTriggered),
            "auto_release_completed" => Ok(Self::AutoReleaseCompleted),
            "release_pipeline_completed" => Ok(Self::ReleasePipelineCompleted),
            "local_install_completed" => Ok(Self::LocalInstallCompleted),
            "project_validation_completed" => Ok(Self::ProjectValidationCompleted),
            "project_iterate_completed" => Ok(Self::ProjectIterateCompleted),
            "project_maintain_completed" => Ok(Self::ProjectMaintainCompleted),
            "project_changes_committed" => Ok(Self::ProjectChangesCommitted),
            "project_changes_pushed" => Ok(Self::ProjectChangesPushed),
            "maintenance_run_started" => Ok(Self::MaintenanceRunStarted),
            "maintenance_run_completed" => Ok(Self::MaintenanceRunCompleted),
            "release_tag_audited" => Ok(Self::ReleaseTagAudited),
            "greet_requested" => Ok(Self::GreetRequested),
            "greeting_composed" => Ok(Self::GreetingComposed),
            "greeting_delivered" => Ok(Self::GreetingDelivered),
            _ => Err(EventTypeParseError(s.to_string())),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("unknown event type: {0}")]
pub struct EventTypeParseError(String);

impl std::fmt::Display for EventType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_id_is_deterministic() {
        let payload = serde_json::json!({"severity": "high"});
        let event_type = EventType::VulnerabilityDetected;
        let project = "hone-cli".to_string();
        let now = Utc::now();

        let id1 = Event::compute_id(&event_type, &project, &now, &payload);
        let id2 = Event::compute_id(&event_type, &project, &now, &payload);

        assert_eq!(id1, id2);
        assert!(id1.starts_with("evt_"));
    }

    #[test]
    fn different_payloads_produce_different_ids() {
        let event_type = EventType::VulnerabilityDetected;
        let project = "hone-cli".to_string();
        let now = Utc::now();

        let id1 = Event::compute_id(
            &event_type,
            &project,
            &now,
            &serde_json::json!({"severity": "high"}),
        );
        let id2 =
            Event::compute_id(&event_type, &project, &now, &serde_json::json!({"severity": "low"}));

        assert_ne!(id1, id2);
    }
}
