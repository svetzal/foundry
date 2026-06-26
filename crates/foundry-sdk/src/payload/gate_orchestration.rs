use serde::{Deserialize, Serialize};

use super::context::{ChainContext, LoopContext};
use crate::gateway::AgentFailureMetadata;

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
    #[serde(default, flatten)]
    pub failure: AgentFailureMetadata,
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
    #[serde(default, flatten)]
    pub failure: AgentFailureMetadata,
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
