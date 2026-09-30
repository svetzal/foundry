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

    /// The verdict's wire tag — the same string the `verdict` field
    /// serializes to.
    ///
    /// Consumers that record *which* verdict a run returned, without its
    /// payload (the work-item ledger's disposition, for one), need the tag on
    /// its own; deriving it here keeps it from being spelled out a second
    /// time somewhere else.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Remainder { .. } => "remainder",
            Self::Defect { .. } => "defect",
            Self::BlockedOnDecision { .. } => "blocked_on_decision",
            Self::RunnerError { .. } => "runner_error",
        }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRunCompletedPayload {
    pub project: String,
    pub success: bool,
    pub landed: bool,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preservation_ref: Option<String>,
    #[serde(flatten)]
    pub verdict: TaskVerdict,
    #[serde(flatten)]
    pub context: LoopContext,
}

#[cfg(test)]
mod tests {
    use super::{TaskRunCompletedPayload, TaskVerdict};

    /// The tag must not drift from the wire format it names.
    #[test]
    fn every_verdict_tag_matches_the_serialized_verdict_field() {
        let verdicts = [
            TaskVerdict::Complete,
            TaskVerdict::Remainder { gaps: vec![] },
            TaskVerdict::Defect {
                diagnosis: String::new(),
            },
            TaskVerdict::BlockedOnDecision {
                finding: String::new(),
                options: vec![],
            },
            TaskVerdict::RunnerError {
                detail: String::new(),
            },
        ];
        for verdict in verdicts {
            let tag = verdict.tag();
            let json = serde_json::to_value(&verdict).unwrap();
            assert_eq!(json["verdict"], tag, "tag drifted for {verdict:?}");
        }
    }

    /// The ledger reads this payload; its shape must stay exactly as it was.
    #[test]
    fn a_task_run_completed_payload_keeps_its_wire_shape() {
        let payload = TaskRunCompletedPayload {
            project: "alpha".to_string(),
            success: true,
            landed: true,
            summary: "done".to_string(),
            preservation_ref: None,
            verdict: TaskVerdict::Complete,
            context: super::LoopContext::default(),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "project": "alpha",
                "success": true,
                "landed": true,
                "summary": "done",
                "verdict": "complete",
            })
        );
    }
}
