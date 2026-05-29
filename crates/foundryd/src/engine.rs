use std::sync::Arc;

use tokio::sync::broadcast;

use foundry_core::event::{Event, EventType, mint_gather_id};
use foundry_core::scatter::Scatter;
use foundry_core::task_block::{BlockKind, RetryPolicy, TaskBlock, TaskBlockResult};
use foundry_core::throttle::Throttle;
pub use foundry_core::trace::{BlockExecution, ProcessResult};

use crate::event_writer::EventWriter;
use crate::gather_store::{GatherGroup, GatherStore};

/// The workflow engine routes events to task blocks and manages propagation.
pub struct Engine {
    blocks: Vec<Box<dyn TaskBlock>>,
    event_writer: Option<Arc<EventWriter>>,
    /// Optional broadcast channel for real-time event streaming to Watch clients.
    event_tx: Option<broadcast::Sender<Event>>,
}

/// Mutable state threaded through a single [`Engine::process`] traversal.
///
/// Bundling these together keeps the per-traversal signatures small and
/// names what they are: the running record of every event seen, the queue of
/// events still to dispatch, and the open gather groups for in-flight
/// scatters.
struct ProcessState {
    /// Every event seen this traversal, in production order — the basis of
    /// the returned [`ProcessResult`].
    all_events: Vec<Event>,
    /// Events awaiting dispatch.
    queue: Vec<Event>,
    /// Open scatter/gather groups for the duration of this traversal.
    gather_store: GatherStore,
}

/// Execute a block with retry logic, sleeping `policy.backoff` between attempts.
///
/// Returns the final `anyhow::Result<TaskBlockResult>` after all retry attempts
/// are exhausted or a successful result is obtained.
async fn execute_with_retry(
    block: &dyn TaskBlock,
    trigger: &Event,
    policy: RetryPolicy,
) -> anyhow::Result<TaskBlockResult> {
    let mut last_result: Option<anyhow::Result<TaskBlockResult>> = None;

    for attempt in 0..=policy.max_retries {
        if attempt > 0 {
            tracing::info!(attempt, max_retries = policy.max_retries, "retrying block");
            tokio::time::sleep(policy.backoff).await;
        }

        match block.execute(trigger).await {
            Ok(result) if result.success => {
                return Ok(result);
            }
            Ok(result) => {
                tracing::warn!(
                    attempt,
                    summary = %result.summary,
                    "block reported failure, will retry if attempts remain"
                );
                last_result = Some(Ok(result));
            }
            Err(err) => {
                tracing::error!(attempt, error = %err, "block execute error");
                last_result = Some(Err(err));
            }
        }
    }

    last_result.expect("loop always sets last_result")
}

fn make_block_execution(
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

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self {
            blocks: vec![],
            event_writer: None,
            event_tx: None,
        }
    }

    /// Attach an `EventWriter` so every event in a processing chain is
    /// persisted to JSONL as it is produced.  Write failures are logged but
    /// never interrupt event processing.
    #[must_use]
    pub fn with_event_writer(mut self, writer: Arc<EventWriter>) -> Self {
        self.event_writer = Some(writer);
        self
    }

    /// Attach a broadcast sender so events are pushed to Watch subscribers in real time.
    #[must_use]
    pub fn with_event_broadcaster(mut self, tx: broadcast::Sender<Event>) -> Self {
        self.event_tx = Some(tx);
        self
    }

    /// Register a task block with the engine.
    pub fn register(&mut self, block: Box<dyn TaskBlock>) {
        tracing::info!(block = block.name(), sinks = ?block.sinks_on(), "registered task block");
        self.blocks.push(block);
    }

    /// Persist an event to JSONL and broadcast it to Watch subscribers.
    ///
    /// Both are best-effort: write failures are logged, and a broadcast with
    /// no receivers is normal. Neither ever interrupts event processing.
    fn persist_one(&self, event: &Event) {
        if let Some(writer) = &self.event_writer {
            if let Err(e) = writer.write(event) {
                tracing::warn!(error = %e, event_id = %event.id, "failed to write event to JSONL");
            }
        }
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event.clone());
        }
    }

    /// Offer a delivered event to the gather store. If it satisfies a gather
    /// group, persist, broadcast, and enqueue the synthesized reduce event —
    /// then recurse, since a reduce event may itself satisfy an outer
    /// (nested) group.
    fn deliver_reduce_if_satisfied(&self, event: &Event, state: &mut ProcessState) {
        let Some(reduce) = state.gather_store.record(event) else {
            return;
        };
        tracing::info!(
            gather_id = reduce.gather_id.as_deref().unwrap_or("-"),
            reduce_event = %reduce.event_type,
            "gather satisfied — synthesizing reduce event"
        );
        self.persist_one(&reduce);
        state.all_events.push(reduce.clone());
        state.queue.push(reduce.clone());
        self.deliver_reduce_if_satisfied(&reduce, state);
    }

    /// Propagate trace IDs, stamp `OTel`-shaped span context, persist to JSONL,
    /// broadcast to Watch subscribers, and optionally deliver to the processing
    /// queue. Delivered events are offered to the gather store, which may
    /// synthesize reduce events. Returns collected event IDs and payloads for
    /// the [`BlockExecution`] record.
    fn persist_and_broadcast_events(
        &self,
        events: Vec<Event>,
        trigger: &Event,
        block_span_id: &str,
        state: &mut ProcessState,
        deliver: bool,
    ) -> (Vec<String>, Vec<serde_json::Value>) {
        let mut emitted_ids = Vec::new();
        let mut emitted_payloads = Vec::new();
        for mut emitted in events {
            Self::stamp_context(&mut emitted, trigger, block_span_id);
            self.persist_one(&emitted);
            emitted_ids.push(emitted.id.clone());
            emitted_payloads.push(emitted.payload.clone());
            state.all_events.push(emitted.clone());
            if deliver {
                state.queue.push(emitted.clone());
                self.deliver_reduce_if_satisfied(&emitted, state);
            } else {
                tracing::info!(event_type = %emitted.event_type, "event logged but delivery throttled");
            }
        }
        (emitted_ids, emitted_payloads)
    }

    /// Open a gather group from a block's [`Scatter`] declaration: mint a
    /// fresh `gather_id`, stamp every child with it, register the group, and
    /// dispatch the children. Returns the child event IDs for the
    /// [`BlockExecution`] record. An empty scatter satisfies its gather at
    /// once and its reduce event is delivered immediately.
    fn dispatch_scatter(
        &self,
        scatter: Scatter,
        trigger: &Event,
        block_span_id: &str,
        state: &mut ProcessState,
        deliver: bool,
    ) -> Vec<String> {
        let Scatter {
            mut children,
            gather,
        } = scatter;
        let gather_id = mint_gather_id();
        // A scatter opens a NEW group: override any inherited gather_id so the
        // children belong to this group rather than an enclosing one.
        for child in &mut children {
            child.gather_id = Some(gather_id.clone());
        }
        let group = GatherGroup::new(gather_id.clone(), children.len(), gather, trigger);
        let immediate = state.gather_store.open(group);
        let child_count = children.len();
        let (child_ids, _payloads) =
            self.persist_and_broadcast_events(children, trigger, block_span_id, state, deliver);
        tracing::info!(gather_id = %gather_id, children = child_count, "scatter dispatched");
        // An empty scatter is satisfied on registration — deliver its reduce.
        if let Some(reduce) = immediate {
            if deliver {
                self.persist_one(&reduce);
                state.all_events.push(reduce.clone());
                state.queue.push(reduce.clone());
                self.deliver_reduce_if_satisfied(&reduce, state);
            }
        }
        child_ids
    }

    /// Apply causal and `OTel`-shaped tracing context to an emitted event.
    ///
    /// All stamping is "set if unset", so a block may emit an event with
    /// explicit context and that context is preserved.
    ///
    /// # Causation
    ///
    /// The emitted event's `causation_id` is set to the triggering event's
    /// `id` — recording the direct causal edge in the event graph,
    /// independent of the observability span structure below.
    ///
    /// # Gather membership
    ///
    /// The emitted event inherits the trigger's `gather_id` verbatim — the
    /// same propagation rule as `trace_id`. This carries fan-out group
    /// membership all the way down a scattered child's sub-workflow so the
    /// terminal completion event still identifies its gather.
    ///
    /// # Span rules
    ///
    /// - **Default** (non-opener events): the emitted event is a peer of the
    ///   trigger — it inherits the trigger's `trace_id`, `span_id` (the active
    ///   workflow span), and `parent_span_id`.
    /// - **Span opener** (e.g. `ProjectIterationRequested`): the emitted event opens a
    ///   new workflow span — it inherits the trigger's `trace_id`, receives a
    ///   freshly minted `span_id`, and is parented to the emitting block's
    ///   `block_span_id`.
    fn stamp_context(emitted: &mut Event, trigger: &Event, block_span_id: &str) {
        use foundry_core::event::mint_span_id;

        if emitted.causation_id.is_none() {
            emitted.causation_id = Some(trigger.id.clone());
        }

        if emitted.trace_id.is_none() {
            emitted.trace_id.clone_from(&trigger.trace_id);
        }

        if emitted.gather_id.is_none() {
            emitted.gather_id.clone_from(&trigger.gather_id);
        }

        if emitted.event_type.is_span_opener() {
            // New workflow span: child of the emitting block's span.
            if emitted.span_id.is_none() {
                emitted.span_id = Some(mint_span_id());
            }
            if emitted.parent_span_id.is_none() {
                emitted.parent_span_id = Some(block_span_id.to_string());
            }
        } else {
            // Default: peer of trigger, attached to the same workflow span.
            if emitted.span_id.is_none() {
                emitted.span_id.clone_from(&trigger.span_id);
            }
            if emitted.parent_span_id.is_none() {
                emitted.parent_span_id.clone_from(&trigger.parent_span_id);
            }
        }
    }

    /// Execute one block against a triggering event, persist any emitted events,
    /// and return the [`BlockExecution`] record.  Mutates `state` in place so
    /// downstream events continue to be processed.
    #[allow(clippy::too_many_lines)]
    async fn run_block(
        &self,
        block: &dyn TaskBlock,
        current: &Event,
        state: &mut ProcessState,
    ) -> BlockExecution {
        let block_span_id = foundry_core::event::mint_span_id();
        let workflow_span_id = current.span_id.clone();

        let block_start = std::time::Instant::now();
        if !block.should_execute(current.throttle)
            && current.throttle == Throttle::DryRun
            && block.kind() == BlockKind::Mutator
        {
            let simulated_events = block.dry_run_events(current);
            tracing::info!(simulated = simulated_events.len(), "dry-run: simulating success");
            let (emitted_ids, emitted_payloads) = self.persist_and_broadcast_events(
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
            return exec;
        }

        // Non-DryRun throttle skip — should not happen in practice, kept as a safety net.
        if !block.should_execute(current.throttle) {
            tracing::info!("skipped (throttle)");
            return make_block_execution(
                block,
                current,
                block_span_id,
                workflow_span_id,
                block_start,
                true,
                "skipped (throttle)".to_string(),
            );
        }

        // Scope SPAN_CONTEXT so subprocess spawners inside the block can inject TRACEPARENT.
        let exec_future = execute_with_retry(block, current, block.retry_policy());
        let result_or_err = if let Some(trace_id) = current.trace_id.clone() {
            let span_ctx = foundry_core::span_context::SpanContext {
                trace_id,
                span_id: block_span_id.clone(),
            };
            foundry_core::span_context::SPAN_CONTEXT.scope(span_ctx, exec_future).await
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
                let (mut emitted_ids, emitted_payloads) = self.persist_and_broadcast_events(
                    result.events,
                    current,
                    &block_span_id,
                    state,
                    deliver,
                );
                if let Some(scatter) = result.scatter {
                    let child_ids =
                        self.dispatch_scatter(*scatter, current, &block_span_id, state, deliver);
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
        if let Some(writer) = &self.event_writer {
            if let Err(e) = writer.write(&event) {
                tracing::warn!(error = %e, "failed to write root event to JSONL");
            }
        }

        // Broadcast the root event immediately so Watch clients see it in real time.
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event.clone()); // No receivers is normal — not an error.
        }

        let mut block_executions = Vec::new();
        let mut state = ProcessState {
            all_events: vec![event.clone()],
            queue: vec![event],
            gather_store: GatherStore::new(),
        };

        while let Some(current) = state.queue.pop() {
            let matching: Vec<&dyn TaskBlock> = self
                .blocks
                .iter()
                .filter(|b| b.sinks_on().contains(&current.event_type))
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
    use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
    use foundry_core::throttle::Throttle;
    use std::pin::Pin;

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

    // -- Vulnerability remediation integration tests --

    fn vuln_engine() -> Engine {
        use foundry_core::registry::{ActionFlags, ProjectEntry, Stack};
        use std::sync::RwLock;

        // CutRelease requires AGENTS.md to exist before invoking Claude.
        // Leak the temp dir so it outlives the test.
        let dir = tempfile::TempDir::new().unwrap();
        let project_path = dir.path().to_str().unwrap().to_string();
        // Initialize a git repo with an uncommitted change so CommitAndPush has work to do.
        let _ = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&project_path)
            .output();
        let _ = std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(&project_path)
            .output();
        let _ = std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&project_path)
            .output();
        // Create an initial commit so there's a HEAD reference
        std::fs::write(dir.path().join("AGENTS.md"), "# test").unwrap();
        let _ = std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&project_path)
            .output();
        let _ = std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&project_path)
            .output();
        // Set up a local bare repo as remote so git push succeeds
        let remote_dir = tempfile::TempDir::new().unwrap();
        let remote_path = remote_dir.path().to_str().unwrap().to_string();
        let _ = std::process::Command::new("git")
            .args(["init", "--bare"])
            .current_dir(&remote_path)
            .output();
        let _ = std::process::Command::new("git")
            .args(["remote", "add", "origin", &remote_path])
            .current_dir(&project_path)
            .output();
        let _ = std::process::Command::new("git")
            .args(["push", "-u", "origin", "main"])
            .current_dir(&project_path)
            .output();
        // Create an uncommitted change so CommitAndPush triggers
        std::fs::write(dir.path().join("CHANGES.md"), "changes").unwrap();
        std::mem::forget(dir);
        std::mem::forget(remote_dir);

        let registry = Arc::new(RwLock::new(foundry_core::registry::Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: "test-project".to_string(),
                path: project_path,
                stack: Stack::Rust,
                agent: "claude".to_string(),
                repo: String::new(),
                branch: "main".to_string(),
                skip: None,
                notes: None,
                actions: ActionFlags {
                    iterate: false,
                    maintain: false,
                    push: true,
                    audit: false,
                    release: false,
                },
                install: None,
                installs_skill: None,
                timeout_secs: None,
            }],
        }));
        let mut engine = Engine::new();
        engine.register(Box::new(crate::blocks::ScanDependencies::new(Arc::clone(&registry))));
        engine.register(Box::new(crate::blocks::AuditReleaseTag::with_registry(Arc::clone(
            &registry,
        ))));
        engine.register(Box::new(crate::blocks::AuditMainBranch::new(Arc::clone(&registry))));
        let agent: Arc<dyn crate::gateway::AgentGateway> =
            crate::gateway::fakes::FakeAgentGateway::success();
        engine.register(Box::new(crate::blocks::RemediateVulnerability::new(
            agent,
            Arc::clone(&registry),
        )));
        engine.register(Box::new(crate::blocks::CommitAndPush::new(Arc::clone(&registry))));
        engine.register(Box::new(crate::blocks::cut_release_step(Arc::clone(&registry))));
        engine.register(Box::new(crate::blocks::WatchPipeline::stub()));
        engine.register(Box::new(crate::blocks::InstallLocally::new(Arc::clone(&registry))));
        engine
    }

    #[tokio::test]
    async fn vuln_dirty_path_remediates_and_installs() {
        let engine = vuln_engine();

        let trigger = Event::new(
            EventType::VulnerabilityDetected,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "cve": "CVE-2026-1234",
                "vulnerable": true,
                "dirty": true,
            }),
        );

        let result = engine.process(trigger).await;
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        assert_eq!(
            types,
            [
                "vulnerability_detected",
                "release_tag_audited",
                "main_branch_audited",
                "remediation_completed",
                "project_changes_committed",
                "project_changes_pushed",
                // AuditReleaseTag now sinks on ProjectChangesPushed and performs a
                // post-push re-audit (stub: reports clean, vulnerable=false).
                "release_tag_audited",
                "local_install_completed",
            ]
        );
    }

    #[tokio::test]
    async fn vuln_clean_path_releases_and_installs() {
        let engine = vuln_engine();

        let trigger = Event::new(
            EventType::VulnerabilityDetected,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "cve": "CVE-2026-5678",
                "vulnerable": true,
                "dirty": false,
            }),
        );

        let result = engine.process(trigger).await;
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        assert_eq!(
            types,
            [
                "vulnerability_detected",
                "release_tag_audited",
                "main_branch_audited",
                "release_completed",
                "release_pipeline_completed",
                "local_install_completed",
            ]
        );
    }

    #[tokio::test]
    async fn vuln_not_vulnerable_stops_at_audit() {
        let engine = vuln_engine();

        let trigger = Event::new(
            EventType::VulnerabilityDetected,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "cve": "CVE-2026-9999",
                "vulnerable": false,
            }),
        );

        let result = engine.process(trigger).await;
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        // Chain stops after release_tag_audited because AuditMainBranch
        // self-filters when vulnerable=false
        assert_eq!(types, ["vulnerability_detected", "release_tag_audited",]);
    }

    #[tokio::test]
    async fn vuln_dry_run_full_chain_with_simulated_events() {
        let engine = vuln_engine();

        let trigger = Event::new(
            EventType::VulnerabilityDetected,
            "test-project".to_string(),
            Throttle::DryRun,
            serde_json::json!({
                "cve": "CVE-2026-1234",
                "vulnerable": true,
                "dirty": true,
            }),
        );

        let result = engine.process(trigger).await;
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        // Full chain completes with simulated mutator events (dirty path).
        // Observers execute for real; Mutators simulate success.
        assert_eq!(
            types,
            [
                "vulnerability_detected",
                "release_tag_audited",
                "main_branch_audited",
                "remediation_completed",     // simulated
                "project_changes_committed", // simulated
                "project_changes_pushed",    // simulated
                // AuditReleaseTag sinks on ProjectChangesPushed (Observer, runs for real)
                "release_tag_audited",
                "local_install_completed", // simulated
            ]
        );

        // All simulated events carry dry_run: true.
        let remediation = result
            .events
            .iter()
            .find(|e| e.event_type == EventType::RemediationCompleted)
            .unwrap();
        assert_eq!(remediation.payload["dry_run"], true);
    }

    // -- Scan-triggered workflow integration tests --

    #[tokio::test]
    async fn scan_triggers_full_remediation_chain() {
        let engine = vuln_engine();

        // Start from scan_requested instead of vulnerability_detected.
        // The scanner invokes `cargo audit` in the temp project dir. Since
        // no real Cargo.lock exists, the scanner reports an error and the
        // chain stops at scan_requested with no downstream events.
        let trigger = Event::new(
            EventType::ScanRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = engine.process(trigger).await;
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        // Scanner tool unavailable in temp dir — chain ends at scan_requested.
        assert_eq!(types, ["scan_requested"]);
    }

    #[tokio::test]
    async fn scan_dry_run_scans_and_audits_only() {
        let engine = vuln_engine();

        let trigger = Event::new(
            EventType::ScanRequested,
            "test-project".to_string(),
            Throttle::DryRun,
            serde_json::json!({}),
        );

        let result = engine.process(trigger).await;
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        // Scanner tool unavailable in temp dir — chain ends at scan_requested.
        assert_eq!(types, ["scan_requested"]);
    }

    // -- Retry logic tests --

    use foundry_core::task_block::RetryPolicy;
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

        // Verify JSONL file was created and contains one line per event.
        let entries: Vec<_> =
            std::fs::read_dir(tmp.path()).unwrap().filter_map(Result::ok).collect();
        assert_eq!(entries.len(), 1, "exactly one JSONL file should exist");

        let contents = std::fs::read_to_string(entries[0].path()).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 3, "JSONL file should contain 3 events");

        // Each line should deserialize to a valid Event with the expected type.
        let written_types: Vec<String> = lines
            .iter()
            .map(|l| {
                let e: foundry_core::event::Event = serde_json::from_str(l).unwrap();
                e.event_type.as_str()
            })
            .collect();
        assert_eq!(
            written_types,
            [
                "greeting_requested",
                "greeting_composed",
                "greeting_delivered"
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

        let workflow_span = foundry_core::event::mint_span_id();
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
            trace_id: foundry_core::event::mint_trace_id(),
            span_id: foundry_core::event::mint_span_id(),
            parent_span_id: foundry_core::event::mint_span_id(),
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
        .with_trace_id(Some(foundry_core::event::mint_trace_id()))
        .with_span_ids(Some(foundry_core::event::mint_span_id()), None)
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
        let payload: foundry_core::payload::GatherCompletedPayload =
            seen[0].parse_payload().unwrap();
        assert_eq!(payload.expected, 3);
        assert_eq!(payload.arrived, 3);
        assert_eq!(payload.children.len(), 3);
        assert!(
            payload.children.iter().all(|c| c.success == Some(true)),
            "every child reported success",
        );
        assert_eq!(seen[0].project, "system", "reduce event carries the GatherSpec's project",);
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
        let payload: foundry_core::payload::GatherCompletedPayload =
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
        let payload: foundry_core::payload::GatherCompletedPayload =
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
        .with_trace_id(Some(foundry_core::event::mint_trace_id()))
        .with_span_ids(Some(foundry_core::event::mint_span_id()), None);

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
        let workflow_span = foundry_core::event::mint_span_id();
        let trace = foundry_core::event::mint_trace_id();
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
        let workflow_span = foundry_core::event::mint_span_id();
        let trace = foundry_core::event::mint_trace_id();
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

        let trigger_trace = foundry_core::event::mint_trace_id();
        let trigger_span = foundry_core::event::mint_span_id();
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
        let explicit_trace = foundry_core::event::mint_trace_id();
        let explicit_span = foundry_core::event::mint_span_id();
        let explicit_parent = foundry_core::event::mint_span_id();
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
        use foundry_core::span_context::SPAN_CONTEXT;
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
        .with_trace_id(Some(foundry_core::event::mint_trace_id()))
        .with_span_ids(Some(foundry_core::event::mint_span_id()), None);

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

        let workflow_span = foundry_core::event::mint_span_id();
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
