use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

task_block_new! {
    /// Cleans up stale git branches and worktrees after validation.
    ///
    /// Observer — sinks on `ProjectValidationCompleted` (status = "ok" only).
    ///
    /// Performs two housekeeping tasks:
    /// 1. Deletes local branches that are fully merged into the expected branch.
    ///    Uses `git branch -d` which is safe — it refuses to delete unmerged work.
    /// 2. Removes stale git worktrees (prunable entries reported by
    ///    `git worktree list --porcelain`).
    ///
    /// This block emits no downstream events — it is pure housekeeping.
    /// Failures are logged as warnings but do not fail the workflow.
    pub struct CleanupBranches {
        shell: ShellGateway = crate::gateway::ProcessShellGateway
    }
}

/// Delete local branches that are fully merged into `target_branch`.
async fn cleanup_merged_branches(
    project: &str,
    path: &Path,
    target_branch: &str,
    shell: &dyn ShellGateway,
) -> Vec<String> {
    let mut deleted = Vec::new();

    let result =
        match shell.run(path, "git", &["branch", "--merged", target_branch], None, None).await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(%project, error = %err, "failed to list merged branches");
                return deleted;
            }
        };

    if !result.success {
        tracing::warn!(%project, stderr = %result.stderr.trim(), "git branch --merged failed");
        return deleted;
    }

    for line in result.stdout.lines() {
        let branch = line.trim();

        // Skip the current branch marker, empty lines, and the target branch itself.
        if branch.is_empty() || branch.starts_with('*') || branch == target_branch {
            continue;
        }

        tracing::info!(%project, %branch, "deleting merged branch");
        match shell.run(path, "git", &["branch", "-d", branch], None, None).await {
            Ok(r) if r.success => {
                deleted.push(branch.to_string());
            }
            Ok(r) => {
                tracing::warn!(%project, %branch, stderr = %r.stderr.trim(), "branch delete failed");
            }
            Err(err) => {
                tracing::warn!(%project, %branch, error = %err, "branch delete error");
            }
        }
    }

    deleted
}

/// Remove stale git worktrees.
///
/// Runs `git worktree prune` to clean up worktree metadata for directories
/// that no longer exist, then removes any remaining worktree directories
/// under `.claude/worktrees/` whose branches are fully merged.
async fn cleanup_stale_worktrees(project: &str, path: &Path, shell: &dyn ShellGateway) -> usize {
    // First, prune worktree metadata for directories that no longer exist on disk.
    match shell.run(path, "git", &["worktree", "prune"], None, None).await {
        Ok(r) if r.success => {
            tracing::debug!(%project, "git worktree prune succeeded");
        }
        Ok(r) => {
            tracing::warn!(%project, stderr = %r.stderr.trim(), "git worktree prune failed");
        }
        Err(err) => {
            tracing::warn!(%project, error = %err, "git worktree prune error");
        }
    }

    // List remaining worktrees and remove any that are not the main working tree.
    let result =
        match shell.run(path, "git", &["worktree", "list", "--porcelain"], None, None).await {
            Ok(r) if r.success => r,
            Ok(r) => {
                tracing::warn!(%project, stderr = %r.stderr.trim(), "git worktree list failed");
                return 0;
            }
            Err(err) => {
                tracing::warn!(%project, error = %err, "git worktree list error");
                return 0;
            }
        };

    // Porcelain format: blocks separated by blank lines.
    // Each block has "worktree <path>" as the first line.
    // The main worktree is the first entry — skip it.
    let mut removed = 0;
    let mut is_first = true;

    for block in result.stdout.split("\n\n") {
        if is_first {
            is_first = false;
            continue;
        }

        let Some(wt_path) = block
            .lines()
            .find(|l| l.starts_with("worktree "))
            .map(|l| l.trim_start_matches("worktree "))
        else {
            continue;
        };

        if wt_path.is_empty() {
            continue;
        }

        tracing::info!(%project, worktree = %wt_path, "removing stale worktree");
        match shell
            .run(path, "git", &["worktree", "remove", "--force", wt_path], None, None)
            .await
        {
            Ok(r) if r.success => {
                removed += 1;
            }
            Ok(r) => {
                tracing::warn!(%project, worktree = %wt_path, stderr = %r.stderr.trim(), "worktree remove failed");
            }
            Err(err) => {
                tracing::warn!(%project, worktree = %wt_path, error = %err, "worktree remove error");
            }
        }
    }

    removed
}

impl TaskBlock for CleanupBranches {
    task_block_meta! {
        name: "Cleanup Branches",
        kind: Observer,
        sinks_on: [ProjectValidationCompleted],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let payload = trigger.payload.clone();
        let registry = Arc::clone(&self.registry);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            // Self-filter: only act on successful validations.
            let status = payload.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if status != "ok" {
                return Ok(TaskBlockResult::success("Skipped: validation not ok", vec![]));
            }

            let Some(entry) = registry.find_project(&project) else {
                return Ok(TaskBlockResult::success(
                    format!("Skipped: {project} not in registry"),
                    vec![],
                ));
            };

            let path = Path::new(&entry.path);
            let target_branch = &entry.branch;

            let deleted =
                cleanup_merged_branches(&project, path, target_branch, shell.as_ref()).await;
            let worktrees_removed = cleanup_stale_worktrees(&project, path, shell.as_ref()).await;

            let summary = format!(
                "{project}: cleaned up {} merged branch(es), {} stale worktree(s)",
                deleted.len(),
                worktrees_removed,
            );
            tracing::info!(%project, branches = deleted.len(), worktrees = worktrees_removed, "cleanup complete");

            Ok(TaskBlockResult::success(summary, vec![]))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::CleanupBranches;

    fn make_registry(name: &str, path: &str) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: name.to_string(),
                path: path.to_string(),
                stack: Stack::Rust,
                agent: String::new(),
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

    fn validation_ok(project: &str) -> Event {
        Event::new(
            EventType::ProjectValidationCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({"status": "ok"}),
        )
    }

    fn validation_error(project: &str) -> Event {
        Event::new(
            EventType::ProjectValidationCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({"status": "error", "reason": "wrong branch"}),
        )
    }

    fn validation_skipped(project: &str) -> Event {
        Event::new(
            EventType::ProjectValidationCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({"status": "skipped"}),
        )
    }

    fn ok(stdout: &str) -> CommandResult {
        CommandResult {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    // -- Metadata tests --

    #[test]
    fn kind_is_observer() {
        let block = CleanupBranches::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_project_validation_completed() {
        let block = CleanupBranches::new(Arc::new(Registry {
            version: 2,
            projects: vec![],
        }));
        assert_eq!(block.sinks_on(), &[EventType::ProjectValidationCompleted]);
    }

    // -- Self-filter tests --

    #[tokio::test]
    async fn skips_when_validation_not_ok() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success();
        let block = CleanupBranches::with_gateways(registry, shell.clone());

        let result = block.execute(&validation_error("my-project")).await.unwrap();

        assert!(result.success);
        assert!(result.summary.contains("Skipped"));
        assert!(shell.invocations().is_empty(), "should not invoke any git commands");
    }

    #[tokio::test]
    async fn skips_when_validation_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success();
        let block = CleanupBranches::with_gateways(registry, shell.clone());

        let result = block.execute(&validation_skipped("my-project")).await.unwrap();

        assert!(result.success);
        assert!(result.summary.contains("Skipped"));
    }

    #[tokio::test]
    async fn skips_when_project_not_in_registry() {
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![],
        });
        let shell = FakeShellGateway::success();
        let block = CleanupBranches::with_gateways(registry, shell);

        let result = block.execute(&validation_ok("unknown")).await.unwrap();

        assert!(result.success);
        assert!(result.summary.contains("Skipped"));
    }

    // -- Branch cleanup tests --

    #[tokio::test]
    async fn deletes_merged_branches() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry("my-project", dir.path().to_str().unwrap());

        // Sequence: git branch --merged → lists branches,
        //           git branch -d feat/old → success,
        //           git branch -d hopper/abc → success,
        //           git worktree prune → success,
        //           git worktree list --porcelain → main only
        let shell = FakeShellGateway::sequence(vec![
            // git branch --merged main
            ok("* main\n  feat/old\n  hopper/abc123\n"),
            // git branch -d feat/old
            ok("Deleted branch feat/old"),
            // git branch -d hopper/abc123
            ok("Deleted branch hopper/abc123"),
            // git worktree prune
            ok(""),
            // git worktree list --porcelain
            ok(&format!(
                "worktree {}\nHEAD abc123\nbranch refs/heads/main\n\n",
                dir.path().display()
            )),
        ]);
        let block = CleanupBranches::with_gateways(registry, shell);

        let result = block.execute(&validation_ok("my-project")).await.unwrap();

        assert!(result.success);
        assert!(result.summary.contains("2 merged branch(es)"));
        assert!(result.summary.contains("0 stale worktree(s)"));
    }

    #[tokio::test]
    async fn skips_current_and_target_branch() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry("my-project", dir.path().to_str().unwrap());

        // Only main listed (with * marker) — nothing to delete
        let shell = FakeShellGateway::sequence(vec![
            ok("* main\n"),
            // git worktree prune
            ok(""),
            // git worktree list --porcelain
            ok(&format!(
                "worktree {}\nHEAD abc123\nbranch refs/heads/main\n\n",
                dir.path().display()
            )),
        ]);
        let block = CleanupBranches::with_gateways(registry, shell);

        let result = block.execute(&validation_ok("my-project")).await.unwrap();

        assert!(result.success);
        assert!(result.summary.contains("0 merged branch(es)"));
    }

    // -- Worktree cleanup tests --

    #[tokio::test]
    async fn removes_stale_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry("my-project", dir.path().to_str().unwrap());

        let worktree_path = format!("{}/.claude/worktrees/agent-abc123", dir.path().display());

        let shell = FakeShellGateway::sequence(vec![
            // git branch --merged main
            ok("* main\n"),
            // git worktree prune
            ok(""),
            // git worktree list --porcelain — main + stale worktree
            ok(&format!(
                "worktree {main}\nHEAD abc123\nbranch refs/heads/main\n\n\
                 worktree {wt}\nHEAD def456\nbranch refs/heads/worktree-agent-abc123\n\n",
                main = dir.path().display(),
                wt = worktree_path,
            )),
            // git worktree remove --force <path>
            ok(""),
        ]);
        let block = CleanupBranches::with_gateways(registry, shell);

        let result = block.execute(&validation_ok("my-project")).await.unwrap();

        assert!(result.success);
        assert!(result.summary.contains("1 stale worktree(s)"));
    }

    // -- No events emitted --

    #[tokio::test]
    async fn emits_no_downstream_events() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry("my-project", dir.path().to_str().unwrap());

        let shell = FakeShellGateway::sequence(vec![
            ok("* main\n  old-branch\n"),
            ok("Deleted branch old-branch"),
            ok(""),
            ok(&format!(
                "worktree {}\nHEAD abc\nbranch refs/heads/main\n\n",
                dir.path().display()
            )),
        ]);
        let block = CleanupBranches::with_gateways(registry, shell);

        let result = block.execute(&validation_ok("my-project")).await.unwrap();

        assert!(result.events.is_empty(), "cleanup should emit no downstream events");
    }

    // -- Graceful failure handling --

    #[tokio::test]
    async fn branch_list_failure_is_graceful() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry("my-project", dir.path().to_str().unwrap());

        let shell = FakeShellGateway::sequence(vec![
            // git branch --merged fails
            CommandResult {
                stdout: String::new(),
                stderr: "not a git repository".to_string(),
                exit_code: 128,
                success: false,
            },
            // git worktree prune
            ok(""),
            // git worktree list --porcelain
            ok(&format!(
                "worktree {}\nHEAD abc\nbranch refs/heads/main\n\n",
                dir.path().display()
            )),
        ]);
        let block = CleanupBranches::with_gateways(registry, shell);

        let result = block.execute(&validation_ok("my-project")).await.unwrap();

        assert!(result.success, "should succeed even when git commands fail");
        assert!(result.summary.contains("0 merged branch(es)"));
    }
}
