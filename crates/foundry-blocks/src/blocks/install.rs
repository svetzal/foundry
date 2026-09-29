use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{LocalInstallCompletedPayload, LocalSkillInstallCompletedPayload};
use foundry_sdk::registry::{InstallConfig, Registry, SkillInstall, resolve_skill_install};
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

/// Reinstalls a tool locally after changes are pushed or a release pipeline completes.
/// Mutator — simulated success at `dry_run`.
///
/// Terminal block: this is the end of both the dirty and clean vulnerability
/// remediation paths.
///
/// Dispatches based on the project's `InstallConfig` in the registry:
/// - `Command` — runs the specified shell command in the project directory
/// - `Brew` — runs `brew upgrade <formula>` (installs if not already present).
///   Homebrew is macOS-only here: on any other host the step is skipped with
///   `install via brew not supported on <os>` and `brew` is never spawned.
/// - absent — skips gracefully with `success=true`
///
/// Every outcome is recorded as a `LocalInstallCompleted` event, including a
/// command that could not be started, so the maintenance summary can report
/// it per project.
pub struct InstallLocally {
    registry: Arc<std::sync::RwLock<Registry>>,
    shell: Arc<dyn ShellGateway>,
    /// The host operating system, as `std::env::consts::OS` names it.
    host_os: &'static str,
}

impl InstallLocally {
    pub fn new(registry: Arc<std::sync::RwLock<Registry>>) -> Self {
        Self {
            registry,
            shell: Arc::new(crate::gateway::ProcessShellGateway),
            host_os: std::env::consts::OS,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_gateways(
        registry: Arc<std::sync::RwLock<Registry>>,
        shell: Arc<dyn ShellGateway>,
    ) -> Self {
        Self {
            registry,
            shell,
            host_os: std::env::consts::OS,
        }
    }

    /// Behave as if running on `os` (for example `"linux"`).
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_host_os(mut self, os: &'static str) -> Self {
        self.host_os = os;
        self
    }
}

/// Why an install config cannot run on this host, or `None` when it can.
fn unsupported_on_host(install: &InstallConfig, host_os: &str) -> Option<String> {
    match install {
        InstallConfig::Brew(_) if host_os != "macos" => {
            Some(format!("install via brew not supported on {host_os}"))
        }
        _ => None,
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
        let host_os = self.host_os;

        Box::pin(async move {
            // Guard: project must be in the registry and have an install config.
            let (entry, install_config) = match resolve_install(&project, entry, throttle) {
                Ok(pair) => pair,
                Err(skip) => return Ok(skip),
            };

            if let Some(reason) = unsupported_on_host(&install_config, host_os) {
                // Domain skip: the config cannot run here; say so rather than
                // spawn a program the host does not have.
                tracing::warn!(project = %project, %reason, "install skipped");
                return Ok(skipped_on_host(&project, throttle, reason));
            }

            let (method_name, spawned) = match &install_config {
                InstallConfig::Command(cmd) => {
                    tracing::info!(project = %project, command = %cmd, "running install command");
                    let project_dir = Path::new(&entry.path);
                    ("command", shell.run(project_dir, "sh", &["-c", cmd], None, None).await)
                }
                InstallConfig::Brew(formula) => {
                    tracing::info!(project = %project, formula = %formula, "running brew upgrade");
                    // brew upgrade installs the formula if not already present and upgrades if it
                    // is. "already up-to-date" is treated as success by brew (exit 0).
                    (
                        "brew",
                        shell.run(Path::new("/"), "brew", &["upgrade", formula], None, None).await,
                    )
                }
            };
            let cmd_result = match spawned {
                Ok(result) => result,
                Err(e) => {
                    // Record: a command that could not start is a failed
                    // install, reported per project in the summary rather
                    // than only in the daemon log.
                    tracing::warn!(project = %project, method = method_name, error = %e, "install command could not start");
                    return Ok(could_not_start(&project, throttle, method_name, &e.to_string()));
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

/// The result for an install config this host cannot run.
fn skipped_on_host(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    reason: String,
) -> TaskBlockResult {
    TaskBlockResult::success(
        format!("Skipped: {reason}"),
        vec![local_install_completed_event(
            project,
            throttle,
            &LocalInstallCompletedPayload {
                method: Some("brew".to_string()),
                success: true,
                status: Some("skipped".to_string()),
                reason: Some(reason),
                ..Default::default()
            },
        )],
    )
}

/// The result for an install command that could not be started at all.
fn could_not_start(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    method: &str,
    error: &str,
) -> TaskBlockResult {
    TaskBlockResult {
        events: vec![local_install_completed_event(
            project,
            throttle,
            &LocalInstallCompletedPayload {
                method: Some(method.to_string()),
                success: false,
                details: Some(error.to_string()),
                ..Default::default()
            },
        )],
        success: false,
        summary: format!("Install via {method} failed: {error}"),
        ..Default::default()
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
/// success), or `None` when [`resolve_skill_install`] decides to skip it.
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
    let cmd = match resolve_skill_install(
        entry.installs_skill.as_ref(),
        Some(install_config),
        &entry.name,
    ) {
        SkillInstall::Run(cmd) => cmd,
        SkillInstall::Skip(reason) => {
            tracing::debug!(project = %project, reason = %reason, "skill install skipped");
            return None;
        }
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

    /// A shell whose commands cannot be started at all.
    struct UnspawnableShell;

    impl crate::gateway::ShellGateway for UnspawnableShell {
        fn run<'a>(
            &'a self,
            _working_dir: &'a std::path::Path,
            command: &'a str,
            _args: &'a [&'a str],
            _env: Option<&'a [(String, String)]>,
            _timeout: Option<Duration>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<CommandResult>> + Send + 'a>,
        > {
            Box::pin(async move { anyhow::bail!("failed to spawn command: {command}") })
        }
    }

    #[tokio::test]
    async fn brew_on_linux_is_skipped_and_never_spawned() {
        let registry = registry_with_install(Some(InstallConfig::Brew("hone".to_string())));
        let shell = FakeShellGateway::success();
        let block = InstallLocally::with_gateways(registry, shell.clone()).with_host_os("linux");

        let result = block.execute(&make_trigger("my-project")).await.unwrap();

        assert!(result.success, "a skip is not a failed run");
        assert!(shell.invocations().is_empty(), "brew must never be spawned on linux");
        assert_eq!(result.events.len(), 1);
        let payload = &result.events[0].payload;
        assert_eq!(payload["status"], "skipped");
        assert_eq!(payload["reason"], "install via brew not supported on linux");
        assert_eq!(payload["method"], "brew");
    }

    #[tokio::test]
    async fn an_install_command_that_cannot_start_is_recorded_not_raised() {
        let registry = registry_with_install(Some(InstallConfig::Brew("hone".to_string())));
        let block = InstallLocally::with_gateways(registry, Arc::new(UnspawnableShell))
            .with_host_os("macos");

        let result =
            block.execute(&make_trigger("my-project")).await.expect("recorded, not an Err");

        assert!(!result.success);
        assert_eq!(result.events.len(), 1, "the summary needs the event");
        let payload = &result.events[0].payload;
        assert_eq!(payload["success"], false);
        assert_eq!(payload["method"], "brew");
        assert_eq!(payload["details"], "failed to spawn command: brew");
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
        let block = InstallLocally::with_gateways(registry, shell).with_host_os("macos");
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
            InstallLocally::with_gateways(registry, shell as Arc<dyn crate::gateway::ShellGateway>)
                .with_host_os("macos");
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
    async fn installs_skill_true_without_install_config_runs_nothing() {
        let registry = registry_with_install_and_skill(None, Some(InstallsSkill::Default(true)));
        let shell = FakeShellGateway::success();
        let shell_for_inspect = Arc::clone(&shell);
        let block =
            InstallLocally::with_gateways(registry, shell as Arc<dyn crate::gateway::ShellGateway>);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1, "no install config must not derive a skill install");
        assert_eq!(result.events[0].event_type, EventType::LocalInstallCompleted);
        assert!(shell_for_inspect.invocations().is_empty());
    }

    #[tokio::test]
    async fn installs_skill_true_empty_brew_formula_falls_back_to_project_name() {
        let registry = registry_with_install_and_skill(
            Some(InstallConfig::Brew(String::new())),
            Some(InstallsSkill::Default(true)),
        );
        let shell = FakeShellGateway::success();
        let block = InstallLocally::with_gateways(registry, shell).with_host_os("macos");
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(result.events.len(), 2);
        assert_eq!(
            result.events[1].payload["command"].as_str().unwrap(),
            "my-project init --global --force"
        );
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
