use foundry_sdk::event::Event;

/// Simulation seam for Mutator blocks — eliminates knowledge duplication between
/// `dry_run_events()` and `execute()`.
///
/// Blocks that implement this trait use `dry_run_via_simulation!()` to generate
/// `dry_run_events`, calling `simulate()` to produce a synthetic outcome and
/// `success_events()` to build the events. Both `dry_run_events` and the
/// `execute()` success path ultimately call the same event-building helpers,
/// making the event shape a single source of truth.
pub(crate) trait SimulatedSuccess {
    /// Facts that `execute()` learns at runtime; dry-run supplies a synthetic value.
    type Outcome;

    /// Produce a synthetic success outcome without performing I/O.
    ///
    /// May read the trigger payload or the registry to compute routing-relevant
    /// fields (e.g., `push_enabled`). Must not spawn processes or do network I/O.
    fn simulate(&self, trigger: &Event) -> Self::Outcome;

    /// SINGLE source of truth for the events emitted on success.
    ///
    /// Called by `dry_run_events()` (via `dry_run_via_simulation!()`) with the
    /// simulated outcome, and used as the canonical event shape reference by
    /// `execute()`.
    fn success_events(&self, trigger: &Event, outcome: &Self::Outcome) -> Vec<Event>;
}

/// Implement `dry_run_events` by delegating to `SimulatedSuccess`.
///
/// Invoke inside an `impl TaskBlock for MyBlock { ... }` block:
/// ```ignore
/// impl TaskBlock for MyBlock {
///     task_block_meta! { ... }
///     dry_run_via_simulation!();
///     fn execute(&self, trigger: &Event) -> BlockFuture<'_> { ... }
/// }
/// ```
macro_rules! dry_run_via_simulation {
    () => {
        fn dry_run_events(
            &self,
            trigger: &foundry_sdk::event::Event,
        ) -> Vec<foundry_sdk::event::Event> {
            self.success_events(trigger, &self.simulate(trigger))
        }
    };
}

#[cfg(test)]
mod tests {
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::task_block::{BlockFuture, BlockKind, TaskBlock, TaskBlockResult};
    use foundry_sdk::throttle::Throttle;

    use super::SimulatedSuccess;

    /// Minimal Mutator block that implements SimulatedSuccess for testing the macro.
    struct FakeSimulated;

    impl SimulatedSuccess for FakeSimulated {
        type Outcome = bool;

        fn simulate(&self, _trigger: &Event) -> bool {
            true
        }

        fn success_events(&self, trigger: &Event, outcome: &bool) -> Vec<Event> {
            if *outcome {
                vec![Event::new(
                    EventType::GreetingDelivered,
                    trigger.project.clone(),
                    trigger.throttle,
                    serde_json::json!({"dry_run": true}),
                )]
            } else {
                vec![]
            }
        }
    }

    impl TaskBlock for FakeSimulated {
        fn name(&self) -> &'static str {
            "FakeSimulated"
        }
        fn kind(&self) -> BlockKind {
            BlockKind::Mutator
        }
        fn sinks_on(&self) -> &[EventType] {
            &[]
        }
        fn execute(&self, _trigger: &Event) -> BlockFuture<'_> {
            Box::pin(async { Ok(TaskBlockResult::success("ok", vec![])) })
        }
        dry_run_via_simulation!();
    }

    #[test]
    fn macro_generated_dry_run_events_routes_through_success_events() {
        let block = FakeSimulated;
        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let events = block.dry_run_events(&trigger);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::GreetingDelivered);
        assert_eq!(events[0].payload["dry_run"], true);
    }
}
