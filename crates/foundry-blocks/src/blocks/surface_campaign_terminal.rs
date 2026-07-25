use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{CampaignTerminalPayload, OpsDigestStartedPayload, OpsEventDigest};
use foundry_sdk::task_block::{BlockKind, TaskBlock};

pub struct SurfaceCampaignTerminal;

impl TaskBlock for SurfaceCampaignTerminal {
    task_block_meta! {
        name: "Surface Campaign Terminal",
        kind: Observer,
        // `CampaignCancelledPayload` flattens `CampaignTerminalPayload`, so it
        // parses here unchanged and needs no special handling.
        sinks_on: [CampaignEscalated, CampaignCompleted, CampaignCancelled],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let terminal = parse_payload!(trigger, CampaignTerminalPayload);
        let event_type = trigger.event_type.to_string();
        let event_id = trigger.id.clone();
        let occurred_at = trigger.occurred_at.to_rfc3339();
        let throttle = trigger.throttle;
        Box::pin(async move {
            super::emit_result(
                format!("campaign terminal event surfaced to ops digest: {}", terminal.campaign),
                EventType::OpsDigestStarted,
                "system",
                throttle,
                &OpsDigestStartedPayload {
                    event_count: 1,
                    forced_event: Some(OpsEventDigest {
                        id: event_id,
                        event_type,
                        occurred_at,
                        domain: "engineering".to_string(),
                        urgency: Some("P1".to_string()),
                        summary: Some(format!(
                            "Campaign '{}' for {}: {} ({} cycles, {} landed)",
                            terminal.campaign,
                            terminal.project,
                            terminal.reason,
                            terminal.cycles_completed,
                            terminal.cycles_landed
                        )),
                        client: None,
                    }),
                },
            )
        })
    }
}
