use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

/// Runs `hone maintain` for a project.
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Sinks on `MaintenanceRequested` only.  The routing decision (direct
/// maintain-only path via `RouteProjectWorkflow`, or post-iterate chain via
/// `RunHoneIterate`) has already been made before this event was emitted.
/// This block simply runs `hone maintain` and emits `ProjectMaintainCompleted`.
pub struct RunHoneMaintain {
    registry: Arc<Registry>,
    shell: Arc<dyn ShellGateway>,
}

impl RunHoneMaintain {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self {
            registry,
            shell: Arc::new(crate::gateway::ProcessShellGateway),
        }
    }

    #[cfg(test)]
    fn with_shell(registry: Arc<Registry>, shell: Arc<dyn ShellGateway>) -> Self {
        Self { registry, shell }
    }
}

impl TaskBlock for RunHoneMaintain {
    fn name(&self) -> &'static str {
        "Run Hone Maintain"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::MaintenanceRequested]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();
        let shell = Arc::clone(&self.shell);

        tracing::info!(%project, "running hone maintain");

        Box::pin(async move {
            let Some(entry) = entry else {
                tracing::warn!(project = %project, "project not found in registry, cannot maintain");
                return Ok(TaskBlockResult {
                    events: vec![Event::new(
                        EventType::ProjectMaintainCompleted,
                        project.clone(),
                        throttle,
                        serde_json::json!({ "project": project, "success": false }),
                    )],
                    success: false,
                    summary: format!("Project '{project}' not found in registry"),
                    raw_output: None,
                    exit_code: None,
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
                "invoking hone maintain"
            );

            let run_result = shell
                .run(
                    Path::new(project_path),
                    "hone",
                    &["maintain", agent, project_path, "--json"],
                    None,
                    Some(entry.timeout()),
                )
                .await;

            let (raw_output, exit_code) = match &run_result {
                Ok(r) => (
                    Some(format!("{}\n{}", r.stdout, r.stderr).trim().to_string()),
                    Some(r.exit_code),
                ),
                Err(_) => (None, None),
            };

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
                "hone maintain completed"
            );

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ProjectMaintainCompleted,
                    project.clone(),
                    throttle,
                    serde_json::json!({
                        "project": project,
                        "success": success,
                        "summary": hone_summary,
                    }),
                )],
                success,
                summary: if success {
                    format!("{project}: hone maintain completed")
                } else {
                    format!("{project}: hone maintain failed: {hone_summary}")
                },
                raw_output,
                exit_code,
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

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::RunHoneMaintain;

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
                timeout_secs: None,
            }],
        })
    }

    fn maintenance_event(project: &str) -> Event {
        Event::new(
            EventType::MaintenanceRequested,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({ "project": project }),
        )
    }

    #[test]
    fn sinks_on_maintenance_requested_only() {
        let block = RunHoneMaintain::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert_eq!(block.sinks_on(), &[EventType::MaintenanceRequested]);
    }

    #[test]
    fn kind_is_mutator() {
        let block = RunHoneMaintain::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert_eq!(block.kind(), BlockKind::Mutator);
    }

    #[test]
    fn does_not_sink_on_project_validation_completed() {
        let block = RunHoneMaintain::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert!(!block.sinks_on().contains(&EventType::ProjectValidationCompleted));
    }

    #[test]
    fn does_not_sink_on_project_iterate_completed() {
        let block = RunHoneMaintain::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert!(!block.sinks_on().contains(&EventType::ProjectIterateCompleted));
    }

    #[tokio::test]
    async fn fails_when_project_not_in_registry() {
        let block = RunHoneMaintain::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let trigger = maintenance_event("unknown-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectMaintainCompleted);
        assert_eq!(
            result.events[0].payload.get("success").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(result.summary.contains("not found in registry"));
    }

    #[tokio::test]
    async fn success_emits_project_maintain_completed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap(), "claude");
        let shell = FakeShellGateway::always(CommandResult {
            stdout: r#"{"summary": "maintained"}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let block = RunHoneMaintain::with_shell(registry, shell);
        let trigger = maintenance_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectMaintainCompleted);
        assert_eq!(result.events[0].project, "my-project");
        assert_eq!(
            result.events[0].payload.get("success").and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[tokio::test]
    async fn failure_emits_project_maintain_completed_with_success_false() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap(), "claude");
        let shell = FakeShellGateway::failure("hone failed");
        let block = RunHoneMaintain::with_shell(registry, shell);
        let trigger = maintenance_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectMaintainCompleted);
        assert_eq!(
            result.events[0].payload.get("success").and_then(serde_json::Value::as_bool),
            Some(false)
        );
        assert!(result.summary.contains("failed"));
    }
}
