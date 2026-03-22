use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

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
    registry: Arc<Registry>,
    shell: Arc<dyn ShellGateway>,
}

impl ValidateProject {
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
    throttle: foundry_core::throttle::Throttle,
    reason: &str,
) -> TaskBlockResult {
    TaskBlockResult {
        events: vec![Event::new(
            EventType::ProjectValidationCompleted,
            project.to_string(),
            throttle,
            serde_json::json!({"status": "error", "reason": reason}),
        )],
        success: false,
        summary: format!("Validation failed for {project}: {reason}"),
    }
}

impl TaskBlock for ValidateProject {
    fn name(&self) -> &'static str {
        "Validate Project"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::MaintenanceRunStarted]
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let registry = Arc::clone(&self.registry);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            // Self-filter: only act on active (non-skipped) projects.
            let Some(entry) = registry.active_projects().into_iter().find(|p| p.name == project)
            else {
                tracing::info!(%project, "project skipped or not in registry, skipping validation");
                return Ok(TaskBlockResult {
                    events: vec![Event::new(
                        EventType::ProjectValidationCompleted,
                        project.clone(),
                        throttle,
                        serde_json::json!({
                            "status": "skipped",
                            "reason": "project skipped or not in registry"
                        }),
                    )],
                    success: true,
                    summary: format!("Project {project} skipped"),
                });
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
            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::ProjectValidationCompleted,
                    project.clone(),
                    throttle,
                    serde_json::json!({"status": "ok", "has_gates": has_gates}),
                )],
                success: true,
                summary: format!("Project {project} validated"),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::registry::{ProjectEntry, Registry};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::*;

    fn make_trigger(project: &str) -> Event {
        Event::new(
            EventType::MaintenanceRunStarted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
    }

    fn make_registry(entries: Vec<ProjectEntry>) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: entries,
        })
    }

    fn active_entry(name: &str, path: &str) -> ProjectEntry {
        ProjectEntry {
            name: name.to_string(),
            path: path.to_string(),
            stack: foundry_core::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: Some(false),
            actions: foundry_core::registry::ActionFlags::default(),
            install: None,
            timeout_secs: None,
        }
    }

    fn skipped_entry(name: &str, path: &str) -> ProjectEntry {
        ProjectEntry {
            name: name.to_string(),
            path: path.to_string(),
            stack: foundry_core::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: Some(true),
            actions: foundry_core::registry::ActionFlags::default(),
            install: None,
            timeout_secs: None,
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

    #[test]
    fn sinks_on_maintenance_run_started() {
        let block = ValidateProject::new(make_registry(vec![]));
        assert_eq!(block.sinks_on(), &[EventType::MaintenanceRunStarted]);
    }

    #[test]
    fn kind_is_observer() {
        let block = ValidateProject::new(make_registry(vec![]));
        assert_eq!(block.kind(), BlockKind::Observer);
    }

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
        // Write a gates file so has_gates=true can be tested too.
        let registry = make_registry(vec![ProjectEntry {
            name: "my-project".to_string(),
            path: path.to_string_lossy().to_string(),
            stack: foundry_core::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: Some(false),
            actions: foundry_core::registry::ActionFlags::default(),
            install: None,
            timeout_secs: None,
        }]);

        let shell = FakeShellGateway::always(ok_result("main"));
        let block = ValidateProject::with_shell(registry, shell);
        let trigger = make_trigger("my-project");

        let result = block.execute(&trigger).await.expect("should not error");
        assert!(result.success, "expected success: {:?}", result.events[0].payload);
        assert_eq!(result.events[0].payload["status"], "ok");
    }

    #[tokio::test]
    async fn wrong_branch_emits_error_with_fake() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        let registry = make_registry(vec![ProjectEntry {
            name: "my-project".to_string(),
            path: path.to_string_lossy().to_string(),
            stack: foundry_core::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: Some(false),
            actions: foundry_core::registry::ActionFlags::default(),
            install: None,
            timeout_secs: None,
        }]);

        // Fake reports we're on "feature-branch" but registry expects "main".
        let shell = FakeShellGateway::always(ok_result("feature-branch"));
        let block = ValidateProject::with_shell(registry, shell);
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
            stack: foundry_core::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: Some(false),
            actions: foundry_core::registry::ActionFlags::default(),
            install: None,
            timeout_secs: None,
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
        let block = ValidateProject::with_shell(registry, shell);
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
            stack: foundry_core::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: Some(false),
            actions: foundry_core::registry::ActionFlags::default(),
            install: None,
            timeout_secs: None,
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
        let block = ValidateProject::with_shell(registry, shell);
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
            stack: foundry_core::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: Some(false),
            actions: foundry_core::registry::ActionFlags::default(),
            install: None,
            timeout_secs: None,
        }]);

        let shell = FakeShellGateway::always(CommandResult {
            stdout: String::new(),
            stderr: "not a git repo".to_string(),
            exit_code: 128,
            success: false,
        });
        let block = ValidateProject::with_shell(registry, shell);
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
            stack: foundry_core::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: Some(false),
            actions: foundry_core::registry::ActionFlags::default(),
            install: None,
            timeout_secs: None,
        }]);
        let block = ValidateProject::new(registry);
        let trigger = make_trigger("test-project");

        let result = block.execute(&trigger).await.expect("should not error");
        assert!(result.success, "expected success: {:?}", result.events[0].payload);
        assert_eq!(result.events[0].payload["status"], "ok");
        assert_eq!(result.events[0].payload["has_gates"], false);
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
            stack: foundry_core::registry::Stack::Rust,
            agent: String::new(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: Some(false),
            actions: foundry_core::registry::ActionFlags::default(),
            install: None,
            timeout_secs: None,
        }]);
        let block = ValidateProject::new(registry);
        let trigger = make_trigger("test-project");

        let result = block.execute(&trigger).await.expect("should not error");
        assert!(result.success);
        assert_eq!(result.events[0].payload["has_gates"], true);
    }
}
