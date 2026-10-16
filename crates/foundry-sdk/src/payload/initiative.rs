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

/// How one steering assessment ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SteeringOutcome {
    /// One decision was recorded and its action requested.
    Applied,
    /// The agent chose `wait`. Only the steering record changed.
    Waited,
    /// The decision was already in force. Only the steering record changed.
    NoOp,
    /// Foundry found nothing a steering pass could do and asked no agent.
    Skipped,
    /// A guard refused the decision. Nothing is owed: the next settled fact
    /// requests another assessment.
    Rejected,
    /// Evidence changed during the assessment; a fresh one was requested.
    Stale,
    /// The assessment faulted: the agent failed or returned unusable output,
    /// or Foundry could not read or write its own state.
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitiativeSteeredPayload {
    /// False only for a `failed` outcome.
    pub success: bool,
    pub outcome: SteeringOutcome,
    /// The fault, the guard's refusal, or why the output was stale.
    pub error: Option<String>,
    pub initiative: String,
    pub steer_id: String,
    pub prompt: String,
    pub decision: Option<SteeringDecision>,
    pub provider: Option<AgentProvider>,
    pub action: Option<DurableSteeringAction>,
    /// Why nothing changed, for `waited`, `no_op` and `skipped`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Members Foundry found eligible to advance when it asked the agent. A
    /// `wait` beside a non-empty list is the agent declining to advance them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub eligible_members: Vec<String>,
}

/// How a steer's work item settles when its outcome decides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteeringSettlement {
    /// Settle `landed`: the assessment finished and nothing is owed.
    Landed(String),
    /// Settle `cancelled`: a guard refused the decision, so it took no effect,
    /// and nothing is owed.
    Cancelled(String),
}

impl InitiativeSteeredPayload {
    /// The settlement this outcome requires, or `None` to keep the workflow's
    /// ordinary judgement (`applied` and `stale` land, `failed` fails).
    #[must_use]
    pub fn settlement(&self) -> Option<SteeringSettlement> {
        let reason = || self.reason.as_deref().unwrap_or("no reason recorded");
        match self.outcome {
            SteeringOutcome::Waited => {
                Some(SteeringSettlement::Landed(format!("steering waited: {}", reason())))
            }
            SteeringOutcome::NoOp => Some(SteeringSettlement::Landed(format!(
                "steering decision already in force: {}",
                reason()
            ))),
            SteeringOutcome::Skipped => {
                Some(SteeringSettlement::Landed(format!("steering skipped: {}", reason())))
            }
            SteeringOutcome::Rejected => Some(SteeringSettlement::Cancelled(format!(
                "steering rejected: {}",
                self.error.as_deref().unwrap_or("no reason recorded")
            ))),
            SteeringOutcome::Applied | SteeringOutcome::Stale | SteeringOutcome::Failed => None,
        }
    }
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
