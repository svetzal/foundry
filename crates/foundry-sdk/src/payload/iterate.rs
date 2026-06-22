use serde::{Deserialize, Serialize};

use super::context::ChainContext;

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
