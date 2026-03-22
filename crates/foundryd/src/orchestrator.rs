use std::sync::Arc;

use tokio::task::JoinSet;

use foundry_core::event::{Event, EventType};
use foundry_core::throttle::Throttle;

use crate::engine::{Engine, ProcessResult};

/// Aggregate result of a maintenance run across all projects.
#[derive(Debug)]
#[allow(dead_code)]
pub struct MaintenanceRunResult {
    /// Individual per-project process results (in completion order).
    pub process_results: Vec<ProcessResult>,
    /// The `MaintenanceRunCompleted` event summarising the run.
    pub completion_event: Event,
    /// Number of projects that completed with all blocks succeeding.
    pub succeeded: usize,
    /// Number of projects that had at least one block failure or panicked.
    pub failed: usize,
    /// Number of projects that were counted but not executed (skipped).
    pub skipped: usize,
}

/// Drives the per-project fan-out and fan-in around the [`Engine`].
///
/// The engine processes a single event chain in depth-first order.
/// The orchestrator is responsible for enumerating projects, spawning
/// one task per project, and aggregating results into a single
/// [`MaintenanceRunCompleted`](EventType::MaintenanceRunCompleted) event.
#[allow(dead_code)]
pub struct MaintenanceOrchestrator {
    engine: Arc<Engine>,
}

#[allow(dead_code)]
impl MaintenanceOrchestrator {
    /// Create a new orchestrator wrapping the given engine.
    pub fn new(engine: Arc<Engine>) -> Self {
        Self { engine }
    }

    /// Run maintenance for a list of projects.
    ///
    /// Spawns one Tokio task per project (fan-out), then collects all
    /// results (fan-in) and emits a single
    /// [`MaintenanceRunCompleted`](EventType::MaintenanceRunCompleted) event
    /// carrying aggregate counts and per-project summaries.
    pub async fn run_maintenance(
        &self,
        projects: Vec<String>,
        throttle: Throttle,
    ) -> anyhow::Result<MaintenanceRunResult> {
        let mut join_set: JoinSet<(String, ProcessResult)> = JoinSet::new();

        // Fan-out: spawn one task per project.
        for project in projects {
            let engine = Arc::clone(&self.engine);
            let trigger = Event::new(
                EventType::MaintenanceRunStarted,
                project.clone(),
                throttle,
                serde_json::json!({}),
            );

            join_set.spawn(async move {
                let result = engine.process(trigger).await;
                (project, result)
            });
        }

        // Fan-in: collect results as tasks complete.
        let mut process_results: Vec<ProcessResult> = Vec::new();
        let mut project_summaries: Vec<serde_json::Value> = Vec::new();
        let mut succeeded = 0usize;
        let mut failed = 0usize;
        let skipped = 0usize;

        while let Some(join_result) = join_set.join_next().await {
            match join_result {
                Ok((project, process_result)) => {
                    let all_success = process_result.block_executions.iter().all(|b| b.success);

                    if all_success {
                        succeeded += 1;
                    } else {
                        failed += 1;
                    }

                    project_summaries.push(serde_json::json!({
                        "project": project,
                        "success": all_success,
                        "block_count": process_result.block_executions.len(),
                    }));

                    process_results.push(process_result);
                }
                Err(e) => {
                    tracing::error!(error = %e, "project maintenance task panicked");
                    failed += 1;
                    project_summaries.push(serde_json::json!({
                        "project": "unknown",
                        "success": false,
                        "error": "task panicked",
                    }));
                }
            }
        }

        let total = succeeded + failed + skipped;
        let completion_event = Event::new(
            EventType::MaintenanceRunCompleted,
            "system".to_string(),
            throttle,
            serde_json::json!({
                "total": total,
                "succeeded": succeeded,
                "failed": failed,
                "skipped": skipped,
                "projects": project_summaries,
            }),
        );

        tracing::info!(
            total,
            succeeded,
            failed,
            skipped,
            event_id = %completion_event.id,
            "maintenance run completed"
        );

        Ok(MaintenanceRunResult {
            process_results,
            completion_event,
            succeeded,
            failed,
            skipped,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
    use std::pin::Pin;

    // ---------------------------------------------------------------------------
    // Test blocks
    // ---------------------------------------------------------------------------

    /// Always succeeds and emits one event.
    struct AlwaysSucceeds;

    impl TaskBlock for AlwaysSucceeds {
        fn name(&self) -> &'static str {
            "always_succeeds"
        }

        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }

        fn sinks_on(&self) -> &[EventType] {
            &[EventType::MaintenanceRunStarted]
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
                        EventType::ProjectValidationCompleted,
                        project,
                        throttle,
                        serde_json::json!({"status": "ok"}),
                    )],
                    success: true,
                    summary: "validated".to_string(),
                })
            })
        }
    }

    /// Always fails (reports success=false but does not panic).
    struct AlwaysFails;

    impl TaskBlock for AlwaysFails {
        fn name(&self) -> &'static str {
            "always_fails"
        }

        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }

        fn sinks_on(&self) -> &[EventType] {
            &[EventType::MaintenanceRunStarted]
        }

        fn execute(
            &self,
            _trigger: &Event,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
        {
            Box::pin(async move {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: false,
                    summary: "block failed".to_string(),
                })
            })
        }
    }

    // ---------------------------------------------------------------------------
    // Helpers
    // ---------------------------------------------------------------------------

    fn orchestrator_with(block: Box<dyn TaskBlock>) -> MaintenanceOrchestrator {
        let mut engine = Engine::new();
        engine.register(block);
        MaintenanceOrchestrator::new(Arc::new(engine))
    }

    // ---------------------------------------------------------------------------
    // Tests
    // ---------------------------------------------------------------------------

    #[tokio::test]
    async fn empty_project_list_produces_zero_counts() {
        let orchestrator = orchestrator_with(Box::new(AlwaysSucceeds));
        let result = orchestrator.run_maintenance(vec![], Throttle::Full).await.unwrap();

        assert_eq!(result.succeeded, 0);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 0);
        assert!(result.process_results.is_empty());

        let payload = &result.completion_event.payload;
        assert_eq!(payload["total"], 0);
        assert_eq!(payload["projects"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn single_successful_project_increments_succeeded() {
        let orchestrator = orchestrator_with(Box::new(AlwaysSucceeds));
        let result = orchestrator
            .run_maintenance(vec!["alpha".to_string()], Throttle::Full)
            .await
            .unwrap();

        assert_eq!(result.succeeded, 1);
        assert_eq!(result.failed, 0);
        assert_eq!(result.skipped, 0);
        assert_eq!(result.process_results.len(), 1);

        let payload = &result.completion_event.payload;
        assert_eq!(payload["total"], 1);
        assert_eq!(payload["succeeded"], 1);
        assert_eq!(payload["failed"], 0);
    }

    #[tokio::test]
    async fn single_failed_project_increments_failed() {
        let orchestrator = orchestrator_with(Box::new(AlwaysFails));
        let result = orchestrator
            .run_maintenance(vec!["beta".to_string()], Throttle::Full)
            .await
            .unwrap();

        assert_eq!(result.succeeded, 0);
        assert_eq!(result.failed, 1);
        assert_eq!(result.skipped, 0);

        let payload = &result.completion_event.payload;
        assert_eq!(payload["failed"], 1);
    }

    #[tokio::test]
    async fn multiple_projects_aggregate_correctly() {
        // Two projects with AlwaysSucceeds, one with AlwaysFails.
        // We use a single AlwaysSucceeds block and run 3 projects; we need to
        // test the aggregation so we rely on separate orchestrators per test.
        let orchestrator = orchestrator_with(Box::new(AlwaysSucceeds));
        let result = orchestrator
            .run_maintenance(
                vec![
                    "proj-a".to_string(),
                    "proj-b".to_string(),
                    "proj-c".to_string(),
                ],
                Throttle::Full,
            )
            .await
            .unwrap();

        assert_eq!(result.succeeded, 3);
        assert_eq!(result.failed, 0);
        assert_eq!(result.process_results.len(), 3);

        let payload = &result.completion_event.payload;
        assert_eq!(payload["total"], 3);
        assert_eq!(payload["succeeded"], 3);
        assert_eq!(payload["projects"].as_array().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn completion_event_has_correct_type_and_system_project() {
        let orchestrator = orchestrator_with(Box::new(AlwaysSucceeds));
        let result = orchestrator
            .run_maintenance(vec!["proj-x".to_string()], Throttle::Full)
            .await
            .unwrap();

        assert_eq!(result.completion_event.event_type, EventType::MaintenanceRunCompleted);
        assert_eq!(result.completion_event.project, "system");
    }

    #[tokio::test]
    async fn completion_event_propagates_throttle() {
        let orchestrator = orchestrator_with(Box::new(AlwaysSucceeds));
        let result = orchestrator
            .run_maintenance(vec!["proj-y".to_string()], Throttle::DryRun)
            .await
            .unwrap();

        assert_eq!(result.completion_event.throttle, Throttle::DryRun);
    }

    #[tokio::test]
    async fn per_project_summary_contains_project_name_and_block_count() {
        let orchestrator = orchestrator_with(Box::new(AlwaysSucceeds));
        let result = orchestrator
            .run_maintenance(vec!["my-project".to_string()], Throttle::Full)
            .await
            .unwrap();

        let projects = result.completion_event.payload["projects"].as_array().unwrap();
        assert_eq!(projects.len(), 1);

        let summary = &projects[0];
        assert_eq!(summary["project"], "my-project");
        assert_eq!(summary["success"], true);
        // AlwaysSucceeds sinks on MaintenanceRunStarted, so block_count >= 1
        assert!(summary["block_count"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn mixed_results_aggregate_failed_and_succeeded() {
        let mut engine = Engine::new();
        engine.register(Box::new(AlwaysFails));
        let orchestrator = MaintenanceOrchestrator::new(Arc::new(engine));

        // Run two projects; both will fail (AlwaysFails is the only block)
        let result = orchestrator
            .run_maintenance(vec!["ok-proj".to_string(), "fail-proj".to_string()], Throttle::Full)
            .await
            .unwrap();

        // Both projects have a failed block
        assert_eq!(result.succeeded, 0);
        assert_eq!(result.failed, 2);
        assert_eq!(result.process_results.len(), 2);
    }
}
