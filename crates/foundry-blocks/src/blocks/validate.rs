use std::path::Path;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::ProjectValidationCompletedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

use super::TriggerContext;

task_block_new! {
    /// Validates a project before the maintenance run proceeds.
    ///
    /// Observer — always runs regardless of throttle.
    ///
    /// Self-filters: projects that are not in the active registry (i.e., marked
    /// `skip = true` or entirely absent) are silently acknowledged without error.
    ///
    /// Checks (in order):
    /// 1. Project directory exists on disk.
    /// 2. Git branch matches the expected branch from the registry.
    ///    Recovers automatically from detached HEAD by checking out the expected branch.
    /// 3. `.hone-gates.json` is present (warning only, not a hard failure).
    ///
    /// Emits `ProjectValidationCompleted` with `status` ("ok" | "error" | "skipped")
    /// and an optional `reason` field.
    pub struct ValidateProject {
        shell: ShellGateway = crate::gateway::ProcessShellGateway
    }
}

/// Result of the git branch check — either an error reason or the resolved branch name.
enum BranchCheckOutcome {
    Ok,
    Err(String),
}

/// Verify the git branch at `path` matches `expected_branch`.
///
/// Recovers from detached HEAD by checking out `expected_branch`.
/// Returns `Ok(BranchCheckOutcome)` on success (spawn-level errors propagate as `Err`).
async fn check_git_branch(
    project: &str,
    path: &Path,
    expected_branch: &str,
    shell: &dyn ShellGateway,
) -> anyhow::Result<BranchCheckOutcome> {
    let result = shell
        .run(path, "git", &["rev-parse", "--abbrev-ref", "HEAD"], None, None)
        .await?;

    if result.exit_code != 0 {
        let reason = format!("git rev-parse failed: {}", result.stderr.trim());
        tracing::warn!(%project, %reason, "git check failed");
        return Ok(BranchCheckOutcome::Err(reason));
    }

    let current_branch = result.stdout.trim().to_string();

    if current_branch == "HEAD" {
        tracing::warn!(%project, %expected_branch, "detached HEAD detected, attempting recovery");
        let checkout = shell.run(path, "git", &["checkout", expected_branch], None, None).await?;
        if checkout.exit_code != 0 {
            let reason = format!("detached HEAD and checkout failed: {}", checkout.stderr.trim());
            return Ok(BranchCheckOutcome::Err(reason));
        }
        tracing::info!(%project, %expected_branch, "recovered from detached HEAD");
        return Ok(BranchCheckOutcome::Ok);
    }

    if current_branch != expected_branch {
        let reason = format!("wrong branch: {current_branch}, expected {expected_branch}");
        tracing::warn!(%project, %reason, "branch mismatch");
        return Ok(BranchCheckOutcome::Err(reason));
    }

    Ok(BranchCheckOutcome::Ok)
}

fn error_result(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    reason: &str,
) -> TaskBlockResult {
    super::emit_event_result(
        format!("Validation failed for {project}: {reason}"),
        false,
        EventType::ProjectValidationCompleted,
        project,
        throttle,
        &ProjectValidationCompletedPayload {
            project: project.to_string(),
            status: "error".to_string(),
            reason: Some(reason.to_string()),
            ..Default::default()
        },
    )
    .expect("ProjectValidationCompletedPayload is infallibly serializable")
}

impl TaskBlock for ValidateProject {
    task_block_meta! {
        name: "Validate Project",
        kind: Observer,
        sinks_on: [ProjectRunStarted],
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project, throttle, ..
        } = TriggerContext::from_trigger(trigger);
        // Extract the entry before the async boundary — the RwLock guard must not cross await.
        let entry = match super::read_registry(&self.registry) {
            Ok(guard) => guard.active_projects().into_iter().find(|p| p.name == project).cloned(),
            Err(e) => return Box::pin(async move { Err(e) }),
        };
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            // Domain skip: emitting ProjectValidationCompleted { status: "skipped" } is the
            // intended domain behavior here — not a routing guard. Per the accepts() convention,
            // this stays in execute() because the skip event is a meaningful domain fact.
            let Some(entry) = entry else {
                tracing::info!(%project, "project skipped or not in registry, skipping validation");
                return super::emit_result(
                    format!("Project {project} skipped"),
                    EventType::ProjectValidationCompleted,
                    &project,
                    throttle,
                    &ProjectValidationCompletedPayload {
                        project: project.clone(),
                        status: "skipped".to_string(),
                        reason: Some("project skipped or not in registry".to_string()),
                        ..Default::default()
                    },
                );
            };

            let path = Path::new(&entry.path);
            let expected_branch = entry.branch.clone();

            // 1. Directory must exist.
            if !path.exists() {
                tracing::warn!(%project, path = %path.display(), "project directory not found");
                return Ok(error_result(&project, throttle, "directory not found"));
            }

            // 2. Check git branch (recovers from detached HEAD).
            if let BranchCheckOutcome::Err(reason) =
                check_git_branch(&project, path, &expected_branch, shell.as_ref()).await?
            {
                return Ok(error_result(&project, throttle, &reason));
            }

            // 3. Check for .hone-gates.json (warning only — validation still passes).
            let has_gates = path.join(".hone-gates.json").exists();
            if !has_gates {
                tracing::warn!(%project, "missing .hone-gates.json");
            }

            tracing::info!(%project, %has_gates, "project validated successfully");
            super::emit_result(
                format!("Project {project} validated"),
                EventType::ProjectValidationCompleted,
                &project,
                throttle,
                &ProjectValidationCompletedPayload {
                    project: project.clone(),
                    status: "ok".to_string(),
                    has_gates,
                    actions: Some(
                        serde_json::to_value(&entry.actions)
                            .expect("ActionFlags is infallibly serializable"),
                    ),
                    ..Default::default()
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::registry::{ProjectEntry, Registry};
    use foundry_sdk::throttle::Throttle;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::*;

    fn make_trigger(project: &str) -> Event {
        Event::new(
            EventType::ProjectRunStarted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
    }

    fn make_registry(entries: Vec<ProjectEntry>) -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
            version: 2,
            projects: entries,
        }))
    }

    fn active_entry(name: &str, path: &str) -> ProjectEntry {
        ProjectEntry {
            name: name.to_string(),
            path: path.to_string(),
            stack: foundry_sdk::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: foundry_sdk::registry::ActionFlags::default(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        }
    }

    fn skipped_entry(name: &str, path: &str) -> ProjectEntry {
        ProjectEntry {
            name: name.to_string(),
            path: path.to_string(),
            stack: foundry_sdk::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: Some("test skip".to_string()),
            notes: None,
            actions: foundry_sdk::registry::ActionFlags::default(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        }
    }

    fn ok_result(branch: &str) -> CommandResult {
        CommandResult {
            stdout: format!("{branch}\n"),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    fn init_git_repo(path: &std::path::Path) {
        std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(path)
            .output()
            .expect("git init");
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(path)
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(path)
            .output()
            .ok();
        // Need at least one commit so HEAD resolves to a branch name.
        std::process::Command::new("git")
            .args(["commit", "--allow-empty", "-m", "init"])
            .current_dir(path)
            .output()
            .expect("git commit");
    }

    // -- Metadata tests (no filesystem or git) --

    assert_block_meta!(
        ValidateProject::new(Arc::new(RwLock::new(Registry { version: 2, projects: vec![] }))),
        kind: Observer,
        sinks_on: [ProjectRunStarted],
    );

    // -- Self-filter tests --

    #[tokio::test]
    async fn skipped_project_emits_skipped_status() {
        let registry = make_registry(vec![skipped_entry("my-project", "/tmp/my-project")]);
        let block = ValidateProject::new(registry);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.expect("should not error");
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].payload["status"], "skipped");
    }

    #[tokio::test]
    async fn project_not_in_registry_emits_skipped_status() {
        let registry = make_registry(vec![]);
        let block = ValidateProject::new(registry);
        let trigger = make_trigger("unknown-project");

        let result = block.execute(&trigger).await.expect("should not error");
        assert!(result.success);
        assert_eq!(result.events[0].payload["status"], "skipped");
    }

    // -- Directory existence tests --

    #[tokio::test]
    async fn missing_directory_emits_error_status() {
        let registry = make_registry(vec![active_entry(
            "my-project",
            "/nonexistent/path/that/does/not/exist",
        )]);
        let block = ValidateProject::new(registry);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.expect("should not error");
        assert!(!result.success);
        assert_eq!(result.events[0].payload["status"], "error");
        assert_eq!(result.events[0].payload["reason"], "directory not found");
    }

    // -- Git branch tests using FakeShellGateway --

    #[tokio::test]
    async fn correct_branch_emits_ok_with_fake() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        let registry = make_registry(vec![ProjectEntry {
            name: "my-project".to_string(),
            path: path.to_string_lossy().to_string(),
            stack: foundry_sdk::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: foundry_sdk::registry::ActionFlags {
                iterate: true,
                maintain: true,
                push: false,
                audit: true,
                release: false,
            },
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        }]);

        let shell = FakeShellGateway::always(ok_result("main"));
        let block = ValidateProject::with_gateways(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.expect("should not error");
        assert!(result.success, "expected success: {:?}", result.events[0].payload);
        assert_eq!(result.events[0].payload["status"], "ok");
        let actions = &result.events[0].payload["actions"];
        assert_eq!(actions["iterate"], true);
        assert_eq!(actions["maintain"], true);
        assert_eq!(actions["push"], false);
        assert_eq!(actions["audit"], true);
        assert_eq!(actions["release"], false);
    }

    #[tokio::test]
    async fn wrong_branch_emits_error_with_fake() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        let registry = make_registry(vec![ProjectEntry {
            name: "my-project".to_string(),
            path: path.to_string_lossy().to_string(),
            stack: foundry_sdk::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: foundry_sdk::registry::ActionFlags::default(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        }]);

        // Fake reports we're on "feature-branch" but registry expects "main".
        let shell = FakeShellGateway::always(ok_result("feature-branch"));
        let block = ValidateProject::with_gateways(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.expect("should not error");
        assert!(!result.success);
        assert_eq!(result.events[0].payload["status"], "error");
        let reason = result.events[0].payload["reason"].as_str().unwrap();
        assert!(reason.contains("wrong branch"), "unexpected reason: {reason}");
    }

    #[tokio::test]
    async fn detached_head_recovery_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        let registry = make_registry(vec![ProjectEntry {
            name: "my-project".to_string(),
            path: path.to_string_lossy().to_string(),
            stack: foundry_sdk::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: foundry_sdk::registry::ActionFlags::default(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        }]);

        // First call: rev-parse returns "HEAD" (detached).
        // Second call: checkout succeeds (exit 0).
        let shell = FakeShellGateway::sequence(vec![
            ok_result("HEAD"),
            CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = ValidateProject::with_gateways(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.expect("should not error");
        assert!(result.success, "expected ok after recovery: {:?}", result.events[0].payload);
        assert_eq!(result.events[0].payload["status"], "ok");
    }

    #[tokio::test]
    async fn detached_head_recovery_failure() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        let registry = make_registry(vec![ProjectEntry {
            name: "my-project".to_string(),
            path: path.to_string_lossy().to_string(),
            stack: foundry_sdk::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: foundry_sdk::registry::ActionFlags::default(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        }]);

        // First: rev-parse returns "HEAD"; second: checkout fails.
        let shell = FakeShellGateway::sequence(vec![
            ok_result("HEAD"),
            CommandResult {
                stdout: String::new(),
                stderr: "branch not found".to_string(),
                exit_code: 1,
                success: false,
            },
        ]);
        let block = ValidateProject::with_gateways(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.expect("should not error");
        assert!(!result.success);
        assert_eq!(result.events[0].payload["status"], "error");
        let reason = result.events[0].payload["reason"].as_str().unwrap();
        assert!(reason.contains("detached HEAD and checkout failed"), "unexpected: {reason}");
    }

    #[tokio::test]
    async fn git_rev_parse_failure_emits_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        let registry = make_registry(vec![ProjectEntry {
            name: "my-project".to_string(),
            path: path.to_string_lossy().to_string(),
            stack: foundry_sdk::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: foundry_sdk::registry::ActionFlags::default(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        }]);

        let shell = FakeShellGateway::always(CommandResult {
            stdout: String::new(),
            stderr: "not a git repo".to_string(),
            exit_code: 128,
            success: false,
        });
        let block = ValidateProject::with_gateways(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.expect("should not error");
        assert!(!result.success);
        assert_eq!(result.events[0].payload["status"], "error");
        let reason = result.events[0].payload["reason"].as_str().unwrap();
        assert!(reason.contains("git rev-parse failed"), "unexpected: {reason}");
    }

    // -- .hone-gates.json tests (uses tempdir) --

    #[tokio::test]
    async fn missing_gates_still_emits_ok() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        init_git_repo(path);

        let registry = make_registry(vec![ProjectEntry {
            name: "test-project".to_string(),
            path: path.to_string_lossy().to_string(),
            stack: foundry_sdk::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: foundry_sdk::registry::ActionFlags::default(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        }]);
        let block = ValidateProject::new(registry);
        let trigger = make_trigger("test-project");

        let result = block.execute(&trigger).await.expect("should not error");
        assert!(result.success, "expected success: {:?}", result.events[0].payload);
        assert_eq!(result.events[0].payload["status"], "ok");
        assert_eq!(result.events[0].payload["has_gates"], false);
        // Default ActionFlags — all false.
        let actions = &result.events[0].payload["actions"];
        assert_eq!(actions["iterate"], false);
        assert_eq!(actions["maintain"], false);
    }

    #[tokio::test]
    async fn gates_file_present_sets_has_gates_true() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        init_git_repo(path);

        // Create the gates file.
        std::fs::write(path.join(".hone-gates.json"), b"{}").expect("write gates");

        let registry = make_registry(vec![ProjectEntry {
            name: "test-project".to_string(),
            path: path.to_string_lossy().to_string(),
            stack: foundry_sdk::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: foundry_sdk::registry::ActionFlags::default(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
        }]);
        let block = ValidateProject::new(registry);
        let trigger = make_trigger("test-project");

        let result = block.execute(&trigger).await.expect("should not error");
        assert!(result.success);
        assert_eq!(result.events[0].payload["has_gates"], true);
        // Default ActionFlags — all false.
        let actions = &result.events[0].payload["actions"];
        assert_eq!(actions["iterate"], false);
        assert_eq!(actions["maintain"], false);
    }
}
