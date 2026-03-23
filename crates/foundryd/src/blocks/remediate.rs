use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

/// Attempts to fix a vulnerability on the main branch.
/// Mutator — suppressed at `audit_only`, skipped at `dry_run`.
///
/// Self-filters: only acts when `dirty=true` in the trigger payload.
///
/// Invokes `hone maintain <agent> <path>` to fix the vulnerable dependency.
pub struct RemediateVulnerability {
    registry: Arc<Registry>,
    shell: Arc<dyn ShellGateway>,
}

impl RemediateVulnerability {
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

impl TaskBlock for RemediateVulnerability {
    fn name(&self) -> &'static str {
        "Remediate Vulnerability"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Mutator
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::MainBranchAudited]
    }

    #[allow(clippy::too_many_lines)]
    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        // Self-filter: only remediate when main branch is dirty.
        let dirty = trigger
            .payload
            .get("dirty")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);

        if !dirty {
            tracing::info!("main branch is clean, skipping remediation");
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "Skipped: main branch is clean".to_string(),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                })
            });
        }

        let cve = trigger
            .payload
            .get("cve")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        // Resolve project agent and path from registry.
        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();
        let shell = Arc::clone(&self.shell);

        tracing::info!(%cve, "remediating vulnerability");

        Box::pin(async move {
            let Some(entry) = entry else {
                tracing::warn!(project = %project, "project not found in registry, cannot remediate");
                return Ok(TaskBlockResult {
                    events: vec![],
                    success: false,
                    summary: format!("Project '{project}' not found in registry"),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                });
            };

            let agent = if entry.agent.is_empty() {
                "claude"
            } else {
                &entry.agent
            };
            let project_path = &entry.path;

            let audit_dir = super::hone_common::audit_dir(&project);
            let audit_dir_str = audit_dir.display().to_string();
            let snapshot_time = std::time::SystemTime::now();

            tracing::info!(
                project = %project,
                agent = agent,
                path = %project_path,
                audit_dir = %audit_dir_str,
                %cve,
                "invoking hone maintain"
            );

            let run_result = shell
                .run(
                    Path::new(project_path),
                    "hone",
                    &[
                        "maintain",
                        agent,
                        project_path,
                        "--json",
                        "--audit-dir",
                        &audit_dir_str,
                    ],
                    None,
                    None,
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
                    EventType::RemediationCompleted,
                    project,
                    throttle,
                    serde_json::json!({
                        "cve": cve,
                        "success": success,
                        "summary": hone_summary,
                    }),
                )],
                success,
                summary: if success {
                    format!("Remediated {cve}: {hone_summary}")
                } else {
                    format!("Remediation of {cve} failed: {hone_summary}")
                },
                raw_output,
                exit_code,
                audit_artifacts: super::hone_common::collect_new_artifacts(
                    &audit_dir,
                    snapshot_time,
                ),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::TaskBlock;
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::RemediateVulnerability;

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
                notes: None,
                actions: ActionFlags::default(),
                install: None,
                timeout_secs: None,
            }],
        })
    }

    fn dirty_trigger(project: &str, cve: &str) -> Event {
        Event::new(
            EventType::MainBranchAudited,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": true, "cve": cve }),
        )
    }

    fn clean_trigger(project: &str) -> Event {
        Event::new(
            EventType::MainBranchAudited,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({ "dirty": false, "cve": "CVE-2026-9999" }),
        )
    }

    #[tokio::test]
    async fn skips_when_main_branch_is_clean() {
        let block = RemediateVulnerability::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let trigger = clean_trigger("any-project");

        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("clean"));
    }

    #[tokio::test]
    async fn fails_when_project_not_in_registry() {
        let block = RemediateVulnerability::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let trigger = dirty_trigger("unknown-project", "CVE-2026-1234");

        let result = block.execute(&trigger).await.unwrap();
        assert!(!result.success);
        assert!(result.events.is_empty());
        assert!(result.summary.contains("not found in registry"));
    }

    #[tokio::test]
    async fn emits_remediation_completed_on_hone_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap(), "claude");
        let shell = FakeShellGateway::always(CommandResult {
            stdout: r#"{"summary": "fixed"}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let block = RemediateVulnerability::with_shell(registry, shell);
        let trigger = dirty_trigger("my-project", "CVE-2026-9999");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::RemediationCompleted);
        assert_eq!(result.events[0].payload["cve"], "CVE-2026-9999");
        assert_eq!(result.events[0].payload["success"], true);
    }

    #[tokio::test]
    async fn emits_remediation_completed_on_hone_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap(), "claude");
        let shell = FakeShellGateway::failure("hone exited with code 1");
        let block = RemediateVulnerability::with_shell(registry, shell);
        let trigger = dirty_trigger("my-project", "CVE-2026-1234");

        let result = block.execute(&trigger).await.unwrap();

        // Block still emits the event even on failure (with success=false).
        assert!(!result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::RemediationCompleted);
        assert_eq!(result.events[0].payload["success"], false);
        assert!(result.summary.contains("failed"));
    }

    #[tokio::test]
    async fn records_shell_invocation_for_hone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().to_str().unwrap().to_string();
        let registry = registry_with_project("my-project", &path, "claude");
        let shell = FakeShellGateway::success();
        let block = RemediateVulnerability::with_shell(
            registry,
            Arc::clone(&shell) as Arc<dyn crate::gateway::ShellGateway>,
        );
        let trigger = dirty_trigger("my-project", "CVE-2026-0001");

        block.execute(&trigger).await.unwrap();

        let invocations = shell.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].command, "hone");
        assert!(invocations[0].args.contains(&"maintain".to_string()));
    }
}
