use foundry_sdk::campaign::CampaignStore;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::initiative::InitiativeStatus;
use foundry_sdk::payload::{CampaignTerminalPayload, InitiativeAdvanceRequestedPayload};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::WorkItemStore;
use foundry_sdk::work_source::WorkSource;
use std::path::PathBuf;

/// Owner facts and idle campaign terminals can wake enabled steering. Execution settlement
/// wakes it through admission only after repository cleanup and ledger accounting complete.
pub struct RequestInitiativeAdvance {
    store_path: PathBuf,
    work_items_path: PathBuf,
}
impl RequestInitiativeAdvance {
    #[must_use]
    pub fn new(store_path: PathBuf, work_items_path: PathBuf) -> Self {
        Self {
            store_path,
            work_items_path,
        }
    }
    fn run(&self, trigger: &Event) -> anyhow::Result<TaskBlockResult> {
        let store = CampaignStore::load(&self.store_path)?;
        let name = if matches!(
            trigger.event_type,
            EventType::InitiativeDecided | EventType::InitiativeOwnerActionRecorded
        ) {
            trigger
                .payload
                .get("initiative")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        } else {
            let terminal = trigger.parse_payload::<CampaignTerminalPayload>()?;
            store.find(&terminal.campaign).and_then(|campaign| campaign.initiative.clone())
        };
        let Some(name) = name else {
            return Ok(TaskBlockResult::success("fact does not belong to an initiative", vec![]));
        };
        let initiative = store.initiative_view(&name)?;
        if !initiative.steering_enabled
            || matches!(
                initiative.status,
                InitiativeStatus::Completed | InitiativeStatus::Cancelled
            )
        {
            return Ok(TaskBlockResult::success("initiative steering is inactive", vec![]));
        }
        let ledger = WorkItemStore::load(&self.work_items_path)?;
        if !matches!(
            trigger.event_type,
            EventType::InitiativeDecided | EventType::InitiativeOwnerActionRecorded
        ) && ledger
            .items
            .iter()
            .any(|item| item.occupies_slot() && initiative.projects.contains(&item.project))
        {
            return Ok(TaskBlockResult::success("initiative waits for execution cleanup", vec![]));
        }
        let mut requested = trigger.with_payload(
            EventType::InitiativeAdvanceRequested,
            &InitiativeAdvanceRequestedPayload {
                initiative: name.clone(),
                member: None,
                operator_origin: None,
                trigger_id: Some(trigger.id.clone()),
            },
        )?;
        requested.id = format!("evt_{}_steer", trigger.id);
        requested.project = "system".into();
        requested.source = Some(WorkSource::initiative(name, 0));
        Ok(TaskBlockResult::success(
            "initiative steering requested from settled fact",
            vec![requested],
        ))
    }
}
impl TaskBlock for RequestInitiativeAdvance {
    task_block_meta! { name: "Request Initiative Advance", kind: Observer, sinks_on: [InitiativeDecided, InitiativeOwnerActionRecorded, CampaignCompleted, CampaignCancelled, CampaignEscalated], }
    fn accepts(&self, trigger: &Event) -> bool {
        trigger.throttle == foundry_sdk::throttle::Throttle::Full
            && if matches!(
                trigger.event_type,
                EventType::InitiativeDecided | EventType::InitiativeOwnerActionRecorded
            ) {
                trigger.payload.get("initiative").and_then(serde_json::Value::as_str).is_some()
            } else {
                trigger.parse_payload::<CampaignTerminalPayload>().is_ok()
            }
    }
    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        {
            let result = self.run(trigger);
            Box::pin(async move { result })
        }
    }
}
