use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Runs `hone iterate` for a validated project.
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Sinks on `IterationRequested` (emitted by `RouteProjectWorkflow` after
/// successful validation when `actions.iterate=true`).  No action-flag
/// self-filtering needed — the router guarantees iterate is enabled.
///
/// After a successful iteration the block checks the forwarded
/// `actions.maintain` flag.  When `true` it also emits `MaintenanceRequested`
/// so the maintain sub-workflow starts automatically without an extra routing
/// step.
///
/// On hone failure, emits `ProjectIterateCompleted` with `success: false` but
/// does NOT emit `MaintenanceRequested`.
pub struct RunHoneIterate {
    registry: Arc<Registry>,
}

impl RunHoneIterate {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl TaskBlock for RunHoneIterate {
    fn name(&self) -> &'static str {
        "Run Hone Iterate"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::IterationRequested]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let maintain = trigger
            .payload
            .get("actions")
            .and_then(|a| a.get("maintain"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();

        tracing::info!(%project, %maintain, "running hone iterate");

        Box::pin(async move {
            let Some(entry) = entry else {
                tracing::warn!(project = %project, "project not found in registry, cannot iterate");
                return Ok(TaskBlockResult {
                    events: vec![Event::new(
                        EventType::ProjectIterateCompleted,
                        project.clone(),
                        throttle,
                        serde_json::json!({ "project": project, "success": false }),
                    )],
                    success: false,
                    summary: format!("Project '{project}' not found in registry"),
                });
            };

            let agent = if entry.agent.is_empty() {
                "claude"
            } else {
                &entry.agent
            };
            let project_path = &entry.path;

            tracing::info!(
                project = %project,
                agent = agent,
                path = %project_path,
                "invoking hone iterate"
            );

            let run_result = crate::shell::run(
                Path::new(project_path),
                "hone",
                &["iterate", agent, project_path, "--json"],
                None,
                None,
            )
            .await;

            let (success, hone_summary) = match run_result {
                Ok(result) => {
                    let s = result.success;
                    let summary = super::hone_common::parse_hone_summary(&result.stdout, s);
                    (s, summary)
                }
                Err(err) => {
                    tracing::warn!(error = %err, "hone not available or failed to spawn");
                    (false, format!("hone unavailable: {err}"))
                }
            };

            tracing::info!(
                project = %project,
                success = success,
                summary = %hone_summary,
                "hone iterate completed"
            );

            let mut events = vec![Event::new(
                EventType::ProjectIterateCompleted,
                project.clone(),
                throttle,
                serde_json::json!({
                    "project": project,
                    "success": success,
                    "summary": hone_summary,
                }),
            )];

            if success && maintain {
                tracing::info!(%project, "iteration succeeded, chaining to maintenance workflow");
                events.push(Event::new(
                    EventType::MaintenanceRequested,
                    project.clone(),
                    throttle,
                    serde_json::json!({ "project": project }),
                ));
            }

            Ok(TaskBlockResult {
                events,
                success,
                summary: if success {
                    format!("{project}: hone iterate completed")
                } else {
                    format!("{project}: hone iterate failed: {hone_summary}")
                },
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

    use super::RunHoneIterate;

    fn registry_with_project(name: &str, path: &str, agent: &str) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: name.to_string(),
                path: path.to_string(),
                stack: Stack::Rust,
                agent: agent.to_string(),
                repo: String::new(),
                branch: "main".to_string(),
                skip: None,
                actions: ActionFlags::default(),
                install: None,
            }],
        })
    }

    fn iteration_event(project: &str, maintain: bool) -> Event {
        Event::new(
            EventType::IterationRequested,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "actions": { "maintain": maintain },
            }),
        )
    }

    #[test]
    fn sinks_on_iteration_requested() {
        let block = RunHoneIterate::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert_eq!(block.sinks_on(), &[EventType::IterationRequested]);
    }

    #[test]
    fn kind_is_mutator() {
        let block = RunHoneIterate::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert_eq!(block.kind(), BlockKind::Mutator);
    }

    #[tokio::test]
    async fn emits_project_iterate_completed_on_success() {
        // hone won't be available in CI, so success=false is acceptable.
        // The important thing is that exactly one ProjectIterateCompleted event is emitted.
        let registry = registry_with_project("my-project", "/tmp", "claude");
        let block = RunHoneIterate::new(registry);
        let trigger = iteration_event("my-project", false);

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.events.is_empty());
        assert_eq!(result.events[0].event_type, EventType::ProjectIterateCompleted);
        assert_eq!(result.events[0].project, "my-project");
    }

    #[test]
    fn does_not_sink_on_project_validation_completed() {
        let block = RunHoneIterate::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert!(!block.sinks_on().contains(&EventType::ProjectValidationCompleted));
    }

    #[test]
    fn does_not_sink_on_maintenance_requested() {
        let block = RunHoneIterate::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert!(!block.sinks_on().contains(&EventType::MaintenanceRequested));
    }

    #[tokio::test]
    async fn fails_when_project_not_in_registry() {
        let block = RunHoneIterate::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let trigger = iteration_event("unknown-project", false);

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterateCompleted);
        assert_eq!(
            result.events[0].payload.get("success").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(result.summary.contains("not found in registry"));
    }

    #[tokio::test]
    async fn hone_failure_does_not_emit_maintenance_requested() {
        // When hone is unavailable (success=false), MaintenanceRequested must NOT be emitted
        // even when maintain=true.
        let registry = registry_with_project("my-project", "/tmp", "claude");
        let block = RunHoneIterate::new(registry);
        let trigger = iteration_event("my-project", true);

        let result = block.execute(&trigger).await.unwrap();

        // hone is unavailable so success=false; no MaintenanceRequested should appear
        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterateCompleted);
    }

    #[tokio::test]
    async fn missing_actions_field_treats_maintain_as_false() {
        let registry = registry_with_project("my-project", "/tmp", "claude");
        let block = RunHoneIterate::new(registry);
        let trigger = Event::new(
            EventType::IterationRequested,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({ "project": "my-project" }),
        );

        let result = block.execute(&trigger).await.unwrap();

        // Only ProjectIterateCompleted, no MaintenanceRequested
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterateCompleted);
    }
}
