use serde::{Deserialize, Serialize};

use super::context::ChainContext;

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
