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
    /// Groups related events into a single workflow instance.
    /// Propagated automatically by the engine from trigger to emitted events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
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
            trace_id: None,
            payload,
        }
    }

    /// Attach a trace ID to this event (builder pattern).
    #[must_use]
    pub fn with_trace_id(mut self, trace_id: Option<String>) -> Self {
        self.trace_id = trace_id;
        self
    }

    pub fn payload_str(&self, key: &str) -> Option<&str> {
        self.payload.get(key).and_then(serde_json::Value::as_str)
    }

    pub fn payload_str_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.payload_str(key).unwrap_or(default)
    }

    pub fn payload_bool(&self, key: &str) -> Option<bool> {
        self.payload.get(key).and_then(serde_json::Value::as_bool)
    }

    pub fn payload_bool_or(&self, key: &str, default: bool) -> bool {
        self.payload_bool(key).unwrap_or(default)
    }

    pub fn payload_u64(&self, key: &str) -> Option<u64> {
        self.payload.get(key).and_then(serde_json::Value::as_u64)
    }

    pub fn payload_u64_or(&self, key: &str, default: u64) -> u64 {
        self.payload_u64(key).unwrap_or(default)
    }

    pub fn payload_i64(&self, key: &str) -> Option<i64> {
        self.payload.get(key).and_then(serde_json::Value::as_i64)
    }

    pub fn payload_i64_or(&self, key: &str, default: i64) -> i64 {
        self.payload_i64(key).unwrap_or(default)
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

/// Generate a fresh trace ID for a new workflow instance.
pub fn mint_trace_id() -> String {
    format!("trc_{}", uuid::Uuid::new_v4().simple())
}

/// Extension methods for extracting typed values from a `serde_json::Value` payload object.
///
/// Provides `str_or`, `bool_or`, `u64_or`, and `i64_or` to replace the repetitive
/// `.get(key).and_then(Value::as_T).unwrap_or(default)` pattern used across task blocks.
pub trait PayloadExt {
    fn str_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str;
    fn bool_or(&self, key: &str, default: bool) -> bool;
    fn u64_or(&self, key: &str, default: u64) -> u64;
    fn i64_or(&self, key: &str, default: i64) -> i64;
}

impl PayloadExt for serde_json::Value {
    fn str_or<'a>(&'a self, key: &str, default: &'a str) -> &'a str {
        self.get(key).and_then(serde_json::Value::as_str).unwrap_or(default)
    }

    fn bool_or(&self, key: &str, default: bool) -> bool {
        self.get(key).and_then(serde_json::Value::as_bool).unwrap_or(default)
    }

    fn u64_or(&self, key: &str, default: u64) -> u64 {
        self.get(key).and_then(serde_json::Value::as_u64).unwrap_or(default)
    }

    fn i64_or(&self, key: &str, default: i64) -> i64 {
        self.get(key).and_then(serde_json::Value::as_i64).unwrap_or(default)
    }
}

/// Known event types in the system.
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    Serialize,
    Deserialize,
    strum::Display,
    strum::EnumString,
    strum::IntoStaticStr,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum EventType {
    // Vulnerability remediation workflow
    ScanRequested,
    VulnerabilityDetected,
    MainBranchAudited,
    RemediationStarted,
    RemediationCompleted,
    ReleaseRequested,
    ReleaseCompleted,
    ReleasePipelineCompleted,
    LocalInstallCompleted,

    // Project lifecycle (used across workflows)
    ProjectValidationCompleted,
    ProjectIterationCompleted,
    ProjectMaintenanceCompleted,
    ProjectChangesCommitted,
    ProjectChangesPushed,

    // Maintenance sub-workflow triggers
    IterationRequested,
    MaintenanceRequested,
    PromptExecutionRequested,

    // Validation workflow
    ValidationRequested,
    ValidationCompleted,

    // Run lifecycle
    MaintenanceRunStarted,
    MaintenanceRunCompleted,

    // Release audit
    ReleaseTagAudited,

    // Native gate orchestration workflow
    GateResolutionCompleted,
    PreflightCompleted,
    ExecutionCompleted,
    GateVerificationCompleted,
    RetryRequested,
    SummarizeCompleted,

    // Native iterate workflow (Phase 3)
    CharterCheckCompleted,
    AssessmentCompleted,
    TriageCompleted,
    PlanCompleted,

    // Strategic loop workflow (nested iteration)
    StrategicAssessmentCompleted,
    StrategicCycleCompleted,
    InnerIterationCompleted,

    // Drift scout workflow
    DriftAssessmentRequested,
    DriftAssessmentCompleted,

    // Pipeline health workflow
    PipelineCheckRequested,
    PipelineChecked,

    // Hello-world workflow (validates engine mechanics)
    GreetRequested,
    GreetingComposed,
    GreetingDelivered,
}

impl EventType {
    pub fn as_str(&self) -> &'static str {
        self.into()
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
            (EventType::ReleaseRequested, "release_requested"),
            (EventType::ReleaseCompleted, "release_completed"),
            (EventType::ReleasePipelineCompleted, "release_pipeline_completed"),
            (EventType::LocalInstallCompleted, "local_install_completed"),
            (EventType::ProjectValidationCompleted, "project_validation_completed"),
            (EventType::ProjectIterationCompleted, "project_iteration_completed"),
            (EventType::ProjectMaintenanceCompleted, "project_maintenance_completed"),
            (EventType::ProjectChangesCommitted, "project_changes_committed"),
            (EventType::ProjectChangesPushed, "project_changes_pushed"),
            (EventType::IterationRequested, "iteration_requested"),
            (EventType::MaintenanceRequested, "maintenance_requested"),
            (EventType::PromptExecutionRequested, "prompt_execution_requested"),
            (EventType::ValidationRequested, "validation_requested"),
            (EventType::ValidationCompleted, "validation_completed"),
            (EventType::MaintenanceRunStarted, "maintenance_run_started"),
            (EventType::MaintenanceRunCompleted, "maintenance_run_completed"),
            (EventType::ReleaseTagAudited, "release_tag_audited"),
            (EventType::GateResolutionCompleted, "gate_resolution_completed"),
            (EventType::PreflightCompleted, "preflight_completed"),
            (EventType::ExecutionCompleted, "execution_completed"),
            (EventType::GateVerificationCompleted, "gate_verification_completed"),
            (EventType::RetryRequested, "retry_requested"),
            (EventType::SummarizeCompleted, "summarize_completed"),
            (EventType::CharterCheckCompleted, "charter_check_completed"),
            (EventType::AssessmentCompleted, "assessment_completed"),
            (EventType::TriageCompleted, "triage_completed"),
            (EventType::PlanCompleted, "plan_completed"),
            (EventType::StrategicAssessmentCompleted, "strategic_assessment_completed"),
            (EventType::StrategicCycleCompleted, "strategic_cycle_completed"),
            (EventType::InnerIterationCompleted, "inner_iteration_completed"),
            (EventType::DriftAssessmentRequested, "drift_assessment_requested"),
            (EventType::DriftAssessmentCompleted, "drift_assessment_completed"),
            (EventType::PipelineCheckRequested, "pipeline_check_requested"),
            (EventType::PipelineChecked, "pipeline_checked"),
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
    fn all_variants_round_trip_through_from_str() {
        let cases = [
            (EventType::ScanRequested, "scan_requested"),
            (EventType::VulnerabilityDetected, "vulnerability_detected"),
            (EventType::MainBranchAudited, "main_branch_audited"),
            (EventType::RemediationStarted, "remediation_started"),
            (EventType::RemediationCompleted, "remediation_completed"),
            (EventType::ReleaseRequested, "release_requested"),
            (EventType::ReleaseCompleted, "release_completed"),
            (EventType::ReleasePipelineCompleted, "release_pipeline_completed"),
            (EventType::LocalInstallCompleted, "local_install_completed"),
            (EventType::ProjectValidationCompleted, "project_validation_completed"),
            (EventType::ProjectIterationCompleted, "project_iteration_completed"),
            (EventType::ProjectMaintenanceCompleted, "project_maintenance_completed"),
            (EventType::ProjectChangesCommitted, "project_changes_committed"),
            (EventType::ProjectChangesPushed, "project_changes_pushed"),
            (EventType::IterationRequested, "iteration_requested"),
            (EventType::MaintenanceRequested, "maintenance_requested"),
            (EventType::PromptExecutionRequested, "prompt_execution_requested"),
            (EventType::ValidationRequested, "validation_requested"),
            (EventType::ValidationCompleted, "validation_completed"),
            (EventType::MaintenanceRunStarted, "maintenance_run_started"),
            (EventType::MaintenanceRunCompleted, "maintenance_run_completed"),
            (EventType::ReleaseTagAudited, "release_tag_audited"),
            (EventType::GateResolutionCompleted, "gate_resolution_completed"),
            (EventType::PreflightCompleted, "preflight_completed"),
            (EventType::ExecutionCompleted, "execution_completed"),
            (EventType::GateVerificationCompleted, "gate_verification_completed"),
            (EventType::RetryRequested, "retry_requested"),
            (EventType::SummarizeCompleted, "summarize_completed"),
            (EventType::CharterCheckCompleted, "charter_check_completed"),
            (EventType::AssessmentCompleted, "assessment_completed"),
            (EventType::TriageCompleted, "triage_completed"),
            (EventType::PlanCompleted, "plan_completed"),
            (EventType::StrategicAssessmentCompleted, "strategic_assessment_completed"),
            (EventType::StrategicCycleCompleted, "strategic_cycle_completed"),
            (EventType::InnerIterationCompleted, "inner_iteration_completed"),
            (EventType::DriftAssessmentRequested, "drift_assessment_requested"),
            (EventType::DriftAssessmentCompleted, "drift_assessment_completed"),
            (EventType::PipelineCheckRequested, "pipeline_check_requested"),
            (EventType::PipelineChecked, "pipeline_checked"),
            (EventType::GreetRequested, "greet_requested"),
            (EventType::GreetingComposed, "greeting_composed"),
            (EventType::GreetingDelivered, "greeting_delivered"),
        ];

        for (variant, expected_str) in &cases {
            assert_eq!(variant.as_str(), *expected_str, "as_str for {variant:?}");
            assert_eq!(&format!("{variant}"), *expected_str, "Display for {variant:?}");
            let parsed: EventType =
                expected_str.parse().unwrap_or_else(|_| panic!("should parse {expected_str}"));
            assert_eq!(&parsed, variant, "FromStr for {expected_str}");
        }
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
    fn trace_id_omitted_from_json_when_none() {
        let event = Event::new(
            EventType::GreetRequested,
            "test".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let json = serde_json::to_value(&event).unwrap();
        assert!(json.get("trace_id").is_none(), "trace_id should be absent when None");
    }

    #[test]
    fn trace_id_present_in_json_when_set() {
        let event = Event::new(
            EventType::GreetRequested,
            "test".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some("trc_abc123".to_string()));
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["trace_id"], "trc_abc123");
    }

    #[test]
    fn trace_id_deserialized_as_none_when_absent() {
        let json = r#"{
            "id": "evt_test",
            "event_type": "greet_requested",
            "project": "test",
            "occurred_at": "2026-01-01T00:00:00Z",
            "recorded_at": "2026-01-01T00:00:00Z",
            "throttle": "full",
            "payload": {}
        }"#;
        let event: Event = serde_json::from_str(json).unwrap();
        assert!(event.trace_id.is_none());
    }

    #[test]
    fn trace_id_round_trip() {
        let event = Event::new(
            EventType::GreetRequested,
            "test".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some("trc_deadbeef".to_string()));
        let json = serde_json::to_string(&event).unwrap();
        let restored: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.trace_id, Some("trc_deadbeef".to_string()));
    }

    #[test]
    fn mint_trace_id_produces_trc_prefix() {
        let id = super::mint_trace_id();
        assert!(id.starts_with("trc_"), "trace ID must start with trc_");
        assert!(id.len() > 10, "trace ID must have sufficient entropy");
    }

    #[test]
    fn mint_trace_id_unique() {
        let id1 = super::mint_trace_id();
        let id2 = super::mint_trace_id();
        assert_ne!(id1, id2);
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
