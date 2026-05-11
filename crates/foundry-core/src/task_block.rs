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

    #[must_use]
    pub fn with_audit_artifacts(mut self, audit_artifacts: Vec<String>) -> Self {
        self.audit_artifacts = audit_artifacts;
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::event::EventType;
    use crate::throttle::Throttle;

    struct StubObserver;
    impl TaskBlock for StubObserver {
        fn name(&self) -> &'static str {
            "StubObserver"
        }
        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }
        fn sinks_on(&self) -> &[EventType] {
            &[]
        }
        fn execute(
            &self,
            _trigger: &Event,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>> {
            Box::pin(async { Ok(TaskBlockResult::success("ok", vec![])) })
        }
    }

    struct StubMutator;
    impl TaskBlock for StubMutator {
        fn name(&self) -> &'static str {
            "StubMutator"
        }
        fn kind(&self) -> BlockKind {
            BlockKind::Mutator
        }
        fn sinks_on(&self) -> &[EventType] {
            &[]
        }
        fn execute(
            &self,
            _trigger: &Event,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>> {
            Box::pin(async { Ok(TaskBlockResult::success("ok", vec![])) })
        }
    }

    fn make_event() -> Event {
        Event::new(
            EventType::GreetRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
    }

    #[test]
    fn retry_policy_default_has_no_retries() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_retries, 0);
        assert_eq!(policy.backoff, Duration::from_secs(0));
    }

    #[test]
    fn observer_should_emit_under_all_throttle_levels() {
        let block = StubObserver;
        assert!(block.should_emit(Throttle::Full));
        assert!(block.should_emit(Throttle::AuditOnly));
        assert!(block.should_emit(Throttle::DryRun));
    }

    #[test]
    fn mutator_should_emit_only_under_full_throttle() {
        let block = StubMutator;
        assert!(block.should_emit(Throttle::Full));
        assert!(!block.should_emit(Throttle::AuditOnly));
        assert!(!block.should_emit(Throttle::DryRun));
    }

    #[test]
    fn observer_should_execute_under_all_throttle_levels() {
        let block = StubObserver;
        assert!(block.should_execute(Throttle::Full));
        assert!(block.should_execute(Throttle::AuditOnly));
        assert!(block.should_execute(Throttle::DryRun));
    }

    #[test]
    fn mutator_should_not_execute_in_dry_run() {
        let block = StubMutator;
        assert!(block.should_execute(Throttle::Full));
        assert!(block.should_execute(Throttle::AuditOnly));
        assert!(!block.should_execute(Throttle::DryRun));
    }

    #[test]
    fn dry_run_events_default_is_empty() {
        let event = make_event();
        assert!(StubObserver.dry_run_events(&event).is_empty());
        assert!(StubMutator.dry_run_events(&event).is_empty());
    }

    #[test]
    fn task_block_result_success_fields() {
        let result = TaskBlockResult::success("all good", vec![]);
        assert!(result.success);
        assert_eq!(result.summary, "all good");
        assert!(result.events.is_empty());
        assert!(result.raw_output.is_none());
        assert!(result.exit_code.is_none());
        assert!(result.audit_artifacts.is_empty());
    }

    #[test]
    fn task_block_result_failure_fields() {
        let result = TaskBlockResult::failure("something broke");
        assert!(!result.success);
        assert_eq!(result.summary, "something broke");
        assert!(result.events.is_empty());
    }

    #[test]
    fn task_block_result_with_output_attaches_command_data() {
        let result = TaskBlockResult::success("ok", vec![])
            .with_output(Some("stdout text".to_string()), Some(0));
        assert_eq!(result.raw_output, Some("stdout text".to_string()));
        assert_eq!(result.exit_code, Some(0));
    }

    #[test]
    fn task_block_result_with_audit_artifacts() {
        let result = TaskBlockResult::success("ok", vec![])
            .with_audit_artifacts(vec!["/path/to/report.json".to_string()]);
        assert_eq!(result.audit_artifacts, vec!["/path/to/report.json"]);
    }

    #[test]
    fn project_not_found_result_mentions_project_name() {
        let result = TaskBlockResult::project_not_found("my-project");
        assert!(!result.success);
        assert!(result.summary.contains("my-project"));
    }
}
