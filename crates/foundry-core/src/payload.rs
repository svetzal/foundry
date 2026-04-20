//! Typed payload structs for all Foundry event types.
//!
//! Each event has a corresponding `*Payload` struct that serializes to exactly
//! the same JSON shape as the `serde_json::json!()` macros it replaces. The
//! wire format is byte-for-byte identical.
//!
//! # Usage
//!
//! Constructing an event payload:
//! ```rust,ignore
//! let payload = GreetingComposedPayload { greeting: "Hello, world!".to_string() };
//! let event = trigger.with_payload(EventType::GreetingComposed, &payload)?;
//! ```
//!
//! Reading a typed payload from an incoming trigger:
//! ```rust,ignore
//! let p: GreetRequestedPayload = trigger.parse_payload()?;
//! let name = p.name.as_deref().unwrap_or("world");
//! ```

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Chain context — propagated through the iterate / maintenance chain
// ---------------------------------------------------------------------------

/// Optional context fields that propagate through the iterate chain.
///
/// Every block that builds an outgoing payload must forward these fields
/// unchanged so downstream blocks can see them. Use `#[serde(flatten)]`
/// when embedding in a payload struct so these fields appear at the top level.
///
/// The fields mirror those copied by `forward_chain_context`:
/// `actions`, `prompt`, `gates`, `audit_name`, and `loop_context`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChainContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gates: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loop_context: Option<serde_json::Value>,
}

impl ChainContext {
    /// Extract chain context fields from a JSON payload object.
    pub fn extract_from(payload: &serde_json::Value) -> Self {
        Self {
            actions: payload.get("actions").cloned(),
            prompt: payload.get("prompt").cloned(),
            gates: payload.get("gates").cloned(),
            audit_name: payload
                .get("audit_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            loop_context: payload.get("loop_context").cloned(),
        }
    }

    /// Merge chain context fields into a mutable JSON payload object.
    ///
    /// Only fields that are `Some` are written; existing fields are overwritten.
    pub fn merge_into(&self, target: &mut serde_json::Value) {
        if let Some(v) = &self.actions {
            target["actions"] = v.clone();
        }
        if let Some(v) = &self.prompt {
            target["prompt"] = v.clone();
        }
        if let Some(v) = &self.gates {
            target["gates"] = v.clone();
        }
        if let Some(v) = &self.audit_name {
            target["audit_name"] = serde_json::json!(v);
        }
        if let Some(v) = &self.loop_context {
            target["loop_context"] = v.clone();
        }
    }
}

/// Subset of `ChainContext` carrying only `loop_context` and `actions`.
///
/// Used by blocks that call `forward_loop_context` (not the full chain context):
/// `execute_plan`, `run_verify_gates`, `retry_execution`, `direct_prompt`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LoopContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loop_context: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<serde_json::Value>,
}

impl LoopContext {
    /// Extract loop context fields from a JSON payload object.
    pub fn extract_from(payload: &serde_json::Value) -> Self {
        Self {
            loop_context: payload.get("loop_context").cloned(),
            actions: payload.get("actions").cloned(),
        }
    }
}

// ---------------------------------------------------------------------------
// Gate result — shared across preflight and verification payloads
// ---------------------------------------------------------------------------

/// A single gate's execution result, nested in `results` arrays.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResultEntry {
    pub name: String,
    pub passed: bool,
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

// ---------------------------------------------------------------------------
// Greet workflow
// ---------------------------------------------------------------------------

/// Payload for `GreetRequested`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GreetRequestedPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Payload for `GreetingComposed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GreetingComposedPayload {
    pub greeting: String,
}

/// Payload for `GreetingDelivered`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GreetingDeliveredPayload {
    pub delivered: bool,
    pub greeting: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
}

// ---------------------------------------------------------------------------
// Vulnerability scan / remediation workflow
// ---------------------------------------------------------------------------

/// Payload for `VulnerabilityDetected`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VulnerabilityDetectedPayload {
    pub cve: String,
    pub vulnerable: bool,
    pub dirty: bool,
    pub package: String,
    pub severity: String,
}

/// Payload for `RemediationStarted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationStartedPayload {
    pub project: String,
    pub cve: String,
}

/// Payload for `RemediationCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationCompletedPayload {
    /// CVE identifier. Present for vulnerability remediations; absent for pipeline remediations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cve: Option<String>,
    pub success: bool,
    /// Human-readable summary. Omitted when empty (e.g. dry-run pipeline path).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    /// Set to `true` when this is a pipeline remediation (not CVE).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pipeline_fix: Option<bool>,
}

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
    pub project: String,
    pub cve: String,
    pub tag: String,
    pub vulnerable: bool,
}

/// Payload for `ReleaseRequested`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseRequestedPayload {
    pub project: String,
    pub cve: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

/// Payload for `ReleaseCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseCompletedPayload {
    pub cve: String,
    pub release: String,
    pub new_tag: String,
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

// ---------------------------------------------------------------------------
// Gate orchestration workflow
// ---------------------------------------------------------------------------

/// Payload for `GateResolutionCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResolutionCompletedPayload {
    pub project: String,
    pub workflow: String,
    pub gates: serde_json::Value,
    #[serde(flatten)]
    pub chain: ChainContext,
}

/// Payload for `PreflightCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightCompletedPayload {
    pub project: String,
    pub workflow: String,
    pub all_passed: bool,
    pub required_passed: bool,
    pub results: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<bool>,
    #[serde(flatten)]
    pub chain: ChainContext,
}

/// Payload for `ExecutionCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionCompletedPayload {
    pub project: String,
    pub workflow: String,
    pub success: bool,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_count: Option<u64>,
    #[serde(flatten)]
    pub context: LoopContext,
}

/// Payload for `GateVerificationCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateVerificationCompletedPayload {
    pub project: String,
    pub workflow: String,
    pub all_passed: bool,
    pub required_passed: bool,
    pub results: Vec<serde_json::Value>,
    pub retry_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_output: Option<String>,
    #[serde(flatten)]
    pub context: LoopContext,
}

/// Payload for `RetryRequested`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryRequestedPayload {
    pub project: String,
    pub workflow: String,
    pub retry_count: u64,
    pub failure_context: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prior_execution_output: Option<String>,
    #[serde(flatten)]
    pub context: LoopContext,
}

/// Payload for `SummarizeCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummarizeCompletedPayload {
    pub project: String,
    pub headline: String,
    pub summary: String,
}

// ---------------------------------------------------------------------------
// Project lifecycle
// ---------------------------------------------------------------------------

/// Payload for `ProjectIterationCompleted` and `ProjectMaintenanceCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectCompletedPayload {
    pub project: String,
    pub success: bool,
    pub summary: String,
    #[serde(default)]
    pub workflow: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loop_context: Option<serde_json::Value>,
}

/// Payload for `ProjectChangesCommitted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectChangesCommittedPayload {
    pub project: String,
    pub cve: String,
    pub message: String,
}

/// Payload for `ProjectChangesPushed`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectChangesPushedPayload {
    pub project: String,
    pub cve: String,
    pub message: String,
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
}

// ---------------------------------------------------------------------------
// Iterate workflow — charter check, assess, triage, plan
// ---------------------------------------------------------------------------

/// Payload for `IterationRequested`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct IterationRequestedPayload {
    pub project: String,
    pub workflow: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategic: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategic_prompt: Option<String>,
    #[serde(flatten)]
    pub chain: ChainContext,
}

/// Payload for `MaintenanceRequested`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MaintenanceRequestedPayload {
    pub project: String,
    pub workflow: String,
    #[serde(flatten)]
    pub chain: ChainContext,
}

/// Payload for `CharterCheckCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CharterCheckCompletedPayload {
    pub project: String,
    pub success: bool,
    pub sources: Vec<serde_json::Value>,
    pub guidance: String,
    pub workflow: String,
    #[serde(flatten)]
    pub chain: ChainContext,
}

/// Payload for `AssessmentCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssessmentCompletedPayload {
    pub project: String,
    pub severity: u64,
    pub principle: String,
    pub category: String,
    pub assessment: String,
    pub workflow: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit_name: Option<String>,
    #[serde(flatten)]
    pub chain: ChainContext,
}

/// Payload for `TriageCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageCompletedPayload {
    pub project: String,
    pub accepted: bool,
    pub reason: String,
    pub severity: u64,
    pub principle: String,
    pub category: String,
    pub assessment: String,
    pub workflow: String,
    #[serde(flatten)]
    pub chain: ChainContext,
}

/// Payload for `PlanCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanCompletedPayload {
    pub project: String,
    pub plan: String,
    pub principle: String,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub assessment: String,
    pub workflow: String,
    #[serde(flatten)]
    pub chain: ChainContext,
}

// ---------------------------------------------------------------------------
// Maintenance run lifecycle
// ---------------------------------------------------------------------------

/// Payload for `MaintenanceRunStarted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceRunStartedPayload {
    pub project_count: u64,
}

/// Payload for `MaintenanceRunCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MaintenanceRunCompletedPayload {
    pub project_count: u64,
    pub success_count: u64,
    pub failure_count: u64,
    pub duration_ms: u64,
}

// ---------------------------------------------------------------------------
// Validation workflow
// ---------------------------------------------------------------------------

/// Payload for `ValidationRequested`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationRequestedPayload {
    pub project: String,
    #[serde(flatten)]
    pub chain: ChainContext,
}

/// Payload for `ValidationCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ValidationCompletedPayload {
    pub project: String,
    pub success: bool,
    pub workflow: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Strategic loop workflow
// ---------------------------------------------------------------------------

/// Payload for `StrategicAssessmentCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategicAssessmentCompletedPayload {
    pub project: String,
    pub areas: Vec<serde_json::Value>,
    pub loop_context: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actions: Option<serde_json::Value>,
}

/// Payload for `InnerIterationCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InnerIterationCompletedPayload {
    pub project: String,
    pub success: bool,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub workflow: String,
    pub loop_context: serde_json::Value,
}

/// Payload for `StrategicCycleCompleted` (terminal event from strategic loop).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategicCycleCompletedPayload {
    pub project: String,
    pub success: bool,
    pub summary: String,
    pub workflow: String,
    pub iterations_completed: u64,
}

// ---------------------------------------------------------------------------
// Drift scout workflow
// ---------------------------------------------------------------------------

/// Payload for `DriftAssessmentRequested`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DriftAssessmentRequestedPayload {
    pub project: String,
}

/// Payload for `DriftAssessmentCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriftAssessmentCompletedPayload {
    pub project: String,
    pub candidate_count: u64,
    pub high_value_count: u64,
    pub candidates: Vec<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Pipeline health workflow
// ---------------------------------------------------------------------------

/// Payload for `PipelineCheckRequested`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PipelineCheckRequestedPayload {
    pub project: String,
}

/// Payload for `PipelineChecked`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineCheckedPayload {
    pub passing: bool,
    pub conclusion: String,
    pub run_id: u64,
    pub run_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_logs: Option<String>,
}

// ---------------------------------------------------------------------------
// Prompt execution workflow
// ---------------------------------------------------------------------------

/// Payload for `PromptExecutionRequested`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptExecutionRequestedPayload {
    pub project: String,
    pub prompt: String,
    #[serde(flatten)]
    pub chain: ChainContext,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chain_context_extract_and_merge_roundtrip() {
        let source = serde_json::json!({
            "actions": {"maintain": true},
            "prompt": "do the thing",
            "gates": [{"name": "fmt"}],
            "audit_name": "fix-audit",
            "loop_context": {"strategic": {"iteration": 2}},
            "unrelated": "noise",
        });

        let chain = ChainContext::extract_from(&source);
        assert!(chain.actions.is_some());
        assert!(chain.prompt.is_some());
        assert!(chain.gates.is_some());
        assert_eq!(chain.audit_name.as_deref(), Some("fix-audit"));
        assert!(chain.loop_context.is_some());

        let mut target = serde_json::json!({ "project": "test" });
        chain.merge_into(&mut target);

        assert_eq!(target["actions"]["maintain"], true);
        assert_eq!(target["prompt"], "do the thing");
        assert_eq!(target["gates"][0]["name"], "fmt");
        assert_eq!(target["audit_name"], "fix-audit");
        assert_eq!(target["loop_context"]["strategic"]["iteration"], 2);
        assert!(target.get("unrelated").is_none());
    }

    #[test]
    fn chain_context_default_serializes_no_fields() {
        let chain = ChainContext::default();
        let json = serde_json::to_value(&chain).unwrap();
        // All fields are None, so they should all be absent
        assert!(json.as_object().unwrap().is_empty());
    }

    #[test]
    fn greeting_composed_payload_round_trips() {
        let p = GreetingComposedPayload {
            greeting: "Hello, world!".to_string(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["greeting"], "Hello, world!");
        let p2: GreetingComposedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p2.greeting, "Hello, world!");
    }

    #[test]
    fn greeting_delivered_payload_omits_dry_run_when_none() {
        let p = GreetingDeliveredPayload {
            delivered: true,
            greeting: "Hello!".to_string(),
            dry_run: None,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert!(json.get("dry_run").is_none());
        assert_eq!(json["delivered"], true);
    }

    #[test]
    fn greeting_delivered_payload_includes_dry_run_when_set() {
        let p = GreetingDeliveredPayload {
            delivered: true,
            greeting: "Hello!".to_string(),
            dry_run: Some(true),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["dry_run"], true);
    }

    #[test]
    fn loop_context_extract_only_copies_loop_context_and_actions() {
        let source = serde_json::json!({
            "loop_context": {"strategic": {"iteration": 1}},
            "actions": {"maintain": true},
            "prompt": "ignored",
            "gates": "ignored",
        });
        let lc = LoopContext::extract_from(&source);
        assert!(lc.loop_context.is_some());
        assert!(lc.actions.is_some());

        let json = serde_json::to_value(&lc).unwrap();
        assert!(json.get("prompt").is_none());
        assert!(json.get("gates").is_none());
    }

    #[test]
    fn preflight_completed_payload_flattens_chain() {
        let chain = ChainContext {
            actions: Some(serde_json::json!({"maintain": true})),
            ..ChainContext::default()
        };
        let p = PreflightCompletedPayload {
            project: "test".to_string(),
            workflow: "iterate".to_string(),
            all_passed: true,
            required_passed: true,
            results: vec![],
            skipped: None,
            chain,
        };
        let json = serde_json::to_value(&p).unwrap();
        // Flattened: actions should appear at top level
        assert_eq!(json["actions"]["maintain"], true);
        assert!(json.get("chain").is_none(), "chain should not appear as a key");
    }

    #[test]
    fn execution_completed_payload_flattens_loop_context() {
        let context = LoopContext {
            loop_context: Some(serde_json::json!({"strategic": {"iteration": 1}})),
            actions: Some(serde_json::json!({"maintain": true})),
        };
        let p = ExecutionCompletedPayload {
            project: "test".to_string(),
            workflow: "iterate".to_string(),
            success: true,
            summary: "done".to_string(),
            execution_output: None,
            dry_run: None,
            retry_count: None,
            context,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["loop_context"]["strategic"]["iteration"], 1);
        assert_eq!(json["actions"]["maintain"], true);
        assert!(json.get("context").is_none(), "context should not appear as a key");
    }

    #[test]
    fn vulnerability_detected_payload_round_trips() {
        let p = VulnerabilityDetectedPayload {
            cve: "CVE-2024-1234".to_string(),
            vulnerable: true,
            dirty: false,
            package: "openssl".to_string(),
            severity: "high".to_string(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["cve"], "CVE-2024-1234");
        assert_eq!(json["vulnerable"], true);
        assert_eq!(json["dirty"], false);
        assert_eq!(json["package"], "openssl");
        assert_eq!(json["severity"], "high");
        let p2: VulnerabilityDetectedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p2.cve, "CVE-2024-1234");
        assert_eq!(p2.severity, "high");
    }

    #[test]
    fn main_branch_audited_payload_round_trips() {
        let p = MainBranchAuditedPayload {
            project: "my-project".to_string(),
            cve: "CVE-2024-5678".to_string(),
            vulnerable: true,
            dirty: true,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["project"], "my-project");
        assert_eq!(json["cve"], "CVE-2024-5678");
        assert_eq!(json["vulnerable"], true);
        assert_eq!(json["dirty"], true);
        let p2: MainBranchAuditedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p2.project, "my-project");
        assert_eq!(p2.dirty, true);
    }

    #[test]
    fn greet_requested_payload_optional_name_round_trips() {
        let with_name = GreetRequestedPayload {
            name: Some("Alice".to_string()),
        };
        let json = serde_json::to_value(&with_name).unwrap();
        assert_eq!(json["name"], "Alice");
        let restored: GreetRequestedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(restored.name.as_deref(), Some("Alice"));

        let without_name = GreetRequestedPayload { name: None };
        let json = serde_json::to_value(&without_name).unwrap();
        assert!(json.get("name").is_none(), "name must be absent when None");
    }

    #[test]
    fn local_skill_install_completed_payload_round_trips() {
        let p = LocalSkillInstallCompletedPayload {
            project: "my-project".to_string(),
            command: "mytool init --global --force".to_string(),
            success: true,
            stdout_tail: "Skill installed.".to_string(),
            stderr_tail: String::new(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["project"], "my-project");
        assert_eq!(json["command"], "mytool init --global --force");
        assert_eq!(json["success"], true);
        assert_eq!(json["stdout_tail"], "Skill installed.");
        assert_eq!(json["stderr_tail"], "");
        let p2: LocalSkillInstallCompletedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p2.project, "my-project");
        assert_eq!(p2.command, "mytool init --global --force");
        assert!(p2.success);
    }

    #[test]
    fn local_skill_install_completed_payload_failure_round_trips() {
        let p = LocalSkillInstallCompletedPayload {
            project: "my-project".to_string(),
            command: "mytool init --global --force".to_string(),
            success: false,
            stdout_tail: String::new(),
            stderr_tail: "error: command not found".to_string(),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["success"], false);
        assert_eq!(json["stderr_tail"], "error: command not found");
        let p2: LocalSkillInstallCompletedPayload = serde_json::from_value(json).unwrap();
        assert!(!p2.success);
        assert_eq!(p2.stderr_tail, "error: command not found");
    }

    #[test]
    fn iteration_requested_payload_flattens_chain() {
        let chain = ChainContext {
            actions: Some(serde_json::json!({"maintain": true})),
            ..ChainContext::default()
        };
        let p = IterationRequestedPayload {
            project: "my-project".to_string(),
            workflow: "iterate".to_string(),
            strategic: Some(true),
            max_iterations: Some(3),
            strategic_prompt: None,
            chain,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["project"], "my-project");
        assert_eq!(json["workflow"], "iterate");
        assert_eq!(json["strategic"], true);
        assert_eq!(json["max_iterations"], 3);
        assert!(json.get("strategic_prompt").is_none());
        // Chain flattened: actions at top level
        assert_eq!(json["actions"]["maintain"], true);
        assert!(json.get("chain").is_none(), "chain must not appear as a key");
        let p2: IterationRequestedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p2.project, "my-project");
        assert_eq!(p2.strategic, Some(true));
        assert_eq!(p2.chain.actions.unwrap()["maintain"], true);
    }
}
