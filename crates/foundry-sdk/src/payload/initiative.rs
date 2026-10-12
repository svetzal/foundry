//! Typed steering commands and reconstructible steering facts.
use crate::gateway::AgentProvider;
use crate::initiative::{DurableSteeringAction, SteeringDecision};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitiativeAdvanceRequestedPayload {
    pub initiative: String,
    #[serde(default)]
    pub member: Option<String>,
    #[serde(default)]
    pub operator_origin: Option<String>,
    #[serde(default)]
    pub trigger_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitiativeSteeredPayload {
    pub success: bool,
    pub outcome: String,
    pub error: Option<String>,
    pub initiative: String,
    pub steer_id: String,
    pub prompt: String,
    pub decision: Option<SteeringDecision>,
    pub provider: Option<AgentProvider>,
    pub action: Option<DurableSteeringAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitiativeActionRequestedPayload {
    pub action: DurableSteeringAction,
}

/// A direct owner control change, persisted before its fact is emitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitiativeOwnerActionRecordedPayload {
    pub initiative: String,
    pub action_id: String,
    pub operator_origin: String,
    pub action: InitiativeOwnerControlAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum InitiativeOwnerControlAction {
    Hold {
        member: String,
        reason: String,
    },
    Milestone {
        member: String,
        milestone: String,
        evidence: Vec<String>,
    },
    MemberOverride {
        member: String,
    },
}
