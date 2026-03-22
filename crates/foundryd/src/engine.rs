use std::sync::Arc;

use tokio::sync::broadcast;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::{RetryPolicy, TaskBlock, TaskBlockResult};

use crate::event_writer::EventWriter;

/// Record of a single block execution within a processing chain.
#[derive(Debug, Clone)]
pub struct BlockExecution {
    /// Name of the block that ran.
    pub block_name: String,
    /// The `event_id` that triggered this block.
    pub trigger_event_id: String,
    /// Whether the block succeeded.
    pub success: bool,
    /// Human-readable summary from the block.
    pub summary: String,
    /// Event IDs emitted by this block (empty if suppressed or failed).
    pub emitted_event_ids: Vec<String>,
    /// Wall-clock milliseconds spent executing this block (including retries).
    pub duration_ms: u64,
}

/// The full result of processing an event chain.
#[derive(Debug, Clone)]
pub struct ProcessResult {
    /// All events produced during the chain (including the root).
    pub events: Vec<Event>,
    /// Record of each block execution in order.
    pub block_executions: Vec<BlockExecution>,
    /// Wall-clock milliseconds for the entire `process()` call.
    pub total_duration_ms: u64,
}

/// The workflow engine routes events to task blocks and manages propagation.
pub struct Engine {
    blocks: Vec<Box<dyn TaskBlock>>,
    event_writer: Option<Arc<EventWriter>>,
    /// Optional broadcast channel for real-time event streaming to Watch clients.
    event_tx: Option<broadcast::Sender<Event>>,
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

impl Engine {
    pub fn new() -> Self {
        Self {
            blocks: vec![],
            event_writer: None,
            event_tx: None,
        }
    }

    /// Attach an [`EventWriter`] so every event in a processing chain is
    /// persisted to JSONL as it is produced.  Write failures are logged but
    /// never interrupt event processing.
    pub fn with_event_writer(mut self, writer: Arc<EventWriter>) -> Self {
        self.event_writer = Some(writer);
        self
    }

    /// Attach a broadcast sender so events are pushed to Watch subscribers in real time.
    pub fn with_event_broadcaster(mut self, tx: broadcast::Sender<Event>) -> Self {
        self.event_tx = Some(tx);
        self
    }

    /// Register a task block with the engine.
    pub fn register(&mut self, block: Box<dyn TaskBlock>) {
        tracing::info!(block = block.name(), sinks = ?block.sinks_on(), "registered task block");
        self.blocks.push(block);
    }

    /// Execute one block against a triggering event, persist any emitted events,
    /// and return the [`BlockExecution`] record.  Mutates `all_events` and
    /// `queue` in place so downstream events continue to be processed.
    async fn run_block(
        &self,
        block: &dyn TaskBlock,
        current: &Event,
        all_events: &mut Vec<Event>,
        queue: &mut Vec<Event>,
    ) -> BlockExecution {
        let block_start = std::time::Instant::now();

        if !block.should_execute(current.throttle) {
            tracing::info!("skipped (throttle)");
            return BlockExecution {
                block_name: block.name().to_string(),
                trigger_event_id: current.id.clone(),
                success: true,
                summary: "skipped (throttle)".to_string(),
                emitted_event_ids: vec![],
                duration_ms: u64::try_from(block_start.elapsed().as_millis()).unwrap_or(u64::MAX),
            };
        }

        match execute_with_retry(block, current, block.retry_policy()).await {
            Ok(result) => {
                let duration_ms =
                    u64::try_from(block_start.elapsed().as_millis()).unwrap_or(u64::MAX);
                tracing::info!(
                    success = result.success,
                    summary = %result.summary,
                    emitted = result.events.len(),
                    "completed"
                );
                let mut emitted_ids = Vec::new();
                if block.should_emit(current.throttle) {
                    for emitted in result.events {
                        if let Some(writer) = &self.event_writer {
                            if let Err(e) = writer.write(&emitted) {
                                tracing::warn!(
                                    error = %e,
                                    event_id = %emitted.id,
                                    "failed to write emitted event to JSONL"
                                );
                            }
                        }
                        // Broadcast each emitted event in real time to Watch subscribers.
                        if let Some(tx) = &self.event_tx {
                            let _ = tx.send(emitted.clone()); // No receivers is normal.
                        }
                        emitted_ids.push(emitted.id.clone());
                        all_events.push(emitted.clone());
                        queue.push(emitted);
                    }
                } else {
                    tracing::info!(
                        suppressed = result.events.len(),
                        "emission suppressed by throttle"
                    );
                }
                BlockExecution {
                    block_name: block.name().to_string(),
                    trigger_event_id: current.id.clone(),
                    success: result.success,
                    summary: result.summary,
                    emitted_event_ids: emitted_ids,
                    duration_ms,
                }
            }
            Err(err) => {
                let duration_ms =
                    u64::try_from(block_start.elapsed().as_millis()).unwrap_or(u64::MAX);
                tracing::error!(error = %err, "failed");
                BlockExecution {
                    block_name: block.name().to_string(),
                    trigger_event_id: current.id.clone(),
                    success: false,
                    summary: format!("error: {err}"),
                    emitted_event_ids: vec![],
                    duration_ms,
                }
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

        let mut all_events = vec![event.clone()];
        let mut block_executions = Vec::new();
        let mut queue = vec![event];

        while let Some(current) = queue.pop() {
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

                let execution = self.run_block(block, &current, &mut all_events, &mut queue).await;
                block_executions.push(execution);
            }
        }

        ProcessResult {
            events: all_events,
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
    use foundry_core::task_block::{BlockKind, TaskBlockResult};
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
            &[EventType::GreetRequested]
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
                })
            })
        }
    }

    #[tokio::test]
    async fn full_throttle_propagates_through_chain() {
        let mut engine = Engine::new();
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let trigger = Event::new(
            EventType::GreetRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({"name": "world"}),
        );

        let result = engine.process(trigger).await;

        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["greet_requested", "greeting_composed", "greeting_delivered"]);
        assert_eq!(result.block_executions.len(), 2);
        assert!(result.block_executions.iter().all(|b| b.success));
    }

    #[tokio::test]
    async fn audit_only_suppresses_mutator_emission() {
        let mut engine = Engine::new();
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let trigger = Event::new(
            EventType::GreetRequested,
            "test-project".to_string(),
            Throttle::AuditOnly,
            serde_json::json!({"name": "world"}),
        );

        let result = engine.process(trigger).await;

        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        // Observer emits greeting_composed, but Mutator's greeting_delivered is suppressed
        assert_eq!(types, ["greet_requested", "greeting_composed"]);
    }

    #[tokio::test]
    async fn dry_run_skips_mutator_execution() {
        let mut engine = Engine::new();
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let trigger = Event::new(
            EventType::GreetRequested,
            "test-project".to_string(),
            Throttle::DryRun,
            serde_json::json!({"name": "world"}),
        );

        let result = engine.process(trigger).await;

        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        // Observer emits, Mutator is skipped entirely (not even executed)
        assert_eq!(types, ["greet_requested", "greeting_composed"]);
        // Mutator was skipped, so its execution is recorded as skipped
        assert!(result.block_executions.iter().any(|b| b.summary == "skipped (throttle)"));
    }

    // -- Vulnerability remediation integration tests --

    fn vuln_engine() -> Engine {
        use foundry_core::registry::{ActionFlags, ProjectEntry, Stack};

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

        let registry = Arc::new(foundry_core::registry::Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: "test-project".to_string(),
                path: project_path,
                stack: Stack::Rust,
                agent: "claude".to_string(),
                repo: String::new(),
                branch: "main".to_string(),
                skip: None,
                actions: ActionFlags {
                    iterate: false,
                    maintain: false,
                    push: true,
                    audit: false,
                    release: false,
                },
                install: None,
            }],
        });
        let mut engine = Engine::new();
        engine.register(Box::new(crate::blocks::ScanDependencies));
        engine.register(Box::new(crate::blocks::AuditReleaseTag::with_registry(Arc::clone(
            &registry,
        ))));
        engine.register(Box::new(crate::blocks::AuditMainBranch::new(Arc::clone(&registry))));
        engine
            .register(Box::new(crate::blocks::RemediateVulnerability::new(Arc::clone(&registry))));
        engine.register(Box::new(crate::blocks::CommitAndPush::new(Arc::clone(&registry))));
        engine.register(Box::new(crate::blocks::CutRelease::new(Arc::clone(&registry))));
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
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

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
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        assert_eq!(
            types,
            [
                "vulnerability_detected",
                "release_tag_audited",
                "main_branch_audited",
                "auto_release_completed",
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
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        // Chain stops after release_tag_audited because AuditMainBranch
        // self-filters when vulnerable=false
        assert_eq!(types, ["vulnerability_detected", "release_tag_audited",]);
    }

    #[tokio::test]
    async fn vuln_dry_run_only_observers_emit() {
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
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        // Only observers emit under dry_run: audit blocks run, mutators are skipped
        assert_eq!(
            types,
            [
                "vulnerability_detected",
                "release_tag_audited",
                "main_branch_audited",
            ]
        );
    }

    // -- Maintenance workflow integration tests --

    /// Builds an engine with only the maintenance workflow blocks registered.
    /// `RunHoneMaintain` is intentionally excluded from `vuln_engine()` — it is
    /// maintenance-only and must not respond to vulnerability-workflow events.
    fn maintenance_engine() -> Engine {
        use foundry_core::registry::{ActionFlags, ProjectEntry, Stack};
        let registry = Arc::new(foundry_core::registry::Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: "my-project".to_string(),
                path: "/tmp".to_string(),
                stack: Stack::Rust,
                agent: "claude".to_string(),
                repo: String::new(),
                branch: "main".to_string(),
                skip: None,
                actions: ActionFlags {
                    iterate: true,
                    maintain: true,
                    ..ActionFlags::default()
                },
                install: None,
            }],
        });
        let mut engine = Engine::new();
        engine.register(Box::new(crate::blocks::RouteProjectWorkflow));
        engine.register(Box::new(crate::blocks::RunHoneIterate::new(Arc::clone(&registry))));
        engine.register(Box::new(crate::blocks::RunHoneMaintain::new(registry)));
        engine
    }

    #[tokio::test]
    async fn maintenance_chain_with_iterate_and_maintain() {
        let engine = maintenance_engine();

        // Simulate what ValidateProject would emit: status=ok, both actions enabled.
        let trigger = Event::new(
            EventType::ProjectValidationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "status": "ok",
                "actions": { "iterate": true, "maintain": true },
            }),
        );

        let result = engine.process(trigger).await;
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        // RouteProjectWorkflow (Observer) emits IterationRequested.
        // RunHoneIterate (Mutator) invokes hone — in CI hone is unavailable so
        // success=false, which means MaintenanceRequested is NOT emitted.
        // The iterate-completed event is always emitted regardless of hone success.
        assert_eq!(
            types,
            [
                "project_validation_completed",
                "iteration_requested",
                "project_iterate_completed",
            ]
        );
    }

    #[tokio::test]
    async fn maintenance_chain_maintain_only() {
        let engine = maintenance_engine();

        // iterate=false, maintain=true — router goes directly to maintenance.
        let trigger = Event::new(
            EventType::ProjectValidationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "status": "ok",
                "actions": { "iterate": false, "maintain": true },
            }),
        );

        let result = engine.process(trigger).await;
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        assert_eq!(
            types,
            [
                "project_validation_completed",
                "maintenance_requested",
                "project_maintain_completed",
            ]
        );
    }

    #[tokio::test]
    async fn maintenance_chain_validation_failure_short_circuits() {
        let engine = maintenance_engine();

        // Validation failed — router emits nothing, chain stops.
        let trigger = Event::new(
            EventType::ProjectValidationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "status": "error",
                "reason": "directory not found",
                "actions": { "iterate": true, "maintain": true },
            }),
        );

        let result = engine.process(trigger).await;
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        // Only the trigger event — no routing, no hone calls.
        assert_eq!(types, ["project_validation_completed"]);
    }

    #[tokio::test]
    async fn maintenance_chain_no_actions_short_circuits() {
        let engine = maintenance_engine();

        // Validation passed but no actions enabled — router emits nothing.
        let trigger = Event::new(
            EventType::ProjectValidationCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "status": "ok",
                "actions": { "iterate": false, "maintain": false },
            }),
        );

        let result = engine.process(trigger).await;
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        assert_eq!(types, ["project_validation_completed"]);
    }

    #[tokio::test]
    async fn maintenance_chain_dry_run_observer_emits_mutators_skipped() {
        let engine = maintenance_engine();

        // DryRun: RouteProjectWorkflow (Observer) emits IterationRequested,
        // but RunHoneIterate (Mutator) is skipped — nothing further fires.
        let trigger = Event::new(
            EventType::ProjectValidationCompleted,
            "my-project".to_string(),
            Throttle::DryRun,
            serde_json::json!({
                "status": "ok",
                "actions": { "iterate": true, "maintain": true },
            }),
        );

        let result = engine.process(trigger).await;
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        // Observer emits IterationRequested; mutators are skipped (DryRun).
        assert_eq!(types, ["project_validation_completed", "iteration_requested"]);
    }

    // -- Scan-triggered workflow integration tests --

    #[tokio::test]
    async fn scan_triggers_full_remediation_chain() {
        let engine = vuln_engine();

        // Start from scan_requested instead of vulnerability_detected.
        // The stub scanner emits a single vulnerability_detected with
        // vulnerable=true, dirty=true, which cascades through the full chain.
        let trigger = Event::new(
            EventType::ScanRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );

        let result = engine.process(trigger).await;
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        assert_eq!(
            types,
            [
                "scan_requested",
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
    async fn scan_dry_run_scans_and_audits_only() {
        let engine = vuln_engine();

        let trigger = Event::new(
            EventType::ScanRequested,
            "test-project".to_string(),
            Throttle::DryRun,
            serde_json::json!({}),
        );

        let result = engine.process(trigger).await;
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

        // ScanDependencies (observer) emits, AuditReleaseTag (observer) emits,
        // AuditMainBranch (observer) emits, then mutators are skipped.
        assert_eq!(
            types,
            [
                "scan_requested",
                "vulnerability_detected",
                "release_tag_audited",
                "main_branch_audited",
            ]
        );
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
            &[EventType::GreetRequested]
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
                        events: vec![],
                        success: false,
                        summary: "transient failure".to_string(),
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
            &[EventType::GreetRequested]
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
            EventType::GreetRequested,
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
            EventType::GreetRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({"name": "world"}),
        );

        let result = engine.process(trigger).await;

        // Verify all three events were returned in process result.
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["greet_requested", "greeting_composed", "greeting_delivered"]);

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
                e.event_type.as_str().to_string()
            })
            .collect();
        assert_eq!(written_types, ["greet_requested", "greeting_composed", "greeting_delivered"]);
    }

    #[tokio::test]
    async fn engine_without_event_writer_still_works() {
        // Confirm backward compatibility: Engine::new() with no writer configured
        // processes events and returns results normally.
        let mut engine = Engine::new();
        engine.register(Box::new(TestObserver));
        engine.register(Box::new(TestMutator));

        let trigger = Event::new(
            EventType::GreetRequested,
            "test-project".to_string(),
            Throttle::Full,
            serde_json::json!({"name": "world"}),
        );

        let result = engine.process(trigger).await;

        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["greet_requested", "greeting_composed", "greeting_delivered"]);
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
            EventType::GreetRequested,
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
            EventType::GreetRequested,
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
            EventType::GreetRequested,
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
            EventType::GreetRequested,
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
            EventType::GreetRequested,
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
}
