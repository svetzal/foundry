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

    // Maintenance sub-workflow triggers
    IterationRequested,
    MaintenanceRequested,

    // Validation workflow
    ValidationRequested,
    ValidationCompleted,

    // Run lifecycle
    MaintenanceRunStarted,
    MaintenanceRunCompleted,

    // Release audit
    ReleaseTagAudited,

    // Native gate orchestration workflow
    GatesResolved,
    PreflightCompleted,
    ExecutionCompleted,
    GateVerificationCompleted,
    RetryRequested,

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
            Self::IterationRequested => "iteration_requested",
            Self::MaintenanceRequested => "maintenance_requested",
            Self::ValidationRequested => "validation_requested",
            Self::ValidationCompleted => "validation_completed",
            Self::MaintenanceRunStarted => "maintenance_run_started",
            Self::MaintenanceRunCompleted => "maintenance_run_completed",
            Self::ReleaseTagAudited => "release_tag_audited",
            Self::GatesResolved => "gates_resolved",
            Self::PreflightCompleted => "preflight_completed",
            Self::ExecutionCompleted => "execution_completed",
            Self::GateVerificationCompleted => "gate_verification_completed",
            Self::RetryRequested => "retry_requested",
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
            "iteration_requested" => Ok(Self::IterationRequested),
            "maintenance_requested" => Ok(Self::MaintenanceRequested),
            "validation_requested" => Ok(Self::ValidationRequested),
            "validation_completed" => Ok(Self::ValidationCompleted),
            "maintenance_run_started" => Ok(Self::MaintenanceRunStarted),
            "maintenance_run_completed" => Ok(Self::MaintenanceRunCompleted),
            "release_tag_audited" => Ok(Self::ReleaseTagAudited),
            "gates_resolved" => Ok(Self::GatesResolved),
            "preflight_completed" => Ok(Self::PreflightCompleted),
            "execution_completed" => Ok(Self::ExecutionCompleted),
            "gate_verification_completed" => Ok(Self::GateVerificationCompleted),
            "retry_requested" => Ok(Self::RetryRequested),
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
    fn event_serializes_to_evt_cli_compatible_schema() {
        let event = Event::new(
            EventType::VulnerabilityDetected,
            "project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let json = serde_json::to_value(&event).unwrap();

        // evt-cli required fields must be present
        assert!(json.get("id").is_some(), "id field must be present");
        assert!(
            json.get("event_type").is_some(),
            "event_type field must be present (not 'type')"
        );
        assert!(json.get("project").is_some(), "project field must be present");
        assert!(json.get("occurred_at").is_some(), "occurred_at field must be present");
        assert!(json.get("recorded_at").is_some(), "recorded_at field must be present");
        assert!(json.get("payload").is_some(), "payload field must be present");

        // event_type must serialize as snake_case string
        assert_eq!(json["event_type"], "vulnerability_detected");

        // timestamps must be RFC3339 parseable strings
        let occurred_at = json["occurred_at"].as_str().expect("occurred_at must be a string");
        chrono::DateTime::parse_from_rfc3339(occurred_at)
            .expect("occurred_at must be RFC3339 formatted");

        let recorded_at = json["recorded_at"].as_str().expect("recorded_at must be a string");
        chrono::DateTime::parse_from_rfc3339(recorded_at)
            .expect("recorded_at must be RFC3339 formatted");
    }

    #[test]
    fn all_event_type_variants_serialize_as_snake_case() {
        let cases = [
            (EventType::ScanRequested, "scan_requested"),
            (EventType::VulnerabilityDetected, "vulnerability_detected"),
            (EventType::MainBranchAudited, "main_branch_audited"),
            (EventType::RemediationStarted, "remediation_started"),
            (EventType::RemediationCompleted, "remediation_completed"),
            (EventType::AutoReleaseTriggered, "auto_release_triggered"),
            (EventType::AutoReleaseCompleted, "auto_release_completed"),
            (EventType::ReleasePipelineCompleted, "release_pipeline_completed"),
            (EventType::LocalInstallCompleted, "local_install_completed"),
            (EventType::ProjectValidationCompleted, "project_validation_completed"),
            (EventType::ProjectIterateCompleted, "project_iterate_completed"),
            (EventType::ProjectMaintainCompleted, "project_maintain_completed"),
            (EventType::ProjectChangesCommitted, "project_changes_committed"),
            (EventType::ProjectChangesPushed, "project_changes_pushed"),
            (EventType::IterationRequested, "iteration_requested"),
            (EventType::MaintenanceRequested, "maintenance_requested"),
            (EventType::ValidationRequested, "validation_requested"),
            (EventType::ValidationCompleted, "validation_completed"),
            (EventType::MaintenanceRunStarted, "maintenance_run_started"),
            (EventType::MaintenanceRunCompleted, "maintenance_run_completed"),
            (EventType::ReleaseTagAudited, "release_tag_audited"),
            (EventType::GatesResolved, "gates_resolved"),
            (EventType::PreflightCompleted, "preflight_completed"),
            (EventType::ExecutionCompleted, "execution_completed"),
            (EventType::GateVerificationCompleted, "gate_verification_completed"),
            (EventType::RetryRequested, "retry_requested"),
            (EventType::GreetRequested, "greet_requested"),
            (EventType::GreetingComposed, "greeting_composed"),
            (EventType::GreetingDelivered, "greeting_delivered"),
        ];

        for (variant, expected) in &cases {
            let serialized = serde_json::to_value(variant).unwrap();
            assert_eq!(
                serialized,
                serde_json::Value::String((*expected).to_string()),
                "EventType::{variant:?} should serialize as {expected:?}",
            );
        }
    }

    #[test]
    fn iteration_requested_as_str() {
        assert_eq!(EventType::IterationRequested.as_str(), "iteration_requested");
    }

    #[test]
    fn maintenance_requested_as_str() {
        assert_eq!(EventType::MaintenanceRequested.as_str(), "maintenance_requested");
    }

    #[test]
    fn iteration_requested_from_str() {
        let parsed: EventType = "iteration_requested".parse().expect("should parse");
        assert_eq!(parsed, EventType::IterationRequested);
    }

    #[test]
    fn maintenance_requested_from_str() {
        let parsed: EventType = "maintenance_requested".parse().expect("should parse");
        assert_eq!(parsed, EventType::MaintenanceRequested);
    }

    #[test]
    fn iteration_requested_serde_round_trip() {
        let original = EventType::IterationRequested;
        let json = serde_json::to_string(&original).expect("should serialize");
        let restored: EventType = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(restored, original);
    }

    #[test]
    fn maintenance_requested_serde_round_trip() {
        let original = EventType::MaintenanceRequested;
        let json = serde_json::to_string(&original).expect("should serialize");
        let restored: EventType = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(restored, original);
    }

    #[test]
    fn validation_requested_serde_round_trip() {
        let original = EventType::ValidationRequested;
        let json = serde_json::to_string(&original).expect("should serialize");
        let restored: EventType = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(restored, original);
    }

    #[test]
    fn validation_completed_serde_round_trip() {
        let original = EventType::ValidationCompleted;
        let json = serde_json::to_string(&original).expect("should serialize");
        let restored: EventType = serde_json::from_str(&json).expect("should deserialize");
        assert_eq!(restored, original);
    }

    #[test]
    fn validation_requested_from_str() {
        let parsed: EventType = "validation_requested".parse().expect("should parse");
        assert_eq!(parsed, EventType::ValidationRequested);
    }

    #[test]
    fn validation_completed_from_str() {
        let parsed: EventType = "validation_completed".parse().expect("should parse");
        assert_eq!(parsed, EventType::ValidationCompleted);
    }

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
