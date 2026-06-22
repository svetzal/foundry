use serde::{Deserialize, Serialize};

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
