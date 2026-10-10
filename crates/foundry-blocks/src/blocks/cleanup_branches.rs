use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::Result;
use foundry_sdk::campaign::CampaignStore;
use foundry_sdk::work_item::{WorkItemState, WorkItemStore, ledger_write_gate};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::ProjectValidationCompletedPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;

/// Housekeeping after successful validation, without disposing of live or unpreserved work.
///
/// Deletes merged branches with `git branch -d`, except live-item branches and
/// recorded preservation refs. Prunes metadata for missing worktree directories.
/// Removes an existing secondary worktree only inside the project's Foundry root,
/// with no live ledger owner or active campaign cycle, no uncommitted changes,
/// and no commits absent from every remote-tracking ref. Every retained worktree
/// is named with its reason in the summary and structured log.
/// State is reloaded under the daemon's ledger gate immediately before deletion;
/// locks are released before Git I/O. Git failures retain work and are reported.
pub struct CleanupBranches {
    registry: Arc<RwLock<Registry>>,
    shell: Arc<dyn ShellGateway>,
    paths: CleanupPaths,
}

#[derive(Clone)]
struct CleanupPaths {
    ledger: PathBuf,
    campaigns: PathBuf,
    worktrees: PathBuf,
}

impl CleanupBranches {
    pub fn new(registry: Arc<RwLock<Registry>>) -> Self {
        Self::with_shell(registry, Arc::new(crate::gateway::ProcessShellGateway))
    }

    fn with_shell(registry: Arc<RwLock<Registry>>, shell: Arc<dyn ShellGateway>) -> Self {
        Self {
            registry,
            shell,
            paths: CleanupPaths {
                ledger: foundry_sdk::paths::work_items_path(),
                campaigns: foundry_sdk::paths::campaigns_path(),
                worktrees: foundry_sdk::paths::worktrees_dir(),
            },
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_gateways(registry: Arc<RwLock<Registry>>, shell: Arc<dyn ShellGateway>) -> Self {
        Self::with_shell(registry, shell)
    }
}

fn live(state: WorkItemState) -> bool {
    matches!(state, WorkItemState::Submitted | WorkItemState::Queued | WorkItemState::Running)
}

fn same_ref(reference: &str, branch: &str) -> bool {
    reference.strip_prefix("refs/heads/").unwrap_or(reference) == branch
        || reference.strip_prefix("refs/remotes/origin/") == Some(branch)
        || reference.strip_prefix("origin/") == Some(branch)
}

fn same_worktree(left: &Path, right: &Path) -> bool {
    left == right
        || match (left.canonicalize(), right.canonicalize()) {
            (Ok(left), Ok(right)) => left == right,
            _ => false,
        }
}

fn protection(
    store: &WorkItemStore,
    project: &str,
    worktree: Option<&Path>,
    branch: Option<&str>,
) -> Option<String> {
    for item in &store.items {
        let disposition = item.disposition.as_ref();
        if live(item.state) {
            // A dispatch is durable before its workspace is recorded. Unknown
            // workspace evidence cannot prove that this candidate is unowned.
            let owns = disposition.map_or(item.project == project, |d| {
                worktree.is_some_and(|path| {
                    d.worktree
                        .as_ref()
                        .map_or(item.project == project, |w| same_worktree(Path::new(w), path))
                }) || branch.is_some_and(|b| {
                    d.task_branch.as_ref().map_or(item.project == project, |r| same_ref(r, b))
                })
            });
            if owns {
                return Some(format!("owned by live work item {}", item.id));
            }
        }
        if let Some(b) = branch
            && disposition
                .and_then(|d| d.preservation_ref.as_deref())
                .is_some_and(|r| same_ref(r, b))
        {
            return Some(format!("recorded preservation ref for work item {}", item.id));
        }
    }
    None
}

/// Read durable ownership without carrying either store lock into Git I/O.
fn ownership_reason(
    project: &str,
    worktree: Option<&Path>,
    branch: Option<&str>,
    paths: &CleanupPaths,
) -> Result<Option<String>> {
    let campaigns = CampaignStore::lock_exclusive(&paths.campaigns)?;
    let _gate = ledger_write_gate().lock().map_err(|e| anyhow::anyhow!("ledger gate: {e}"))?;
    let store = WorkItemStore::load(&paths.ledger)?;
    if let Some(reason) = protection(&store, project, worktree, branch) {
        return Ok(Some(reason));
    }
    for campaign in &campaigns.store.campaigns {
        let workspace_id =
            worktree.and_then(|wt| wt.file_name()).and_then(|n| n.to_str()).or_else(|| {
                branch.and_then(|b| {
                    b.strip_prefix(&format!("foundry-task/{}-", crate::workspace::slug(project)))
                })
            });
        if campaign.project == project
            && !campaign.status.is_terminal()
            && campaign.objective_history.iter().any(|cycle| cycle.outcome.is_none())
            && workspace_id
                .is_some_and(|id| crate::workspace::is_campaign_workspace(&campaign.name, id))
        {
            return Ok(Some(format!("owned by active campaign cycle {}", campaign.name)));
        }
    }
    Ok(None)
}

/// Shared guard for non-owner branch and worktree removal.
async fn guarded_delete(
    project: String,
    repo: PathBuf,
    worktree: Option<PathBuf>,
    branch: Option<String>,
    shell: Arc<dyn ShellGateway>,
    paths: CleanupPaths,
) -> Result<Option<String>> {
    if let Some(reason) =
        ownership_reason(&project, worktree.as_deref(), branch.as_deref(), &paths)?
    {
        return Ok(Some(reason));
    }
    if let Some(wt) = &worktree {
        let root = paths.worktrees.join(crate::workspace::slug(&project)).canonicalize()?;
        let canonical = wt.canonicalize()?;
        if canonical == root || !canonical.starts_with(&root) {
            return Ok(Some("not Foundry-owned".into()));
        }
        let status = crate::workspace::checked(
            shell.as_ref(),
            wt,
            &[
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--ignored",
            ],
        )
        .await?;
        let unpushed = crate::workspace::checked(
            shell.as_ref(),
            wt,
            &["rev-list", "HEAD", "--not", "--remotes"],
        )
        .await?;
        if !status.is_empty() || !unpushed.is_empty() {
            return Ok(Some("holds unpreserved work".into()));
        }
    }
    // RecordWorkItem admits a live item on ExecutionRequested, before ExecutePlan
    // creates the workspace on PlanCompleted; resume admission also precedes dispatch.
    // The path is recorded AFTER `git worktree add`, so missing workspace evidence
    // protects every candidate in that project. Creation refuses an existing path
    // (task_workspace::prepare_task_workspace_unchecked): an admission after this
    // check cannot adopt the existing candidate being removed. No lock spans Git I/O.
    if let Some(reason) =
        ownership_reason(&project, worktree.as_deref(), branch.as_deref(), &paths)?
    {
        return Ok(Some(reason));
    }
    if let Some(wt) = &worktree {
        let text = wt.to_string_lossy();
        crate::workspace::checked(shell.as_ref(), &repo, &["worktree", "remove", &text]).await?;
    } else if let Some(branch) = &branch {
        crate::workspace::checked(shell.as_ref(), &repo, &["branch", "-d", branch]).await?;
    }
    Ok(None)
}

/// Delete local branches that are fully merged into `target_branch`.
async fn cleanup_merged_branches(
    project: &str,
    path: &Path,
    target_branch: &str,
    shell: Arc<dyn ShellGateway>,
    paths: &CleanupPaths,
) -> Vec<String> {
    let mut deleted = Vec::new();

    // Best-effort: branch housekeeping must not fail an already validated workflow;
    // Git failures are logged and leave the branch intact.
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
        if branch.is_empty()
            || branch.starts_with('*')
            || branch.starts_with('+')
            || branch == target_branch
        {
            continue;
        }

        match guarded_delete(
            project.into(),
            path.into(),
            None,
            Some(branch.into()),
            Arc::clone(&shell),
            paths.clone(),
        )
        .await
        {
            Ok(None) => deleted.push(branch.to_string()),
            Ok(Some(reason)) => tracing::info!(%project, %branch, %reason, "keeping merged branch"),
            Err(error) => {
                tracing::warn!(%project, %branch, %error, "branch delete failed; retained");
            }
        }
    }

    deleted
}

/// Consider secondary worktrees individually; listing never establishes ownership.
async fn remove_worktrees(
    project: &str,
    path: &Path,
    shell: Arc<dyn ShellGateway>,
    paths: &CleanupPaths,
    porcelain_output: &str,
) -> (usize, Vec<String>) {
    let mut removed = 0;
    let mut kept = Vec::new();
    for block in porcelain_output.split("\n\n").skip(1) {
        let Some(wt_path) = block
            .lines()
            .find_map(|l| l.strip_prefix("worktree "))
            .filter(|p| !p.is_empty())
        else {
            continue;
        };
        let wt = Path::new(wt_path);
        let root = paths.worktrees.join(crate::workspace::slug(project));
        let owned = match (wt.canonicalize(), root.canonicalize()) {
            (Ok(wt), Ok(root)) => wt != root && wt.starts_with(root),
            _ => false,
        };
        let reason = if owned {
            match guarded_delete(
                project.into(),
                path.into(),
                Some(wt.into()),
                None,
                Arc::clone(&shell),
                paths.clone(),
            )
            .await
            {
                Ok(reason) => reason,
                Err(error) => {
                    tracing::warn!(%project, worktree = %wt_path, %error, "worktree retained: safety check or removal failed");
                    Some(format!("safety check or removal failed: {error}"))
                }
            }
        } else {
            Some("not Foundry-owned".to_string())
        };
        if let Some(reason) = reason {
            tracing::info!(%project, worktree = %wt_path, %reason, "keeping worktree");
            kept.push(format!("{wt_path}: {reason}"));
        } else {
            removed += 1;
        }
    }
    (removed, kept)
}

/// Remove stale git worktrees.
///
/// Runs `git worktree prune` to clean up worktree metadata for directories
/// that no longer exist, then considers secondary worktrees under the Foundry project root. Only clean,
/// remotely preserved worktrees without live owners can be removed.
async fn cleanup_stale_worktrees(
    project: &str,
    path: &Path,
    shell: Arc<dyn ShellGateway>,
    paths: &CleanupPaths,
) -> (usize, Vec<String>) {
    // Best-effort: pruning only removes obsolete metadata; failure does not
    // prevent checking existing worktrees and is logged below.
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

    // List remaining worktrees; each candidate still needs a fresh safety check.
    let result =
        match shell.run(path, "git", &["worktree", "list", "--porcelain"], None, None).await {
            Ok(r) if r.success => r,
            Ok(r) => {
                tracing::warn!(%project, stderr = %r.stderr.trim(), "git worktree list failed");
                return (0, vec![format!("worktree listing failed: {}", r.stderr.trim())]);
            }
            Err(err) => {
                tracing::warn!(%project, error = %err, "git worktree list error");
                return (0, vec![format!("worktree listing failed: {err}")]);
            }
        };

    remove_worktrees(project, path, shell, paths, &result.stdout).await
}

fn accepts_cleanup(trigger: &Event) -> bool {
    trigger
        .parse_payload::<ProjectValidationCompletedPayload>()
        .is_ok_and(|p| p.status == "ok")
}

impl TaskBlock for CleanupBranches {
    task_block_meta! {
        name: "Cleanup Branches",
        kind: Observer,
        sinks_on: [ProjectValidationCompleted],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        accepts_cleanup(trigger)
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let project = trigger.project.clone();
        let shell = Arc::clone(&self.shell);
        let paths = self.paths.clone();

        // Extract registry data before the async boundary — the RwLock guard must not cross .await.
        let entry = match super::read_registry(&self.registry) {
            Ok(guard) => guard.find_project(&project).cloned(),
            Err(e) => return Box::pin(async move { Err(e) }),
        };

        Box::pin(async move {
            // accepts() already filtered non-ok validation events.
            let Some(entry) = entry else {
                return Ok(TaskBlockResult::success(
                    format!("Skipped: {project} not in registry"),
                    vec![],
                ));
            };

            let path = Path::new(&entry.path);
            let target_branch = &entry.branch;

            let deleted =
                cleanup_merged_branches(&project, path, target_branch, Arc::clone(&shell), &paths)
                    .await;
            let (worktrees_removed, kept) =
                cleanup_stale_worktrees(&project, path, shell, &paths).await;

            let mut summary = format!(
                "{project}: cleaned up {} merged branch(es), {} stale worktree(s)",
                deleted.len(),
                worktrees_removed,
            );
            if !kept.is_empty() {
                summary.push_str("; kept worktrees: ");
                summary.push_str(&kept.join("; "));
            }
            tracing::info!(%project, branches = deleted.len(), worktrees = worktrees_removed, "cleanup complete");

            Ok(TaskBlockResult::success(summary, vec![]))
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::CleanupBranches;

    fn make_registry(name: &str, path: &str) -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
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
                installs_skill: None,
                timeout_secs: None,
                audit_exceptions: Vec::new(),
                update_policy: None,
            }],
        }))
    }

    fn validation_ok(project: &str) -> Event {
        Event::new(
            EventType::ProjectValidationCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({"project": project, "status": "ok", "has_gates": true}),
        )
    }

    fn validation_error(project: &str) -> Event {
        Event::new(
            EventType::ProjectValidationCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({"project": project, "status": "error", "has_gates": false}),
        )
    }

    fn validation_skipped(project: &str) -> Event {
        Event::new(
            EventType::ProjectValidationCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({"project": project, "status": "skipped", "has_gates": false}),
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

    assert_block_meta!(
        CleanupBranches::new(Arc::new(RwLock::new(Registry { version: 2, projects: vec![] }))),
        kind: Observer,
        sinks_on: [ProjectValidationCompleted],
    );

    // -- Self-filter tests (accepts()) --

    #[test]
    fn accepts_returns_false_when_validation_not_ok() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success();
        let block = CleanupBranches::with_gateways(registry, shell);

        assert!(
            !block.accepts(&validation_error("my-project")),
            "should not accept error status"
        );
    }

    #[test]
    fn accepts_returns_false_when_validation_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success();
        let block = CleanupBranches::with_gateways(registry, shell);

        assert!(
            !block.accepts(&validation_skipped("my-project")),
            "should not accept skipped status"
        );
    }

    #[test]
    fn accepts_returns_true_for_ok_status() {
        let dir = tempfile::tempdir().unwrap();
        let registry = make_registry("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success();
        let block = CleanupBranches::with_gateways(registry, shell);

        assert!(block.accepts(&validation_ok("my-project")), "should accept ok status");
    }

    #[tokio::test]
    async fn skips_when_project_not_in_registry() {
        let registry = Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        }));
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
    async fn keeps_non_foundry_worktrees() {
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
        ]);
        let block = CleanupBranches::with_gateways(registry, shell);

        let result = block.execute(&validation_ok("my-project")).await.unwrap();

        assert!(result.success);
        assert!(result.summary.contains("not Foundry-owned"));
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
    // Each fixture runs in a child test process: path overrides never mutate
    // the environment of another concurrently running Rust test.
    fn isolated(name: &str) -> bool {
        if std::env::var_os("CLEANUP_FIXTURE").is_some() {
            return false;
        }
        let root = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        if name.starts_with("aliased_") {
            let actual = root.path().join("actual-worktrees");
            std::fs::create_dir_all(&actual).unwrap();
            std::os::unix::fs::symlink(actual, root.path().join("worktrees")).unwrap();
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("blocks::cleanup_branches::tests::{name}"),
                "--nocapture",
            ])
            .env("CLEANUP_FIXTURE", "1")
            .env("FOUNDRY_WORKTREES_DIR", root.path().join("worktrees"))
            .env("FOUNDRY_WORK_ITEMS_PATH", root.path().join("ledger.json"))
            .env("FOUNDRY_CAMPAIGNS_PATH", root.path().join("campaigns.json"))
            .status()
            .unwrap();
        assert!(status.success(), "fixture {name} failed");
        true
    }

    fn owner(worktree: &std::path::Path) {
        use foundry_sdk::work_item::{
            WorkItem, WorkItemKind, WorkItemSpec, WorkItemStore, WorkLane,
        };
        let mut item = WorkItem::dispatched(
            WorkItemSpec {
                project: "my-project".into(),
                objective: "reviewed task".into(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "test".into(),
                trace_id: None,
            },
            chrono::Utc::now(),
        );
        item.id = "wi_live".into();
        // Production dispatches can have no disposition until settlement.
        assert!(item.disposition.is_none());
        let _gate = foundry_sdk::work_item::ledger_write_gate().lock().unwrap();
        let mut store = WorkItemStore::load(&foundry_sdk::paths::work_items_path()).unwrap();
        store.items.push(item);
        store.save(&foundry_sdk::paths::work_items_path()).unwrap();
        assert!(worktree.exists());
    }

    struct AdmitAfterList {
        worktree: std::path::PathBuf,
    }
    impl crate::gateway::ShellGateway for AdmitAfterList {
        fn run<'a>(
            &'a self,
            cwd: &'a std::path::Path,
            command: &'a str,
            args: &'a [&'a str],
            env: Option<&'a [(String, String)]>,
            timeout: Option<std::time::Duration>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<CommandResult>> + Send + 'a>,
        > {
            Box::pin(async move {
                use crate::blocks::test_helpers::git_repo::CleanProcessShellGateway;
                let result = CleanProcessShellGateway.run(cwd, command, args, env, timeout).await?;
                if args == ["worktree", "list", "--porcelain"] {
                    owner(&self.worktree);
                }
                Ok(result)
            })
        }
    }

    /// Git I/O yields while a concurrent admission tries the synchronous gate.
    struct AdmitDuringGit {
        worktree: std::path::PathBuf,
    }
    impl crate::gateway::ShellGateway for AdmitDuringGit {
        fn run<'a>(
            &'a self,
            cwd: &'a std::path::Path,
            command: &'a str,
            args: &'a [&'a str],
            env: Option<&'a [(String, String)]>,
            timeout: Option<std::time::Duration>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<CommandResult>> + Send + 'a>,
        > {
            Box::pin(async move {
                use crate::blocks::test_helpers::git_repo::CleanProcessShellGateway;
                // The removal fallback exercises the unsafe base implementation,
                // which never checks status. try_lock avoids stranding a worker
                // if a regression holds the gate while awaiting this command.
                if args.first() == Some(&"status") || args.starts_with(&["worktree", "remove"]) {
                    let wt = self.worktree.clone();
                    tokio::time::timeout(
                        std::time::Duration::from_secs(2),
                        tokio::spawn(async move {
                            tokio::task::yield_now().await;
                            {
                                let _gate = foundry_sdk::work_item::ledger_write_gate()
                                    .try_lock()
                                    .map_err(|e| {
                                        anyhow::anyhow!("Git await holds ledger gate: {e}")
                                    })?;
                            }
                            owner(&wt);
                            anyhow::Ok(())
                        }),
                    )
                    .await???;
                }
                CleanProcessShellGateway.run(cwd, command, args, env, timeout).await
            })
        }
    }

    fn record_fixture_state(scenario: &str, wt: &std::path::Path) {
        use crate::blocks::test_helpers::git_repo::git;
        if scenario == "running" {
            owner(wt);
        }
        if scenario == "referenced" {
            owner(wt);
            let mut store =
                foundry_sdk::work_item::WorkItemStore::load(&foundry_sdk::paths::work_items_path())
                    .unwrap();
            store.items[0].project = "other-registration".into();
            store.items[0].disposition =
                Some(serde_json::from_value(serde_json::json!({ "worktree": wt })).unwrap());
            store.save(&foundry_sdk::paths::work_items_path()).unwrap();
        }
        if scenario == "campaign" {
            let store: foundry_sdk::campaign::CampaignStore = serde_json::from_value(serde_json::json!({
                "campaigns": [{ "name": "mission", "project": "my-project", "mission": "review",
                    "status": "active", "objective_history": [{ "cycle": 1, "objective": "task" }] }]
            })).unwrap();
            store.save(&foundry_sdk::paths::campaigns_path()).unwrap();
        }
        if scenario == "preservation" {
            owner(wt);
            let mut store =
                foundry_sdk::work_item::WorkItemStore::load(&foundry_sdk::paths::work_items_path())
                    .unwrap();
            store.items[0].state = foundry_sdk::work_item::WorkItemState::Preserved;
            store.items[0].disposition = Some(
                serde_json::from_value(
                    serde_json::json!({ "preservation_ref": "refs/heads/candidate" }),
                )
                .unwrap(),
            );
            store.save(&foundry_sdk::paths::work_items_path()).unwrap();
            git(wt, &["checkout", "--detach"]);
        }
    }

    async fn regression(name: &str, scenario: &str) {
        use crate::blocks::test_helpers::git_repo::{CleanProcessShellGateway, commit, git, repo};
        if isolated(name) {
            return;
        }
        let repo = repo();
        let root = foundry_sdk::paths::worktrees_dir().join("my-project");
        std::fs::create_dir_all(&root).unwrap();
        let wt = if scenario == "manual" {
            repo.work.parent().unwrap().join("manual")
        } else {
            root.join(if scenario == "campaign" {
                "mission-c1-123abc"
            } else {
                "orphan"
            })
        };
        git(
            &repo.work,
            &[
                "worktree",
                "add",
                "-b",
                "candidate",
                wt.to_str().unwrap(),
                "main",
            ],
        );
        record_fixture_state(scenario, &wt);
        if scenario == "dirty" {
            std::fs::write(wt.join("untracked"), "reviewed work").unwrap();
        }
        if scenario == "unpushed" {
            commit(&wt, "README.md", "unpreserved", "work");
        }
        let shell: Arc<dyn crate::gateway::ShellGateway> = if scenario == "race" {
            Arc::new(AdmitAfterList {
                worktree: wt.clone(),
            })
        } else if scenario == "lock" {
            Arc::new(AdmitDuringGit {
                worktree: wt.clone(),
            })
        } else {
            Arc::new(CleanProcessShellGateway)
        };
        let block = CleanupBranches::with_gateways(
            make_registry("my-project", repo.work.to_str().unwrap()),
            shell,
        );
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            block.execute(&validation_ok("my-project")),
        )
        .await
        .expect("cleanup and admission must finish")
        .unwrap();
        assert!(result.success);
        if scenario == "preservation" {
            assert_eq!(
                git(&repo.work, &["rev-parse", "candidate"]),
                git(&repo.work, &["rev-parse", "main"]),
                "recorded preservation branch was deleted"
            );
        } else if scenario == "orphan" {
            assert!(
                !wt.exists(),
                "clean remotely preserved orphan should be removed: {}",
                result.summary
            );
            assert!(result.summary.contains("1 stale worktree(s)"));
        } else {
            assert!(wt.exists(), "validation destroyed {scenario} workspace: {}", result.summary);
            let reason = match scenario {
                "manual" => "not Foundry-owned",
                "campaign" => "owned by active campaign cycle mission",
                "dirty" | "unpushed" => "holds unpreserved work",
                _ => "owned by live work item wi_live",
            };
            assert!(result.summary.contains(reason), "{}", result.summary);
            assert!(result.summary.contains(wt.canonicalize().unwrap().to_str().unwrap()));
            if scenario == "running" || scenario == "race" {
                // Review/finalization can still spawn Git after validation.
                assert!(!git(&wt, &["rev-parse", "HEAD"]).is_empty());
            }
        }
    }

    #[tokio::test]
    async fn running_task_survives_validation_and_review() {
        regression("running_task_survives_validation_and_review", "running").await;
    }
    #[tokio::test]
    async fn handmade_worktree_survives_validation() {
        regression("handmade_worktree_survives_validation", "manual").await;
    }
    #[tokio::test]
    async fn clean_foundry_orphan_is_removed() {
        regression("clean_foundry_orphan_is_removed", "orphan").await;
    }
    #[tokio::test]
    async fn dirty_orphan_is_kept_and_reported() {
        regression("dirty_orphan_is_kept_and_reported", "dirty").await;
    }
    #[tokio::test]
    async fn unpushed_orphan_is_kept_and_reported() {
        regression("unpushed_orphan_is_kept_and_reported", "unpushed").await;
    }
    #[tokio::test]
    async fn task_admitted_after_listing_keeps_worktree() {
        regression("task_admitted_after_listing_keeps_worktree", "race").await;
    }
    #[tokio::test]
    async fn concurrent_admission_during_git_keeps_worktree_without_locking_runtime() {
        regression(
            "concurrent_admission_during_git_keeps_worktree_without_locking_runtime",
            "lock",
        )
        .await;
    }
    #[tokio::test]
    async fn active_campaign_cycle_survives_validation() {
        regression("active_campaign_cycle_survives_validation", "campaign").await;
    }
    #[tokio::test]
    async fn recorded_preservation_branch_survives_validation() {
        regression("recorded_preservation_branch_survives_validation", "preservation").await;
    }
    #[tokio::test]
    async fn exact_live_worktree_reference_is_protected_across_registrations() {
        regression("exact_live_worktree_reference_is_protected_across_registrations", "referenced")
            .await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn aliased_clean_foundry_orphan_is_removed() {
        regression("aliased_clean_foundry_orphan_is_removed", "orphan").await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn aliased_live_worktree_reference_is_protected_across_registrations() {
        regression(
            "aliased_live_worktree_reference_is_protected_across_registrations",
            "referenced",
        )
        .await;
    }
}
