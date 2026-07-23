use std::pin::Pin;
use std::sync::{Arc, RwLock};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::registry::Registry;
use foundry_sdk::scatter::Scatter;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Fans out a system-level maintenance cycle to individual per-project runs.
///
/// Observer — always runs regardless of throttle.
///
/// Sinks on `MaintenanceCycleStarted`. When the triggering event's project is
/// `"system"` (emitted by `foundry run` without `--project`), the block reads
/// the registry, collects all active (non-skipped) projects, and **scatters**
/// a `ProjectRunStarted` event for each one. Each child triggers the existing
/// per-project chain (`ValidateProject` → `RouteProjectWorkflow` → …), which
/// terminates in a `ProjectRunCompleted`.
///
/// The scatter declares a gather over those `ProjectRunCompleted` events: once
/// every per-project run completes, the engine synthesizes a single
/// `MaintenanceCycleCompleted` — a genuine fan-in, no longer hand-aggregated
/// by the service layer.
///
/// Per-project events do not mint a fresh `trace_id`; the engine propagates
/// the cycle root's `trace_id` to emitted events, keeping the entire cycle on
/// a single OTel-compatible trace.
///
/// When the triggering project is anything other than `"system"`, the block
/// returns immediately with no emitted events — the per-project event is
/// handled by downstream blocks.
pub struct FanOutMaintenance {
    registry: Arc<RwLock<Registry>>,
}

impl FanOutMaintenance {
    pub fn new(registry: Arc<RwLock<Registry>>) -> Self {
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
        &[EventType::MaintenanceCycleStarted]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        // Extract data before the async boundary — the RwLock guard must not cross .await.
        let (project_names, active_count, skipped_count) = if project == "system" {
            let reg = match foundry_sdk::error::read_lock(&self.registry, "registry") {
                Ok(reg) => reg,
                Err(e) => {
                    // A poisoned registry lock must not take down the daemon
                    // (foundryd is long-lived state). Skip this dispatch and
                    // propagate the fault so the caller can act on it.
                    tracing::error!(error = %e, "registry lock poisoned; skipping maintenance cycle dispatch");
                    return Box::pin(async move { Err(e) });
                }
            };
            let active = reg.active_projects();
            let names: Vec<String> = active.iter().map(|p| p.name.clone()).collect();
            let active_count = names.len();
            let skipped_count = reg.projects.len() - active_count;
            (names, active_count, skipped_count)
        } else {
            (vec![], 0, 0)
        };

        Box::pin(async move {
            if project != "system" {
                return Ok(TaskBlockResult {
                    success: true,
                    summary: format!("per-project run (project={project}), fan-out not applicable"),
                    ..Default::default()
                });
            }

            tracing::info!(
                active = active_count,
                skipped = skipped_count,
                "fanning out maintenance to active projects"
            );

            let mut children = Vec::with_capacity(active_count);

            for name in &project_names {
                // Per-project events inherit the cycle root's trace_id, and the
                // scatter's freshly minted gather_id, via the engine's
                // stamping pass — so the entire cycle stays on one trace and
                // every per-project run is counted toward the gather.
                children.push(Event::new(
                    EventType::ProjectRunStarted,
                    name.clone(),
                    throttle,
                    serde_json::json!({}),
                ));
            }

            Ok(TaskBlockResult::scattering(
                format!("fanned out to {active_count} active projects ({skipped_count} skipped)"),
                Scatter::all(
                    children,
                    vec![EventType::ProjectRunCompleted],
                    EventType::MaintenanceCycleCompleted,
                    "system",
                )
                .with_context(serde_json::json!({
                    "active": active_count,
                    "skipped": skipped_count,
                })),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_sdk::task_block::{BlockKind, TaskBlock};
    use foundry_sdk::throttle::Throttle;

    use super::*;

    fn make_registry(entries: Vec<ProjectEntry>) -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
            version: 2,
            projects: entries,
        }))
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
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
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
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        }
    }

    fn system_trigger(throttle: Throttle) -> Event {
        Event::new(
            EventType::MaintenanceCycleStarted,
            "system".to_string(),
            throttle,
            serde_json::json!({}),
        )
        .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
    }

    fn project_trigger(project: &str) -> Event {
        Event::new(
            EventType::MaintenanceCycleStarted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
    }

    // -- Metadata tests --

    #[test]
    fn sinks_on_maintenance_cycle_started() {
        let block = FanOutMaintenance::new(make_registry(vec![]));
        assert_eq!(block.sinks_on(), &[EventType::MaintenanceCycleStarted]);
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
        assert!(result.scatter.is_none(), "a per-project trigger declares no scatter");
    }

    // -- Fan-out tests --

    #[tokio::test]
    async fn system_trigger_scatters_per_project_runs() {
        let registry = make_registry(vec![
            active_entry("alpha"),
            active_entry("beta"),
            active_entry("gamma"),
        ]);
        let block = FanOutMaintenance::new(registry);
        let trigger = system_trigger(Throttle::DryRun);

        let result = block.execute(&trigger).await.expect("should succeed");
        assert!(result.success);

        let scatter = result.scatter.expect("system trigger declares a scatter");
        assert_eq!(scatter.children.len(), 3);
        assert!(scatter.children.iter().all(|e| e.event_type == EventType::ProjectRunStarted));
        // The gather collects ProjectRunCompleted and reduces to the cycle event.
        assert_eq!(scatter.gather.on, vec![EventType::ProjectRunCompleted]);
        assert_eq!(scatter.gather.reduce_event_type, EventType::MaintenanceCycleCompleted);
        assert_eq!(scatter.gather.reduce_project, "system");

        let mut project_names: Vec<&str> =
            scatter.children.iter().map(|e| e.project.as_str()).collect();
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

        // 2 scattered children (beta skipped)
        let scatter = result.scatter.expect("system trigger declares a scatter");
        let project_names: Vec<&str> =
            scatter.children.iter().map(|e| e.project.as_str()).collect();
        assert_eq!(project_names.len(), 2);
        assert!(project_names.contains(&"alpha"));
        assert!(project_names.contains(&"gamma"));
        assert!(!project_names.contains(&"beta"));
    }

    #[tokio::test]
    async fn per_project_events_have_no_trace_id_set_at_block_emission() {
        // The block no longer mints a fresh trace_id per project; the engine
        // propagates the cycle root's trace_id to each scattered child during
        // its stamping pass. At the block level, children carry no trace_id —
        // the engine fills it in from the trigger.
        let registry = make_registry(vec![active_entry("alpha"), active_entry("beta")]);
        let block = FanOutMaintenance::new(registry);
        let trigger = system_trigger(Throttle::Full);
        assert!(trigger.trace_id.is_some(), "test setup: trigger must have a trace_id");

        let result = block.execute(&trigger).await.expect("should succeed");
        let scatter = result.scatter.expect("system trigger declares a scatter");
        assert_eq!(scatter.children.len(), 2);

        for event in &scatter.children {
            assert!(
                event.trace_id.is_none(),
                "scattered child should leave trace_id unset; engine inherits it from the cycle root",
            );
        }
    }

    #[tokio::test]
    async fn throttle_propagated_to_per_project_events() {
        let block = FanOutMaintenance::new(make_registry(vec![active_entry("alpha")]));
        let trigger = system_trigger(Throttle::DryRun);

        let result = block.execute(&trigger).await.expect("should succeed");
        let scatter = result.scatter.expect("system trigger declares a scatter");

        for event in &scatter.children {
            assert_eq!(event.throttle, Throttle::DryRun);
        }
    }

    #[tokio::test]
    async fn empty_registry_scatters_no_children() {
        let block = FanOutMaintenance::new(make_registry(vec![]));
        let trigger = system_trigger(Throttle::Full);

        let result = block.execute(&trigger).await.expect("should succeed");
        assert!(result.success);
        // An empty scatter is still a scatter — the engine reduces it at once.
        let scatter = result.scatter.expect("system trigger declares a scatter");
        assert!(scatter.children.is_empty());
    }

    #[tokio::test]
    async fn fan_out_children_are_all_per_project() {
        let registry = make_registry(vec![
            active_entry("alpha"),
            skipped_entry("beta"),
            active_entry("gamma"),
        ]);
        let block = FanOutMaintenance::new(registry);
        let trigger = system_trigger(Throttle::Full);

        let result = block.execute(&trigger).await.expect("should succeed");
        let scatter = result.scatter.expect("system trigger declares a scatter");

        assert!(scatter.children.iter().all(|e| e.event_type == EventType::ProjectRunStarted));
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
