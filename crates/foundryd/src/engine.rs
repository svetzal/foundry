use tokio::sync::broadcast;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::TaskBlock;

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
}

/// The full result of processing an event chain.
#[derive(Debug, Clone)]
pub struct ProcessResult {
    /// All events produced during the chain (including the root).
    pub events: Vec<Event>,
    /// Record of each block execution in order.
    pub block_executions: Vec<BlockExecution>,
}

/// The workflow engine routes events to task blocks and manages propagation.
pub struct Engine {
    blocks: Vec<Box<dyn TaskBlock>>,
    /// Optional broadcast channel for real-time event streaming to Watch clients.
    event_tx: Option<broadcast::Sender<Event>>,
}

impl Engine {
    pub fn new() -> Self {
        Self {
            blocks: vec![],
            event_tx: None,
        }
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

    /// Process an event: find matching task blocks, execute them, and propagate
    /// any emitted events through the chain.
    pub async fn process(&self, event: Event) -> ProcessResult {
        let process_span = tracing::info_span!(
            "process",
            root_event_id = %event.id,
            root_event_type = %event.event_type,
        );
        let _process_guard = process_span.enter();

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

                if !block.should_execute(current.throttle) {
                    tracing::info!("skipped (throttle)");
                    block_executions.push(BlockExecution {
                        block_name: block.name().to_string(),
                        trigger_event_id: current.id.clone(),
                        success: true,
                        summary: "skipped (throttle)".to_string(),
                        emitted_event_ids: vec![],
                    });
                    continue;
                }

                match block.execute(&current).await {
                    Ok(result) => {
                        tracing::info!(
                            success = result.success,
                            summary = %result.summary,
                            emitted = result.events.len(),
                            "completed"
                        );

                        let mut emitted_ids = Vec::new();
                        if block.should_emit(current.throttle) {
                            for emitted in result.events {
                                emitted_ids.push(emitted.id.clone());
                                // Broadcast each emitted event in real time.
                                if let Some(tx) = &self.event_tx {
                                    let _ = tx.send(emitted.clone()); // No receivers is normal.
                                }
                                all_events.push(emitted.clone());
                                queue.push(emitted);
                            }
                        } else {
                            tracing::info!(
                                suppressed = result.events.len(),
                                "emission suppressed by throttle"
                            );
                        }

                        block_executions.push(BlockExecution {
                            block_name: block.name().to_string(),
                            trigger_event_id: current.id.clone(),
                            success: result.success,
                            summary: result.summary,
                            emitted_event_ids: emitted_ids,
                        });
                    }
                    Err(err) => {
                        tracing::error!(error = %err, "failed");
                        block_executions.push(BlockExecution {
                            block_name: block.name().to_string(),
                            trigger_event_id: current.id.clone(),
                            success: false,
                            summary: format!("error: {err}"),
                            emitted_event_ids: vec![],
                        });
                    }
                }
            }
        }

        ProcessResult {
            events: all_events,
            block_executions,
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
        let mut engine = Engine::new();
        engine.register(Box::new(crate::blocks::ScanDependencies));
        engine.register(Box::new(crate::blocks::AuditReleaseTag::new()));
        engine.register(Box::new(crate::blocks::AuditMainBranch));
        engine.register(Box::new(crate::blocks::RemediateVulnerability));
        engine.register(Box::new(crate::blocks::CommitAndPush));
        engine.register(Box::new(crate::blocks::CutRelease));
        engine.register(Box::new(crate::blocks::WatchPipeline));
        engine.register(Box::new(crate::blocks::InstallLocally));
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
}
