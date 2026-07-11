use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{LocalInstallCompletedPayload, LocalSkillInstallCompletedPayload};
use foundry_sdk::registry::{
    InstallConfig, InstallsSkill, Registry, derive_default_skill_install_command,
};
use foundry_sdk::task_block::{BlockKind, RetryPolicy, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

use super::{SimulatedSuccess, TriggerContext};

/// Build a single `LocalInstallCompleted` event.
///
/// Single source of truth for the `EventType::LocalInstallCompleted` +
/// `LocalInstallCompletedPayload` pairing — called by `dry_run_events`,
/// `resolve_install`, and `execute` so no path can silently drift.
fn local_install_completed_event(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    payload: &LocalInstallCompletedPayload,
) -> Event {
    super::event_from_infallible_payload(
        EventType::LocalInstallCompleted,
        project,
        throttle,
        payload,
    )
}

task_block_new! {
    /// Reinstalls a tool locally after changes are pushed or a release pipeline completes.
    /// Mutator — simulated success at `dry_run`.
    ///
    /// Terminal block: this is the end of both the dirty and clean vulnerability
    /// remediation paths.
    ///
    /// Dispatches based on the project's `InstallConfig` in the registry:
    /// - `Command` — runs the specified shell command in the project directory
    /// - `Brew` — runs `brew upgrade <formula>` (installs if not already present)
    /// - absent — skips gracefully with `success=true`
    pub struct InstallLocally {
        shell: ShellGateway = crate::gateway::ProcessShellGateway
    }
}

impl SimulatedSuccess for InstallLocally {
    type Outcome = LocalInstallCompletedPayload;

    fn simulate(&self, _trigger: &Event) -> LocalInstallCompletedPayload {
        LocalInstallCompletedPayload {
            success: true,
            dry_run: Some(true),
            ..Default::default()
        }
    }

    fn success_events(
        &self,
        trigger: &Event,
        outcome: &LocalInstallCompletedPayload,
    ) -> Vec<Event> {
        vec![local_install_completed_event(
            &trigger.project,
            trigger.throttle,
            outcome,
        )]
    }
}

impl TaskBlock for InstallLocally {
    task_block_meta! {
        name: "Install Locally",
        kind: Mutator,
        sinks_on: [ProjectChangesPushed, ReleasePipelineCompleted],
    }

    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy {
            max_retries: 1,
            backoff: Duration::from_secs(10),
        }
    }

    dry_run_via_simulation!();

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project, throttle, ..
        } = TriggerContext::from_trigger(trigger);

        // Resolve install config and project path from registry.
        let entry = match super::read_registry(&self.registry) {
            Ok(guard) => guard.find_project(&project).cloned(),
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            // Guard: project must be in the registry and have an install config.
            let (entry, install_config) = match resolve_install(&project, entry, throttle) {
                Ok(pair) => pair,
                Err(skip) => return Ok(skip),
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
                    // brew upgrade installs the formula if not already present and upgrades if it
                    // is. "already up-to-date" is treated as success by brew (exit 0).
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

            tracing::info!(project = %project, method = method_name, success, "install completed");

            let mut events = vec![local_install_completed_event(
                &project,
                throttle,
                &LocalInstallCompletedPayload {
                    method: Some(method_name.to_string()),
                    success,
                    details: Some(details.clone()),
                    ..Default::default()
                },
            )];

            // Skill install step — only run when binary install succeeded.
            if success
                && let Some(skill_event) =
                    run_skill_install(&project, throttle, &entry, &install_config, shell.as_ref())
                        .await
            {
                events.push(skill_event);
            }

            Ok(TaskBlockResult {
                events,
                success,
                summary: if success {
                    format!("Installed locally via {method_name}")
                } else {
                    format!("Install via {method_name} failed: {details}")
                },
                raw_output,
                exit_code,
                ..Default::default()
            })
        })
    }
}

/// Validate that the registry entry and install config are present, returning
/// a skip `TaskBlockResult` when either is absent.
///
/// Returns `Ok((entry, install_config))` on success, `Err(skip_result)` to signal
/// that the caller should return immediately with the given (success) result.
fn resolve_install(
    project: &str,
    entry: Option<foundry_sdk::registry::ProjectEntry>,
    throttle: foundry_sdk::throttle::Throttle,
) -> Result<(foundry_sdk::registry::ProjectEntry, InstallConfig), TaskBlockResult> {
    let Some(entry) = entry else {
        tracing::warn!(project = %project, "project not found in registry, skipping install");
        return Err(TaskBlockResult::success(
            "Skipped: project not found in registry",
            vec![local_install_completed_event(
                project,
                throttle,
                &LocalInstallCompletedPayload {
                    success: true,
                    status: Some("skipped".to_string()),
                    reason: Some("project not found in registry".to_string()),
                    ..Default::default()
                },
            )],
        ));
    };

    let Some(install_config) = entry.install.clone() else {
        tracing::info!(project = %project, "no install config, skipping");
        return Err(TaskBlockResult::success(
            "Skipped: no install config defined",
            vec![local_install_completed_event(
                project,
                throttle,
                &LocalInstallCompletedPayload {
                    success: true,
                    status: Some("skipped".to_string()),
                    reason: Some("no install config".to_string()),
                    ..Default::default()
                },
            )],
        ));
    };

    Ok((entry, install_config))
}

/// Resolve and run the skill-install command for a project, if configured.
///
/// Returns `Some(event)` when the skill install was attempted (regardless of
/// success), or `None` when no skill install is configured.
///
/// Failures are logged as warnings and do NOT fail the caller — binary install
/// already succeeded; skill drift is a soft warning only.
async fn run_skill_install(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    entry: &foundry_sdk::registry::ProjectEntry,
    install_config: &InstallConfig,
    shell: &dyn ShellGateway,
) -> Option<Event> {
    let installs_skill = entry.installs_skill.as_ref()?;

    // Resolve the command to run.
    let cmd = match installs_skill {
        InstallsSkill::Default(false) => return None,
        InstallsSkill::Default(true) => {
            derive_default_skill_install_command(Some(install_config), &entry.name)
        }
        InstallsSkill::Custom { command } => command.clone(),
    };

    tracing::info!(project = %project, command = %cmd, "running skill install");

    let result = match shell.run(Path::new("/"), "sh", &["-c", &cmd], None, None).await {
        Ok(r) => r,
        Err(err) => {
            tracing::warn!(project = %project, command = %cmd, error = %err, "skill install command failed to spawn");
            return Some(super::event_from_infallible_payload(
                EventType::LocalSkillInstallCompleted,
                project,
                throttle,
                &LocalSkillInstallCompletedPayload {
                    project: project.to_string(),
                    command: cmd,
                    success: false,
                    stdout_tail: String::new(),
                    stderr_tail: err.to_string(),
                },
            ));
        }
    };

    let skill_success = result.success;
    let stdout_tail = tail_lines(&result.stdout, 5);
    let stderr_tail = tail_lines(&result.stderr, 5);

    if skill_success {
        tracing::info!(project = %project, command = %cmd, "skill install succeeded");
    } else {
        tracing::warn!(
            project = %project,
            command = %cmd,
            stderr = %stderr_tail,
            "skill install failed (non-fatal)"
        );
    }

    Some(super::event_from_infallible_payload(
        EventType::LocalSkillInstallCompleted,
        project,
        throttle,
        &LocalSkillInstallCompletedPayload {
            project: project.to_string(),
            command: cmd,
            success: skill_success,
            stdout_tail,
            stderr_tail,
        },
    ))
}

/// Return the last `n` non-empty lines of `text`, joined by newline.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::{InstallConfig, InstallsSkill, ProjectEntry, Registry};
    use foundry_sdk::task_block::TaskBlock;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::super::test_helpers;
    use super::InstallLocally;

    fn registry_with_install(install: Option<InstallConfig>) -> Arc<RwLock<Registry>> {
        registry_with_install_and_skill(install, None)
    }

    fn registry_with_install_and_skill(
        install: Option<InstallConfig>,
        installs_skill: Option<InstallsSkill>,
    ) -> Arc<RwLock<Registry>> {
        test_helpers::registry_with_entry(ProjectEntry {
            agent: String::new(),
            install,
            installs_skill,
            ..test_helpers::project_entry("my-project", "/tmp")
        })
    }

    fn make_trigger(project: &str) -> Event {
        test_helpers::make_trigger(EventType::ProjectChangesPushed, project, serde_json::json!({}))
    }

    #[tokio::test]
    async fn skips_when_project_not_in_registry() {
        let block = InstallLocally::new(test_helpers::empty_registry());
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
        let block = InstallLocally::with_gateways(registry, shell);
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
        let block = InstallLocally::with_gateways(registry, shell);
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
        let block = InstallLocally::with_gateways(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["method"], "brew");
        assert_eq!(result.events[0].payload["success"], true);
        assert!(result.summary.contains("brew"));
    }

    #[test]
    fn retry_policy_allows_one_retry() {
        let block = InstallLocally::new(test_helpers::empty_registry());
        let policy = block.retry_policy();
        assert_eq!(policy.max_retries, 1);
        assert_eq!(policy.backoff, Duration::from_secs(10));
    }

    // --- Skill install tests ---

    #[tokio::test]
    async fn no_installs_skill_emits_only_local_install_completed() {
        let registry = registry_with_install_and_skill(
            Some(InstallConfig::Command("make install".to_string())),
            None,
        );
        let shell = FakeShellGateway::success();
        let block = InstallLocally::with_gateways(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1, "should emit only LocalInstallCompleted");
        assert_eq!(result.events[0].event_type, EventType::LocalInstallCompleted);
    }

    #[tokio::test]
    async fn installs_skill_false_emits_only_local_install_completed() {
        let registry = registry_with_install_and_skill(
            Some(InstallConfig::Command("make install".to_string())),
            Some(InstallsSkill::Default(false)),
        );
        let shell = FakeShellGateway::success();
        let block = InstallLocally::with_gateways(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1, "Default(false) should not trigger skill install");
        assert_eq!(result.events[0].event_type, EventType::LocalInstallCompleted);
    }

    #[tokio::test]
    async fn installs_skill_true_derives_command_from_brew_formula() {
        let registry = registry_with_install_and_skill(
            Some(InstallConfig::Brew("mytool".to_string())),
            Some(InstallsSkill::Default(true)),
        );
        // Two calls: brew upgrade (success), then skill init (success)
        let shell = FakeShellGateway::always(CommandResult {
            stdout: "ok\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let shell_for_inspect = Arc::clone(&shell);
        let block =
            InstallLocally::with_gateways(registry, shell as Arc<dyn crate::gateway::ShellGateway>);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 2);
        assert_eq!(result.events[0].event_type, EventType::LocalInstallCompleted);
        assert_eq!(result.events[1].event_type, EventType::LocalSkillInstallCompleted);

        let skill_event = &result.events[1];
        assert_eq!(skill_event.payload["success"], true);
        // Command should be derived from the brew formula name
        let cmd = skill_event.payload["command"].as_str().unwrap();
        assert_eq!(cmd, "mytool init --global --force");

        // Verify two shell invocations
        let invocations = shell_for_inspect.invocations();
        assert_eq!(invocations.len(), 2);
        // First: brew upgrade
        assert_eq!(invocations[0].command, "brew");
        // Second: sh -c "mytool init --global --force"
        assert_eq!(invocations[1].command, "sh");
        assert!(invocations[1].args.contains(&"mytool init --global --force".to_string()));
    }

    #[tokio::test]
    async fn installs_skill_true_derives_command_from_project_name_for_command_install() {
        let registry = registry_with_install_and_skill(
            Some(InstallConfig::Command("cargo install --path .".to_string())),
            Some(InstallsSkill::Default(true)),
        );
        let shell = FakeShellGateway::success();
        let block = InstallLocally::with_gateways(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 2);

        let skill_event = &result.events[1];
        let cmd = skill_event.payload["command"].as_str().unwrap();
        // Falls back to project name when install is Command (not Brew)
        assert_eq!(cmd, "my-project init --global --force");
    }

    #[tokio::test]
    async fn installs_skill_custom_runs_verbatim_command() {
        let registry = registry_with_install_and_skill(
            Some(InstallConfig::Command("make install".to_string())),
            Some(InstallsSkill::Custom {
                command: "gilt skill-init --global --force".to_string(),
            }),
        );
        let shell = FakeShellGateway::success();
        let shell_for_inspect = Arc::clone(&shell);
        let block =
            InstallLocally::with_gateways(registry, shell as Arc<dyn crate::gateway::ShellGateway>);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 2);

        let skill_event = &result.events[1];
        assert_eq!(skill_event.payload["success"], true);
        assert_eq!(
            skill_event.payload["command"].as_str().unwrap(),
            "gilt skill-init --global --force"
        );

        let invocations = shell_for_inspect.invocations();
        assert_eq!(invocations.len(), 2);
        assert!(invocations[1].args.contains(&"gilt skill-init --global --force".to_string()));
    }

    #[tokio::test]
    async fn skill_install_failure_does_not_fail_parent_block() {
        let registry = registry_with_install_and_skill(
            Some(InstallConfig::Command("make install".to_string())),
            Some(InstallsSkill::Default(true)),
        );
        // First call (binary install) succeeds; second call (skill) fails.
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: "install ok\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: String::new(),
                stderr: "skill: command not found\n".to_string(),
                exit_code: 1,
                success: false,
            },
        ]);
        let block = InstallLocally::with_gateways(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();

        // Parent block succeeds even though skill install failed.
        assert!(result.success, "parent block must succeed when skill install fails");
        assert_eq!(result.events.len(), 2);

        let install_event = &result.events[0];
        assert_eq!(install_event.event_type, EventType::LocalInstallCompleted);
        assert_eq!(install_event.payload["success"], true);

        let skill_event = &result.events[1];
        assert_eq!(skill_event.event_type, EventType::LocalSkillInstallCompleted);
        assert_eq!(skill_event.payload["success"], false);
        assert!(!skill_event.payload["stderr_tail"].as_str().unwrap().is_empty());
    }

    #[tokio::test]
    async fn skill_install_not_run_when_binary_install_fails() {
        let registry = registry_with_install_and_skill(
            Some(InstallConfig::Command("make install".to_string())),
            Some(InstallsSkill::Default(true)),
        );
        // Binary install fails.
        let shell = FakeShellGateway::failure("make: error");
        let shell_for_inspect = Arc::clone(&shell);
        let block =
            InstallLocally::with_gateways(registry, shell as Arc<dyn crate::gateway::ShellGateway>);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        // Only LocalInstallCompleted, no skill event.
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::LocalInstallCompleted);

        // Shell should have been called exactly once (binary install only).
        let invocations = shell_for_inspect.invocations();
        assert_eq!(invocations.len(), 1);
    }

    #[test]
    fn dry_run_and_execute_agree_on_primary_output_event_type_for_install() {
        // dry_run_events and execute (command install success) must both emit LocalInstallCompleted.
        // After the refactor, this is guaranteed structurally by local_install_completed_event.
        let block = InstallLocally::new(registry_with_install(Some(InstallConfig::Command(
            "true".to_string(),
        ))));
        let trigger = make_trigger("my-project");
        let events = block.dry_run_events(&trigger);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::LocalInstallCompleted);
    }

    #[test]
    fn tail_lines_returns_last_n_nonempty_lines() {
        let text = "line1\nline2\nline3\nline4\nline5\nline6";
        assert_eq!(super::tail_lines(text, 3), "line4\nline5\nline6");
    }

    #[test]
    fn tail_lines_skips_empty_lines() {
        let text = "line1\n\nline2\n\nline3";
        assert_eq!(super::tail_lines(text, 5), "line1\nline2\nline3");
    }

    #[test]
    fn tail_lines_returns_all_when_fewer_than_n() {
        let text = "a\nb";
        assert_eq!(super::tail_lines(text, 10), "a\nb");
    }

    #[test]
    fn tail_lines_empty_input_returns_empty() {
        assert_eq!(super::tail_lines("", 5), "");
    }
}
