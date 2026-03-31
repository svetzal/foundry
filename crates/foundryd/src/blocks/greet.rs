use std::pin::Pin;

use foundry_core::event::{Event, EventType, PayloadExt};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Composes a greeting message from a greet request.
/// Observer — always runs regardless of throttle.
pub struct ComposeGreeting;

impl TaskBlock for ComposeGreeting {
    task_block_meta! {
        name: "Compose Greeting",
        kind: Observer,
        sinks_on: [GreetRequested],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let name = trigger.payload.str_or("name", "world");
        let greeting = format!("Hello, {name}!");

        tracing::info!(%greeting, "composed greeting");

        Box::pin(async move {
            Ok(TaskBlockResult::success(
                format!("Composed: {greeting}"),
                vec![Event::new(
                    EventType::GreetingComposed,
                    project,
                    throttle,
                    serde_json::json!({ "greeting": greeting }),
                )],
            ))
        })
    }
}

/// Delivers a composed greeting (simulates a side effect).
/// Mutator — events logged but not delivered at `audit_only`;
/// simulated success at `dry_run`.
pub struct DeliverGreeting;

impl TaskBlock for DeliverGreeting {
    task_block_meta! {
        name: "Deliver Greeting",
        kind: Mutator,
        sinks_on: [GreetingComposed],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let greeting = trigger.payload.str_or("greeting", "(no greeting)");

        tracing::info!(%greeting, "delivering greeting");

        let greeting = greeting.to_string();
        Box::pin(async move {
            Ok(TaskBlockResult::success(
                format!("Delivered: {greeting}"),
                vec![Event::new(
                    EventType::GreetingDelivered,
                    project,
                    throttle,
                    serde_json::json!({ "delivered": true, "greeting": greeting }),
                )],
            ))
        })
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        let greeting = trigger.payload.str_or("greeting", "(no greeting)");
        vec![Event::new(
            EventType::GreetingDelivered,
            trigger.project.clone(),
            trigger.throttle,
            serde_json::json!({ "delivered": true, "greeting": greeting, "dry_run": true }),
        )]
    }
}
