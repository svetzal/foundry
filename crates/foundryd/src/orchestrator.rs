use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Fans out a system-level maintenance run to individual per-project runs.
///
/// Observer — always runs regardless of throttle.
///
/// Sinks on `MaintenanceRunStarted`. When the triggering event's project is
/// `"system"` (emitted by `foundry run` without `--project`), the block reads
/// the registry, collects all active (non-skipped) projects, and emits a
/// `MaintenanceRunStarted` event for each one. These per-project events then
/// trigger the existing chain (`ValidateProject` → `RouteProjectWorkflow` → …).
///
/// A `MaintenanceRunCompleted` event is emitted first in the list so that the
/// engine's LIFO queue processes it *last*, after all per-project chains finish.
///
/// When the triggering project is anything other than `"system"`, the block
/// returns immediately with no emitted events — the per-project event is
/// handled by downstream blocks.
pub struct FanOutMaintenance {
    registry: Arc<Registry>,
}

impl FanOutMaintenance {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl TaskBlock for FanOutMaintenance {
    fn name(&self) -> &'static str {
        "Fan Out Maintenance"
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
        let registry = Arc::clone(&self.registry);

        Box::pin(async move {
            if project != "system" {
                return Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: format!("per-project run (project={project}), fan-out not applicable"),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                });
            }

            let active = registry.active_projects();
            let project_names: Vec<String> = active.iter().map(|p| p.name.clone()).collect();
            let active_count = project_names.len();
            let skipped_count = registry.projects.len() - active_count;

            tracing::info!(
                active = active_count,
                skipped = skipped_count,
                "fanning out maintenance to active projects"
            );

            // The engine processes emitted events via a LIFO queue (Vec::pop).
            // Place MaintenanceRunCompleted first so it is processed *last*,
            // after all per-project chains have completed.
            let mut events = Vec::with_capacity(active_count + 1);

            events.push(Event::new(
                EventType::MaintenanceRunCompleted,
                "system".to_string(),
                throttle,
                serde_json::json!({
                    "project_count": active_count,
                    "skipped_count": skipped_count,
                    "projects": project_names,
                }),
            ));

            for name in &project_names {
                events.push(Event::new(
                    EventType::MaintenanceRunStarted,
                    name.clone(),
                    throttle,
                    serde_json::json!({}),
                ));
            }

            Ok(TaskBlockResult {
                events,
                success: true,
                summary: format!(
                    "fanned out to {active_count} active projects ({skipped_count} skipped)"
                ),
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use super::*;

    fn make_registry(entries: Vec<ProjectEntry>) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: entries,
        })
    }

    fn active_entry(name: &str) -> ProjectEntry {
        ProjectEntry {
            name: name.to_string(),
            path: format!("/projects/{name}"),
            stack: Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: ActionFlags::default(),
            install: None,
            timeout_secs: None,
        }
    }

    fn skipped_entry(name: &str) -> ProjectEntry {
        ProjectEntry {
            name: name.to_string(),
            path: format!("/projects/{name}"),
            stack: Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: Some("reason".to_string()),
            notes: None,
            actions: ActionFlags::default(),
            install: None,
            timeout_secs: None,
        }
    }

    fn system_trigger(throttle: Throttle) -> Event {
        Event::new(
            EventType::MaintenanceRunStarted,
            "system".to_string(),
            throttle,
            serde_json::json!({}),
        )
    }

    fn project_trigger(project: &str) -> Event {
        Event::new(
            EventType::MaintenanceRunStarted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
    }

    // -- Metadata tests --

    #[test]
    fn sinks_on_maintenance_run_started() {
        let block = FanOutMaintenance::new(make_registry(vec![]));
        assert_eq!(block.sinks_on(), &[EventType::MaintenanceRunStarted]);
    }

    #[test]
    fn kind_is_observer() {
        let block = FanOutMaintenance::new(make_registry(vec![]));
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    // -- Self-filter tests --

    #[tokio::test]
    async fn non_system_project_emits_no_events() {
        let block = FanOutMaintenance::new(make_registry(vec![active_entry("alpha")]));
        let trigger = project_trigger("alpha");

        let result = block.execute(&trigger).await.expect("should succeed");
        assert!(result.success);
        assert!(result.events.is_empty());
    }

    // -- Fan-out tests --

    #[tokio::test]
    async fn system_trigger_emits_per_project_events() {
        let registry = make_registry(vec![
            active_entry("alpha"),
            active_entry("beta"),
            active_entry("gamma"),
        ]);
        let block = FanOutMaintenance::new(registry);
        let trigger = system_trigger(Throttle::DryRun);

        let result = block.execute(&trigger).await.expect("should succeed");
        assert!(result.success);

        // 1 MaintenanceRunCompleted + 3 per-project MaintenanceRunStarted
        assert_eq!(result.events.len(), 4);

        // First event is MaintenanceRunCompleted (processed last by LIFO queue).
        assert_eq!(result.events[0].event_type, EventType::MaintenanceRunCompleted);
        assert_eq!(result.events[0].project, "system");

        // Remaining events are per-project MaintenanceRunStarted.
        let project_events: Vec<&Event> = result.events[1..].iter().collect();
        assert!(project_events.iter().all(|e| e.event_type == EventType::MaintenanceRunStarted));

        let mut project_names: Vec<&str> =
            project_events.iter().map(|e| e.project.as_str()).collect();
        project_names.sort_unstable();
        assert_eq!(project_names, vec!["alpha", "beta", "gamma"]);
    }

    #[tokio::test]
    async fn skipped_projects_excluded_from_fan_out() {
        let registry = make_registry(vec![
            active_entry("alpha"),
            skipped_entry("beta"),
            active_entry("gamma"),
        ]);
        let block = FanOutMaintenance::new(registry);
        let trigger = system_trigger(Throttle::Full);

        let result = block.execute(&trigger).await.expect("should succeed");
        assert!(result.success);

        // 1 MaintenanceRunCompleted + 2 per-project events (beta skipped)
        assert_eq!(result.events.len(), 3);

        let project_names: Vec<&str> =
            result.events[1..].iter().map(|e| e.project.as_str()).collect();
        assert!(project_names.contains(&"alpha"));
        assert!(project_names.contains(&"gamma"));
        assert!(!project_names.contains(&"beta"));
    }

    #[tokio::test]
    async fn throttle_propagated_to_per_project_events() {
        let block = FanOutMaintenance::new(make_registry(vec![active_entry("alpha")]));
        let trigger = system_trigger(Throttle::DryRun);

        let result = block.execute(&trigger).await.expect("should succeed");

        for event in &result.events {
            assert_eq!(event.throttle, Throttle::DryRun);
        }
    }

    #[tokio::test]
    async fn empty_registry_emits_only_completed() {
        let block = FanOutMaintenance::new(make_registry(vec![]));
        let trigger = system_trigger(Throttle::Full);

        let result = block.execute(&trigger).await.expect("should succeed");
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::MaintenanceRunCompleted);
        assert_eq!(result.events[0].payload["project_count"], 0);
    }

    #[tokio::test]
    async fn completed_event_contains_project_metadata() {
        let registry = make_registry(vec![
            active_entry("alpha"),
            skipped_entry("beta"),
            active_entry("gamma"),
        ]);
        let block = FanOutMaintenance::new(registry);
        let trigger = system_trigger(Throttle::Full);

        let result = block.execute(&trigger).await.expect("should succeed");

        let completed = &result.events[0];
        assert_eq!(completed.event_type, EventType::MaintenanceRunCompleted);
        assert_eq!(completed.payload["project_count"], 2);
        assert_eq!(completed.payload["skipped_count"], 1);
        let projects = completed.payload["projects"].as_array().unwrap();
        assert_eq!(projects.len(), 2);
    }

    #[tokio::test]
    async fn summary_includes_counts() {
        let registry = make_registry(vec![active_entry("alpha"), skipped_entry("beta")]);
        let block = FanOutMaintenance::new(registry);
        let trigger = system_trigger(Throttle::Full);

        let result = block.execute(&trigger).await.expect("should succeed");
        assert!(result.summary.contains("1 active"));
        assert!(result.summary.contains("1 skipped"));
    }
}
