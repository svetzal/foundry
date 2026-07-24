use std::sync::Arc;
use std::time::Instant;

use tokio::sync::broadcast;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::task_block::TaskBlock;
pub use foundry_sdk::trace::{BlockExecution, ProcessResult};

use crate::dispatch::{Dispatch, classify_dispatch, make_block_execution};
use crate::emit::EventEmitter;
use crate::event_writer::EventWriter;
use crate::propagation::{ProcessState, Propagator};
use crate::retry::execute_with_retry;
use crate::tracing_context::SpanStamper;

/// The workflow engine routes events to task blocks and manages propagation.
pub struct Engine {
    blocks: Vec<Box<dyn TaskBlock>>,
    emitter: EventEmitter,
    stamper: SpanStamper,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            blocks: vec![],
            emitter: EventEmitter::new(),
            stamper: SpanStamper::new(),
        }
    }

    /// Register additional event types that open a new trace span when emitted.
    ///
    /// Built-in openers (see [`EventType::is_span_opener`]) are always
    /// recognized; this is how a contributor-defined workflow declares that its
    /// own root event (typically an [`EventType::Custom`] `*_requested`) should
    /// open a span, so the workflow shows up as a proper sub-tree in traces.
    #[must_use]
    pub fn with_span_openers(mut self, openers: impl IntoIterator<Item = EventType>) -> Self {
        self.stamper = self.stamper.with_openers(openers);
        self
    }

    /// Whether an emitted event of this type should open a new workflow span —
    /// either a built-in opener or one registered via [`Engine::with_span_openers`].
    #[allow(dead_code)] // called from test assertions only
    fn opens_span(&self, event_type: &EventType) -> bool {
        self.stamper.opens_span(event_type)
    }

    /// Attach an `EventWriter` so every event in a processing chain is
    /// persisted to JSONL as it is produced.  Write failures are logged but
    /// never interrupt event processing.
    #[must_use]
    pub fn with_event_writer(mut self, writer: Arc<EventWriter>) -> Self {
        self.emitter = self.emitter.with_writer(writer);
        self
    }

    /// Attach a broadcast sender so events are pushed to Watch subscribers in real time.
    #[must_use]
    pub fn with_event_broadcaster(mut self, tx: broadcast::Sender<Event>) -> Self {
        self.emitter = self.emitter.with_broadcaster(tx);
        self
    }

    /// Register a task block with the engine.
    pub fn register(&mut self, block: Box<dyn TaskBlock>) {
        tracing::info!(block = block.name(), sinks = ?block.sinks_on(), "registered task block");
        self.blocks.push(block);
    }

    /// Run the `Execute` path: retry the block, emit events, handle scatter, and
    /// return the resulting [`BlockExecution`].  Extracted from [`Engine::run_block`] to
    /// keep that method within the line budget.
    async fn execute_block(
        &self,
        block: &dyn TaskBlock,
        current: &Event,
        block_span_id: String,
        workflow_span_id: Option<String>,
        block_start: std::time::Instant,
        state: &mut ProcessState,
    ) -> BlockExecution {
        let propagator = Propagator::new(&self.emitter, &self.stamper);
        // Scope SPAN_CONTEXT so subprocess spawners inside the block can inject TRACEPARENT.
        let exec_future = execute_with_retry(block, current, block.retry_policy());
        let result_or_err = if let Some(trace_id) = current.trace_id.clone() {
            let span_ctx = foundry_sdk::span_context::SpanContext {
                trace_id,
                span_id: block_span_id.clone(),
            };
            foundry_sdk::span_context::SPAN_CONTEXT.scope(span_ctx, exec_future).await
        } else {
            exec_future.await
        };

        match result_or_err {
            Ok(result) => {
                tracing::info!(
                    success = result.success,
                    summary = %result.summary,
                    emitted = result.events.len(),
                    "completed"
                );
                let deliver = block.should_emit(current.throttle);
                let (mut emitted_ids, emitted_payloads) = propagator.persist_and_broadcast_events(
                    result.events,
                    current,
                    &block_span_id,
                    state,
                    deliver,
                );
                if let Some(scatter) = result.scatter {
                    let child_ids = propagator.dispatch_scatter(
                        *scatter,
                        current,
                        &block_span_id,
                        state,
                        deliver,
                    );
                    emitted_ids.extend(child_ids);
                }
                let mut exec = make_block_execution(
                    block,
                    current,
                    block_span_id,
                    workflow_span_id,
                    block_start,
                    result.success,
                    result.summary,
                );
                exec.emitted_event_ids = emitted_ids;
                exec.raw_output = result.raw_output;
                exec.exit_code = result.exit_code;
                exec.emitted_payloads = emitted_payloads;
                exec.audit_artifacts = result.audit_artifacts;
                exec
            }
            Err(err) => {
                tracing::error!(error = %err, "failed");
                make_block_execution(
                    block,
                    current,
                    block_span_id,
                    workflow_span_id,
                    block_start,
                    false,
                    format!("error: {err}"),
                )
            }
        }
    }

    /// Execute one block against a triggering event, persist any emitted events,
    /// and return the [`BlockExecution`] record.  Mutates `state` in place so
    /// downstream events continue to be processed.
    async fn run_block(
        &self,
        block: &dyn TaskBlock,
        current: &Event,
        state: &mut ProcessState,
    ) -> BlockExecution {
        let block_span_id = foundry_sdk::event::mint_span_id();
        let workflow_span_id = current.span_id.clone();
        let block_start = Instant::now();

        self.emitter.record_block_started(block, current);

        let propagator = Propagator::new(&self.emitter, &self.stamper);
        let execution = match classify_dispatch(
            block.kind(),
            current.throttle,
            block.should_execute(current.throttle),
        ) {
            Dispatch::DryRunSimulate => {
                let simulated_events = block.dry_run_events(current);
                tracing::info!(simulated = simulated_events.len(), "dry-run: simulating success");
                let (emitted_ids, emitted_payloads) = propagator.persist_and_broadcast_events(
                    simulated_events,
                    current,
                    &block_span_id,
                    state,
                    true,
                );
                let mut exec = make_block_execution(
                    block,
                    current,
                    block_span_id,
                    workflow_span_id,
                    block_start,
                    true,
                    "dry-run (simulated)".to_string(),
                );
                exec.emitted_event_ids = emitted_ids;
                exec.emitted_payloads = emitted_payloads;
                exec
            }
            Dispatch::ThrottleSkip => {
                // Non-DryRun throttle skip — should not happen in practice, kept as a safety net.
                tracing::info!("skipped (throttle)");
                make_block_execution(
                    block,
                    current,
                    block_span_id,
                    workflow_span_id,
                    block_start,
                    true,
                    "skipped (throttle)".to_string(),
                )
            }
            Dispatch::Execute => {
                self.execute_block(
                    block,
                    current,
                    block_span_id,
                    workflow_span_id,
                    block_start,
                    state,
                )
                .await
            }
        };

        self.emitter.record_block_completed(&execution, current);
        execution
    }

    /// Process an event: find matching task blocks, execute them, and propagate
    /// any emitted events through the chain.
    pub async fn process(&self, event: Event) -> ProcessResult {
        let process_start = std::time::Instant::now();

        let process_span = tracing::info_span!(
            "process",
            root_event_id = %event.id,
            root_event_type = %event.event_type,
        );
        let _process_guard = process_span.enter();

        // Persist the root event before processing begins so it is recorded
        // even if a downstream block panics.
        self.emitter.persist_one(&event);

        let mut block_executions = Vec::new();
        let mut state = ProcessState::new(event);

        while let Some(current) = state.queue.pop() {
            let matching: Vec<&dyn TaskBlock> = self
                .blocks
                .iter()
                .filter(|b| b.sinks_on().contains(&current.event_type) && b.accepts(&current))
                .map(std::convert::AsRef::as_ref)
                .collect();

            for block in matching {
                let block_span = tracing::info_span!(
                    "block",
                    block = block.name(),
                    trigger_event = %current.event_type,
                    trigger_id = %current.id,
                    throttle = %current.throttle,
                );
                let _block_guard = block_span.enter();
                tracing::info!("executing");

                let execution = self.run_block(block, &current, &mut state).await;
                block_executions.push(execution);
            }
        }

        ProcessResult {
            events: state.all_events,
            block_executions,
            total_duration_ms: u64::try_from(process_start.elapsed().as_millis())
                .unwrap_or(u64::MAX),
        }
    }

    /// List registered block names and what they sink on.
    #[allow(dead_code)]
    pub fn list_blocks(&self) -> Vec<(&str, &[EventType])> {
        self.blocks.iter().map(|b| (b.name(), b.sinks_on())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_sdk::scatter::Scatter;
    use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
    use foundry_sdk::throttle::Throttle;
    use std::pin::Pin;

    /// A block that sinks on the right event type but always rejects via `accepts()`.
    struct RejectingObserver;

    impl TaskBlock for RejectingObserver {
        fn name(&self) -> &'static str {
            "Rejecting Observer"
        }

        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }

        fn sinks_on(&self) -> &[EventType] {
            &[EventType::GreetingRequested]
        }

        fn accepts(&self, _trigger: &Event) -> bool {
            false
        }

        fn execute(
            &self,
            _trigger: &Event,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
        {
            Box::pin(async { Ok(TaskBlockResult::success("should not be called", vec![])) })
        }
    }

    #[tokio::test]
    async fn accepts_false_blocks_no_executions_and_no_events() {
        let mut engine = Engine::new();
        engine.register(Box::new(RejectingObserver));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = engine.process(trigger).await;

        // Block sinks on GreetingRequested but accepts() returns false — never executes.
        assert_eq!(result.block_executions.len(), 0, "block should not be executed");
        // Only the root event; the rejecting block produces no downstream events.
        assert_eq!(result.events.len(), 1, "only the root event should be present");
    }

    #[test]
    fn opens_span_recognizes_builtin_and_registered_custom_openers() {
        let custom = EventType::Custom("my_workflow_requested".to_string());
        let engine = Engine::new().with_span_openers([custom.clone()]);

        // Built-in openers remain recognized.
        assert!(engine.opens_span(&EventType::ProjectIterationRequested));
        // A contributor-registered custom opener is recognized.
        assert!(engine.opens_span(&custom));
        // An unregistered custom event does not open a span.
        assert!(!engine.opens_span(&EventType::Custom("unregistered_event".to_string())));
        // A default engine has no custom openers (only built-ins).
        assert!(!Engine::new().opens_span(&custom));
    }

    struct TestObserver;

    impl TaskBlock for TestObserver {
        fn name(&self) -> &'static str {
            "Test Observer"
        }

        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }

        fn sinks_on(&self) -> &[EventType] {
            &[EventType::GreetingRequested]
        }

        fn execute(
            &self,
            trigger: &Event,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
        {
            let project = trigger.project.clone();
            let throttle = trigger.throttle;
            Box::pin(async move {
                Ok(TaskBlockResult {
                    events: vec![Event::new(
                        EventType::GreetingComposed,
                        project,
                        throttle,
                        serde_json::json!({"greeting": "hello"}),
                    )],
                    success: true,
                    summary: "composed greeting".to_string(),
                    ..Default::default()
                })
            })
        }
    }

    struct TestMutator;

    impl TaskBlock for TestMutator {
        fn name(&self) -> &'static str {
            "Test Mutator"
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
            Box::pin(async move {
                Ok(TaskBlockResult {
                    events: vec![Event::new(
                        EventType::GreetingDelivered,
                        project,
                        throttle,
                        serde_json::json!({"delivered": true}),
                    )],
                    success: true,
                    summary: "delivered greeting".to_string(),
                    ..Default::default()
                })
            })
        }

        fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
            vec![Event::new(
                EventType::GreetingDelivered,
                trigger.project.clone(),
                trigger.throttle,
                serde_json::json!({"delivered": true, "dry_run": true}),
            )]
        }
    }

    #[tokio::test]
    async fn full_throttle_propagates_through_chain() {
        let mut engine = Engine::new();
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({"name": "world"}),
        );

        let result = engine.process(trigger).await;

        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            types,
            [
                "greeting_requested",
                "greeting_composed",
                "greeting_delivered"
            ]
        );
        assert_eq!(result.block_executions.len(), 2);
        assert!(result.block_executions.iter().all(|b| b.success));
    }

    #[tokio::test]
    async fn streams_block_progress_without_routing_it() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let mut engine = Engine::new().with_event_broadcaster(tx);
        engine.register(Box::new(TestObserver));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({"name": "world"}),
        );

        let result = engine.process(trigger).await;

        let mut streamed = Vec::new();
        while let Ok(event) = rx.try_recv() {
            streamed.push(event);
        }

        let streamed_types: Vec<String> = streamed.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            streamed_types,
            [
                "greeting_requested",
                "block_started",
                "greeting_composed",
                "block_completed",
            ]
        );

        let result_types: Vec<String> =
            result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(result_types, ["greeting_requested", "greeting_composed"]);

        let started = streamed
            .iter()
            .find(|e| e.event_type.as_str() == "block_started")
            .expect("block_started is streamed");
        assert_eq!(started.payload["block"], "Test Observer");
        assert_eq!(started.payload["status"], "running");

        let completed = streamed
            .iter()
            .find(|e| e.event_type.as_str() == "block_completed")
            .expect("block_completed is streamed");
        assert_eq!(completed.payload["block"], "Test Observer");
        assert_eq!(completed.payload["status"], "ok");
        assert_eq!(completed.payload["success"], true);
    }

    #[tokio::test]
    async fn dry_run_simulates_mutator_success() {
        let mut engine = Engine::new();
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::DryRun,
            serde_json::json!({"name": "world"}),
        );

        let result = engine.process(trigger).await;

        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        // Full chain completes: Observer emits, Mutator simulates success via dry_run_events.
        assert_eq!(
            types,
            [
                "greeting_requested",
                "greeting_composed",
                "greeting_delivered"
            ]
        );
        // Mutator's execution is recorded as simulated.
        let mutator_exec =
            result.block_executions.iter().find(|b| b.block_name == "Test Mutator").unwrap();
        assert!(mutator_exec.success);
        assert_eq!(mutator_exec.summary, "dry-run (simulated)");
        // The emitted event carries dry_run: true in its payload.
        let delivered = result
            .events
            .iter()
            .find(|e| e.event_type == EventType::GreetingDelivered)
            .unwrap();
        assert_eq!(delivered.payload["dry_run"], true);
    }

    // -- Retry logic tests --

    use foundry_sdk::task_block::RetryPolicy;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// A block that fails the first N calls then succeeds.
    struct FailFirstN {
        remaining_failures: Arc<AtomicU32>,
        policy: RetryPolicy,
    }

    impl FailFirstN {
        fn new(failures: u32, policy: RetryPolicy) -> Self {
            Self {
                remaining_failures: Arc::new(AtomicU32::new(failures)),
                policy,
            }
        }
    }

    impl TaskBlock for FailFirstN {
        fn name(&self) -> &'static str {
            "FailFirstN"
        }

        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }

        fn sinks_on(&self) -> &[EventType] {
            &[EventType::GreetingRequested]
        }

        fn retry_policy(&self) -> RetryPolicy {
            self.policy
        }

        fn execute(
            &self,
            trigger: &Event,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
        {
            let remaining = self.remaining_failures.clone();
            let project = trigger.project.clone();
            let throttle = trigger.throttle;
            Box::pin(async move {
                let prev = remaining.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                    if v > 0 { Some(v - 1) } else { None }
                });
                if prev.is_ok() {
                    Ok(TaskBlockResult {
                        summary: "transient failure".to_string(),
                        ..Default::default()
                    })
                } else {
                    Ok(TaskBlockResult {
                        events: vec![Event::new(
                            EventType::GreetingComposed,
                            project,
                            throttle,
                            serde_json::json!({}),
                        )],
                        success: true,
                        summary: "succeeded".to_string(),
                        ..Default::default()
                    })
                }
            })
        }
    }

    /// A block that always returns an `Err` from `execute()`.
    struct AlwaysErrors {
        policy: RetryPolicy,
        call_count: Arc<AtomicU32>,
    }

    impl TaskBlock for AlwaysErrors {
        fn name(&self) -> &'static str {
            "AlwaysErrors"
        }

        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }

        fn sinks_on(&self) -> &[EventType] {
            &[EventType::GreetingRequested]
        }

        fn retry_policy(&self) -> RetryPolicy {
            self.policy
        }

        fn execute(
            &self,
            _trigger: &Event,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
        {
            let count = self.call_count.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::anyhow!("system error"))
            })
        }
    }

    #[tokio::test]
    async fn retry_policy_default_is_zero_retries() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_retries, 0);
        assert_eq!(policy.backoff, Duration::from_secs(0));
    }

    #[tokio::test]
    async fn block_succeeds_first_try_no_retry_needed() {
        let mut engine = Engine::new();
        engine.register(Box::new(FailFirstN::new(
            0,
            RetryPolicy {
                max_retries: 3,
                backoff: Duration::from_millis(0),
            },
        )));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let result = engine.process(trigger).await;

        assert_eq!(result.block_executions.len(), 1);
        assert!(result.block_executions[0].success);
    }

    // -- EventWriter integration tests --

    #[tokio::test]
    async fn engine_with_event_writer_persists_all_events() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(EventWriter::new(tmp.path()));

        let mut engine = Engine::new().with_event_writer(Arc::clone(&writer));
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({"name": "world"}),
        );

        let result = engine.process(trigger).await;

        // Verify all three events were returned in process result.
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            types,
            [
                "greeting_requested",
                "greeting_composed",
                "greeting_delivered"
            ]
        );

        // Verify domain events and non-routable block audit observations were
        // all persisted.
        let entries: Vec<_> =
            std::fs::read_dir(tmp.path()).unwrap().filter_map(Result::ok).collect();
        assert_eq!(entries.len(), 1, "exactly one JSONL file should exist");

        let contents = std::fs::read_to_string(entries[0].path()).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 7, "JSONL file should contain 7 events");

        // Each line should deserialize to a valid Event with the expected type.
        let written_types: Vec<String> = lines
            .iter()
            .map(|l| {
                let e: foundry_sdk::event::Event = serde_json::from_str(l).unwrap();
                e.event_type.as_str()
            })
            .collect();
        assert_eq!(
            written_types,
            [
                "greeting_requested",
                "block_started",
                "greeting_composed",
                "block_completed",
                "block_started",
                "greeting_delivered",
                "block_completed"
            ]
        );
    }

    #[tokio::test]
    async fn engine_without_event_writer_still_works() {
        // Confirm backward compatibility: Engine::new() with no writer configured
        // processes events and returns results normally.
        let mut engine = Engine::new();
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({"name": "world"}),
        );

        let result = engine.process(trigger).await;

        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(
            types,
            [
                "greeting_requested",
                "greeting_composed",
                "greeting_delivered"
            ]
        );
        assert_eq!(result.block_executions.len(), 2);
        assert!(result.block_executions.iter().all(|b| b.success));
    }

    #[tokio::test]
    async fn engine_with_event_writer_persists_root_event_even_with_no_matching_blocks() {
        // Root event must be written even when no blocks fire.
        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(EventWriter::new(tmp.path()));

        let engine = Engine::new().with_event_writer(Arc::clone(&writer));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let result = engine.process(trigger).await;

        // No blocks registered — root event still written to JSONL
        assert_eq!(result.block_executions.len(), 0);
        let entries: Vec<_> =
            std::fs::read_dir(tmp.path()).unwrap().filter_map(Result::ok).collect();
        assert_eq!(entries.len(), 1, "expected one JSONL file");
        let contents = std::fs::read_to_string(entries[0].path()).unwrap();
        assert_eq!(contents.lines().count(), 1, "expected one event line");
    }

    #[tokio::test]
    async fn block_succeeds_on_retry_after_transient_failure() {
        let mut engine = Engine::new();
        // Fails twice then succeeds; policy allows 3 retries — should recover.
        engine.register(Box::new(FailFirstN::new(
            2,
            RetryPolicy {
                max_retries: 3,
                backoff: Duration::from_millis(0),
            },
        )));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let result = engine.process(trigger).await;

        assert_eq!(result.block_executions.len(), 1);
        assert!(result.block_executions[0].success);
        // The recovered execution emits an event.
        assert_eq!(result.block_executions[0].emitted_event_ids.len(), 1);
    }

    #[tokio::test]
    async fn block_exhausts_retries_records_final_failure() {
        let mut engine = Engine::new();
        // Fails 5 times but policy only allows 2 retries (3 total attempts).
        engine.register(Box::new(FailFirstN::new(
            5,
            RetryPolicy {
                max_retries: 2,
                backoff: Duration::from_millis(0),
            },
        )));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let result = engine.process(trigger).await;

        assert_eq!(result.block_executions.len(), 1);
        assert!(!result.block_executions[0].success);
        assert_eq!(result.block_executions[0].summary, "transient failure");
    }

    #[tokio::test]
    async fn block_with_no_retry_policy_fails_immediately() {
        let mut engine = Engine::new();
        // 1 failure, but default policy = 0 retries → fails on the only attempt.
        engine.register(Box::new(FailFirstN::new(1, RetryPolicy::default())));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let result = engine.process(trigger).await;

        assert_eq!(result.block_executions.len(), 1);
        assert!(!result.block_executions[0].success);
    }

    #[tokio::test]
    async fn err_result_retried_and_exhausted_records_failure() {
        let call_count = Arc::new(AtomicU32::new(0));
        let block = AlwaysErrors {
            policy: RetryPolicy {
                max_retries: 2,
                backoff: Duration::from_millis(0),
            },
            call_count: call_count.clone(),
        };

        let mut engine = Engine::new();
        engine.register(Box::new(block));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let result = engine.process(trigger).await;

        // 1 initial attempt + 2 retries = 3 total calls.
        assert_eq!(call_count.load(Ordering::SeqCst), 3);
        assert_eq!(result.block_executions.len(), 1);
        assert!(!result.block_executions[0].success);
        assert!(result.block_executions[0].summary.contains("error:"));
    }

    // -- Trace ID propagation tests --

    #[tokio::test]
    async fn trace_id_propagates_through_chain() {
        let mut engine = Engine::new();
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let workflow_span = foundry_sdk::event::mint_span_id();
        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some("trc_test123".to_string()))
        .with_span_ids(Some(workflow_span.clone()), None);

        let result = engine.process(trigger).await;

        // All events in the chain should share the same trace_id and carry a span_id.
        for event in &result.events {
            assert_eq!(
                event.trace_id.as_deref(),
                Some("trc_test123"),
                "event {} should have propagated trace_id",
                event.event_type,
            );
            assert!(
                event.span_id.is_some(),
                "event {} should have a span_id after stamping",
                event.event_type,
            );
        }

        // GreetingRequested is a span opener — the trigger itself keeps its span,
        // and the emitted GreetingComposed (non-opener) inherits that span as
        // a peer.
        let composed = result
            .events
            .iter()
            .find(|e| e.event_type == EventType::GreetingComposed)
            .unwrap();
        assert_eq!(
            composed.span_id.as_ref(),
            Some(&workflow_span),
            "non-opener child should share the trigger's workflow span",
        );
    }

    #[tokio::test]
    async fn none_trace_id_propagates_as_none() {
        let mut engine = Engine::new();
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        // trace_id is None by default

        let result = engine.process(trigger).await;

        for event in &result.events {
            assert!(
                event.trace_id.is_none(),
                "event {} should have no trace_id when root has none",
                event.event_type,
            );
        }
    }

    #[tokio::test]
    async fn causation_id_stamps_direct_causal_parent_through_chain() {
        let mut engine = Engine::new();
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = engine.process(trigger).await;

        let find = |t: EventType| result.events.iter().find(|e| e.event_type == t).unwrap();
        let requested = find(EventType::GreetingRequested);
        let composed = find(EventType::GreetingComposed);
        let delivered = find(EventType::GreetingDelivered);

        // The root event has no causal parent.
        assert!(requested.causation_id.is_none(), "root event has no causation_id");
        // Each emitted event points at the event that triggered its emitting block.
        assert_eq!(
            composed.causation_id.as_ref(),
            Some(&requested.id),
            "composed was caused by the requested event",
        );
        assert_eq!(
            delivered.causation_id.as_ref(),
            Some(&composed.id),
            "delivered was caused by the composed event",
        );
    }

    #[tokio::test]
    async fn causation_id_preserves_block_provided_value() {
        // Contract: stamping is "set if unset". A block that emits an event
        // with an explicit causation_id keeps it.
        let explicit = "evt_explicit_parent".to_string();
        let block = ExplicitlyStampedBlock {
            name: "B",
            sinks: vec![EventType::VulnerabilityDetected],
            emit_type: EventType::ProjectChangesPushed,
            trace_id: foundry_sdk::event::mint_trace_id(),
            span_id: foundry_sdk::event::mint_span_id(),
            parent_span_id: foundry_sdk::event::mint_span_id(),
            causation_id: Some(explicit.clone()),
        };

        let trigger = Event::new(
            EventType::VulnerabilityDetected,
            "p".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let mut engine = Engine::new();
        engine.register(Box::new(block));
        let result = engine.process(trigger).await;

        let emitted = result
            .events
            .iter()
            .find(|e| e.event_type == EventType::ProjectChangesPushed)
            .unwrap();
        assert_eq!(
            emitted.causation_id.as_deref(),
            Some(explicit.as_str()),
            "block-provided causation_id must NOT be overwritten",
        );
    }

    #[tokio::test]
    async fn gather_id_propagates_verbatim_through_chain() {
        let mut engine = Engine::new();
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_gather_id(Some("gth_group1".to_string()));

        let result = engine.process(trigger).await;

        // Every event in the chain carries the same gather_id verbatim —
        // unlike causation_id, it is not rewritten at each hop.
        for event in &result.events {
            assert_eq!(
                event.gather_id.as_deref(),
                Some("gth_group1"),
                "event {} should carry the inherited gather_id",
                event.event_type,
            );
        }
    }

    #[tokio::test]
    async fn gather_id_survives_span_opener_boundary() {
        // A scattered child workflow opens with a span opener. The gather_id
        // must cross that boundary so the child's terminal events still
        // identify their fan-out group.
        let trigger = Event::new(
            EventType::PipelineChecked,
            "p".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
        .with_span_ids(Some(foundry_sdk::event::mint_span_id()), None)
        .with_gather_id(Some("gth_outer".to_string()));

        let block = emitting_block(
            "B",
            EventType::PipelineChecked,
            vec![EventType::ProjectIterationRequested],
        );
        let mut engine = Engine::new();
        engine.register(Box::new(block));
        let result = engine.process(trigger).await;

        let opener = result
            .events
            .iter()
            .find(|e| e.event_type == EventType::ProjectIterationRequested)
            .expect("opener must be emitted");
        // The opener gets a fresh span_id but keeps the gather_id verbatim.
        assert_eq!(
            opener.gather_id.as_deref(),
            Some("gth_outer"),
            "gather_id must propagate across a span-opener boundary",
        );
    }

    #[tokio::test]
    async fn gather_id_none_propagates_as_none() {
        let mut engine = Engine::new();
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = engine.process(trigger).await;

        for event in &result.events {
            assert!(
                event.gather_id.is_none(),
                "event {} should have no gather_id when root has none",
                event.event_type,
            );
        }
    }

    // -- Scatter/gather integration tests --

    /// Test block: scatters `child_count` children of a fixed type, gathering
    /// their completions (`on`) into a synthesized reduce event.
    struct ScatterBlock {
        name: &'static str,
        sinks: Vec<EventType>,
        child_type: EventType,
        child_count: usize,
        on: Vec<EventType>,
        reduce_event_type: EventType,
        reduce_project: &'static str,
    }

    impl TaskBlock for ScatterBlock {
        fn name(&self) -> &'static str {
            self.name
        }

        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }

        fn sinks_on(&self) -> &[EventType] {
            &self.sinks
        }

        fn execute(
            &self,
            trigger: &Event,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
        {
            let throttle = trigger.throttle;
            let child_type = self.child_type.clone();
            let count = self.child_count;
            let on = self.on.clone();
            let reduce_event_type = self.reduce_event_type.clone();
            let reduce_project = self.reduce_project.to_string();
            Box::pin(async move {
                let children: Vec<Event> = (0..count)
                    .map(|i| {
                        Event::new(
                            child_type.clone(),
                            format!("child-{i}"),
                            throttle,
                            serde_json::json!({}),
                        )
                    })
                    .collect();
                let scatter = Scatter::all(children, on, reduce_event_type, reduce_project);
                Ok(TaskBlockResult::scattering("scattered", scatter))
            })
        }
    }

    /// Test block: turns each `ProjectRunStarted` child into a successful
    /// `ProjectRunCompleted` completion in the same project.
    struct ChildWorker;

    impl TaskBlock for ChildWorker {
        fn name(&self) -> &'static str {
            "ChildWorker"
        }

        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }

        fn sinks_on(&self) -> &[EventType] {
            &[EventType::ProjectRunStarted]
        }

        fn execute(
            &self,
            trigger: &Event,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
        {
            let project = trigger.project.clone();
            let throttle = trigger.throttle;
            Box::pin(async move {
                Ok(TaskBlockResult::success(
                    "child completed",
                    vec![Event::new(
                        EventType::ProjectRunCompleted,
                        project,
                        throttle,
                        serde_json::json!({ "success": true }),
                    )],
                ))
            })
        }
    }

    /// Test block: records every reduce event it sinks on, for assertions.
    struct ReduceObserver {
        sinks: Vec<EventType>,
        seen: Arc<std::sync::Mutex<Vec<Event>>>,
    }

    impl TaskBlock for ReduceObserver {
        fn name(&self) -> &'static str {
            "ReduceObserver"
        }

        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }

        fn sinks_on(&self) -> &[EventType] {
            &self.sinks
        }

        fn execute(
            &self,
            trigger: &Event,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
        {
            let seen = Arc::clone(&self.seen);
            let event = trigger.clone();
            Box::pin(async move {
                seen.lock().unwrap().push(event);
                Ok(TaskBlockResult::success("observed reduce", vec![]))
            })
        }
    }

    fn type_count(result: &ProcessResult, ty: &EventType) -> usize {
        result.events.iter().filter(|e| &e.event_type == ty).count()
    }

    #[tokio::test]
    async fn scatter_gathers_children_into_a_reduce_event() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut engine = Engine::new();
        engine.register(Box::new(ScatterBlock {
            name: "Scatterer",
            sinks: vec![EventType::GreetingRequested],
            child_type: EventType::ProjectRunStarted,
            child_count: 3,
            on: vec![EventType::ProjectRunCompleted],
            reduce_event_type: EventType::MaintenanceCycleCompleted,
            reduce_project: "system",
        }));
        engine.register(Box::new(ChildWorker));
        engine.register(Box::new(ReduceObserver {
            sinks: vec![EventType::MaintenanceCycleCompleted],
            seen: Arc::clone(&seen),
        }));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "p".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let result = engine.process(trigger).await;

        assert_eq!(type_count(&result, &EventType::ProjectRunStarted), 3);
        assert_eq!(type_count(&result, &EventType::ProjectRunCompleted), 3);
        assert_eq!(
            type_count(&result, &EventType::MaintenanceCycleCompleted),
            1,
            "the gather synthesizes exactly one reduce event",
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "the reduce block ran once");
        let payload: foundry_sdk::payload::GatherCompletedPayload =
            seen[0].parse_payload().unwrap();
        assert_eq!(payload.expected, 3);
        assert_eq!(payload.arrived, 3);
        assert_eq!(payload.children.len(), 3);
        assert!(
            payload.children.iter().all(|c| c.success == Some(true)),
            "every child reported success",
        );
        assert_eq!(seen[0].project, "system", "reduce event carries the GatherSpec's project");
    }

    #[tokio::test]
    async fn scatter_children_and_completions_share_one_gather_id() {
        let mut engine = Engine::new();
        engine.register(Box::new(ScatterBlock {
            name: "Scatterer",
            sinks: vec![EventType::GreetingRequested],
            child_type: EventType::ProjectRunStarted,
            child_count: 2,
            on: vec![EventType::ProjectRunCompleted],
            reduce_event_type: EventType::MaintenanceCycleCompleted,
            reduce_project: "system",
        }));
        engine.register(Box::new(ChildWorker));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "p".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let result = engine.process(trigger).await;

        let started: Vec<&Event> = result
            .events
            .iter()
            .filter(|e| e.event_type == EventType::ProjectRunStarted)
            .collect();
        let gid = started[0].gather_id.clone();
        assert!(gid.is_some(), "scatter children carry a gather_id");
        assert!(
            started.iter().all(|e| e.gather_id == gid),
            "all children share the one minted gather_id",
        );

        // The gather_id propagates verbatim into each child's completion.
        let completed: Vec<&Event> = result
            .events
            .iter()
            .filter(|e| e.event_type == EventType::ProjectRunCompleted)
            .collect();
        assert!(
            completed.iter().all(|e| e.gather_id == gid),
            "completions inherit the group's gather_id",
        );

        // The reduce event has no enclosing group, so its gather_id is None.
        let reduce = result
            .events
            .iter()
            .find(|e| e.event_type == EventType::MaintenanceCycleCompleted)
            .unwrap();
        assert!(reduce.gather_id.is_none(), "top-level reduce has no parent gather");
    }

    #[tokio::test]
    async fn empty_scatter_reduces_immediately() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut engine = Engine::new();
        engine.register(Box::new(ScatterBlock {
            name: "Scatterer",
            sinks: vec![EventType::GreetingRequested],
            child_type: EventType::ProjectRunStarted,
            child_count: 0,
            on: vec![EventType::ProjectRunCompleted],
            reduce_event_type: EventType::MaintenanceCycleCompleted,
            reduce_project: "system",
        }));
        engine.register(Box::new(ReduceObserver {
            sinks: vec![EventType::MaintenanceCycleCompleted],
            seen: Arc::clone(&seen),
        }));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "p".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let result = engine.process(trigger).await;

        assert_eq!(type_count(&result, &EventType::ProjectRunStarted), 0);
        assert_eq!(
            type_count(&result, &EventType::MaintenanceCycleCompleted),
            1,
            "an empty scatter still produces its reduce event",
        );
        let seen = seen.lock().unwrap();
        let payload: foundry_sdk::payload::GatherCompletedPayload =
            seen[0].parse_payload().unwrap();
        assert_eq!(payload.expected, 0);
        assert_eq!(payload.arrived, 0);
        assert!(payload.children.is_empty());
    }

    #[tokio::test]
    async fn nested_scatters_reduce_inner_then_outer() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut engine = Engine::new();
        // Outer: 2 children, each a PipelineCheckRequested; gathers the inner
        // reduce events (MaintenanceCycleCompleted) into StrategicCycleCompleted.
        engine.register(Box::new(ScatterBlock {
            name: "OuterScatterer",
            sinks: vec![EventType::GreetingRequested],
            child_type: EventType::PipelineCheckRequested,
            child_count: 2,
            on: vec![EventType::MaintenanceCycleCompleted],
            reduce_event_type: EventType::StrategicCycleCompleted,
            reduce_project: "outer",
        }));
        // Inner: each PipelineCheckRequested scatters 1 ProjectRunStarted,
        // gathering ProjectRunCompleted into MaintenanceCycleCompleted.
        engine.register(Box::new(ScatterBlock {
            name: "InnerScatterer",
            sinks: vec![EventType::PipelineCheckRequested],
            child_type: EventType::ProjectRunStarted,
            child_count: 1,
            on: vec![EventType::ProjectRunCompleted],
            reduce_event_type: EventType::MaintenanceCycleCompleted,
            reduce_project: "inner",
        }));
        engine.register(Box::new(ChildWorker));
        engine.register(Box::new(ReduceObserver {
            sinks: vec![EventType::StrategicCycleCompleted],
            seen: Arc::clone(&seen),
        }));

        let trigger = Event::new(
            EventType::GreetingRequested,
            "p".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let result = engine.process(trigger).await;

        assert_eq!(type_count(&result, &EventType::PipelineCheckRequested), 2);
        assert_eq!(type_count(&result, &EventType::ProjectRunCompleted), 2);
        assert_eq!(
            type_count(&result, &EventType::MaintenanceCycleCompleted),
            2,
            "each inner gather synthesizes one reduce event",
        );
        assert_eq!(
            type_count(&result, &EventType::StrategicCycleCompleted),
            1,
            "the inner reduce events satisfy the outer gather",
        );

        // Each inner reduce rejoins the outer group.
        let inner_reduces: Vec<&Event> = result
            .events
            .iter()
            .filter(|e| e.event_type == EventType::MaintenanceCycleCompleted)
            .collect();
        assert!(
            inner_reduces.iter().all(|e| e.gather_id.is_some()),
            "inner reduce events carry the outer gather_id",
        );

        let seen = seen.lock().unwrap();
        let payload: foundry_sdk::payload::GatherCompletedPayload =
            seen[0].parse_payload().unwrap();
        assert_eq!(payload.arrived, 2, "outer gather collected both inner reduces");
    }

    /// Minimal observer block that sinks on `VulnerabilityDetected` and emits
    /// no events. Used for span-id propagation tests where we only care about
    /// the `BlockExecution` record, not downstream chaining.
    struct SilentObserver;

    impl TaskBlock for SilentObserver {
        fn name(&self) -> &'static str {
            "B"
        }

        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }

        fn sinks_on(&self) -> &[EventType] {
            &[EventType::VulnerabilityDetected]
        }

        fn execute(
            &self,
            _trigger: &Event,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
        {
            Box::pin(async {
                Ok(TaskBlockResult {
                    success: true,
                    summary: "ok".to_string(),
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn block_execution_records_block_span_id() {
        let mut engine = Engine::new();
        engine.register(Box::new(SilentObserver));

        let trigger = Event::new(
            EventType::VulnerabilityDetected,
            "p".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
        .with_span_ids(Some(foundry_sdk::event::mint_span_id()), None);

        let workflow_span = trigger.span_id.clone();
        let result = engine.process(trigger).await;

        let block_exec = &result.block_executions[0];
        let block_span = block_exec.span_id.as_ref().expect("block must have span_id");
        assert_eq!(block_span.len(), 16, "block span_id must be 16 hex chars");
        assert_eq!(
            block_exec.parent_span_id, workflow_span,
            "block parent_span_id must equal the triggering event's span_id"
        );
    }

    /// Test helper: a block that sinks on a configured event type and emits
    /// a fixed list of event types with empty payloads. Used by span-stamping
    /// tests where we want to assert how the engine attaches span fields to
    /// arbitrary emitted events.
    struct EmittingBlock {
        name: &'static str,
        sinks: Vec<EventType>,
        emits: Vec<EventType>,
    }

    impl TaskBlock for EmittingBlock {
        fn name(&self) -> &'static str {
            self.name
        }

        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }

        fn sinks_on(&self) -> &[EventType] {
            &self.sinks
        }

        fn execute(
            &self,
            trigger: &Event,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
        {
            let project = trigger.project.clone();
            let throttle = trigger.throttle;
            let emits = self.emits.clone();
            Box::pin(async move {
                let events = emits
                    .into_iter()
                    .map(|t| Event::new(t, project.clone(), throttle, serde_json::json!({})))
                    .collect();
                Ok(TaskBlockResult {
                    events,
                    success: true,
                    summary: "emitted".to_string(),
                    ..Default::default()
                })
            })
        }
    }

    fn emitting_block(
        name: &'static str,
        sinks_on: EventType,
        emits: Vec<EventType>,
    ) -> EmittingBlock {
        EmittingBlock {
            name,
            sinks: vec![sinks_on],
            emits,
        }
    }

    #[tokio::test]
    async fn default_rule_emitted_event_inherits_trigger_span() {
        let workflow_span = foundry_sdk::event::mint_span_id();
        let trace = foundry_sdk::event::mint_trace_id();
        let trigger = Event::new(
            EventType::VulnerabilityDetected,
            "p".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some(trace.clone()))
        .with_span_ids(Some(workflow_span.clone()), Some("0000000000000001".to_string()));

        // A block that emits a non-opener event (ProjectChangesPushed).
        let block = emitting_block(
            "B",
            EventType::VulnerabilityDetected,
            vec![EventType::ProjectChangesPushed],
        );
        let mut engine = Engine::new();
        engine.register(Box::new(block));
        let result = engine.process(trigger).await;

        let emitted: Vec<&Event> = result
            .events
            .iter()
            .filter(|e| e.event_type == EventType::ProjectChangesPushed)
            .collect();
        assert_eq!(emitted.len(), 1);
        assert_eq!(emitted[0].trace_id.as_deref(), Some(trace.as_str()));
        assert_eq!(emitted[0].span_id, Some(workflow_span));
        assert_eq!(emitted[0].parent_span_id.as_deref(), Some("0000000000000001"));
    }

    #[tokio::test]
    async fn span_opener_rule_mints_fresh_span_parented_to_block() {
        let workflow_span = foundry_sdk::event::mint_span_id();
        let trace = foundry_sdk::event::mint_trace_id();
        let trigger = Event::new(
            EventType::PipelineChecked,
            "p".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some(trace.clone()))
        .with_span_ids(Some(workflow_span), None);

        // A block that emits a workflow opener (ProjectIterationRequested).
        let block = emitting_block(
            "B",
            EventType::PipelineChecked,
            vec![EventType::ProjectIterationRequested],
        );
        let mut engine = Engine::new();
        engine.register(Box::new(block));
        let result = engine.process(trigger).await;

        let opener = result
            .events
            .iter()
            .find(|e| e.event_type == EventType::ProjectIterationRequested)
            .expect("opener must be emitted");
        let block_exec = result.block_executions.iter().find(|b| b.block_name == "B").unwrap();

        assert_eq!(
            opener.trace_id.as_deref(),
            Some(trace.as_str()),
            "trace_id propagates through opener"
        );
        assert_ne!(
            opener.span_id, block_exec.parent_span_id,
            "opener gets a FRESH span_id, not the workflow span"
        );
        assert_eq!(
            opener.parent_span_id, block_exec.span_id,
            "opener's parent is the block's own span"
        );
        assert_eq!(
            opener.span_id.as_ref().map(String::len),
            Some(16),
            "minted span_id is 16 hex chars"
        );
    }

    /// Test helper: a block that emits a single event with explicit
    /// `trace_id`, `span_id`, and `parent_span_id` pre-populated. Used to
    /// pin the "set if unset" contract: when a block emits an event that
    /// already carries span context, the engine must preserve it.
    struct ExplicitlyStampedBlock {
        name: &'static str,
        sinks: Vec<EventType>,
        emit_type: EventType,
        trace_id: String,
        span_id: String,
        parent_span_id: String,
        causation_id: Option<String>,
    }

    impl ExplicitlyStampedBlock {
        fn new(
            name: &'static str,
            sinks_on: EventType,
            emit_type: EventType,
            trace_id: String,
            span_id: String,
            parent_span_id: String,
        ) -> Self {
            Self {
                name,
                sinks: vec![sinks_on],
                emit_type,
                trace_id,
                span_id,
                parent_span_id,
                causation_id: None,
            }
        }
    }

    impl TaskBlock for ExplicitlyStampedBlock {
        fn name(&self) -> &'static str {
            self.name
        }

        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }

        fn sinks_on(&self) -> &[EventType] {
            &self.sinks
        }

        fn execute(
            &self,
            trigger: &Event,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
        {
            let project = trigger.project.clone();
            let throttle = trigger.throttle;
            let emit_type = self.emit_type.clone();
            let trace_id = self.trace_id.clone();
            let span_id = self.span_id.clone();
            let parent_span_id = self.parent_span_id.clone();
            let causation_id = self.causation_id.clone();
            Box::pin(async move {
                let emitted = Event::new(emit_type, project, throttle, serde_json::json!({}))
                    .with_trace_id(Some(trace_id))
                    .with_span_ids(Some(span_id), Some(parent_span_id))
                    .with_causation_id(causation_id);
                Ok(TaskBlockResult {
                    events: vec![emitted],
                    success: true,
                    summary: "emitted".to_string(),
                    ..Default::default()
                })
            })
        }
    }

    #[tokio::test]
    async fn stamp_span_context_preserves_block_provided_span_fields() {
        // Contract: stamping is "set if unset". If a block emits an event with
        // explicit span context (trace_id / span_id / parent_span_id all
        // populated), the engine must NOT overwrite those values, even though
        // the trigger context is different.

        let trigger_trace = foundry_sdk::event::mint_trace_id();
        let trigger_span = foundry_sdk::event::mint_span_id();
        let trigger = Event::new(
            EventType::VulnerabilityDetected,
            "p".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some(trigger_trace.clone()))
        .with_span_ids(Some(trigger_span.clone()), None);

        // Block emits a non-opener event with explicit span context.
        // Use a distinct trace_id / span_id from the trigger so any
        // accidental overwrite is observable.
        let explicit_trace = foundry_sdk::event::mint_trace_id();
        let explicit_span = foundry_sdk::event::mint_span_id();
        let explicit_parent = foundry_sdk::event::mint_span_id();
        assert_ne!(explicit_trace, trigger_trace, "test setup: must differ");
        assert_ne!(explicit_span, trigger_span, "test setup: must differ");

        let block = ExplicitlyStampedBlock::new(
            "B",
            EventType::VulnerabilityDetected,
            EventType::ProjectChangesPushed,
            explicit_trace.clone(),
            explicit_span.clone(),
            explicit_parent.clone(),
        );

        let mut engine = Engine::new();
        engine.register(Box::new(block));
        let result = engine.process(trigger).await;

        let emitted = result
            .events
            .iter()
            .find(|e| e.event_type == EventType::ProjectChangesPushed)
            .expect("explicit-stamped event must be emitted");

        // All three fields preserved — none overwritten.
        assert_eq!(
            emitted.trace_id.as_deref(),
            Some(explicit_trace.as_str()),
            "block-provided trace_id must NOT be overwritten"
        );
        assert_eq!(
            emitted.span_id.as_deref(),
            Some(explicit_span.as_str()),
            "block-provided span_id must NOT be overwritten"
        );
        assert_eq!(
            emitted.parent_span_id.as_deref(),
            Some(explicit_parent.as_str()),
            "block-provided parent_span_id must NOT be overwritten"
        );
    }

    #[tokio::test]
    async fn block_inside_engine_sees_span_context_via_task_local() {
        // Task 5.2: the engine wraps `block.execute` in
        // `SPAN_CONTEXT::scope` so subprocess spawners inside the block
        // can read (trace_id, block_span_id) without explicit plumbing.
        use foundry_sdk::span_context::SPAN_CONTEXT;
        use std::sync::Mutex;

        struct ContextProbingBlock {
            seen_trace: Arc<Mutex<Option<String>>>,
            seen_span: Arc<Mutex<Option<String>>>,
        }

        impl TaskBlock for ContextProbingBlock {
            fn name(&self) -> &'static str {
                "Probe"
            }

            fn kind(&self) -> BlockKind {
                BlockKind::Observer
            }

            fn sinks_on(&self) -> &[EventType] {
                &[EventType::VulnerabilityDetected]
            }

            fn execute(
                &self,
                _trigger: &Event,
            ) -> Pin<
                Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>,
            > {
                let seen_trace = Arc::clone(&self.seen_trace);
                let seen_span = Arc::clone(&self.seen_span);
                Box::pin(async move {
                    let _ = SPAN_CONTEXT.try_with(|ctx| {
                        *seen_trace.lock().unwrap() = Some(ctx.trace_id.clone());
                        *seen_span.lock().unwrap() = Some(ctx.span_id.clone());
                    });
                    Ok(TaskBlockResult {
                        success: true,
                        summary: "probed".to_string(),
                        ..Default::default()
                    })
                })
            }
        }

        let seen_trace: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let seen_span: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

        let block = Box::new(ContextProbingBlock {
            seen_trace: Arc::clone(&seen_trace),
            seen_span: Arc::clone(&seen_span),
        });

        let mut engine = Engine::new();
        engine.register(block);

        let trigger = Event::new(
            EventType::VulnerabilityDetected,
            "p".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
        .with_span_ids(Some(foundry_sdk::event::mint_span_id()), None);

        let trigger_trace = trigger.trace_id.clone();
        let result = engine.process(trigger).await;

        let block_exec = &result.block_executions[0];
        let block_span = block_exec.span_id.clone();

        assert_eq!(
            *seen_trace.lock().unwrap(),
            trigger_trace,
            "block must see trigger's trace_id via SPAN_CONTEXT"
        );
        assert_eq!(
            *seen_span.lock().unwrap(),
            block_span,
            "block must see its OWN span_id via SPAN_CONTEXT (NOT trigger's span_id)"
        );
    }

    #[tokio::test]
    async fn trace_id_propagates_in_dry_run() {
        let mut engine = Engine::new();
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let workflow_span = foundry_sdk::event::mint_span_id();
        let trigger = Event::new(
            EventType::GreetingRequested,
            "test-project".to_string(),
            Throttle::DryRun,
            serde_json::json!({}),
        )
        .with_trace_id(Some("trc_dryrun".to_string()))
        .with_span_ids(Some(workflow_span), None);

        let result = engine.process(trigger).await;

        // Even dry-run simulated events should carry the trace_id and a span_id.
        for event in &result.events {
            assert_eq!(
                event.trace_id.as_deref(),
                Some("trc_dryrun"),
                "dry-run event {} should have propagated trace_id",
                event.event_type,
            );
            assert!(
                event.span_id.is_some(),
                "dry-run event {} should have a stamped span_id",
                event.event_type,
            );
        }
    }
}
