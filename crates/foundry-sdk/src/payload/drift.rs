use serde::{Deserialize, Serialize};

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
