use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    ProjectChangesCommittedPayload, ProjectChangesPushedPayload, ProjectCompletedPayload,
    RemediationCompletedPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, RetryPolicy, TaskBlock, TaskBlockResult};

use foundry_sdk::loop_context::has_loop_context;

use crate::gateway::ShellGateway;

task_block_new! {
    /// Commits staged changes and pushes to the remote.
    /// Mutator — simulated success at `dry_run`.
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
    /// - [`EventType::ProjectIterationCompleted`] → `chore(<project>): automated iterate`
    /// - [`EventType::ProjectMaintenanceCompleted`] → `chore(<project>): automated maintenance`
    /// - All other triggers → `chore(<project>): automated remediation`
    ///
    /// When the project is not found in the registry, the block returns a failure result.
    pub struct CommitAndPush {
        shell: ShellGateway = crate::gateway::ProcessShellGateway
    }
}

impl CommitAndPush {
    async fn commit_and_push(
        registry: Arc<RwLock<Registry>>,
        shell: Arc<dyn ShellGateway>,
        project: String,
        throttle: foundry_sdk::throttle::Throttle,
        event_type: EventType,
        cve: String,
    ) -> anyhow::Result<TaskBlockResult> {
        // Resolve the project path and push flag from the registry.
        // Extract synchronously before any .await point so the lock is not held across yields.
        let entry_data = super::read_registry(&registry)?
            .find_project(&project)
            .map(|e| (e.path.clone(), e.actions.push));

        let Some((path_str, push_enabled)) = entry_data else {
            tracing::warn!(project = %project, "project not found in registry");
            return Ok(TaskBlockResult::project_not_found(&project));
        };

        let path = std::path::Path::new(&path_str);

        tracing::info!(%project, "checking for changes to commit");

        // Self-filter: nothing to do if the working tree is clean.
        let status = shell.run(path, "git", &["status", "--porcelain"], None, None).await?;
        if status.stdout.trim().is_empty() {
            tracing::info!(%project, "working tree clean, skipping commit");
            return Ok(TaskBlockResult::success("No changes to commit", vec![]));
        }

        let Some(commit_msg) = commit_changes(&*shell, path, &project, &event_type).await? else {
            return Ok(TaskBlockResult::success("No changes to commit", vec![]));
        };

        let push_payload = push_if_enabled(&*shell, path, &project, &cve, push_enabled).await?;

        let events = build_commit_push_events(
            &project,
            throttle,
            &ProjectChangesCommittedPayload {
                project: project.clone(),
                cve: cve.clone(),
                message: commit_msg.clone(),
                dry_run: None,
            },
            push_payload.as_ref(),
        )?;

        Ok(TaskBlockResult::success("Committed and pushed changes", events))
    }
}

/// Decision outcome for a commit-and-push trigger.
///
/// Centralises the three-way guard logic shared between `dry_run_events` and
/// `execute`, eliminating drift risk between the two paths.
#[derive(Debug, PartialEq)]
enum CommitDecision {
    /// Intermediate completion inside a nested loop — skip.
    SkipNestedLoop,
    /// Payload explicitly signals no changes were made — skip.
    SkipNoChanges,
    /// Proceed with the commit; CVE identifier extracted from payload (or "unknown").
    Proceed { cve: String },
}

/// Evaluate the trigger and decide what `CommitAndPush` should do.
///
/// Both `dry_run_events` and `execute` delegate to this function so the
/// loop-context guard, the no-changes self-filter, and CVE extraction are
/// always derived from a single source of truth.
fn decide_commit(trigger: &Event) -> CommitDecision {
    // 1. loop_context guard: completion events inside a nested loop are skipped.
    let is_completion_event = matches!(
        trigger.event_type,
        EventType::ProjectIterationCompleted | EventType::ProjectMaintenanceCompleted
    );
    if is_completion_event && has_loop_context(&trigger.payload) {
        return CommitDecision::SkipNestedLoop;
    }

    // 2. self-filter: payload explicitly says no changes.
    let changes_flag =
        trigger.parse_payload::<ProjectCompletedPayload>().ok().and_then(|p| p.changes);
    if changes_flag == Some(false) {
        return CommitDecision::SkipNoChanges;
    }

    // 3. extract CVE (defaults to "unknown" when absent).
    let cve = trigger
        .parse_payload::<RemediationCompletedPayload>()
        .ok()
        .and_then(|p| p.cve)
        .unwrap_or_else(|| "unknown".to_string());

    CommitDecision::Proceed { cve }
}

impl TaskBlock for CommitAndPush {
    task_block_meta! {
        name: "Commit and Push",
        kind: Mutator,
        sinks_on: [RemediationCompleted, ProjectIterationCompleted, ProjectMaintenanceCompleted, InnerIterationCompleted],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        matches!(decide_commit(trigger), CommitDecision::Proceed { .. })
    }

    fn retry_policy(&self) -> RetryPolicy {
        RetryPolicy {
            max_retries: 2,
            backoff: Duration::from_secs(5),
        }
    }

    fn dry_run_events(&self, trigger: &Event) -> Vec<Event> {
        let CommitDecision::Proceed { cve } = decide_commit(trigger) else {
            return vec![];
        };

        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        // Simulate push if the project has push enabled, or if unknown (stub path).
        let push_enabled = match super::read_registry(&self.registry) {
            Ok(guard) => guard.find_project(&project).is_none_or(|e| e.actions.push),
            Err(e) => {
                tracing::error!(error = %e, "registry lock poisoned in dry_run_events; defaulting push_enabled=true");
                true
            }
        };

        let push_payload = push_enabled.then(|| ProjectChangesPushedPayload {
            project: project.clone(),
            cve: cve.clone(),
            message: None,
            dry_run: Some(true),
        });

        build_commit_push_events(
            &project,
            throttle,
            &ProjectChangesCommittedPayload {
                project: project.clone(),
                cve: cve.clone(),
                message: commit_message(&trigger.event_type, &project),
                dry_run: Some(true),
            },
            push_payload.as_ref(),
        )
        .expect("commit and push event payloads are infallibly serializable")
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let CommitDecision::Proceed { cve } = decide_commit(trigger) else {
            // Defensive: accepts() filters SkipNestedLoop and SkipNoChanges before dispatch.
            return skip!("Skipped: no commit needed");
        };
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let event_type = trigger.event_type.clone();
        let registry = Arc::clone(&self.registry);
        let shell = Arc::clone(&self.shell);
        Box::pin(Self::commit_and_push(registry, shell, project, throttle, event_type, cve))
    }
}

#[derive(Debug, PartialEq)]
enum CommitOutcome {
    Committed,
    NothingToCommit,
    Failed,
}

/// Classify the outcome of a `git commit` invocation from the exit status and
/// combined stdout+stderr output.
fn classify_commit_outcome(success: bool, stdout: &str, stderr: &str) -> CommitOutcome {
    if success {
        CommitOutcome::Committed
    } else {
        let combined = format!("{stdout} {stderr}").to_lowercase();
        if combined.contains("nothing to commit") || combined.contains("no changes added") {
            CommitOutcome::NothingToCommit
        } else {
            CommitOutcome::Failed
        }
    }
}

/// Stage all changes and commit; returns the commit message on success, `None` when
/// git reports nothing to commit (a successful no-op), and `Err` on real failures.
async fn commit_changes(
    shell: &dyn ShellGateway,
    path: &std::path::Path,
    project: &str,
    event_type: &EventType,
) -> anyhow::Result<Option<String>> {
    shell.run(path, "git", &["add", "-A"], None, None).await?;

    let commit_msg = commit_message(event_type, project);
    let commit = shell.run(path, "git", &["commit", "-m", &commit_msg], None, None).await?;

    match classify_commit_outcome(commit.success, &commit.stdout, &commit.stderr) {
        CommitOutcome::Committed => {
            tracing::info!(%project, "committed changes");
            Ok(Some(commit_msg))
        }
        CommitOutcome::NothingToCommit => {
            // Can happen when both iterate and maintain trigger CommitAndPush and the
            // first one already committed everything, or when git add -A stages content
            // identical to HEAD.
            tracing::info!(%project, "git commit found nothing to commit");
            Ok(None)
        }
        CommitOutcome::Failed => {
            Err(anyhow::anyhow!("git commit failed: {}", commit.stderr.trim()))
        }
    }
}

/// Push the committed changes when `push_enabled`; returns the push payload on success.
async fn push_if_enabled(
    shell: &dyn ShellGateway,
    path: &std::path::Path,
    project: &str,
    cve: &str,
    push_enabled: bool,
) -> anyhow::Result<Option<ProjectChangesPushedPayload>> {
    if !push_enabled {
        tracing::info!(%project, "push disabled in registry, skipping");
        return Ok(None);
    }
    tracing::info!(%project, "pushing changes");
    let push = shell.run(path, "git", &["push"], None, None).await?;
    if push.success {
        Ok(Some(ProjectChangesPushedPayload {
            project: project.to_string(),
            cve: cve.to_string(),
            message: None,
            dry_run: None,
        }))
    } else {
        tracing::warn!(%project, stderr = %push.stderr.trim(), "git push failed");
        Ok(None)
    }
}

/// Build a `Vec<Event>` for a commit (and optional push) from typed payloads.
///
/// Eliminates the repeated serialize-and-construct pattern in the real execution
/// path, `dry_run_events`, and the stub fallback.
fn build_commit_push_events(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    commit_payload: &ProjectChangesCommittedPayload,
    push_payload: Option<&ProjectChangesPushedPayload>,
) -> anyhow::Result<Vec<Event>> {
    let mut events = vec![super::event_from_payload(
        EventType::ProjectChangesCommitted,
        project,
        throttle,
        commit_payload,
    )?];
    if let Some(push) = push_payload {
        events.push(super::event_from_payload(
            EventType::ProjectChangesPushed,
            project,
            throttle,
            push,
        )?);
    }
    Ok(events)
}

/// Map a trigger event type to the appropriate `chore(project): ...` commit message.
fn commit_message(event_type: &EventType, project: &str) -> String {
    match event_type {
        EventType::ProjectIterationCompleted => format!("chore({project}): automated iterate"),
        EventType::ProjectMaintenanceCompleted => {
            format!("chore({project}): automated maintenance")
        }
        EventType::InnerIterationCompleted => format!("chore({project}): strategic iterate cycle"),
        _ => format!("chore({project}): automated remediation"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry};
    use tempfile::TempDir;

    use foundry_sdk::task_block::TaskBlock;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::super::test_helpers;
    use super::{
        CommitAndPush, CommitDecision, CommitOutcome, classify_commit_outcome, commit_message,
        decide_commit,
    };

    fn make_trigger(project: &str, cve: &str) -> Event {
        test_helpers::make_trigger(
            EventType::RemediationCompleted,
            project,
            serde_json::json!({ "cve": cve }),
        )
    }

    fn make_trigger_for(event_type: EventType, project: &str) -> Event {
        test_helpers::make_trigger(event_type, project, serde_json::json!({}))
    }

    fn make_trigger_no_changes(event_type: EventType, project: &str) -> Event {
        test_helpers::make_trigger(event_type, project, serde_json::json!({ "changes": false }))
    }

    fn registry_for(name: &str, path: &str, push: bool) -> Arc<RwLock<Registry>> {
        test_helpers::registry_with_entry(ProjectEntry {
            agent: String::new(),
            actions: ActionFlags {
                push,
                ..Default::default()
            },
            ..test_helpers::project_entry(name, path)
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

    // -- classify_commit_outcome pure function tests --

    #[test]
    fn classify_commit_outcome_success_is_committed() {
        assert_eq!(classify_commit_outcome(true, "[main abc] msg\n", ""), CommitOutcome::Committed);
    }

    #[test]
    fn classify_commit_outcome_nothing_to_commit() {
        assert_eq!(
            classify_commit_outcome(
                false,
                "On branch main\nnothing to commit, working tree clean\n",
                ""
            ),
            CommitOutcome::NothingToCommit
        );
        assert_eq!(
            classify_commit_outcome(false, "", "no changes added to commit"),
            CommitOutcome::NothingToCommit
        );
    }

    #[test]
    fn classify_commit_outcome_failure() {
        assert_eq!(
            classify_commit_outcome(false, "", "error: failed to push some refs"),
            CommitOutcome::Failed
        );
    }

    // -- commit_message pure function tests --

    #[test]
    fn commit_message_iterate() {
        assert_eq!(
            commit_message(&EventType::ProjectIterationCompleted, "my-project"),
            "chore(my-project): automated iterate"
        );
    }

    #[test]
    fn commit_message_maintenance() {
        assert_eq!(
            commit_message(&EventType::ProjectMaintenanceCompleted, "my-project"),
            "chore(my-project): automated maintenance"
        );
    }

    #[test]
    fn commit_message_strategic() {
        assert_eq!(
            commit_message(&EventType::InnerIterationCompleted, "my-project"),
            "chore(my-project): strategic iterate cycle"
        );
    }

    #[test]
    fn commit_message_remediation_default() {
        assert_eq!(
            commit_message(&EventType::RemediationCompleted, "my-project"),
            "chore(my-project): automated remediation"
        );
    }

    #[tokio::test]
    async fn unknown_project_returns_failure() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = make_trigger("no-such-project", "CVE-2026-0001");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn clean_tree_emits_no_events() {
        let dir = TempDir::new().unwrap();
        let registry = registry_for("my-project", dir.path().to_str().unwrap(), true);
        let shell = clean_sequence();
        let block = CommitAndPush::with_gateways(registry, shell);
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
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = make_trigger("my-project", "CVE-2026-0003");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();
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
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = make_trigger("my-project", "CVE-2026-0004");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();
        assert_eq!(types, ["project_changes_committed"]);
    }

    #[test]
    fn sinks_on_includes_all_event_types() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let sinks = block.sinks_on();
        assert!(sinks.contains(&EventType::RemediationCompleted));
        assert!(sinks.contains(&EventType::ProjectIterationCompleted));
        assert!(sinks.contains(&EventType::ProjectMaintenanceCompleted));
        assert!(sinks.contains(&EventType::InnerIterationCompleted));
    }

    #[test]
    fn accepts_returns_false_when_nested_loop() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = test_event!(EventType::ProjectIterationCompleted, "proj", {
            "project": "proj",
            "success": true,
            "loop_context": { "strategic": { "iteration": 1 } }
        });
        assert!(!block.accepts(&trigger));
    }

    #[test]
    fn accepts_returns_false_when_no_changes() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = make_trigger_no_changes(EventType::ProjectIterationCompleted, "proj");
        assert!(!block.accepts(&trigger));
    }

    #[test]
    fn accepts_returns_true_when_remediation_in_loop_context() {
        // RemediationCompleted is not a completion event — loop_context does not trigger the
        // nested-loop guard, so accepts() must return true.
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = test_event!(EventType::RemediationCompleted, "proj", {
            "cve": "CVE-2026-0001",
            "loop_context": { "strategic": { "iteration": 1 } }
        });
        assert!(block.accepts(&trigger));
    }

    #[tokio::test]
    async fn inner_iteration_completed_commits_with_strategic_message() {
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
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = test_event!(EventType::InnerIterationCompleted, "my-project", {
            "project": "my-project",
            "success": true,
            "loop_context": { "strategic": { "iteration": 1 } }
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        let msg = result.events[0].payload["message"].as_str().unwrap();
        assert!(msg.contains("strategic"), "expected 'strategic' in '{msg}'");
    }

    #[tokio::test]
    async fn does_not_skip_remediation_with_loop_context() {
        // RemediationCompleted is not a completion event, so loop_context should not trigger skip.
        // However, with no registry entry the block returns an honest failure.
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = test_event!(EventType::RemediationCompleted, "my-project", {
            "cve": "CVE-2026-0001",
            "loop_context": { "strategic": { "iteration": 1 } }
        });

        let result = block.execute(&trigger).await.unwrap();

        // Should NOT skip (no nested-loop guard) but returns failure: project not in registry.
        assert!(!result.success);
        assert!(result.events.is_empty());
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
        let block = CommitAndPush::with_gateways(registry, shell);
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
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = make_trigger_for(EventType::ProjectIterationCompleted, "my-project");

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
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = make_trigger_for(EventType::ProjectMaintenanceCompleted, "my-project");

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
        let block = CommitAndPush::with_gateways(registry, shell);
        let trigger = make_trigger_for(EventType::ProjectIterationCompleted, "my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success, "should be success, not error");
        assert!(result.events.is_empty());
        assert_eq!(result.summary, "No changes to commit");
    }

    #[test]
    fn retry_policy_allows_retries() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
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

    // --- decide_commit pure function tests ---

    #[test]
    fn decide_commit_skips_nested_loop() {
        let trigger = test_event!(EventType::ProjectIterationCompleted, "proj", {
            "project": "proj",
            "success": true,
            "loop_context": { "strategic": { "iteration": 1 } }
        });
        assert_eq!(decide_commit(&trigger), CommitDecision::SkipNestedLoop);
    }

    #[test]
    fn decide_commit_skips_nested_loop_for_maintenance_completion() {
        let trigger = test_event!(EventType::ProjectMaintenanceCompleted, "proj", {
            "project": "proj",
            "success": true,
            "loop_context": { "strategic": { "iteration": 2 } }
        });
        assert_eq!(decide_commit(&trigger), CommitDecision::SkipNestedLoop);
    }

    #[test]
    fn decide_commit_skips_no_changes() {
        let trigger = make_trigger_no_changes(EventType::ProjectIterationCompleted, "proj");
        assert_eq!(decide_commit(&trigger), CommitDecision::SkipNoChanges);
    }

    #[test]
    fn decide_commit_proceeds_with_cve() {
        // make_trigger uses EventType::RemediationCompleted with a full payload including cve.
        // The RemediationCompletedPayload requires `success: bool` — use a complete payload.
        let trigger = test_helpers::make_trigger(
            EventType::RemediationCompleted,
            "proj",
            serde_json::json!({ "cve": "CVE-2026-5555", "success": true }),
        );
        assert!(
            matches!(decide_commit(&trigger), CommitDecision::Proceed { cve } if cve == "CVE-2026-5555")
        );
    }

    #[test]
    fn decide_commit_defaults_cve_to_unknown_when_payload_missing_required_fields() {
        // When the payload cannot be parsed as RemediationCompletedPayload (e.g. missing
        // required `success` field), CVE defaults to "unknown".
        let trigger = make_trigger_for(EventType::ProjectIterationCompleted, "proj");
        assert!(
            matches!(decide_commit(&trigger), CommitDecision::Proceed { cve } if cve == "unknown")
        );
    }

    #[test]
    fn dry_run_returns_empty_for_loop_context_trigger() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = test_event!(EventType::ProjectIterationCompleted, "proj", {
            "project": "proj",
            "success": true,
            "loop_context": { "strategic": { "iteration": 1 } }
        });
        assert!(block.dry_run_events(&trigger).is_empty());
    }

    #[test]
    fn dry_run_returns_empty_for_no_changes_trigger() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = make_trigger_no_changes(EventType::ProjectIterationCompleted, "proj");
        assert!(block.dry_run_events(&trigger).is_empty());
    }

    #[test]
    fn dry_run_and_accepts_agree_on_skip_for_nested_loop() {
        // Both dry_run_events and accepts() must reject a nested loop trigger.
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = test_event!(EventType::ProjectIterationCompleted, "proj", {
            "project": "proj",
            "success": true,
            "loop_context": { "strategic": { "iteration": 1 } }
        });
        assert!(!block.accepts(&trigger), "accepts() must reject nested loop");
        assert!(block.dry_run_events(&trigger).is_empty(), "dry_run must skip nested loop");
    }

    #[test]
    fn dry_run_and_accepts_agree_on_skip_for_no_changes() {
        let block = CommitAndPush::new(test_helpers::empty_registry());
        let trigger = make_trigger_no_changes(EventType::ProjectIterationCompleted, "proj");
        assert!(!block.accepts(&trigger), "accepts() must reject no-changes trigger");
        assert!(block.dry_run_events(&trigger).is_empty(), "dry_run must skip when no changes");
    }
}
