use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{CampaignAdvanceRequestedPayload, TaskRunCompletedPayload};
use foundry_sdk::task_block::{BlockKind, TaskBlock};
use foundry_sdk::throttle::Throttle;

pub struct RequestCampaignAdvance;

impl TaskBlock for RequestCampaignAdvance {
    task_block_meta! {
        name: "Request Campaign Advance",
        kind: Observer,
        sinks_on: [TaskRunCompleted],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        trigger.throttle == Throttle::Full
            && trigger
                .parse_payload::<TaskRunCompletedPayload>()
                .ok()
                .and_then(|payload| payload.context.campaign)
                .is_some()
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let payload = parse_payload!(trigger, TaskRunCompletedPayload);
        let event_id = trigger.id.clone();
        let throttle = trigger.throttle;
        let project = payload.project.clone();
        Box::pin(async move {
            let campaign = payload
                .context
                .campaign
                .clone()
                .ok_or_else(|| anyhow::anyhow!("campaign task result missing campaign name"))?;
            super::emit_result(
                format!("campaign '{campaign}' advance requested from typed task result"),
                EventType::CampaignAdvanceRequested,
                &project,
                throttle,
                &CampaignAdvanceRequestedPayload {
                    campaign,
                    run_event_id: Some(event_id),
                    run_result: Some(payload),
                },
            )
        })
    }
}
