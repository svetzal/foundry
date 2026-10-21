use serde::{Deserialize, Serialize};

use crate::work_item::{
    FollowUp, WorkDisposition, WorkItem, WorkItemKind, WorkItemState, WorkLane,
};
use crate::work_source::WorkSource;

/// Payload for every work-item lifecycle event — `work_item_submitted`,
/// `work_item_started`, `work_item_settled`, `work_item_cancelled`,
/// `work_item_held` and `work_item_released`.
///
/// One payload serves all six because they report the same record at
/// different points in its life; the event type says which point, and `state`
/// says where the item stands. A `work_item_submitted` reports the item
/// `queued`, and its `reason` is why it waits (see [`crate::pacing`]).
/// `disposition` is present only once the item has settled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItemEventPayload {
    /// The work item's stable id.
    pub item_id: String,
    /// Registry project name.
    pub project: String,
    /// The task description or campaign objective the work serves.
    pub objective: String,
    /// What sort of work this is.
    pub kind: WorkItemKind,
    /// Which queue it belongs to.
    pub lane: WorkLane,
    /// Where the item stands as of this event.
    pub state: WorkItemState,
    /// Why it is in that state, in one line.
    pub reason: String,
    /// Opaque submitter text.
    pub origin: String,
    /// What dispatched the work, typed. Absent when the item records none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<WorkSource>,
    /// Member campaign identity, retained when the dispatcher is an initiative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign: Option<String>,
    /// Member campaign cycle, independent of the dispatcher's source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub campaign_cycle: Option<u64>,
    /// How the item ended. Present only on a settlement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<WorkDisposition>,
    /// Exact preserved item this task continues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumes: Option<String>,
    /// Open items this task follows up on. Absent when it continues none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continues: Vec<String>,
    /// The obligation a follow-up item holds. Absent on every other kind.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up: Option<FollowUp>,
    /// Owner action, without replacing the original submission origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_action: Option<crate::work_item::WorkItemOperatorAction>,
    /// Items this one waits on. Absent when it waits on none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// The earliest time the scheduler may start it. Absent when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<chrono::DateTime<chrono::Utc>>,
}

impl WorkItemEventPayload {
    /// Report `item` as it currently stands.
    #[must_use]
    pub fn from_item(item: &WorkItem) -> Self {
        Self {
            item_id: item.id.clone(),
            project: item.project.clone(),
            objective: item.objective.clone(),
            kind: item.kind,
            lane: item.lane,
            state: item.state,
            reason: item.reason.clone(),
            origin: item.origin.clone(),
            source: item.source.clone(),
            campaign: item.campaign_name().map(str::to_string),
            campaign_cycle: item.campaign_cycle(),
            disposition: item.disposition.clone(),
            operator_action: item.operator_action.clone(),
            resumes: item.resumes.clone(),
            continues: item.continues.clone(),
            follow_up: item.follow_up.clone(),
            depends_on: item.depends_on.clone(),
            not_before: item.not_before,
        }
    }
}

/// Payload for `pacing_paused` and `pacing_resumed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PacingPayload {
    /// The lanes this action named.
    pub lanes: Vec<WorkLane>,
    /// Every lane paused after the action was applied.
    pub paused: Vec<WorkLane>,
    /// CLI hostname and optional operator context.
    pub operator_origin: String,
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::WorkItemEventPayload;
    use crate::work_item::{WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkLane};
    use crate::work_source::WorkSource;

    fn item() -> WorkItem {
        WorkItem::dispatched(
            WorkItemSpec {
                project: "alpha".to_string(),
                objective: "Add a --quiet flag.".to_string(),
                kind: WorkItemKind::CampaignCycle,
                lane: WorkLane::Campaign,
                origin: "campaign tidy-cli cycle 3".to_string(),
                trace_id: Some("a".repeat(32)),
            },
            Utc::now(),
        )
    }

    #[test]
    fn a_running_item_reports_every_identifying_field_and_no_disposition() {
        let payload = WorkItemEventPayload::from_item(&item());
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["project"], "alpha");
        assert_eq!(json["kind"], "campaign_cycle");
        assert_eq!(json["lane"], "campaign");
        assert_eq!(json["state"], "running");
        assert_eq!(json["reason"], "running");
        assert_eq!(json["origin"], "campaign tidy-cli cycle 3");
        assert_eq!(json["objective"], "Add a --quiet flag.");
        assert!(json["item_id"].as_str().unwrap().starts_with("wi_"));
        assert!(json.get("disposition").is_none(), "an unsettled item has no disposition");
        assert!(json.get("source").is_none(), "an item with no recorded source writes no key");
    }

    #[test]
    fn a_recorded_source_is_reported_beside_the_untouched_origin() {
        let sourced = item().with_source(Some(WorkSource::campaign("tidy-cli", 3)));
        let payload = WorkItemEventPayload::from_item(&sourced);
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["origin"], "campaign tidy-cli cycle 3");
        assert_eq!(
            json["source"],
            serde_json::json!({"kind": "campaign", "ref": "tidy-cli", "cycle": 3})
        );
        let round_tripped: WorkItemEventPayload = serde_json::from_value(json).unwrap();
        assert_eq!(round_tripped, payload);
    }

    #[test]
    fn a_settled_item_reports_its_disposition() {
        let mut settled = item();
        settled.settle_failed("daemon restarted", Utc::now());
        let payload = WorkItemEventPayload::from_item(&settled);
        assert_eq!(payload.state, WorkItemState::Failed);
        assert_eq!(payload.reason, "daemon restarted");

        let round_tripped: WorkItemEventPayload =
            serde_json::from_value(serde_json::to_value(&payload).unwrap()).unwrap();
        assert_eq!(round_tripped, payload);
    }
}
