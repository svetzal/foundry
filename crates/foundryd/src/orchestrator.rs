// The Orchestrator is wired into main.rs in a later task (CLI "run" command).
// Until then, all public items are intentionally unused from the binary.
#![allow(dead_code)]

use std::sync::Arc;

use anyhow::Result;
use tokio::task::JoinSet;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::throttle::Throttle;

use crate::engine::{Engine, ProcessResult};

/// Runs maintenance for all active projects in the registry, fan-out style.
///
/// The orchestrator is intentionally separate from the [`Engine`]. The engine
/// processes a single event chain depth-first. The orchestrator handles
/// project enumeration, parallel spawning via [`JoinSet`], and result
/// aggregation. This keeps the engine simple and independently testable.
pub struct Orchestrator {
    registry: Arc<Registry>,
    engine: Arc<Engine>,
    max_concurrent: usize,
}

impl Orchestrator {
    /// Create a new orchestrator with a default concurrency limit of 10.
    pub fn new(registry: Arc<Registry>, engine: Arc<Engine>) -> Self {
        Self {
            registry,
            engine,
            max_concurrent: 10,
        }
    }

    /// Override the maximum number of project tasks that run concurrently.
    pub fn with_max_concurrent(mut self, max: usize) -> Self {
        self.max_concurrent = max;
        self
    }

    /// Run maintenance for all active projects concurrently.
    ///
    /// For each active project a [`MaintenanceRunStarted`][EventType::MaintenanceRunStarted]
    /// event is injected into the engine. The per-project event chains run
    /// in parallel, bounded by `max_concurrent`. All [`ProcessResult`]s are
    /// collected and returned once every task completes.
    ///
    /// A task that panics is logged as an error but does not abort the run —
    /// remaining projects continue processing.
    pub async fn run_maintenance(&self, throttle: Throttle) -> Result<Vec<ProcessResult>> {
        let active = self.registry.active_projects();

        if active.is_empty() {
            tracing::info!("no active projects in registry");
            return Ok(vec![]);
        }

        let semaphore = Arc::new(tokio::sync::Semaphore::new(self.max_concurrent));
        let mut join_set: JoinSet<ProcessResult> = JoinSet::new();

        for project in active {
            let project_name = project.name.clone();
            let engine = Arc::clone(&self.engine);
            let sem = Arc::clone(&semaphore);

            join_set.spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore closed unexpectedly");

                tracing::info!(project = %project_name, "starting maintenance run");

                let event = Event::new(
                    EventType::MaintenanceRunStarted,
                    project_name.clone(),
                    throttle,
                    serde_json::json!({ "project": project_name }),
                );

                engine.process(event).await
            });
        }

        let mut results = Vec::new();
        while let Some(outcome) = join_set.join_next().await {
            match outcome {
                Ok(process_result) => results.push(process_result),
                Err(join_err) => {
                    tracing::error!(error = %join_err, "project maintenance task panicked");
                }
            }
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry};

    fn make_registry(project_names: &[&str]) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: project_names
                .iter()
                .map(|name| ProjectEntry {
                    name: name.to_string(),
                    path: "/tmp/test".to_string(),
                    stack: foundry_core::registry::Stack::Rust,
                    agent: String::new(),
                    repo: String::new(),
                    branch: "main".to_string(),
                    skip: Some(false),
                    actions: ActionFlags::default(),
                    install: None,
                })
                .collect(),
        })
    }

    fn make_engine() -> Arc<Engine> {
        Arc::new(Engine::new())
    }

    #[tokio::test]
    async fn run_maintenance_with_empty_registry_returns_empty() {
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![],
        });
        let orchestrator = Orchestrator::new(registry, make_engine());

        let results = orchestrator.run_maintenance(Throttle::Full).await.unwrap();

        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn run_maintenance_spawns_one_task_per_active_project() {
        let registry = make_registry(&["alpha", "beta", "gamma"]);
        let orchestrator = Orchestrator::new(registry, make_engine());

        let results = orchestrator.run_maintenance(Throttle::Full).await.unwrap();

        // Three active projects → three ProcessResults
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn each_result_contains_maintenance_run_started_event() {
        let registry = make_registry(&["alpha", "beta"]);
        let orchestrator = Orchestrator::new(registry, make_engine());

        let results = orchestrator.run_maintenance(Throttle::Full).await.unwrap();

        for result in &results {
            let has_start =
                result.events.iter().any(|e| e.event_type == EventType::MaintenanceRunStarted);
            assert!(has_start, "expected MaintenanceRunStarted in result events");
        }
    }

    #[tokio::test]
    async fn throttle_level_propagated_to_all_events() {
        let registry = make_registry(&["alpha", "beta"]);
        let orchestrator = Orchestrator::new(registry, make_engine());

        let results = orchestrator.run_maintenance(Throttle::DryRun).await.unwrap();

        for result in &results {
            for event in &result.events {
                assert_eq!(
                    event.throttle,
                    Throttle::DryRun,
                    "throttle must be DryRun on event {}",
                    event.id
                );
            }
        }
    }

    #[tokio::test]
    async fn skipped_projects_not_processed() {
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![
                ProjectEntry {
                    name: "active".to_string(),
                    path: "/tmp/active".to_string(),
                    stack: foundry_core::registry::Stack::Rust,
                    agent: String::new(),
                    repo: String::new(),
                    branch: "main".to_string(),
                    skip: Some(false),
                    actions: ActionFlags::default(),
                    install: None,
                },
                ProjectEntry {
                    name: "skipped".to_string(),
                    path: "/tmp/skipped".to_string(),
                    stack: foundry_core::registry::Stack::Rust,
                    agent: String::new(),
                    repo: String::new(),
                    branch: "main".to_string(),
                    skip: Some(true),
                    actions: ActionFlags::default(),
                    install: None,
                },
            ],
        });

        let orchestrator = Orchestrator::new(registry, make_engine());
        let results = orchestrator.run_maintenance(Throttle::Full).await.unwrap();

        // Only the active project is processed
        assert_eq!(results.len(), 1);

        let project_name = &results[0].events[0].project;
        assert_eq!(project_name, "active");
    }

    #[tokio::test]
    async fn with_max_concurrent_is_honoured() {
        // Verify the builder method sets the field and the run completes
        // successfully with a restrictive concurrency limit.
        let registry = make_registry(&["a", "b", "c", "d", "e"]);
        let orchestrator = Orchestrator::new(registry, make_engine()).with_max_concurrent(2);

        let results = orchestrator.run_maintenance(Throttle::Full).await.unwrap();

        assert_eq!(results.len(), 5);
    }

    #[tokio::test]
    async fn single_project_produces_one_result() {
        let registry = make_registry(&["solo"]);
        let orchestrator = Orchestrator::new(registry, make_engine());

        let results = orchestrator.run_maintenance(Throttle::AuditOnly).await.unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].events[0].project, "solo");
    }
}
