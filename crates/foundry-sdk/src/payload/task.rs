use serde::{Deserialize, Serialize};

use super::context::LoopContext;
use crate::gates::GateResult;

/// Skeptical-review outcome for a one-shot task.
///
/// This is deliberately not a boolean. A non-complete run carries the exact
/// information the campaign cutter needs for its next cycle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum TaskVerdict {
    Complete,
    Remainder {
        gaps: Vec<String>,
    },
    Defect {
        diagnosis: String,
    },
    BlockedOnDecision {
        finding: String,
        options: Vec<String>,
    },
    RunnerError {
        detail: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRunStartedPayload {
    pub project: String,
    pub objective: String,
    #[serde(flatten)]
    pub context: LoopContext,
}

impl TaskVerdict {
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// Domain fact emitted after the skeptical reviewer returns a typed verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskReviewedPayload {
    pub project: String,
    pub objective: String,
    pub review: String,
    pub gate_results: Vec<GateResult>,
    #[serde(flatten)]
    pub verdict: TaskVerdict,
    #[serde(flatten)]
    pub context: LoopContext,
}

/// Typed terminal result emitted by the task runner wrapper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRunCompletedPayload {
    pub project: String,
    pub success: bool,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preservation_ref: Option<String>,
    #[serde(flatten)]
    pub verdict: TaskVerdict,
    #[serde(flatten)]
    pub context: LoopContext,
}
