use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::{GreetingComposedPayload, GreetingDeliveredPayload};
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
        let name = trigger
            .payload
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("world");
        let greeting = format!("Hello, {name}!");

        tracing::info!(%greeting, "composed greeting");

        Box::pin(async move {
            Ok(TaskBlockResult::success(
                format!("Composed: {greeting}"),
                vec![Event::new(
                    EventType::GreetingComposed,
                    project,
                    throttle,
                    Event::serialize_payload(&GreetingComposedPayload { greeting })?,
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
        let greeting = trigger
            .payload
            .get("greeting")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("(no greeting)")
            .to_string();

        tracing::info!(%greeting, "delivering greeting");

        Box::pin(async move {
            Ok(TaskBlockResult::success(
                format!("Delivered: {greeting}"),
                vec![Event::new(
                    EventType::GreetingDelivered,
                    project,
                    throttle,
                    Event::serialize_payload(&GreetingDeliveredPayload {
                        delivered: true,
                        greeting,
                        dry_run: None,
                    })?,
                )],
            ))
        })
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        let greeting = trigger
            .payload
            .get("greeting")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("(no greeting)")
            .to_string();
        let payload = Event::serialize_payload(&GreetingDeliveredPayload {
            delivered: true,
            greeting,
            dry_run: Some(true),
        })
        .expect("GreetingDeliveredPayload is infallibly serializable");
        vec![Event::new(
            EventType::GreetingDelivered,
            trigger.project.clone(),
            trigger.throttle,
            payload,
        )]
    }
}
