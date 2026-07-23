use serde::{Deserialize, Serialize};

use super::TaskRunCompletedPayload;
use crate::gates::GateResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignAdvanceRequestedPayload {
    pub campaign: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_result: Option<TaskRunCompletedPayload>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum CampaignDecision {
    Done { reason: String },
    Advance { objective: String, reason: String },
    Escalate { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignAdvanceCompletedPayload {
    pub campaign: String,
    pub project: String,
    pub cycles_completed: u64,
    pub cycles_landed: u64,
    #[serde(flatten)]
    pub outcome: CampaignDecision,
    /// The exact prompt the formation agent was shown.
    ///
    /// Formation is the most consequential decision in a campaign and was the
    /// only agent-invoking block that did not record its prompt, so there was
    /// no way to audit what the agent actually saw — whether it was shown the
    /// accumulated unmerged work, the objective history, or a stale tree.
    /// `None` when the decision was forced without asking an agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Which provider formed the decision. `None` for forced decisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_provider: Option<String>,
    /// Mechanical done-evidence gate results as they stood at formation.
    ///
    /// These run every cycle and gate the `done` decision, but were previously
    /// visible only as prose inside `reason` — so "was the done gate red at
    /// cycle N?" could not be queried, which is exactly the question raised by
    /// a campaign burning its budget against an unsatisfiable gate.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gate_results: Vec<GateResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignTerminalPayload {
    pub campaign: String,
    pub project: String,
    pub reason: String,
    pub cycles_completed: u64,
    pub cycles_landed: u64,
}
