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
