use std::pin::Pin;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Composes a greeting message from a greet request.
/// Observer — always runs regardless of throttle.
pub struct ComposeGreeting;

impl TaskBlock for ComposeGreeting {
    fn name(&self) -> &'static str {
        "Compose Greeting"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::GreetRequested]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let name = trigger.payload.get("name").and_then(|v| v.as_str()).unwrap_or("world");
        let greeting = format!("Hello, {name}!");

        tracing::info!(%greeting, "composed greeting");

        Box::pin(async move {
            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::GreetingComposed,
                    project,
                    throttle,
                    serde_json::json!({ "greeting": greeting }),
                )],
                success: true,
                summary: format!("Composed: {greeting}"),
            })
        })
    }
}

/// Delivers a composed greeting (simulates a side effect).
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
pub struct DeliverGreeting;

impl TaskBlock for DeliverGreeting {
    fn name(&self) -> &'static str {
        "Deliver Greeting"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::GreetingComposed]
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
            .and_then(|v| v.as_str())
            .unwrap_or("(no greeting)");

        tracing::info!(%greeting, "delivering greeting");

        let greeting = greeting.to_string();
        Box::pin(async move {
            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::GreetingDelivered,
                    project,
                    throttle,
                    serde_json::json!({ "delivered": true, "greeting": greeting }),
                )],
                success: true,
                summary: format!("Delivered: {greeting}"),
            })
        })
    }
}
