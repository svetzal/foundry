use std::future::Future;
use std::pin::Pin;

use crate::event::{Event, EventType};
use crate::throttle::Throttle;

/// The result of executing a task block.
#[derive(Debug)]
pub struct TaskBlockResult {
    /// Events to emit downstream (subject to throttle).
    pub events: Vec<Event>,
    /// Whether the block's work succeeded.
    pub success: bool,
    /// Human-readable summary of what happened.
    pub summary: String,
}

/// Whether a task block performs mutations or only observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockKind {
    /// Observation only — reads state, runs scans, checks conditions.
    /// Always emits regardless of throttle.
    Observer,
    /// Mutates state — commits, pushes, releases, installs.
    /// Emission controlled by throttle.
    Mutator,
}

/// Trait for a reusable task block in the Foundry workflow engine.
///
/// Task blocks are the processing units of a workflow. Each block:
/// - Sinks on specific event types
/// - Performs work when triggered
/// - Emits events on completion (subject to throttle)
///
/// This trait is object-safe so the engine can hold `Box<dyn TaskBlock>`.
pub trait TaskBlock: Send + Sync {
    /// Human-readable name for this block (e.g., "Audit Release Tag").
    fn name(&self) -> &'static str;

    /// Whether this block is an observer or mutator.
    fn kind(&self) -> BlockKind;

    /// The event types this block sinks on.
    fn sinks_on(&self) -> &[EventType];

    /// Execute the block's work in response to a triggering event.
    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>;

    /// Whether this block should emit its output events given the current throttle.
    fn should_emit(&self, throttle: Throttle) -> bool {
        match self.kind() {
            BlockKind::Observer => true,
            BlockKind::Mutator => throttle.allows_mutation(),
        }
    }

    /// Whether this block should execute side effects given the current throttle.
    fn should_execute(&self, throttle: Throttle) -> bool {
        match self.kind() {
            BlockKind::Observer => true,
            BlockKind::Mutator => throttle.allows_side_effects(),
        }
    }
}
