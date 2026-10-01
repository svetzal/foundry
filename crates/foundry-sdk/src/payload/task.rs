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

impl TaskRunCompletedPayload {
    /// Whether the reviewer's verdict admitted the work but it did not reach
    /// trunk — preserved work to reconcile, not a defect.
    #[must_use]
    pub fn is_unlanded_complete(&self) -> bool {
        self.verdict.is_complete() && !self.landed && self.land_blocked.is_some()
    }
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

/// Why landing-eligible work (a `complete`, or a converging `remainder` with
/// green required gates) did not reach trunk.
///
/// Landing is Foundry's integration step, not part of the reviewer's judgement,
/// so a blocked landing never rewrites the verdict. It is recorded beside it:
/// a `complete` that could not land is still complete work, preserved for
/// reconciliation rather than reported as a defect.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LandBlocked {
    /// Trunk advanced during the run and rebasing the task branch onto it
    /// conflicted.
    TrunkMovedConflict,
    /// Trunk advanced during the run; the rebase was clean but a required
    /// gate failed on the rebased tree.
    TrunkMovedGatesFailed,
    /// Trunk kept advancing: it moved again after the final permitted rebase.
    TrunkMovedRepeatedly,
    /// The registered checkout was not safe to land into (dirty, or on the
    /// wrong branch).
    CheckoutNotReady,
    /// A git operation needed to land failed for any other reason.
    GitFailed,
}

impl LandBlocked {
    /// The reason's wire tag — the same string the `land_blocked` field
    /// serializes to.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::TrunkMovedConflict => "trunk_moved_conflict",
            Self::TrunkMovedGatesFailed => "trunk_moved_gates_failed",
            Self::TrunkMovedRepeatedly => "trunk_moved_repeatedly",
            Self::CheckoutNotReady => "checkout_not_ready",
            Self::GitFailed => "git_failed",
        }
    }
}

impl std::fmt::Display for LandBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.tag())
    }
}

/// A commit that reached the registered trunk while a task was running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrunkArrival {
    pub commit: String,
    pub subject: String,
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
    /// Set when landing-eligible work could not reach trunk. The verdict is
    /// left exactly as the reviewer returned it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub land_blocked: Option<LandBlocked>,
    /// Trunk commits that arrived while the task ran, oldest first. Empty
    /// when trunk did not move.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trunk_arrivals: Vec<TrunkArrival>,
    /// Durable directory holding early acceptance proof and its copied logs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof_evidence: Option<String>,
    #[serde(flatten)]
    pub verdict: TaskVerdict,
    #[serde(flatten)]
    pub context: LoopContext,
}

#[cfg(test)]
mod tests {
    use super::{LandBlocked, TaskRunCompletedPayload, TaskVerdict, TrunkArrival};

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

    /// A blocked landing and the commits that arrived on trunk are recorded
    /// beside the verdict, never in place of it.
    #[test]
    fn a_blocked_landing_keeps_the_complete_verdict_on_the_wire() {
        let payload = TaskRunCompletedPayload {
            project: "alpha".to_string(),
            success: false,
            landed: false,
            summary: "preserved".to_string(),
            preservation_ref: Some("foundry-task/alpha-1".to_string()),
            land_blocked: Some(LandBlocked::TrunkMovedGatesFailed),
            proof_evidence: None,
            trunk_arrivals: vec![TrunkArrival {
                commit: "abc123".to_string(),
                subject: "docs: note".to_string(),
            }],
            verdict: TaskVerdict::Complete,
            context: super::LoopContext::default(),
        };
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["verdict"], "complete");
        assert_eq!(json["land_blocked"], "trunk_moved_gates_failed");
        assert_eq!(json["trunk_arrivals"][0]["commit"], "abc123");
        assert!(payload.is_unlanded_complete());
        let back: TaskRunCompletedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn every_land_blocked_tag_matches_its_serialized_form() {
        for reason in [
            LandBlocked::TrunkMovedConflict,
            LandBlocked::TrunkMovedGatesFailed,
            LandBlocked::TrunkMovedRepeatedly,
            LandBlocked::CheckoutNotReady,
            LandBlocked::GitFailed,
        ] {
            assert_eq!(serde_json::to_value(reason).unwrap(), reason.tag());
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
            land_blocked: None,
            proof_evidence: None,
            trunk_arrivals: Vec::new(),
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
