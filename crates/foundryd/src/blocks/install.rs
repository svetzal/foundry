use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::{InstallConfig, Registry};
use foundry_core::task_block::{BlockKind, RetryPolicy, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

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
    shell: Arc<dyn ShellGateway>,
}

impl InstallLocally {
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

    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy {
            max_retries: 1,
            backoff: Duration::from_secs(10),
        }
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
        let shell = Arc::clone(&self.shell);

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
                    raw_output: None,
                    exit_code: None,
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
                    raw_output: None,
                    exit_code: None,
                });
            };

            let (method_name, cmd_result) = match &install_config {
                InstallConfig::Command(cmd) => {
                    tracing::info!(project = %project, command = %cmd, "running install command");
                    let project_dir = Path::new(&entry.path);
                    let result = shell.run(project_dir, "sh", &["-c", cmd], None, None).await?;
                    ("command", result)
                }
                InstallConfig::Brew(formula) => {
                    tracing::info!(project = %project, formula = %formula, "running brew upgrade");
                    // brew upgrade installs the formula if not already present and upgrades if it is.
                    // "already up-to-date" is treated as success by brew (exit 0).
                    let result = shell
                        .run(Path::new("/"), "brew", &["upgrade", formula], None, None)
                        .await?;
                    ("brew", result)
                }
            };

            let success = cmd_result.success;
            let raw_output =
                Some(format!("{}\n{}", cmd_result.stdout, cmd_result.stderr).trim().to_string());
            let exit_code = Some(cmd_result.exit_code);
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
                raw_output,
                exit_code,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, InstallConfig, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::TaskBlock;
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::InstallLocally;

    fn empty_registry() -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![],
        })
    }

    fn registry_with_install(install: Option<InstallConfig>) -> Arc<Registry> {
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
                timeout_secs: None,
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
        let block = InstallLocally::new(empty_registry());
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
    async fn command_install_success() {
        let registry =
            registry_with_install(Some(InstallConfig::Command("make install".to_string())));
        let shell = FakeShellGateway::always(CommandResult {
            stdout: "install ok\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let block = InstallLocally::with_shell(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].event_type, EventType::LocalInstallCompleted);
        assert_eq!(result.events[0].payload["method"], "command");
        assert_eq!(result.events[0].payload["success"], true);
        assert!(result.summary.contains("command"));
    }

    #[tokio::test]
    async fn command_install_failure_emits_event_with_success_false() {
        let registry =
            registry_with_install(Some(InstallConfig::Command("make install".to_string())));
        let shell = FakeShellGateway::failure("make: error\n");
        let block = InstallLocally::with_shell(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events[0].payload["success"], false);
        assert!(result.summary.contains("failed"));
    }

    #[tokio::test]
    async fn brew_install_success() {
        let registry = registry_with_install(Some(InstallConfig::Brew("mytool".to_string())));
        let shell = FakeShellGateway::always(CommandResult {
            stdout: "==> Upgrading mytool\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let block = InstallLocally::with_shell(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["method"], "brew");
        assert_eq!(result.events[0].payload["success"], true);
        assert!(result.summary.contains("brew"));
    }

    #[test]
    fn retry_policy_allows_one_retry() {
        let block = InstallLocally::new(empty_registry());
        let policy = block.retry_policy();
        assert_eq!(policy.max_retries, 1);
        assert_eq!(policy.backoff, Duration::from_secs(10));
    }
}
