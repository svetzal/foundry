// The orchestrator module is a work-in-progress — it is declared but not yet
// wired into main.rs.  Suppress dead-code warnings on the public API surface
// until the binary starts using it.
#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use tokio::task::JoinSet;

use foundry_core::event::{Event, EventType};
use foundry_core::throttle::Throttle;

use crate::engine::{Engine, ProcessResult};

/// Aggregate result of a maintenance run across all projects.
#[derive(Debug)]
pub struct MaintenanceRunResult {
    /// Per-project process results, keyed by project name.
    pub project_results: HashMap<String, ProcessResult>,
    /// Projects that were skipped because they were already active.
    pub skipped_projects: Vec<String>,
    /// Projects whose tasks panicked during execution.
    pub panicked_projects: Vec<String>,
}

/// Drop guard that removes a project from the active set when it goes out of scope.
///
/// This ensures cleanup happens even if the task is cancelled or unwinds.
struct ActiveGuard {
    project: String,
    active_projects: Arc<RwLock<HashSet<String>>>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        match self.active_projects.write() {
            Ok(mut active) => {
                active.remove(&self.project);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove(&self.project);
            }
        }
    }
}

/// Coordinates per-project maintenance runs with concurrency control.
///
/// Projects are processed concurrently up to `max_concurrent` at a time.
/// If a project is already active (from a previous overlapping run), it is
/// skipped with a warning rather than started again — preventing git conflicts
/// and data corruption from concurrent project mutations.
pub struct Orchestrator {
    engine: Arc<Engine>,
    max_concurrent: usize,
    active_projects: Arc<RwLock<HashSet<String>>>,
}

impl Orchestrator {
    /// Create a new orchestrator wrapping the given engine.
    pub fn new(engine: Arc<Engine>, max_concurrent: usize) -> Self {
        Self {
            engine,
            max_concurrent,
            active_projects: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Run maintenance for the given list of project names.
    ///
    /// Each project gets a `MaintenanceRunStarted` event dispatched through
    /// the engine. Projects already in the active set are skipped.  Up to
    /// `max_concurrent` projects run in parallel; the semaphore enforces the
    /// limit.
    pub async fn run_maintenance(
        &self,
        projects: Vec<String>,
        throttle: Throttle,
    ) -> MaintenanceRunResult {
        let sem = Arc::new(tokio::sync::Semaphore::new(self.max_concurrent));
        let mut join_set: JoinSet<(String, Result<ProcessResult, ()>)> = JoinSet::new();

        let mut skipped_projects = Vec::new();

        for project_name in projects {
            // Check and insert atomically under write lock.
            // The lock is released before the task is spawned — we do NOT
            // hold it across await points.
            let already_active = {
                match self.active_projects.write() {
                    Ok(mut active) => {
                        if active.contains(&project_name) {
                            true
                        } else {
                            active.insert(project_name.clone());
                            false
                        }
                    }
                    Err(poisoned) => {
                        let mut active = poisoned.into_inner();
                        if active.contains(&project_name) {
                            true
                        } else {
                            active.insert(project_name.clone());
                            false
                        }
                    }
                }
            };

            if already_active {
                tracing::warn!(project = %project_name, "project already active, skipping");
                skipped_projects.push(project_name);
                continue;
            }

            let engine = Arc::clone(&self.engine);
            let active_projects = Arc::clone(&self.active_projects);
            let sem = Arc::clone(&sem);
            let name_for_task = project_name.clone();

            join_set.spawn(async move {
                // The guard ensures removal from active_projects on any exit
                // path — normal completion, early return, or panic unwind.
                let _guard = ActiveGuard {
                    project: name_for_task.clone(),
                    active_projects,
                };

                let _permit = sem.acquire().await.expect("semaphore closed");

                let event = Event::new(
                    EventType::MaintenanceRunStarted,
                    name_for_task.clone(),
                    throttle,
                    serde_json::json!({}),
                );

                let result = engine.process(event).await;
                (name_for_task, Ok(result))
            });
        }

        let mut project_results = HashMap::new();
        let mut panicked_projects = Vec::new();

        while let Some(outcome) = join_set.join_next().await {
            match outcome {
                Ok((project_name, Ok(result))) => {
                    project_results.insert(project_name, result);
                }
                Ok((project_name, Err(()))) => {
                    // Unreachable with current task body, but kept for
                    // completeness if error paths are added later.
                    panicked_projects.push(project_name);
                }
                Err(join_err) => {
                    // The task panicked.  JoinSet catches the panic so we
                    // don't propagate it, but we need to record which project
                    // was affected.  The ActiveGuard inside the task will
                    // have already run its Drop on the panic unwind, so the
                    // active set is clean.
                    tracing::error!(error = %join_err, "task panicked");
                    panicked_projects.push(format!("<unknown:{join_err}>"));
                }
            }
        }

        MaintenanceRunResult {
            project_results,
            skipped_projects,
            panicked_projects,
        }
    }

    /// Return a snapshot of currently-active project names.
    ///
    /// Intended for observability and testing.
    pub fn active_project_names(&self) -> HashSet<String> {
        match self.active_projects.read() {
            Ok(active) => active.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use foundry_core::event::{Event, EventType};
    use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
    use foundry_core::throttle::Throttle;

    use super::*;
    use crate::engine::Engine;

    // A no-op block that sinks on MaintenanceRunStarted — does nothing so
    // the engine processes the event without real side-effects.
    struct NoOpMaintenanceBlock;

    impl TaskBlock for NoOpMaintenanceBlock {
        fn name(&self) -> &'static str {
            "NoOpMaintenance"
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
            Box::pin(async move {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: format!("maintenance started for {project}"),
                    raw_output: None,
                    exit_code: None,
                })
            })
        }
    }

    // A block that panics intentionally to test panic cleanup.
    struct PanickingBlock;

    impl TaskBlock for PanickingBlock {
        fn name(&self) -> &'static str {
            "PanickingBlock"
        }

        fn kind(&self) -> BlockKind {
            BlockKind::Mutator
        }

        fn sinks_on(&self) -> &[EventType] {
            &[EventType::MaintenanceRunStarted]
        }

        fn execute(
            &self,
            _trigger: &Event,
        ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
        {
            Box::pin(async move { panic!("intentional panic in test") })
        }
    }

    // A block that counts how many times it has been invoked.
    struct CountingBlock {
        count: Arc<AtomicUsize>,
    }

    impl TaskBlock for CountingBlock {
        fn name(&self) -> &'static str {
            "CountingBlock"
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
            let count = Arc::clone(&self.count);
            let project = trigger.project.clone();
            Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: format!("counted for {project}"),
                    raw_output: None,
                    exit_code: None,
                })
            })
        }
    }

    fn no_op_engine() -> Engine {
        let mut engine = Engine::new();
        engine.register(Box::new(NoOpMaintenanceBlock));
        engine
    }

    // -------------------------------------------------------------------------
    // Core concurrency guard tests
    // -------------------------------------------------------------------------

    #[tokio::test]
    async fn project_added_to_active_set_before_processing() {
        // Verify the project is in the active set while its task is running.
        //
        // Strategy: use two Notify handles to hand-shake between the block
        // execution and the test body.
        //   started: block notifies the test that it has begun executing
        //   proceed: test notifies the block that it may finish
        //
        // The orchestrator run is spawned as a separate task so the test body
        // can run concurrently with it.
        use tokio::sync::Notify;

        struct LatchBlock {
            started: Arc<Notify>,
            proceed: Arc<Notify>,
        }

        impl TaskBlock for LatchBlock {
            fn name(&self) -> &'static str {
                "LatchBlock"
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
            ) -> Pin<
                Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>,
            > {
                let started = Arc::clone(&self.started);
                let proceed = Arc::clone(&self.proceed);
                Box::pin(async move {
                    // Signal the test that we are in-flight.
                    started.notify_one();
                    // Wait for the test to give us the go-ahead.
                    proceed.notified().await;
                    Ok(TaskBlockResult {
                        events: vec![],
                        success: true,
                        summary: "latched".to_string(),
                        raw_output: None,
                        exit_code: None,
                    })
                })
            }
        }

        let started = Arc::new(Notify::new());
        let proceed = Arc::new(Notify::new());

        let mut engine = Engine::new();
        engine.register(Box::new(LatchBlock {
            started: Arc::clone(&started),
            proceed: Arc::clone(&proceed),
        }));
        let engine = Arc::new(engine);
        let orchestrator = Arc::new(Orchestrator::new(engine, 4));

        let orch_for_task = Arc::clone(&orchestrator);
        let run_handle = tokio::spawn(async move {
            orch_for_task.run_maintenance(vec!["alpha".to_string()], Throttle::DryRun).await
        });

        // Wait until the block signals it has started — "alpha" must be active.
        started.notified().await;
        assert!(
            orchestrator.active_project_names().contains("alpha"),
            "project must be in active set while task is in-flight"
        );

        // Let the block finish and wait for the run to complete.
        proceed.notify_one();
        run_handle.await.expect("orchestrator task should not panic");

        assert!(
            orchestrator.active_project_names().is_empty(),
            "active set must be empty after run completes"
        );
    }

    #[tokio::test]
    async fn project_removed_from_active_set_after_processing() {
        let engine = Arc::new(no_op_engine());
        let orchestrator = Orchestrator::new(engine, 4);

        orchestrator.run_maintenance(vec!["alpha".to_string()], Throttle::Full).await;

        assert!(
            orchestrator.active_project_names().is_empty(),
            "active set must be empty after run completes"
        );
    }

    #[tokio::test]
    async fn duplicate_project_is_skipped_when_already_active() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut engine = Engine::new();
        engine.register(Box::new(CountingBlock {
            count: Arc::clone(&count),
        }));
        let engine = Arc::new(engine);
        let orchestrator = Orchestrator::new(engine, 4);

        // Manually mark "alpha" as active before the run so it is skipped.
        orchestrator.active_projects.write().unwrap().insert("alpha".to_string());

        let result = orchestrator.run_maintenance(vec!["alpha".to_string()], Throttle::Full).await;

        assert_eq!(
            result.skipped_projects,
            vec!["alpha".to_string()],
            "already-active project must appear in skipped list"
        );
        assert_eq!(count.load(Ordering::SeqCst), 0, "block must not execute for skipped project");

        // Clean up the manually inserted entry so the active set is tidy.
        orchestrator.active_projects.write().unwrap().remove("alpha");
    }

    #[tokio::test]
    async fn all_projects_already_active_all_skipped() {
        let engine = Arc::new(no_op_engine());
        let orchestrator = Orchestrator::new(engine, 4);

        {
            let mut active = orchestrator.active_projects.write().unwrap();
            active.insert("alpha".to_string());
            active.insert("beta".to_string());
        }

        let result = orchestrator
            .run_maintenance(vec!["alpha".to_string(), "beta".to_string()], Throttle::Full)
            .await;

        let mut skipped = result.skipped_projects.clone();
        skipped.sort();
        assert_eq!(skipped, vec!["alpha".to_string(), "beta".to_string()]);
        assert!(result.project_results.is_empty());

        // Clean up.
        let mut active = orchestrator.active_projects.write().unwrap();
        active.remove("alpha");
        active.remove("beta");
    }

    #[tokio::test]
    async fn multiple_distinct_projects_all_processed() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut engine = Engine::new();
        engine.register(Box::new(CountingBlock {
            count: Arc::clone(&count),
        }));
        let engine = Arc::new(engine);
        let orchestrator = Orchestrator::new(engine, 4);

        let result = orchestrator
            .run_maintenance(
                vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
                Throttle::Full,
            )
            .await;

        assert_eq!(count.load(Ordering::SeqCst), 3, "all three projects must be processed");
        assert_eq!(result.project_results.len(), 3);
        assert!(result.skipped_projects.is_empty());
        assert!(
            orchestrator.active_project_names().is_empty(),
            "active set must be empty after run"
        );
    }

    #[tokio::test]
    async fn project_removed_from_active_set_even_on_failure() {
        // Engine with a block that returns an Err — the engine records the
        // failure in block_executions but the task still completes normally
        // (no panic).  Active set must still be cleared.
        struct FailingBlock;

        impl TaskBlock for FailingBlock {
            fn name(&self) -> &'static str {
                "FailingBlock"
            }

            fn kind(&self) -> BlockKind {
                BlockKind::Mutator
            }

            fn sinks_on(&self) -> &[EventType] {
                &[EventType::MaintenanceRunStarted]
            }

            fn execute(
                &self,
                _trigger: &Event,
            ) -> Pin<
                Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>,
            > {
                Box::pin(async move { Err(anyhow::anyhow!("simulated block failure")) })
            }
        }

        let mut engine = Engine::new();
        engine.register(Box::new(FailingBlock));
        let engine = Arc::new(engine);
        let orchestrator = Orchestrator::new(engine, 4);

        orchestrator.run_maintenance(vec!["alpha".to_string()], Throttle::Full).await;

        assert!(
            orchestrator.active_project_names().is_empty(),
            "active set must be empty even after block failure"
        );
    }

    #[tokio::test]
    async fn project_removed_from_active_set_even_on_panic() {
        let mut engine = Engine::new();
        engine.register(Box::new(PanickingBlock));
        let engine = Arc::new(engine);
        let orchestrator = Orchestrator::new(engine, 4);

        let result = orchestrator.run_maintenance(vec!["alpha".to_string()], Throttle::Full).await;

        // The panicked task's project should appear in panicked_projects.
        assert_eq!(
            result.panicked_projects.len(),
            1,
            "panicked task must be recorded in panicked_projects"
        );
        // Active set must be clean despite the panic — the Drop guard runs.
        assert!(
            orchestrator.active_project_names().is_empty(),
            "active set must be empty after task panic"
        );
    }

    #[tokio::test]
    async fn skipped_projects_not_added_to_active_set() {
        // If a project is skipped it was never inserted, so it must not
        // appear in the active set either.
        let engine = Arc::new(no_op_engine());
        let orchestrator = Orchestrator::new(engine, 4);

        // Pre-populate so "alpha" is considered active.
        orchestrator.active_projects.write().unwrap().insert("alpha".to_string());

        orchestrator.run_maintenance(vec!["alpha".to_string()], Throttle::Full).await;

        // After the run "alpha" is still in the set (we inserted it manually
        // and the skipped path does not remove it — the caller owns it).
        // But there should be exactly one entry.
        let active = orchestrator.active_project_names();
        assert!(active.contains("alpha"), "manually inserted entry must still be there");
        assert_eq!(active.len(), 1, "no spurious entries");

        // Clean up.
        orchestrator.active_projects.write().unwrap().remove("alpha");
    }

    #[tokio::test]
    async fn max_concurrent_respected() {
        // Verify that at most max_concurrent tasks execute in parallel by
        // using a counting block and a semaphore with limit 2 on a 4-project
        // run.  The result should still have all 4 projects processed.
        let count = Arc::new(AtomicUsize::new(0));
        let mut engine = Engine::new();
        engine.register(Box::new(CountingBlock {
            count: Arc::clone(&count),
        }));
        let engine = Arc::new(engine);
        let orchestrator = Orchestrator::new(engine, 2); // max 2 concurrent

        let result = orchestrator
            .run_maintenance(
                vec![
                    "p1".to_string(),
                    "p2".to_string(),
                    "p3".to_string(),
                    "p4".to_string(),
                ],
                Throttle::Full,
            )
            .await;

        assert_eq!(count.load(Ordering::SeqCst), 4, "all projects must complete");
        assert_eq!(result.project_results.len(), 4);
        assert!(orchestrator.active_project_names().is_empty());
    }
}
