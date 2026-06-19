use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    GreetingComposedPayload, GreetingDeliveredPayload, GreetingRequestedPayload,
};
use foundry_sdk::task_block::{BlockKind, TaskBlock};

/// Composes a greeting message from a greet request.
/// Observer — always runs regardless of throttle.
pub struct ComposeGreeting;

impl TaskBlock for ComposeGreeting {
    task_block_meta! {
        name: "Compose Greeting",
        kind: Observer,
        sinks_on: [GreetingRequested],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let name_owned =
            trigger.parse_payload::<GreetingRequestedPayload>().ok().and_then(|p| p.name);
        let name = name_owned.as_deref().unwrap_or("world");
        let greeting = format!("Hello, {name}!");

        tracing::info!(%greeting, "composed greeting");

        Box::pin(async move {
            super::emit_result(
                format!("Composed: {greeting}"),
                EventType::GreetingComposed,
                &project,
                throttle,
                &GreetingComposedPayload { greeting },
            )
        })
    }
}

/// Delivers a composed greeting (simulates a side effect).
/// Mutator — simulated success at `dry_run`.
pub struct DeliverGreeting;

impl TaskBlock for DeliverGreeting {
    task_block_meta! {
        name: "Deliver Greeting",
        kind: Mutator,
        sinks_on: [GreetingComposed],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let greeting = parse_payload!(trigger, GreetingComposedPayload).greeting;

        tracing::info!(%greeting, "delivering greeting");

        Box::pin(async move {
            super::emit_result(
                format!("Delivered: {greeting}"),
                EventType::GreetingDelivered,
                &project,
                throttle,
                &GreetingDeliveredPayload {
                    delivered: true,
                    greeting,
                    dry_run: None,
                },
            )
        })
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        let greeting = trigger
            .parse_payload::<GreetingComposedPayload>()
            .map_or_else(|_| "(no greeting)".to_string(), |p| p.greeting);
        super::dry_run_single_event(
            trigger,
            EventType::GreetingDelivered,
            &GreetingDeliveredPayload {
                delivered: true,
                greeting,
                dry_run: Some(true),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::payload::{GreetingComposedPayload, GreetingRequestedPayload};
    use foundry_sdk::task_block::{BlockKind, TaskBlock};
    use foundry_sdk::throttle::Throttle;

    use super::{ComposeGreeting, DeliverGreeting};

    fn greet_requested(name: Option<&str>) -> Event {
        let payload = Event::serialize_payload(&GreetingRequestedPayload {
            name: name.map(str::to_string),
        })
        .unwrap();
        Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            payload,
        )
    }

    fn greeting_composed(greeting: &str) -> Event {
        let payload = Event::serialize_payload(&GreetingComposedPayload {
            greeting: greeting.to_string(),
        })
        .unwrap();
        Event::new(EventType::GreetingComposed, "test-project".to_string(), Throttle::Full, payload)
    }

    #[tokio::test]
    async fn compose_greeting_with_name() {
        let block = ComposeGreeting;
        let trigger = greet_requested(Some("Alice"));

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::GreetingComposed);
        assert_eq!(result.events[0].payload["greeting"], "Hello, Alice!");
    }

    #[tokio::test]
    async fn compose_greeting_without_name_defaults_to_world() {
        let block = ComposeGreeting;
        let trigger = greet_requested(None);

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].payload["greeting"], "Hello, world!");
    }

    #[test]
    fn compose_greeting_metadata() {
        let block = ComposeGreeting;
        assert_eq!(block.kind(), BlockKind::Observer);
        assert_eq!(block.sinks_on(), &[EventType::GreetingRequested]);
    }

    #[tokio::test]
    async fn deliver_greeting_emits_delivered_event() {
        let block = DeliverGreeting;
        let trigger = greeting_composed("Hello, Bob!");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::GreetingDelivered);
        assert_eq!(result.events[0].payload["delivered"], true);
        assert_eq!(result.events[0].payload["greeting"], "Hello, Bob!");
        assert!(result.events[0].payload.get("dry_run").is_none());
    }

    #[test]
    fn deliver_greeting_dry_run_events() {
        let block = DeliverGreeting;
        let trigger = greeting_composed("Hello, dry run!");

        let events = block.dry_run_events(&trigger);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::GreetingDelivered);
        assert_eq!(events[0].payload["delivered"], true);
        assert_eq!(events[0].payload["dry_run"], true);
    }

    #[test]
    fn deliver_greeting_dry_run_with_bad_payload() {
        let block = DeliverGreeting;
        let trigger = Event::new(
            EventType::GreetingComposed,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({"not_a_greeting": "oops"}),
        );

        let events = block.dry_run_events(&trigger);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].payload["greeting"], "(no greeting)");
        assert_eq!(events[0].payload["dry_run"], true);
    }
}
