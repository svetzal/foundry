use std::path::Path;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{GitSyncFailure, ProjectValidationCompletedPayload};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

use super::TriggerContext;
use super::checkout_sync::{SyncOutcome, sync_checkout};

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
    /// 3. The checkout is synced with `origin/<branch>` by fast-forward only
    ///    (see `checkout_sync::sync_checkout`). A dirty tree, a diverged branch,
    ///    or an unreachable remote fails validation with a typed `sync_failure`
    ///    (`dirty_tree` | `diverged` | `remote_unavailable`) and leaves the
    ///    checkout untouched. Under `dry_run` the sync is simulated: no fetch,
    ///    no merge, and `fast_forwarded` reports what would be applied.
    /// 4. `.hone-gates.json` is present (warning only, not a hard failure).
    ///
    /// Emits `ProjectValidationCompleted` with `status` ("ok" | "error" | "skipped"),
    /// an optional `reason` field, and — once the sync has run — `fast_forwarded`
    /// or `sync_failure`.
    pub struct ValidateProject {
        shell: ShellGateway = crate::gateway::ProcessShellGateway
    }
}

/// Result of the git branch check — either an error reason or the resolved branch name.
enum BranchCheckOutcome {
    Ok,
    Err(String),
    /// The checkout is on another branch than the registry's.
    WrongBranch(String),
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
        return Ok(BranchCheckOutcome::WrongBranch(reason));
    }

    Ok(BranchCheckOutcome::Ok)
}

/// Why a run must not start here, when the filesystem holding the project or
/// `~/.foundry` is low on space.
fn insufficient_disk(path: &Path, threshold: foundry_sdk::disk::DiskThreshold) -> Option<String> {
    foundry_sdk::disk::ensure_room(
        &[path.to_path_buf(), foundry_sdk::paths::foundry_home()],
        threshold,
    )
    .err()
}

fn error_result(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    reason: &str,
) -> TaskBlockResult {
    failure_result(project, throttle, reason, None)
}

fn failure_result(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    reason: &str,
    sync_failure: Option<GitSyncFailure>,
) -> TaskBlockResult {
    #[allow(
        clippy::expect_used,
        reason = "ProjectValidationCompletedPayload is infallibly serializable (Payload Conventions, AGENTS.md)"
    )]
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
            sync_failure,
            dry_run: (!throttle.permits_mutation()).then_some(true),
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

            // 1b. Refuse to start work on a full disk: it fails late and
            // leaves half-written state behind.
            if let Some(reason) =
                insufficient_disk(path, foundry_sdk::disk::DiskThreshold::from_env())
            {
                tracing::warn!(%project, %reason, "refusing to start: insufficient disk");
                return Ok(error_result(&project, throttle, &reason));
            }

            // 2. Check git branch (recovers from detached HEAD).
            match check_git_branch(&project, path, &expected_branch, shell.as_ref()).await? {
                BranchCheckOutcome::Ok => {}
                BranchCheckOutcome::Err(reason) => {
                    return Ok(error_result(&project, throttle, &reason));
                }
                BranchCheckOutcome::WrongBranch(reason) => {
                    return Ok(failure_result(
                        &project,
                        throttle,
                        &reason,
                        Some(GitSyncFailure::WrongBranch),
                    ));
                }
            }

            // 3. Sync the checkout with its remote before any work touches it.
            let dry_run = !throttle.permits_mutation();
            let fast_forwarded = match sync_checkout(
                shell.as_ref(),
                path,
                &expected_branch,
                dry_run,
            )
            .await?
            {
                SyncOutcome::Synced { fast_forwarded } => {
                    tracing::info!(%project, fast_forwarded, dry_run, "checkout synced with remote");
                    fast_forwarded
                }
                SyncOutcome::Refused { failure, detail } => {
                    tracing::warn!(%project, %failure, %detail, "checkout sync refused");
                    let reason = format!("{failure}: {detail}");
                    return Ok(failure_result(&project, throttle, &reason, Some(failure)));
                }
            };

            // 4. Check for .hone-gates.json (warning only — validation still passes).
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
                    fast_forwarded: Some(fast_forwarded),
                    dry_run: dry_run.then_some(true),
                    actions: Some({
                        #[allow(
                            clippy::expect_used,
                            reason = "ActionFlags is infallibly serializable (Payload Conventions, AGENTS.md)"
                        )]
                        let value = serde_json::to_value(&entry.actions)
                            .expect("ActionFlags is infallibly serializable");
                        value
                    }),
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
            update_policy: None,
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
            update_policy: None,
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

    /// Script: rev-parse → `branch`, status clean, fetch ok, level with origin.
    fn synced_sequence(branch: &str) -> Vec<CommandResult> {
        vec![
            ok_result(branch),
            ok_result(""),
            ok_result(""),
            ok_result("0\t0"),
        ]
    }

    /// Run git hermetically: ignore global config and any `GIT_CONFIG_*`
    /// overrides the host environment injects (e.g. a disabled push URL).
    fn git(path: &std::path::Path, args: &[&str]) {
        let mut command = std::process::Command::new("git");
        command
            .args(args)
            .current_dir(path)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS");
        for index in 0..8 {
            command.env_remove(format!("GIT_CONFIG_KEY_{index}"));
            command.env_remove(format!("GIT_CONFIG_VALUE_{index}"));
        }
        let out = command.output().expect("spawn git");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// Init a repo at `path` on `main` with one commit, pushed to a bare
    /// `origin` created inside the returned tempdir.
    fn init_git_repo(path: &std::path::Path) -> tempfile::TempDir {
        let origin = tempfile::tempdir().expect("origin tempdir");
        git(origin.path(), &["init", "--bare", "-b", "main"]);
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
        git(path, &["remote", "add", "origin", &origin.path().to_string_lossy()]);
        git(path, &["push", "-u", "origin", "main"]);
        origin
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
            update_policy: None,
        }]);

        let shell = FakeShellGateway::sequence(synced_sequence("main"));
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
            update_policy: None,
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
        assert_eq!(
            result.events[0].payload["sync_failure"], "wrong_branch",
            "typed, so the maintenance summary can list the project loudly"
        );
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
            update_policy: None,
        }]);

        // First call: rev-parse returns "HEAD" (detached).
        // Second call: checkout succeeds (exit 0).
        let mut script = vec![ok_result("HEAD")];
        script.extend(synced_sequence("")); // checkout succeeds, then a level sync
        let shell = FakeShellGateway::sequence(script);
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
            update_policy: None,
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
            update_policy: None,
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
        let _origin = init_git_repo(path);

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
            update_policy: None,
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
        let _origin = init_git_repo(path);

        // Create and commit the gates file (an untracked file would be a dirty tree).
        std::fs::write(path.join(".hone-gates.json"), b"{}").expect("write gates");
        git(path, &["add", ".hone-gates.json"]);
        git(path, &["commit", "-m", "gates"]);

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
            update_policy: None,
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

    // -- Checkout sync (scripted shell) --

    fn sync_registry(path: &std::path::Path) -> Arc<RwLock<Registry>> {
        make_registry(vec![active_entry("my-project", &path.to_string_lossy())])
    }

    fn git_calls(shell: &FakeShellGateway) -> Vec<String> {
        shell.invocations().into_iter().map(|i| i.args.join(" ")).collect()
    }

    #[tokio::test]
    async fn clean_and_behind_fast_forwards_and_records_count() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shell = FakeShellGateway::sequence(vec![
            ok_result("main"),
            ok_result(""),
            ok_result(""),
            ok_result("0\t12"),
            ok_result("Updating abc..def"),
        ]);
        let block =
            ValidateProject::with_gateways(sync_registry(dir.path()), Arc::clone(&shell) as _);

        let result = block.execute(&make_trigger("my-project")).await.expect("should not error");

        assert!(result.success, "{:?}", result.events[0].payload);
        let payload = &result.events[0].payload;
        assert_eq!(payload["status"], "ok");
        assert_eq!(payload["fast_forwarded"], 12);
        assert!(payload.get("dry_run").is_none());
        assert_eq!(
            git_calls(&shell).last().map(String::as_str),
            Some("merge --ff-only origin/main")
        );
    }

    #[tokio::test]
    async fn dirty_tree_fails_with_typed_reason_and_no_mutations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shell = FakeShellGateway::sequence(vec![ok_result("main"), ok_result(" M src/lib.rs")]);
        let block =
            ValidateProject::with_gateways(sync_registry(dir.path()), Arc::clone(&shell) as _);

        let result = block.execute(&make_trigger("my-project")).await.expect("should not error");

        assert!(!result.success);
        let payload = &result.events[0].payload;
        assert_eq!(payload["status"], "error");
        assert_eq!(payload["sync_failure"], "dirty_tree");
        assert_eq!(git_calls(&shell), ["rev-parse --abbrev-ref HEAD", "status --porcelain"]);
    }

    #[tokio::test]
    async fn diverged_fails_with_typed_reason_and_no_checkout_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shell = FakeShellGateway::sequence(vec![
            ok_result("main"),
            ok_result(""),
            ok_result(""),
            ok_result("1\t3"),
        ]);
        let block =
            ValidateProject::with_gateways(sync_registry(dir.path()), Arc::clone(&shell) as _);

        let result = block.execute(&make_trigger("my-project")).await.expect("should not error");

        assert!(!result.success);
        let payload = &result.events[0].payload;
        assert_eq!(payload["status"], "error");
        assert_eq!(payload["sync_failure"], "diverged");
        let calls = git_calls(&shell);
        assert!(
            !calls.iter().any(|c| c.starts_with("merge") || c.starts_with("rebase")),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn dry_run_reports_would_fast_forward_without_fetch_or_merge() {
        let dir = tempfile::tempdir().expect("tempdir");
        let shell =
            FakeShellGateway::sequence(vec![ok_result("main"), ok_result(""), ok_result("0\t5")]);
        let block =
            ValidateProject::with_gateways(sync_registry(dir.path()), Arc::clone(&shell) as _);
        let trigger = Event::new(
            EventType::ProjectRunStarted,
            "my-project".to_string(),
            Throttle::DryRun,
            serde_json::json!({}),
        );

        let result = block.execute(&trigger).await.expect("should not error");

        assert!(result.success);
        let payload = &result.events[0].payload;
        assert_eq!(payload["fast_forwarded"], 5);
        assert_eq!(payload["dry_run"], true);
        let calls = git_calls(&shell);
        assert!(
            !calls.iter().any(|c| c.starts_with("fetch") || c.starts_with("merge")),
            "{calls:?}"
        );
    }

    // -- Checkout sync (real git) --

    #[tokio::test]
    async fn real_git_behind_checkout_is_fast_forwarded_before_work() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path();
        let origin = init_git_repo(path);

        // Another host pushes two commits to origin.
        let other = tempfile::tempdir().expect("other tempdir");
        git(other.path(), &["clone", &origin.path().to_string_lossy(), "."]);
        git(other.path(), &["config", "user.email", "o@example.com"]);
        git(other.path(), &["config", "user.name", "Other"]);
        git(other.path(), &["commit", "--allow-empty", "-m", "one"]);
        git(other.path(), &["commit", "--allow-empty", "-m", "two"]);
        git(other.path(), &["push", "origin", "main"]);

        let block = ValidateProject::new(sync_registry(path));
        let result = block.execute(&make_trigger("my-project")).await.expect("should not error");

        assert!(result.success, "{:?}", result.events[0].payload);
        assert_eq!(result.events[0].payload["fast_forwarded"], 2);
        let head = std::process::Command::new("git")
            .args(["log", "-1", "--format=%s"])
            .current_dir(path)
            .output()
            .expect("git log");
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "two");
    }

    #[test]
    fn a_low_disk_refuses_the_run_with_a_clear_reason() {
        use foundry_sdk::disk::DiskThreshold;
        let dir = tempfile::tempdir().unwrap();
        let off = DiskThreshold {
            min_free_bytes: 0,
            min_free_percent: 0,
        };
        assert_eq!(super::insufficient_disk(dir.path(), off), None);
        let impossible = DiskThreshold {
            min_free_bytes: u64::MAX,
            min_free_percent: 101,
        };
        let reason = super::insufficient_disk(dir.path(), impossible).unwrap();
        assert!(reason.starts_with("insufficient disk: "), "{reason}");
    }
}
