use serde::{Deserialize, Serialize};

use super::TaskRunCompletedPayload;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignTerminalPayload {
    pub campaign: String,
    pub project: String,
    pub reason: String,
    pub cycles_completed: u64,
    pub cycles_landed: u64,
}
