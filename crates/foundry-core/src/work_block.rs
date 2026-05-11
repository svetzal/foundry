use std::future::Future;
use std::pin::Pin;

use crate::event::{Event, EventType};
use crate::task_block::{BlockKind, RetryPolicy, TaskBlock, TaskBlockResult};
use crate::throttle::Throttle;

/// Pure behavior with typed input and output. No knowledge of events.
///
/// A `WorkBlock` encapsulates reusable logic that can be composed into
/// different workflow steps via [`ComposedStep`]. Unlike [`TaskBlock`],
/// it has no awareness of event routing — adapters handle that translation.
pub trait WorkBlock: Send + Sync {
    /// The typed input this block accepts.
    type Input: Send;
    /// The typed output this block produces.
    type Output: Send;

    /// Human-readable name for this work block.
    fn name(&self) -> &'static str;

    /// Execute the block's work with typed input.
    fn execute(
        &self,
        input: Self::Input,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<Self::Output>> + Send + '_>>;
}

/// Transforms a trigger [`Event`] into a work block's typed input.
///
/// Returns `None` to skip execution — this replaces self-filtering patterns
/// like dirty=true/false checks in traditional `TaskBlock` implementations.
pub trait EventAdapter<I>: Send + Sync {
    fn adapt(&self, trigger: &Event) -> Option<I>;
}

/// Transforms a work block's typed output back into a [`TaskBlockResult`].
///
/// Also provides dry-run event synthesis for the composed step.
pub trait OutputMapper<O>: Send + Sync {
    /// Map a successful output into a `TaskBlockResult`.
    fn map(&self, output: O, trigger: &Event) -> TaskBlockResult;

    /// Map a block execution error into a `TaskBlockResult`.
    ///
    /// Default: produces a failure result with no events.
    fn map_error(&self, error: anyhow::Error, _trigger: &Event) -> TaskBlockResult {
        TaskBlockResult::failure(format!("Block execution failed: {error}"))
    }

    /// Events this composed step would emit on success during dry-run.
    ///
    /// Default: empty (no dry-run events).
    fn dry_run_events(&self, _trigger: &Event) -> Vec<Event> {
        vec![]
    }
}

/// Glues a [`WorkBlock`] + [`EventAdapter`] + [`OutputMapper`] together and
/// implements [`TaskBlock`].
///
/// The engine sees a normal `TaskBlock`; internally it's a composed pipeline:
/// 1. `adapter.adapt(trigger)` — if `None`, skip with a success result
/// 2. `block.execute(input).await`
/// 3. `mapper.map(output, trigger)` (or `mapper.map_error()` on failure)
pub struct ComposedStep<B, A, M> {
    pub name: &'static str,
    pub kind: BlockKind,
    pub sinks_on: Vec<EventType>,
    pub block: B,
    pub adapter: A,
    pub mapper: M,
    pub retry_policy: RetryPolicy,
}

impl<B, A, M> ComposedStep<B, A, M> {
    pub fn new(
        name: &'static str,
        kind: BlockKind,
        sinks_on: Vec<EventType>,
        block: B,
        adapter: A,
        mapper: M,
    ) -> Self {
        Self {
            name,
            kind,
            sinks_on,
            block,
            adapter,
            mapper,
            retry_policy: RetryPolicy::default(),
        }
    }

    #[must_use]
    pub fn with_retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.retry_policy = policy;
        self
    }
}

impl<B, A, M> TaskBlock for ComposedStep<B, A, M>
where
    B: WorkBlock,
    A: EventAdapter<B::Input>,
    M: OutputMapper<B::Output>,
{
    fn name(&self) -> &'static str {
        self.name
    }

    fn kind(&self) -> BlockKind {
        self.kind
    }

    fn sinks_on(&self) -> &[EventType] {
        &self.sinks_on
    }

    fn retry_policy(&self) -> RetryPolicy {
        self.retry_policy
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        self.mapper.dry_run_events(trigger)
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>> {
        let Some(input) = self.adapter.adapt(trigger) else {
            let name = self.name;
            return Box::pin(async move {
                Ok(TaskBlockResult::success(
                    format!("Skipped: {name} adapter filtered out trigger"),
                    vec![],
                ))
            });
        };

        // Capture trigger data needed by the mapper before entering the async block.
        // We clone the trigger so the mapper can use it after the await point.
        let trigger_clone = trigger.clone();

        Box::pin(async move {
            match self.block.execute(input).await {
                Ok(output) => Ok(self.mapper.map(output, &trigger_clone)),
                Err(err) => Ok(self.mapper.map_error(err, &trigger_clone)),
            }
        })
    }

    fn should_emit(&self, throttle: Throttle) -> bool {
        match self.kind {
            BlockKind::Observer => true,
            BlockKind::Mutator => throttle.allows_mutation(),
        }
    }

    fn should_execute(&self, throttle: Throttle) -> bool {
        match self.kind {
            BlockKind::Observer => true,
            BlockKind::Mutator => throttle.allows_side_effects(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::event::EventType;
    use crate::task_block::{RetryPolicy, TaskBlock};
    use crate::throttle::Throttle;

    fn make_trigger() -> Event {
        Event::new(
            EventType::GreetRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
    }

    struct EchoBlock;
    impl WorkBlock for EchoBlock {
        type Input = String;
        type Output = String;
        fn name(&self) -> &'static str {
            "Echo"
        }
        fn execute(
            &self,
            input: String,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<String>> + Send + '_>> {
            Box::pin(async move { Ok(input) })
        }
    }

    struct FailBlock;
    impl WorkBlock for FailBlock {
        type Input = ();
        type Output = ();
        fn name(&self) -> &'static str {
            "Fail"
        }
        fn execute(
            &self,
            _input: (),
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
            Box::pin(async { Err(anyhow::anyhow!("intentional failure")) })
        }
    }

    struct FixedAdapter(String);
    impl EventAdapter<String> for FixedAdapter {
        fn adapt(&self, _trigger: &Event) -> Option<String> {
            Some(self.0.clone())
        }
    }

    struct FilterAdapter;
    impl EventAdapter<String> for FilterAdapter {
        fn adapt(&self, _trigger: &Event) -> Option<String> {
            None
        }
    }

    struct UnitAdapter;
    impl EventAdapter<()> for UnitAdapter {
        fn adapt(&self, _trigger: &Event) -> Option<()> {
            Some(())
        }
    }

    struct EchoMapper;
    impl OutputMapper<String> for EchoMapper {
        fn map(&self, output: String, _trigger: &Event) -> TaskBlockResult {
            TaskBlockResult::success(output, vec![])
        }
    }

    struct UnitMapper;
    impl OutputMapper<()> for UnitMapper {
        fn map(&self, _output: (), _trigger: &Event) -> TaskBlockResult {
            TaskBlockResult::success("unit done", vec![])
        }
    }

    #[tokio::test]
    async fn adapter_returning_none_produces_skip_success() {
        let step = ComposedStep::new(
            "TestStep",
            BlockKind::Observer,
            vec![EventType::GreetRequested],
            EchoBlock,
            FilterAdapter,
            EchoMapper,
        );
        let result = step.execute(&make_trigger()).await.unwrap();
        assert!(result.success);
        assert!(result.summary.contains("Skipped"));
        assert!(result.summary.contains("TestStep"));
    }

    #[tokio::test]
    async fn adapter_returning_some_runs_work_block() {
        let step = ComposedStep::new(
            "TestStep",
            BlockKind::Observer,
            vec![EventType::GreetRequested],
            EchoBlock,
            FixedAdapter("hello".to_string()),
            EchoMapper,
        );
        let result = step.execute(&make_trigger()).await.unwrap();
        assert!(result.success);
        assert_eq!(result.summary, "hello");
    }

    #[tokio::test]
    async fn failing_work_block_calls_map_error() {
        let step = ComposedStep::new(
            "FailStep",
            BlockKind::Observer,
            vec![EventType::GreetRequested],
            FailBlock,
            UnitAdapter,
            UnitMapper,
        );
        let result = step.execute(&make_trigger()).await.unwrap();
        assert!(!result.success);
        assert!(result.summary.contains("intentional failure"));
    }

    #[test]
    fn with_retry_policy_overrides_default() {
        let step = ComposedStep::new(
            "RetryStep",
            BlockKind::Observer,
            vec![],
            EchoBlock,
            FixedAdapter("x".to_string()),
            EchoMapper,
        )
        .with_retry_policy(RetryPolicy {
            max_retries: 3,
            backoff: Duration::from_millis(500),
        });
        assert_eq!(TaskBlock::retry_policy(&step).max_retries, 3);
    }

    #[test]
    fn dry_run_events_delegates_to_mapper() {
        let step = ComposedStep::new(
            "DryStep",
            BlockKind::Mutator,
            vec![],
            EchoBlock,
            FixedAdapter("x".to_string()),
            EchoMapper,
        );
        assert!(TaskBlock::dry_run_events(&step, &make_trigger()).is_empty());
    }

    #[test]
    fn observer_step_emits_under_all_throttles() {
        let step = ComposedStep::new(
            "ObsStep",
            BlockKind::Observer,
            vec![],
            EchoBlock,
            FixedAdapter("x".to_string()),
            EchoMapper,
        );
        assert!(TaskBlock::should_emit(&step, Throttle::Full));
        assert!(TaskBlock::should_emit(&step, Throttle::AuditOnly));
        assert!(TaskBlock::should_emit(&step, Throttle::DryRun));
    }

    #[test]
    fn mutator_step_emits_only_under_full_throttle() {
        let step = ComposedStep::new(
            "MutStep",
            BlockKind::Mutator,
            vec![],
            EchoBlock,
            FixedAdapter("x".to_string()),
            EchoMapper,
        );
        assert!(TaskBlock::should_emit(&step, Throttle::Full));
        assert!(!TaskBlock::should_emit(&step, Throttle::AuditOnly));
        assert!(!TaskBlock::should_emit(&step, Throttle::DryRun));
    }
}
