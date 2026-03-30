use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::event::{Event, EventType};
use crate::throttle::Throttle;

/// Retry policy for a task block.
///
/// Controls how many times the engine will retry a failed execution and how
/// long to wait between attempts.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Maximum number of retries (0 = no retries, execute once only).
    pub max_retries: u32,
    /// Delay between retry attempts.
    pub backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 0,
            backoff: Duration::from_secs(0),
        }
    }
}

/// The result of executing a task block.
#[derive(Debug)]
pub struct TaskBlockResult {
    /// Events to emit downstream (subject to throttle).
    pub events: Vec<Event>,
    /// Whether the block's work succeeded.
    pub success: bool,
    /// Human-readable summary of what happened.
    pub summary: String,
    /// Combined stdout+stderr from any shell command run by this block.
    pub raw_output: Option<String>,
    /// Exit code from any shell command run by this block.
    pub exit_code: Option<i32>,
    /// Paths to audit artifacts produced by this block (e.g., audit logs).
    pub audit_artifacts: Vec<String>,
}

impl TaskBlockResult {
    pub fn success(summary: impl Into<String>, events: Vec<Event>) -> Self {
        Self {
            events,
            success: true,
            summary: summary.into(),
            raw_output: None,
            exit_code: None,
            audit_artifacts: vec![],
        }
    }

    pub fn failure(summary: impl Into<String>) -> Self {
        Self {
            events: vec![],
            success: false,
            summary: summary.into(),
            raw_output: None,
            exit_code: None,
            audit_artifacts: vec![],
        }
    }

    #[must_use]
    pub fn with_output(mut self, raw_output: Option<String>, exit_code: Option<i32>) -> Self {
        self.raw_output = raw_output;
        self.exit_code = exit_code;
        self
    }

    pub fn project_not_found(project: &str) -> Self {
        Self::failure(format!("Project '{project}' not found in registry"))
    }
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

    /// The retry policy for this block.
    ///
    /// Defaults to no retries (execute once). Override to enable automatic
    /// retry of transient failures.
    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy::default()
    }

    /// Whether this block's emitted events should be delivered to downstream
    /// subscribers (i.e., added to the processing queue).
    ///
    /// Events are **always** persisted to JSONL and broadcast to Watch clients
    /// regardless of this flag — they are facts. This method only controls
    /// whether they propagate through the event chain to trigger further blocks.
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

    /// Events this block would emit on success, used for dry-run simulation.
    ///
    /// When the engine runs in `DryRun` mode, Mutator blocks are not executed.
    /// Instead, the engine calls this method to obtain synthetic success events
    /// with `dry_run: true` in the payload. These events are persisted,
    /// broadcast, and delivered so the full chain shape is visible.
    ///
    /// Default: empty (block contributes no events in dry-run).
    /// Override for Mutator blocks that need to propagate the chain.
    fn dry_run_events(&self, _trigger: &Event) -> Vec<Event> {
        vec![]
    }
}
