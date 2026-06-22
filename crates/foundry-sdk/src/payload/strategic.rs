use serde::{Deserialize, Serialize};

use super::context::{AreaEntry, StrategicLoopContext};

// ---------------------------------------------------------------------------
// Strategic loop workflow
// ---------------------------------------------------------------------------

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
