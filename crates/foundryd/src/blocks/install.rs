use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::{InstallConfig, Registry};
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

/// Reinstalls a tool locally after changes are pushed or a release pipeline completes.
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Terminal block: this is the end of both the dirty and clean vulnerability
/// remediation paths.
///
/// Dispatches based on the project's `InstallConfig` in the registry:
/// - `Command` — runs the specified shell command in the project directory
/// - `Brew` — runs `brew upgrade <formula>` (installs if not already present)
/// - absent — skips gracefully with `success=true`
pub struct InstallLocally {
    registry: Arc<Registry>,
}

impl InstallLocally {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry }
    }
}

impl TaskBlock for InstallLocally {
    fn name(&self) -> &'static str {
        "Install Locally"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[
            EventType::ProjectChangesPushed,
            EventType::ReleasePipelineCompleted,
        ]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        // Resolve install config and project path from registry.
        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();

        Box::pin(async move {
            let Some(entry) = entry else {
                tracing::warn!(project = %project, "project not found in registry, skipping install");
                return Ok(TaskBlockResult {
                    events: vec![Event::new(
                        EventType::LocalInstallCompleted,
                        project,
                        throttle,
                        serde_json::json!({ "status": "skipped", "reason": "project not found in registry" }),
                    )],
                    success: true,
                    summary: "Skipped: project not found in registry".to_string(),
                });
            };

            let Some(install_config) = entry.install else {
                tracing::info!(project = %project, "no install config, skipping");
                return Ok(TaskBlockResult {
                    events: vec![Event::new(
                        EventType::LocalInstallCompleted,
                        project,
                        throttle,
                        serde_json::json!({ "status": "skipped", "reason": "no install config" }),
                    )],
                    success: true,
                    summary: "Skipped: no install config defined".to_string(),
                });
            };

            let (method_name, cmd_result) = match &install_config {
                InstallConfig::Command(cmd) => {
                    tracing::info!(project = %project, command = %cmd, "running install command");
                    let project_dir = Path::new(&entry.path);
                    let result =
                        crate::shell::run(project_dir, "sh", &["-c", cmd], None, None).await?;
                    ("command", result)
                }
                InstallConfig::Brew(formula) => {
                    tracing::info!(project = %project, formula = %formula, "running brew upgrade");
                    // brew upgrade installs the formula if not already present and upgrades if it is.
                    // "already up-to-date" is treated as success by brew (exit 0).
                    let result = crate::shell::run(
                        Path::new("/"),
                        "brew",
                        &["upgrade", formula],
                        None,
                        None,
                    )
                    .await?;
                    ("brew", result)
                }
            };

            let success = cmd_result.success;
            let details = if success {
                cmd_result.stdout.lines().next().unwrap_or("ok").to_string()
            } else {
                cmd_result.stderr.lines().next().unwrap_or("(no output)").to_string()
            };

            tracing::info!(
                project = %project,
                method = method_name,
                success = success,
                "install completed"
            );

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::LocalInstallCompleted,
                    project.clone(),
                    throttle,
                    serde_json::json!({
                        "method": method_name,
                        "success": success,
                        "details": details,
                    }),
                )],
                success,
                summary: if success {
                    format!("Installed locally via {method_name}")
                } else {
                    format!("Install via {method_name} failed: {details}")
                },
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::registry::{ActionFlags, ProjectEntry};
    use foundry_core::throttle::Throttle;

    fn registry_with_install(install: Option<InstallConfig>) -> Arc<Registry> {
        use foundry_core::registry::Stack;
        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: "my-project".to_string(),
                path: "/tmp".to_string(),
                stack: Stack::Rust,
                agent: String::new(),
                repo: String::new(),
                branch: "main".to_string(),
                skip: None,
                actions: ActionFlags::default(),
                install,
            }],
        })
    }

    fn make_trigger(project: &str) -> Event {
        Event::new(
            EventType::ProjectChangesPushed,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
    }

    #[tokio::test]
    async fn skips_when_project_not_in_registry() {
        let block = InstallLocally::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let trigger = make_trigger("unknown-project");

        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::LocalInstallCompleted);
        let reason = result.events[0].payload["reason"].as_str().unwrap();
        assert!(reason.contains("not found in registry"));
    }

    #[tokio::test]
    async fn skips_when_no_install_config() {
        let block = InstallLocally::new(registry_with_install(None));
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::LocalInstallCompleted);
        assert!(result.summary.contains("no install config"));
    }

    #[tokio::test]
    async fn command_install_runs_shell_command() {
        // Use `true` as the command — always succeeds, writes nothing.
        let block = InstallLocally::new(registry_with_install(Some(InstallConfig::Command(
            "true".to_string(),
        ))));
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.summary.contains("command"));
    }

    #[tokio::test]
    async fn command_install_failure_emits_event_with_success_false() {
        let block = InstallLocally::new(registry_with_install(Some(InstallConfig::Command(
            "false".to_string(),
        ))));
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert!(!result.events[0].payload["success"].as_bool().unwrap());
        assert!(result.summary.contains("failed"));
    }
}
