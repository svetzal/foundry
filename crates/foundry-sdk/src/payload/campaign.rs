use serde::{Deserialize, Serialize};

use super::TaskRunCompletedPayload;
use crate::campaign::CampaignStatus;
use crate::gates::GateResult;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignAdvanceRequestedPayload {
    pub campaign: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_result: Option<TaskRunCompletedPayload>,
    /// Opaque operator context from a manual `foundry campaign advance`.
    ///
    /// Absent for the automatic post-result advance, which no operator issued.
    /// Forwarded verbatim onto the cycle's `ExecutionRequested` so the ledger
    /// can record who asked; never parsed or acted on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_origin: Option<String>,
    /// The initiative-selected pairing for this cycle, below explicit member overrides.
    #[serde(default)]
    pub providers: crate::gateway::StageProviders,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum CampaignDecision {
    Done { reason: String },
    Advance { objective: String, reason: String },
    Escalate { reason: String },
}

/// What one campaign advance concluded.
///
/// Either formation decided, or the advance stood down because someone else
/// stopped or settled the campaign first. Both arrive under the `decision`
/// tag, so a reader that switches on it sees `stood_down` as its own value and
/// can never take it for `advance` or `done`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CampaignAdvanceOutcome {
    Decided(CampaignDecision),
    StoodDown(CampaignStandDown),
}

/// An advance that applied nothing because the campaign was paused,
/// cancelled, completed or escalated before (or while) it formed.
///
/// It dispatches nothing, changes no status and counts no cycle: the stop
/// belongs to whoever made it. It is a separate type from
/// [`CampaignDecision`] because a decision agent can never choose it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CampaignStandDown {
    /// Always `stood_down` on the wire. Private, so the only way to build one
    /// is [`CampaignStandDown::new`], and a payload carrying any other
    /// `decision` never reads as a stand-down.
    #[serde(rename = "decision")]
    tag: StandDownTag,
    /// The campaign's status when the advance stood down.
    pub campaign_status: CampaignStatus,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StandDownTag {
    StoodDown,
}

impl CampaignStandDown {
    #[must_use]
    pub fn new(campaign_status: CampaignStatus, reason: String) -> Self {
        Self {
            tag: StandDownTag::StoodDown,
            campaign_status,
            reason,
        }
    }
}

fn advance_succeeded() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignAdvanceCompletedPayload {
    /// Most recent task result known to the campaign. The prompt records what was supplied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_task_run_event_id: Option<String>,
    pub campaign: String,
    pub project: String,
    pub cycles_completed: u64,
    pub cycles_landed: u64,
    /// Whether the advance did its job: decided, or stood down for a stop.
    ///
    /// `false` only for a fault that kept the advance from settling the
    /// campaign (store unreadable, save failed, campaign missing). The work
    /// item ledger settles an advance `failed` on exactly this. Events
    /// written before the field existed read as `true`, matching how the
    /// ledger read them.
    #[serde(default = "advance_succeeded")]
    pub success: bool,
    #[serde(flatten)]
    pub outcome: CampaignAdvanceOutcome,
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

/// An operator-issued cancellation, carrying the disposition choices the
/// operator made alongside the shared terminal fields.
///
/// The flatten is load-bearing: `CampaignTerminalPayload` does not
/// `deny_unknown_fields`, so anything reading a terminal event generically —
/// `SurfaceCampaignTerminal`, the ops digest — parses this payload unchanged
/// and needs no knowledge of the extra fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignCancelledPayload {
    #[serde(flatten)]
    pub terminal: CampaignTerminalPayload,
    /// The operator passed `--now`: the in-flight workflow was aborted rather
    /// than left to finish. Also the signal that an orphaned worktree may need
    /// disposing, since `FinalizeTask` never ran for that cycle.
    pub terminated_now: bool,
    /// The operator passed `--discard-work`: throw the aborted cycle's
    /// uncommitted work away instead of committing and preserving it.
    pub discard_work: bool,
    /// Root event of the workflow that was aborted to satisfy `terminated_now`.
    ///
    /// That run has no trace file — the abort skipped `run_workflow`'s tail, so
    /// nothing ever called `trace_store.insert` — which makes this the only
    /// handle onto its partial events in the JSONL log.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aborted_event_id: Option<String>,
    /// Trace of the workflow that was aborted to satisfy `terminated_now`.
    ///
    /// The cancellation itself opens a fresh trace, so the killed cycle's trace
    /// is carried here rather than on the event. It is what correlates the
    /// cancellation with the work-item ledger: the cycle's item was recorded
    /// `running` under this trace, and nothing else can settle it now that the
    /// run producing its `TaskRunCompleted` is gone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aborted_trace_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cancelled() -> CampaignCancelledPayload {
        CampaignCancelledPayload {
            terminal: CampaignTerminalPayload {
                campaign: "c".to_string(),
                project: "p".to_string(),
                reason: "abandoned".to_string(),
                cycles_completed: 3,
                cycles_landed: 1,
            },
            terminated_now: true,
            discard_work: false,
            aborted_event_id: Some("evt_abc".to_string()),
            aborted_trace_id: Some("a".repeat(32)),
        }
    }

    /// `SurfaceCampaignTerminal` and the ops digest parse every terminal
    /// campaign event as a `CampaignTerminalPayload`. If the flatten ever
    /// regressed to a nested field they would silently stop reporting
    /// cancellations, so assert the generic read directly.
    #[test]
    fn cancelled_payload_reads_as_a_plain_terminal_payload() {
        let json = serde_json::to_value(cancelled()).unwrap();
        assert_eq!(json["campaign"], "c");
        assert_eq!(json["cycles_landed"], 1);

        let terminal: CampaignTerminalPayload = serde_json::from_value(json).unwrap();
        assert_eq!(terminal.reason, "abandoned");
        assert_eq!(terminal.cycles_completed, 3);
    }

    #[test]
    fn cancelled_payload_round_trips_its_disposition_flags() {
        let json = serde_json::to_value(cancelled()).unwrap();
        let back: CampaignCancelledPayload = serde_json::from_value(json).unwrap();
        assert!(back.terminated_now);
        assert!(!back.discard_work);
        assert_eq!(back.aborted_event_id.as_deref(), Some("evt_abc"));
    }

    fn advance_completed(outcome: CampaignAdvanceOutcome) -> CampaignAdvanceCompletedPayload {
        CampaignAdvanceCompletedPayload {
            last_task_run_event_id: None,
            campaign: "c".to_string(),
            project: "p".to_string(),
            cycles_completed: 2,
            cycles_landed: 1,
            success: true,
            outcome,
            prompt: None,
            agent_provider: None,
            gate_results: Vec::new(),
        }
    }

    #[test]
    fn a_decided_advance_keeps_its_wire_shape_and_gains_only_success() {
        let json = serde_json::to_value(advance_completed(CampaignAdvanceOutcome::Decided(
            CampaignDecision::Advance {
                objective: "next".to_string(),
                reason: "gap".to_string(),
            },
        )))
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "campaign": "c", "project": "p", "cycles_completed": 2, "cycles_landed": 1,
                "success": true, "decision": "advance", "objective": "next", "reason": "gap"
            })
        );
        let back: CampaignAdvanceCompletedPayload = serde_json::from_value(json).unwrap();
        assert!(matches!(
            back.outcome,
            CampaignAdvanceOutcome::Decided(CampaignDecision::Advance { .. })
        ));
    }

    #[test]
    fn a_stand_down_is_its_own_decision_value_naming_the_status() {
        let json = serde_json::to_value(advance_completed(CampaignAdvanceOutcome::StoodDown(
            CampaignStandDown::new(CampaignStatus::Paused, "campaign 'c' is paused".to_string()),
        )))
        .unwrap();
        assert_eq!(json["decision"], "stood_down");
        assert_eq!(json["campaign_status"], "paused");
        assert_eq!(json["success"], true);
        assert!(json.get("objective").is_none());
        let back: CampaignAdvanceCompletedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(
            back.outcome,
            CampaignAdvanceOutcome::StoodDown(CampaignStandDown::new(
                CampaignStatus::Paused,
                "campaign 'c' is paused".to_string()
            ))
        );
    }

    #[test]
    fn an_advance_written_before_success_existed_reads_as_succeeded() {
        let back: CampaignAdvanceCompletedPayload = serde_json::from_value(serde_json::json!({
            "campaign": "c", "project": "p", "cycles_completed": 1, "cycles_landed": 0,
            "decision": "escalate", "reason": "owner review needed"
        }))
        .unwrap();
        assert!(back.success);
        assert!(matches!(
            back.outcome,
            CampaignAdvanceOutcome::Decided(CampaignDecision::Escalate { .. })
        ));
    }

    #[test]
    fn a_stand_down_requires_its_own_tag() {
        let parsed = serde_json::from_value::<CampaignStandDown>(serde_json::json!({
            "decision": "advance", "campaign_status": "paused", "reason": "x"
        }));
        assert!(parsed.is_err());
    }

    #[test]
    fn a_decision_agent_cannot_answer_stood_down() {
        let parsed = serde_json::from_value::<CampaignDecision>(serde_json::json!({
            "decision": "stood_down", "campaign_status": "paused", "reason": "x"
        }));
        assert!(parsed.is_err());
    }

    #[test]
    fn absent_aborted_event_id_is_omitted_and_defaults() {
        let mut payload = cancelled();
        payload.aborted_event_id = None;
        let json = serde_json::to_value(&payload).unwrap();
        assert!(json.get("aborted_event_id").is_none());
        let back: CampaignCancelledPayload = serde_json::from_value(json).unwrap();
        assert!(back.aborted_event_id.is_none());
    }
}
