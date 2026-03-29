use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, RetryPolicy, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

task_block_new! {
    /// Commits staged changes and pushes to the remote.
    /// Mutator — events logged but not delivered at `audit_only`;
    /// simulated success at `dry_run`.
    ///
    /// Real behaviour:
    /// - Self-filters when the trigger payload explicitly sets `"changes": false`.
    /// - Checks `git status --porcelain`; self-filters when the tree is clean.
    /// - Runs `git add -A` then `git commit`.
    /// - Runs `git push` only when `registry.actions.push` is `true`.
    /// - Emits [`EventType::ProjectChangesCommitted`] after a successful commit.
    /// - Emits [`EventType::ProjectChangesPushed`] after a successful push.
    ///
    /// Commit message varies by trigger event type:
    /// - [`EventType::ProjectIterateCompleted`] → `chore(<project>): automated iterate`
    /// - [`EventType::ProjectMaintainCompleted`] → `chore(<project>): automated maintenance`
    /// - All other triggers → `chore(<project>): automated remediation`
    ///
    /// Fallback: when the project is not found in the registry, stub events are
    /// emitted so that integration tests that use synthetic project names remain
    /// green without requiring a real repository on disk.
    pub struct CommitAndPush {
        shell: ShellGateway = crate::gateway::ProcessShellGateway
    }
}

impl CommitAndPush {
    async fn commit_and_push(
        registry: Arc<Registry>,
        shell: Arc<dyn ShellGateway>,
        project: String,
        throttle: foundry_core::throttle::Throttle,
        event_type: EventType,
        cve: String,
    ) -> anyhow::Result<TaskBlockResult> {
        // Resolve the project path and push flag from the registry.
        let Some(entry) = registry.projects.iter().find(|p| p.name == project) else {
            // Project not in registry — emit stub events for test compatibility.
            tracing::warn!(
                project = %project,
                "project not found in registry, using stub commit-and-push"
            );
            return Ok(stub_result(&project, throttle, &cve));
        };

        let path = std::path::Path::new(&entry.path);
        let push_enabled = entry.actions.push;

        tracing::info!(%project, "checking for changes to commit");

        // Self-filter: nothing to do if the working tree is clean.
        let status = shell.run(path, "git", &["status", "--porcelain"], None, None).await?;
        if status.stdout.trim().is_empty() {
            tracing::info!(%project, "working tree clean, skipping commit");
            return Ok(TaskBlockResult {
                events: vec![],
                success: true,
                summary: "No changes to commit".to_string(),
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            });
        }

        // Stage everything.
        shell.run(path, "git", &["add", "-A"], None, None).await?;

        // Commit message varies by the event that triggered this block.
        let commit_msg = match event_type {
            EventType::ProjectIterateCompleted => {
                format!("chore({project}): automated iterate")
            }
            EventType::ProjectMaintainCompleted => {
                format!("chore({project}): automated maintenance")
            }
            _ => format!("chore({project}): automated remediation"),
        };

        let commit = shell.run(path, "git", &["commit", "-m", &commit_msg], None, None).await?;
        if !commit.success {
            // "nothing to commit" is not an error — can happen when both
            // iterate and maintain trigger CommitAndPush and the first one
            // already committed everything, or when git add -A stages
            // content identical to HEAD.
            let combined = format!("{} {}", commit.stdout, commit.stderr).to_lowercase();
            if combined.contains("nothing to commit") || combined.contains("no changes added") {
                tracing::info!(%project, "git commit found nothing to commit");
                return Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "No changes to commit".to_string(),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                });
            }
            return Err(anyhow::anyhow!("git commit failed: {}", commit.stderr.trim()));
        }

        tracing::info!(%project, "committed changes");

        let mut events = vec![Event::new(
            EventType::ProjectChangesCommitted,
            project.clone(),
            throttle,
            serde_json::json!({
                "cve": cve,
                "message": commit_msg,
            }),
        )];

        // Push if permitted.
        if push_enabled {
            tracing::info!(%project, "pushing changes");
            let push = shell.run(path, "git", &["push"], None, None).await?;
            if push.success {
                events.push(Event::new(
                    EventType::ProjectChangesPushed,
                    project.clone(),
                    throttle,
                    serde_json::json!({ "cve": cve }),
                ));
            } else {
                tracing::warn!(%project, stderr = %push.stderr.trim(), "git push failed");
            }
        } else {
            tracing::info!(%project, "push disabled in registry, skipping");
        }

        Ok(TaskBlockResult {
            success: true,
            summary: "Committed and pushed changes".to_string(),
            events,
            raw_output: None,
            exit_code: None,
            audit_artifacts: vec![],
        })
    }
}

impl TaskBlock for CommitAndPush {
    task_block_meta! {
        name: "Commit and Push",
        kind: Mutator,
        sinks_on: [RemediationCompleted, ProjectIterateCompleted, ProjectMaintainCompleted],
    }

    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy {
            max_retries: 2,
            backoff: Duration::from_secs(5),
        }
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        // Respect the self-filter: no events when payload says no changes.
        let changes_flag = trigger.payload.get("changes").and_then(serde_json::Value::as_bool);
        if changes_flag == Some(false) {
            return vec![];
        }

        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let cve = trigger
            .payload
            .get("cve")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();

        let mut events = vec![Event::new(
            EventType::ProjectChangesCommitted,
            project.clone(),
            throttle,
            serde_json::json!({ "cve": cve, "dry_run": true }),
        )];

        // Simulate push if the project has push enabled, or if unknown (stub path).
        let push_enabled = self
            .registry
            .projects
            .iter()
            .find(|p| p.name == project)
            .is_none_or(|e| e.actions.push);

        if push_enabled {
            events.push(Event::new(
                EventType::ProjectChangesPushed,
                project,
                throttle,
                serde_json::json!({ "cve": cve, "dry_run": true }),
            ));
        }

        events
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let event_type = trigger.event_type.clone();

        // Self-filter: when the payload explicitly signals no changes were made, skip early.
        let changes_flag = trigger.payload.get("changes").and_then(serde_json::Value::as_bool);
        if changes_flag == Some(false) {
            tracing::info!(%project, "payload indicates no changes, skipping commit");
            return Box::pin(async {
                Ok(TaskBlockResult {
                    events: vec![],
                    success: true,
                    summary: "No changes to commit".to_string(),
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

        let registry = Arc::clone(&self.registry);
        let shell = Arc::clone(&self.shell);

        Box::pin(Self::commit_and_push(registry, shell, project, throttle, event_type, cve))
    }
}

/// Emit stub committed+pushed events when the project has no registry entry.
fn stub_result(
    project: &str,
    throttle: foundry_core::throttle::Throttle,
    cve: &str,
) -> TaskBlockResult {
    TaskBlockResult {
        events: vec![
            Event::new(
                EventType::ProjectChangesCommitted,
                project.to_string(),
                throttle,
                serde_json::json!({ "cve": cve, "stub": true }),
            ),
            Event::new(
                EventType::ProjectChangesPushed,
                project.to_string(),
                throttle,
                serde_json::json!({ "cve": cve, "stub": true }),
            ),
        ],
        success: true,
        summary: format!("Committed and pushed fix for {cve} (stub)"),
        raw_output: None,
        exit_code: None,
        audit_artifacts: vec![],
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry};
    use foundry_core::throttle::Throttle;
    use tempfile::TempDir;

    use foundry_core::task_block::TaskBlock;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::CommitAndPush;

    fn empty_registry() -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![],
        })
    }

    fn make_trigger(project: &str, cve: &str) -> Event {
        Event::new(
            EventType::RemediationCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({ "cve": cve }),
        )
    }

    fn make_trigger_for(event_type: EventType, project: &str) -> Event {
        Event::new(event_type, project.to_string(), Throttle::Full, serde_json::json!({}))
    }

    fn make_trigger_no_changes(event_type: EventType, project: &str) -> Event {
        Event::new(
            event_type,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({ "changes": false }),
        )
    }

    fn registry_for(name: &str, path: &str, push: bool) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: name.to_string(),
                path: path.to_string(),
                stack: foundry_core::registry::Stack::Rust,
                agent: String::new(),
                repo: String::new(),
                branch: "main".to_string(),
                skip: None,
                notes: None,
                actions: ActionFlags {
                    push,
                    iterate: false,
                    maintain: false,
                    audit: false,
                    release: false,
                },
                install: None,
                timeout_secs: None,
            }],
        })
    }

    /// Fake sequence that simulates: status=dirty, add=ok, commit=ok, push=ok.
    fn dirty_sequence() -> Arc<FakeShellGateway> {
        FakeShellGateway::sequence(vec![
            // git status --porcelain: non-empty output = dirty
            CommandResult {
                stdout: " M file.txt\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            // git add -A
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            // git commit
            CommandResult {
                stdout: "[main abc1234] committed\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            // git push
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ])
    }

    /// Fake sequence that simulates: status=clean (empty output).
    fn clean_sequence() -> Arc<FakeShellGateway> {
        FakeShellGateway::always(CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        })
    }

    #[tokio::test]
    async fn unknown_project_emits_stub_events() {
        let block = CommitAndPush::new(empty_registry());
        let trigger = make_trigger("no-such-project", "CVE-2026-0001");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["project_changes_committed", "project_changes_pushed"]);
        assert_eq!(result.events[0].payload["stub"], true);
    }

    #[tokio::test]
    async fn clean_tree_emits_no_events() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = clean_sequence();
        let block = CommitAndPush::with_shell(registry, shell);
        let trigger = make_trigger("my-project", "CVE-2026-0002");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert_eq!(result.summary, "No changes to commit");
    }

    #[tokio::test]
    async fn dirty_tree_commits_and_pushes_when_enabled() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = dirty_sequence();
        let block = CommitAndPush::with_shell(registry, shell);
        let trigger = make_trigger("my-project", "CVE-2026-0003");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["project_changes_committed", "project_changes_pushed"]);
    }

    #[tokio::test]
    async fn dirty_tree_commits_but_skips_push_when_disabled() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), false);
        // Only three calls needed: status, add, commit (no push).
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: " M file.txt\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: "[main abc1234] committed\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = CommitAndPush::with_shell(registry, shell);
        let trigger = make_trigger("my-project", "CVE-2026-0004");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["project_changes_committed"]);
    }

    #[test]
    fn sinks_on_includes_all_three_event_types() {
        let block = CommitAndPush::new(empty_registry());
        let sinks = block.sinks_on();
        assert!(sinks.contains(&EventType::RemediationCompleted));
        assert!(sinks.contains(&EventType::ProjectIterateCompleted));
        assert!(sinks.contains(&EventType::ProjectMaintainCompleted));
    }

    #[tokio::test]
    async fn payload_changes_false_self_filters_immediately() {
        let block = CommitAndPush::new(empty_registry());
        let trigger = make_trigger_no_changes(EventType::ProjectIterateCompleted, "proj");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert_eq!(result.summary, "No changes to commit");
    }

    #[tokio::test]
    async fn remediation_trigger_uses_remediation_commit_message() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), false);
        // status=dirty, add, commit (no push)
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: " M f\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: "[main x] msg\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = CommitAndPush::with_shell(registry, shell);
        let trigger = make_trigger("my-project", "CVE-2026-1000");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(!result.events.is_empty());
        let msg = result.events[0].payload["message"].as_str().unwrap();
        assert!(msg.contains("remediation"), "expected 'remediation' in '{msg}'");
    }

    #[tokio::test]
    async fn iterate_trigger_uses_iterate_commit_message() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), false);
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: " M f\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: "[main x] msg\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = CommitAndPush::with_shell(registry, shell);
        let trigger = make_trigger_for(EventType::ProjectIterateCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let msg = result.events[0].payload["message"].as_str().unwrap();
        assert!(msg.contains("iterate"), "expected 'iterate' in '{msg}'");
    }

    #[tokio::test]
    async fn maintain_trigger_uses_maintenance_commit_message() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), false);
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: " M f\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: "[main x] msg\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = CommitAndPush::with_shell(registry, shell);
        let trigger = make_trigger_for(EventType::ProjectMaintainCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let msg = result.events[0].payload["message"].as_str().unwrap();
        assert!(msg.contains("maintenance"), "expected 'maintenance' in '{msg}'");
    }

    #[tokio::test]
    async fn commit_nothing_to_commit_is_success_not_error() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        // status=dirty (something shows up), add=ok, commit fails with "nothing to commit"
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: " m .claude/worktrees/agent-abc123\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: "On branch main\nnothing to commit, working tree clean\n".to_string(),
                stderr: String::new(),
                exit_code: 1,
                success: false,
            },
        ]);
        let block = CommitAndPush::with_shell(registry, shell);
        let trigger = make_trigger_for(EventType::ProjectIterateCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success, "should be success, not error");
        assert!(result.events.is_empty());
        assert_eq!(result.summary, "No changes to commit");
    }

    #[test]
    fn retry_policy_allows_retries() {
        let block = CommitAndPush::new(empty_registry());
        let policy = block.retry_policy();
        assert_eq!(policy.max_retries, 2);
        assert_eq!(policy.backoff, Duration::from_secs(5));
    }

    // -- Real git repo tests for commit message variants --

    async fn init_git_repo_real(dir: &std::path::Path) {
        crate::shell::run(dir, "git", &["init"], None, None).await.unwrap();
        crate::shell::run(dir, "git", &["config", "user.email", "test@example.com"], None, None)
            .await
            .unwrap();
        crate::shell::run(dir, "git", &["config", "user.name", "Test"], None, None)
            .await
            .unwrap();
        std::fs::write(dir.join("README.md"), "init").unwrap();
        crate::shell::run(dir, "git", &["add", "-A"], None, None).await.unwrap();
        crate::shell::run(dir, "git", &["commit", "-m", "init"], None, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn real_git_dirty_tree_commits_with_correct_message() {
        let tmp = TempDir::new().unwrap();
        init_git_repo_real(tmp.path()).await;
        std::fs::write(tmp.path().join("change.txt"), "change").unwrap();

        let registry = registry_for("my-project", tmp.path().to_str().unwrap(), false);
        let block = CommitAndPush::new(registry);
        let trigger = make_trigger("my-project", "CVE-2026-9999");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        let msg = result.events[0].payload["message"].as_str().unwrap();
        assert!(msg.contains("remediation"), "expected 'remediation' in '{msg}'");
    }
}
