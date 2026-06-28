use foundry_sdk::task_block::{BlockKind, TaskBlock};
use foundry_sdk::throttle::Throttle;
pub use foundry_sdk::trace::BlockExecution;

use foundry_sdk::event::Event;

#[derive(Debug, PartialEq)]
pub enum Dispatch {
    DryRunSimulate,
    ThrottleSkip,
    Execute,
}

/// Classify how the engine should handle a block invocation based on its kind,
/// the event throttle, and whether the block says it should execute.
pub fn classify_dispatch(kind: BlockKind, throttle: Throttle, should_execute: bool) -> Dispatch {
    if !should_execute && throttle == Throttle::DryRun && kind == BlockKind::Mutator {
        Dispatch::DryRunSimulate
    } else if !should_execute {
        Dispatch::ThrottleSkip
    } else {
        Dispatch::Execute
    }
}

pub fn make_block_execution(
    block: &dyn TaskBlock,
    current: &Event,
    block_span_id: String,
    workflow_span_id: Option<String>,
    block_start: std::time::Instant,
    success: bool,
    summary: String,
) -> BlockExecution {
    let duration_ms = u64::try_from(block_start.elapsed().as_millis()).unwrap_or(u64::MAX);
    BlockExecution {
        success,
        summary,
        span_id: Some(block_span_id),
        parent_span_id: workflow_span_id,
        ..BlockExecution::new(block.name(), &current.id, duration_ms, current.payload.clone())
    }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::task_block::BlockKind;
    use foundry_sdk::throttle::Throttle;

    use super::{Dispatch, classify_dispatch};

    #[test]
    fn classify_dispatch_dry_run_mutator_simulates() {
        assert_eq!(
            classify_dispatch(BlockKind::Mutator, Throttle::DryRun, false),
            Dispatch::DryRunSimulate
        );
    }

    #[test]
    fn classify_dispatch_observer_dry_run_skips() {
        assert_eq!(
            classify_dispatch(BlockKind::Observer, Throttle::DryRun, false),
            Dispatch::ThrottleSkip
        );
    }

    #[test]
    fn classify_dispatch_throttle_skip_when_should_not_execute() {
        assert_eq!(
            classify_dispatch(BlockKind::Mutator, Throttle::Full, false),
            Dispatch::ThrottleSkip
        );
    }

    #[test]
    fn classify_dispatch_executes_when_should_execute() {
        assert_eq!(classify_dispatch(BlockKind::Mutator, Throttle::Full, true), Dispatch::Execute);
        assert_eq!(
            classify_dispatch(BlockKind::Observer, Throttle::DryRun, true),
            Dispatch::Execute
        );
    }
}
