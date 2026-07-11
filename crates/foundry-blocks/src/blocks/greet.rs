use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    GreetingComposedPayload, GreetingDeliveredPayload, GreetingRequestedPayload,
};
use foundry_sdk::task_block::{BlockKind, TaskBlock};
use foundry_sdk::throttle::Throttle;

use super::{SimulatedSuccess, TriggerContext};

/// Build a single `GreetingDelivered` event.
///
/// Single source of truth for the `EventType::GreetingDelivered` +
/// `GreetingDeliveredPayload` pairing — called by both `dry_run_events`
/// and `execute` in [`DeliverGreeting`] so no path can silently drift.
fn greeting_delivered_event(
    project: &str,
    throttle: Throttle,
    payload: &GreetingDeliveredPayload,
) -> Event {
    super::event_from_infallible_payload(EventType::GreetingDelivered, project, throttle, payload)
}

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
        let TriggerContext {
            project, throttle, ..
        } = TriggerContext::from_trigger(trigger);
        let name_owned =
            trigger.parse_payload::<GreetingRequestedPayload>().ok().and_then(|p| p.name);
        let name = name_owned.as_deref().unwrap_or("world");
        let greeting = format!("Hello, {name}!");

        tracing::info!(%greeting, "composed greeting");

        emit_observer!(
            project,
            throttle,
            format!("Composed: {greeting}"),
            GreetingComposed,
            GreetingComposedPayload { greeting }
        )
    }
}

/// Delivers a composed greeting (simulates a side effect).
/// Mutator — simulated success at `dry_run`.
pub struct DeliverGreeting;

/// Outcome of a deliver-greeting dry-run simulation.
pub(crate) struct GreetingOutcome {
    greeting: String,
}

impl SimulatedSuccess for DeliverGreeting {
    type Outcome = GreetingOutcome;

    fn simulate(&self, trigger: &Event) -> GreetingOutcome {
        let greeting = trigger
            .parse_payload::<GreetingComposedPayload>()
            .map_or_else(|_| "(no greeting)".to_string(), |p| p.greeting);
        GreetingOutcome { greeting }
    }

    fn success_events(&self, trigger: &Event, outcome: &GreetingOutcome) -> Vec<Event> {
        vec![greeting_delivered_event(
            &trigger.project,
            trigger.throttle,
            &GreetingDeliveredPayload {
                delivered: true,
                greeting: outcome.greeting.clone(),
                dry_run: Some(true),
            },
        )]
    }
}

impl TaskBlock for DeliverGreeting {
    task_block_meta! {
        name: "Deliver Greeting",
        kind: Mutator,
        sinks_on: [GreetingComposed],
    }

    dry_run_via_simulation!();

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project, throttle, ..
        } = TriggerContext::from_trigger(trigger);
        let greeting = parse_payload!(trigger, GreetingComposedPayload).greeting;

        tracing::info!(%greeting, "delivering greeting");

        let event = greeting_delivered_event(
            &project,
            throttle,
            &GreetingDeliveredPayload {
                delivered: true,
                greeting: greeting.clone(),
                dry_run: None,
            },
        );
        Box::pin(async move {
            Ok(foundry_sdk::task_block::TaskBlockResult::success(
                format!("Delivered: {greeting}"),
                vec![event],
            ))
        })
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
    fn dry_run_and_execute_agree_on_primary_output_event_type_for_deliver_greeting() {
        // After the refactor, this is guaranteed structurally by greeting_delivered_event.
        let block = DeliverGreeting;
        let trigger = greeting_composed("Hello, world!");
        let dry_events = block.dry_run_events(&trigger);
        assert_eq!(dry_events[0].event_type, EventType::GreetingDelivered);
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
