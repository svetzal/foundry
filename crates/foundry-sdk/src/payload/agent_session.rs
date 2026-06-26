use serde::{Deserialize, Serialize};

use crate::gateway::AgentFailureMetadata;

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
    #[serde(default, flatten)]
    pub failure: AgentFailureMetadata,
}
