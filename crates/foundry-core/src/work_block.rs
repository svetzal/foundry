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
