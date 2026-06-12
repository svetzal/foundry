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
//! let p: GreetingRequestedPayload = trigger.parse_payload()?;
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
    /// Per-request agent provider override (`"claude"` | `"opencode"` |
    /// `"codex"`). Set on the entry request and forwarded unchanged through the
    /// chain so every agent invocation in the run uses the same backend. Absent
    /// means "use the daemon's default provider".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_provider: Option<String>,
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
            agent_provider: payload
                .get("agent_provider")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
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
        if let Some(v) = &self.agent_provider {
            target["agent_provider"] = serde_json::json!(v);
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
// Greet workflow
// ---------------------------------------------------------------------------

/// Payload for `GreetingRequested`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GreetingRequestedPayload {
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
    #[serde(default)]
    pub dirty: bool,
    #[serde(default)]
    pub package: String,
    #[serde(default)]
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
    #[serde(default)]
    pub project: String,
    pub cve: String,
    #[serde(default)]
    pub tag: String,
    pub vulnerable: bool,
    /// Fallback dirty flag forwarded from upstream payloads when the scanner
    /// cannot run (project not in registry, no lockfile, etc.).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty: Option<bool>,
}

/// Payload for `ReleaseRequested`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseRequestedPayload {
    #[serde(default)]
    pub project: String,
    #[serde(default)]
    pub cve: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Optional version bump type (`patch`, `minor`, `major`). When absent
    /// the release agent determines the bump from the changelog.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bump: Option<String>,
}

/// Payload for `ReleaseCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseCompletedPayload {
    #[serde(default)]
    pub cve: String,
    #[serde(default)]
    pub release: String,
    #[serde(default)]
    pub new_tag: Option<String>,
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
    pub results: Vec<crate::gates::GateResult>,
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
    /// Whether the working tree had uncommitted changes after agent execution.
    /// `None` for events recorded before this field was added.
    /// Entries in `files_changed` are raw `git status --porcelain` paths;
    /// rename entries appear as `"old -> new"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes_detected: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_changed: Vec<String>,
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
    pub results: Vec<crate::gates::GateResult>,
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
}

/// Payload for `ProjectChangesCommitted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectChangesCommittedPayload {
    pub project: String,
    pub cve: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stub: Option<bool>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stub: Option<bool>,
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

/// Payload for `ProjectIterationRequested`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectIterationRequestedPayload {
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

/// Payload for `ProjectMaintenanceRequested`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectMaintenanceRequestedPayload {
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
    #[serde(default)]
    pub sources: Vec<serde_json::Value>,
    #[serde(default)]
    pub guidance: String,
    #[serde(default = "default_iterate_workflow")]
    pub workflow: String,
    #[serde(flatten)]
    pub chain: ChainContext,
}

fn default_iterate_workflow() -> String {
    "iterate".to_string()
}

fn default_true() -> bool {
    true
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
    /// Whether the plan agent concluded that a code correction is actually needed.
    ///
    /// `true` (default) means the agent produced a plan and believes changes must
    /// be applied.  `false` means the agent examined the codebase and determined
    /// that the assessed violation does not in fact require correction — the
    /// assessment was overstated or has already been addressed.
    ///
    /// When `false`, a subsequent clean working tree after `ExecutePlan` is treated
    /// as a legitimate no-op (success) rather than a silent flake (failure).
    #[serde(default = "default_true")]
    pub correction_needed: bool,
    /// One-sentence reason provided by the plan agent when `correction_needed` is
    /// `false`.  Empty string when `correction_needed` is `true`.
    #[serde(default)]
    pub correction_reason: String,
    #[serde(flatten)]
    pub chain: ChainContext,
}

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

fn default_strategic_max() -> u64 {
    5
}

/// Typed sub-fields of `loop_context.strategic`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategicContext {
    #[serde(default)]
    pub iteration: u64,
    #[serde(default = "default_strategic_max")]
    pub max: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_area: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_areas: Option<u64>,
}

/// Typed `loop_context` payload used by the strategic loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategicLoopContext {
    pub strategic: StrategicContext,
}

/// A single improvement area from a strategic assessment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AreaEntry {
    pub area: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// Captures any additional fields the AI assessment may include.
    #[serde(flatten)]
    pub extra: std::collections::HashMap<String, serde_json::Value>,
}

/// Payload for `StrategicAssessmentCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategicAssessmentCompletedPayload {
    pub project: String,
    pub areas: Vec<AreaEntry>,
    pub loop_context: StrategicLoopContext,
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
    pub loop_context: StrategicLoopContext,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_logs: Option<String>,
}

// ---------------------------------------------------------------------------
// Prompt execution workflow
// ---------------------------------------------------------------------------

/// Payload for `ExecutionRequested`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionRequestedPayload {
    pub project: String,
    pub prompt: String,
    #[serde(flatten)]
    pub chain: ChainContext,
}

// ---------------------------------------------------------------------------
// Agent session lifecycle payloads
// ---------------------------------------------------------------------------

/// Emitted when a Foundry-launched agent session begins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionStartedPayload {
    pub session_id: String,
    pub agent_type: String,
    pub project: String,
    pub working_dir: std::path::PathBuf,
    pub source_log_path: std::path::PathBuf,
    pub tier: String,
    pub effort: String,
    pub access: String,
    pub started_at: String,
    pub trace_id: String,
}

/// Emitted when a Foundry-launched agent session ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionEndedPayload {
    pub session_id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub ended_at: String,
    pub bytes_written: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Scatter/gather coordination payloads
// ---------------------------------------------------------------------------

/// One completed child of a gather group, as recorded for the reduce step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatheredChild {
    /// The `id` of the child's completion event.
    pub event_id: String,
    /// The completion event's type.
    pub event_type: crate::event::EventType,
    /// The completion event's project.
    pub project: String,
    /// The `success` flag read from the completion payload, if it carried
    /// one. Foundry payloads report boolean results under the `success` key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub success: Option<bool>,
    /// The completion event's full payload.
    pub payload: serde_json::Value,
}

/// Payload for the reduce event the engine synthesizes when a gather group
/// is satisfied.
///
/// A reduce block sinks on the gather's `reduce_event_type` and parses this
/// payload to decide what the mix of child outcomes means. The engine never
/// interprets child success/failure itself — it only counts arrivals.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatherCompletedPayload {
    /// The fan-out group this reduce belongs to.
    pub gather_id: String,
    /// How many children were scattered.
    pub expected: usize,
    /// How many completions had arrived when the policy was satisfied.
    pub arrived: usize,
    /// Verbatim context supplied by the scattering block via `GatherSpec`.
    pub context: serde_json::Value,
    /// The completed children, in arrival order.
    pub children: Vec<GatheredChild>,
}

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

// ---------------------------------------------------------------------------
// Ops-digest workflow — periodic summary of MBOS operational events
// ---------------------------------------------------------------------------

/// Payload for `OpsDigestStarted` (cycle-root, emitted by the sentinel).
///
/// Mirrors `CommitDigestStartedPayload` for symmetry. The sentinel emits an
/// empty payload (`{}`); the event count defaults to zero on the wire.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpsDigestStartedPayload {
    #[serde(default)]
    pub event_count: u64,
}

/// A lean per-event summary carried in the `OpsObserved` payload.
///
/// Captures only the fields the downstream summariser actually needs.
/// We deliberately avoid carrying full event bodies — the digest is a
/// high-level operational scan, not a raw event dump.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct OpsEventDigest {
    /// Unique MBOS event ID.
    pub id: String,
    /// MBOS event type string (e.g., `"ci_pipeline_failure"`).
    pub event_type: String,
    /// ISO 8601 timestamp when the event occurred (`occurredAt`).
    pub occurred_at: String,
    /// Classified domain bucket (e.g., `"clients"`, `"infrastructure"`, `"ai"`).
    pub domain: String,
    /// MBOS urgency label (`"P0"`, `"P1"`, `"P2"`). Absent on legacy events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub urgency: Option<String>,
    /// Human-readable one-line `summary` from the MBOS event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Client name when the event carries a `client` object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

/// Payload for `OpsObserved` — the pressure-gated evidence the summariser
/// will turn into an ops digest.
///
/// When `proceed` is `false` the downstream blocks self-filter and
/// `OpsDigestCompleted{skipped: true}` is emitted instead.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpsObservedPayload {
    /// `true` when the gate was satisfied (count >= threshold or anomaly present).
    #[serde(default)]
    pub proceed: bool,
    /// Number of new MBOS events since the last watermark.
    #[serde(default)]
    pub new_event_count: u64,
    /// `true` when at least one event in the window is classified as an anomaly.
    #[serde(default)]
    pub anomaly_present: bool,
    /// The watermark that would be written if the chain completes. `None` when
    /// there are no new events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_watermark: Option<String>,
    /// Lean summaries of every event in the window, for the summariser.
    #[serde(default)]
    pub events: Vec<OpsEventDigest>,
}

/// Payload for `OpsSummaryCompleted` — the agent's rendered digest body plus
/// bookkeeping totals.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpsSummaryCompletedPayload {
    pub markdown: String,
    #[serde(default)]
    pub event_count: u64,
    /// The watermark to advance once the digest is written to disk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_watermark: Option<String>,
}

/// Payload for `OpsDigestCompleted` — the formation's terminal event.
///
/// `digest_path` is `None` on a dry-run firing (chain ran, file not written),
/// on a skipped firing (`skipped: true`), and on any persistence failure
/// (`success: false`).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OpsDigestCompletedPayload {
    pub success: bool,
    /// `true` when the pressure gate was not satisfied and the chain was
    /// short-circuited without calling the agent or writing a file.
    #[serde(default)]
    pub skipped: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest_path: Option<String>,
    #[serde(default)]
    pub event_count: u64,
}

// ---------------------------------------------------------------------------
// Post-maintenance failure triage formation — propose-only
// ---------------------------------------------------------------------------

/// Payload for `MaintenanceTriageCompleted` — the triage formation's terminal event.
///
/// Carries the classified verdicts, correlated infra incidents, and summary
/// counts so downstream consumers (dashboards, digests) can act on the
/// triage results without re-processing the raw failure data.
///
/// All fields default to zero/empty for backward compatibility with consumers
/// that may encounter events from before this payload was defined.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MaintenanceTriageCompletedPayload {
    #[serde(default)]
    pub success: bool,
    /// `true` when the triage skipped writing the digest (dry-run or no failures).
    #[serde(default)]
    pub skipped: bool,
    /// Path to the written triage digest file, when written successfully.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest_path: Option<String>,
    /// Per-gate failure verdicts after classification and de-duplication.
    #[serde(default)]
    pub verdicts: Vec<crate::triage::FailureVerdict>,
    /// Correlated infra incidents (N≥3 projects, same signature → one incident).
    #[serde(default)]
    pub infra_incidents: Vec<crate::triage::InfraIncident>,
    /// Total gate failures before suppression.
    #[serde(default)]
    pub total_failures: u64,
    /// Count of verdicts / incidents whose decision is `SuppressInfra`.
    #[serde(default)]
    pub suppressed_count: u64,
    /// Count of verdicts whose decision is `AutoFixable`.
    #[serde(default)]
    pub auto_fixable_count: u64,
    /// Count of verdicts whose decision is `PolicyCall`.
    #[serde(default)]
    pub policy_count: u64,
    /// Count of verdicts whose decision is `NeedsInvestigation`.
    #[serde(default)]
    pub investigation_count: u64,
    /// Count of verdicts whose decision is `Escalate`.
    #[serde(default)]
    pub escalation_count: u64,
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
            "agent_provider": "codex",
            "unrelated": "noise",
        });

        let chain = ChainContext::extract_from(&source);
        assert!(chain.actions.is_some());
        assert!(chain.prompt.is_some());
        assert!(chain.gates.is_some());
        assert_eq!(chain.audit_name.as_deref(), Some("fix-audit"));
        assert!(chain.loop_context.is_some());
        assert_eq!(chain.agent_provider.as_deref(), Some("codex"));

        let mut target = serde_json::json!({ "project": "test" });
        chain.merge_into(&mut target);

        assert_eq!(target["actions"]["maintain"], true);
        assert_eq!(target["prompt"], "do the thing");
        assert_eq!(target["gates"][0]["name"], "fmt");
        assert_eq!(target["audit_name"], "fix-audit");
        assert_eq!(target["loop_context"]["strategic"]["iteration"], 2);
        assert_eq!(target["agent_provider"], "codex");
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
            changes_detected: None,
            files_changed: vec![],
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
        assert!(p2.dirty);
    }

    #[test]
    fn greeting_requested_payload_optional_name_round_trips() {
        let with_name = GreetingRequestedPayload {
            name: Some("Alice".to_string()),
        };
        let json = serde_json::to_value(&with_name).unwrap();
        assert_eq!(json["name"], "Alice");
        let restored: GreetingRequestedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(restored.name.as_deref(), Some("Alice"));

        let without_name = GreetingRequestedPayload { name: None };
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
    fn project_iteration_requested_payload_flattens_chain() {
        let chain = ChainContext {
            actions: Some(serde_json::json!({"maintain": true})),
            ..ChainContext::default()
        };
        let p = ProjectIterationRequestedPayload {
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
        let p2: ProjectIterationRequestedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(p2.project, "my-project");
        assert_eq!(p2.strategic, Some(true));
        assert_eq!(p2.chain.actions.unwrap()["maintain"], true);
    }

    #[test]
    fn agent_session_started_payload_serializes_to_expected_json() {
        use std::path::PathBuf;
        let payload = AgentSessionStartedPayload {
            session_id: "11111111-2222-3333-4444-555555555555".to_string(),
            agent_type: "claude-code".to_string(),
            project: "demo".to_string(),
            working_dir: PathBuf::from("/tmp/demo"),
            source_log_path: PathBuf::from("/home/u/.foundry/agent-sessions/11111111.jsonl"),
            tier: "balanced".to_string(),
            effort: "medium".to_string(),
            access: "full".to_string(),
            started_at: "2026-05-09T12:00:00Z".to_string(),
            trace_id: "trace-abc".to_string(),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["session_id"], "11111111-2222-3333-4444-555555555555");
        assert_eq!(json["agent_type"], "claude-code");
        assert_eq!(json["project"], "demo");
        assert_eq!(json["working_dir"], "/tmp/demo");
        assert_eq!(json["source_log_path"], "/home/u/.foundry/agent-sessions/11111111.jsonl");
        assert_eq!(json["tier"], "balanced");
        assert_eq!(json["effort"], "medium");
        assert_eq!(json["access"], "full");
        assert_eq!(json["started_at"], "2026-05-09T12:00:00Z");
        assert_eq!(json["trace_id"], "trace-abc");
    }

    #[test]
    fn agent_session_ended_payload_serializes_to_expected_json() {
        let payload = AgentSessionEndedPayload {
            session_id: "11111111-2222-3333-4444-555555555555".to_string(),
            status: "ok".to_string(),
            exit_code: Some(0),
            ended_at: "2026-05-09T12:01:00Z".to_string(),
            bytes_written: 1234,
            error: None,
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["session_id"], "11111111-2222-3333-4444-555555555555");
        assert_eq!(json["status"], "ok");
        assert_eq!(json["exit_code"], 0);
        assert_eq!(json["ended_at"], "2026-05-09T12:01:00Z");
        assert_eq!(json["bytes_written"], 1234);
        assert!(json.get("error").is_none(), "error should be omitted when None");
    }

    #[test]
    fn agent_session_ended_payload_includes_error_when_set() {
        let payload = AgentSessionEndedPayload {
            session_id: "id".to_string(),
            status: "unavailable".to_string(),
            exit_code: None,
            ended_at: "2026-05-09T12:01:00Z".to_string(),
            bytes_written: 0,
            error: Some("spawn failed: claude not on PATH".to_string()),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["error"], "spawn failed: claude not on PATH");
        assert!(json.get("exit_code").is_none(), "exit_code should be omitted when None");
    }

    // ---------------------------------------------------------------------
    // Commit-digest payloads
    // ---------------------------------------------------------------------

    fn sample_commit() -> CommitInfo {
        CommitInfo {
            sha: "abcdef0123456789abcdef0123456789abcdef01".to_string(),
            author: "Stacey Vetzal".to_string(),
            timestamp: "2026-05-28T16:30:00-04:00".to_string(),
            subject: "feat(slice2): add the commit-digest formation".to_string(),
        }
    }

    #[test]
    fn commit_digest_started_payload_defaults_project_count_to_zero() {
        let parsed: CommitDigestStartedPayload = serde_json::from_str("{}").unwrap();
        assert_eq!(parsed.project_count, 0);
    }

    #[test]
    fn commit_digest_started_payload_round_trips() {
        let payload = CommitDigestStartedPayload { project_count: 17 };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["project_count"], 17);
        let back: CommitDigestStartedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(back.project_count, 17);
    }

    #[test]
    fn project_commits_with_error_omits_commits_when_serialized() {
        let p = ProjectCommits {
            name: "broken".to_string(),
            branch: "main".to_string(),
            commits: vec![],
            error: Some("git log exited 128: fatal: not a git repository".to_string()),
        };
        let json = serde_json::to_value(&p).unwrap();
        assert_eq!(json["name"], "broken");
        assert_eq!(json["branch"], "main");
        assert_eq!(json["error"], "git log exited 128: fatal: not a git repository");
        assert_eq!(json["commits"], serde_json::json!([]));
    }

    #[test]
    fn project_commits_without_error_omits_error_field() {
        let p = ProjectCommits {
            name: "ok".to_string(),
            branch: "main".to_string(),
            commits: vec![sample_commit()],
            error: None,
        };
        let json = serde_json::to_value(&p).unwrap();
        assert!(json.get("error").is_none(), "error should be omitted when None");
        assert_eq!(json["commits"][0]["sha"], sample_commit().sha);
    }

    #[test]
    fn commits_observed_payload_total_and_project_count_helpers() {
        let payload = CommitsObservedPayload {
            window_hours: 24,
            projects: vec![
                ProjectCommits {
                    name: "alpha".to_string(),
                    branch: "main".to_string(),
                    commits: vec![sample_commit(), sample_commit()],
                    error: None,
                },
                ProjectCommits {
                    name: "broken".to_string(),
                    branch: "main".to_string(),
                    commits: vec![],
                    error: Some("nope".to_string()),
                },
            ],
        };
        assert_eq!(payload.project_count(), 2);
        assert_eq!(payload.total_commits(), 2, "errored project contributes zero");
    }

    #[test]
    fn commits_observed_payload_round_trips_through_json() {
        let payload = CommitsObservedPayload {
            window_hours: 24,
            projects: vec![ProjectCommits {
                name: "foundry".to_string(),
                branch: "main".to_string(),
                commits: vec![sample_commit()],
                error: None,
            }],
        };
        let json = serde_json::to_value(&payload).unwrap();
        let back: CommitsObservedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(back.window_hours, 24);
        assert_eq!(back.projects.len(), 1);
        assert_eq!(back.projects[0].commits[0], sample_commit());
    }

    #[test]
    fn commit_summary_completed_payload_round_trips() {
        let payload = CommitSummaryCompletedPayload {
            markdown: "# Commit Digest\n\nNothing today.\n".to_string(),
            project_count: 17,
            total_commits: 0,
        };
        let json = serde_json::to_value(&payload).unwrap();
        let back: CommitSummaryCompletedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(back.markdown, payload.markdown);
        assert_eq!(back.project_count, 17);
        assert_eq!(back.total_commits, 0);
    }

    #[test]
    fn commit_digest_completed_payload_omits_digest_path_when_none() {
        let payload = CommitDigestCompletedPayload {
            success: true,
            digest_path: None,
            project_count: 0,
            total_commits: 0,
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert!(json.get("digest_path").is_none(), "digest_path should be omitted when None");
        assert_eq!(json["success"], true);
    }

    #[test]
    fn commit_digest_completed_payload_round_trips_with_path() {
        let payload = CommitDigestCompletedPayload {
            success: true,
            digest_path: Some("/Users/svetzal/.foundry/digests/2026-05-28.md".to_string()),
            project_count: 17,
            total_commits: 42,
        };
        let json = serde_json::to_value(&payload).unwrap();
        let back: CommitDigestCompletedPayload = serde_json::from_value(json).unwrap();
        assert!(back.success);
        assert_eq!(back.digest_path.as_deref(), payload.digest_path.as_deref());
        assert_eq!(back.project_count, 17);
        assert_eq!(back.total_commits, 42);
    }

    // -------------------------------------------------------------------------
    // MaintenanceTriageCompleted payload
    // -------------------------------------------------------------------------

    #[test]
    fn maintenance_triage_completed_payload_defaults_from_empty_json() {
        let parsed: MaintenanceTriageCompletedPayload = serde_json::from_str("{}").unwrap();
        assert!(!parsed.success);
        assert!(!parsed.skipped);
        assert!(parsed.digest_path.is_none());
        assert!(parsed.verdicts.is_empty());
        assert!(parsed.infra_incidents.is_empty());
        assert_eq!(parsed.total_failures, 0);
        assert_eq!(parsed.suppressed_count, 0);
        assert_eq!(parsed.auto_fixable_count, 0);
        assert_eq!(parsed.policy_count, 0);
        assert_eq!(parsed.investigation_count, 0);
        assert_eq!(parsed.escalation_count, 0);
    }

    #[test]
    fn maintenance_triage_completed_payload_round_trips() {
        use crate::triage::{Decision, FailureClass, FailureVerdict, InfraIncident};

        let payload = MaintenanceTriageCompletedPayload {
            success: true,
            skipped: false,
            digest_path: Some("~/.foundry/triage/2026-06-12.md".to_string()),
            verdicts: vec![FailureVerdict {
                project: "alpha".to_string(),
                gate: "fmt".to_string(),
                class: FailureClass::FormatAndLintDrift,
                decision: Decision::AutoFixable,
                evidence: "cargo fmt produced diffs".to_string(),
                proposed_command: Some("cargo fmt".to_string()),
            }],
            infra_incidents: vec![InfraIncident {
                signature: "os_error_2".to_string(),
                decision: Decision::SuppressInfra,
                projects: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                sample_evidence: "os error 2".to_string(),
            }],
            total_failures: 5,
            suppressed_count: 3,
            auto_fixable_count: 1,
            policy_count: 0,
            investigation_count: 1,
            escalation_count: 0,
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["success"], true);
        assert_eq!(json["total_failures"], 5);
        assert_eq!(json["verdicts"].as_array().unwrap().len(), 1);
        assert_eq!(json["infra_incidents"].as_array().unwrap().len(), 1);
        assert_eq!(json["digest_path"], "~/.foundry/triage/2026-06-12.md");

        let back: MaintenanceTriageCompletedPayload = serde_json::from_value(json).unwrap();
        assert!(back.success);
        assert_eq!(back.total_failures, 5);
        assert_eq!(back.verdicts.len(), 1);
        assert_eq!(back.infra_incidents.len(), 1);
        assert_eq!(back.digest_path.as_deref(), Some("~/.foundry/triage/2026-06-12.md"));
    }

    #[test]
    fn maintenance_triage_completed_payload_omits_digest_path_when_none() {
        let payload = MaintenanceTriageCompletedPayload {
            success: true,
            ..MaintenanceTriageCompletedPayload::default()
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert!(
            json.get("digest_path").is_none(),
            "digest_path must be absent from JSON when None"
        );
    }
}
