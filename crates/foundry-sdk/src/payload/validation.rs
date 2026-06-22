use serde::{Deserialize, Serialize};

use super::context::ChainContext;

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
