use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use foundry_sdk::payload::WorkspaceBase;
use foundry_sdk::registry::ProjectEntry;

use crate::gateway::ShellGateway;
use crate::workspace::task_workspace_paths;

pub(crate) struct TaskWorkspace {
    pub path: PathBuf,
    pub branch: String,
    /// Where the workspace started and the trunk observed while preparing it.
    pub base: WorkspaceBase,
}

/// A continuation refused before any agent session: the preserved work it was
/// to continue is behind trunk, and replaying it onto trunk conflicts.
///
/// An agent cannot repair this. It may not rewrite history, so finalize would
/// replay the same preserved commits onto trunk and hit the same conflict after
/// the whole session had been spent. Preparation refuses instead and names the
/// files, so whoever dispatched the continuation can choose what to do with the
/// preserved work, which stays untouched at its ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContinuationConflict {
    /// The continuation ref as dispatched: a branch, a commit, or `bundle:<path>`.
    pub base_ref: String,
    /// The preserved commit that ref resolved to.
    pub preserved: String,
    /// The trunk commit observed while preparing.
    pub trunk: String,
    /// Trunk commits the preserved work's base lacked.
    pub behind: u64,
    /// Paths left conflicted where the rebase stopped (the first preserved
    /// commit that conflicted), in Git's order.
    pub files: Vec<String>,
}

impl std::fmt::Display for ContinuationConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "continuation refused before any agent session: the preserved work at {} ({}) is \
             {} commit(s) behind trunk ({}) and replaying it onto trunk conflicts in: {}. \
             The preserved ref is untouched; carry the work forward by reapplying it onto trunk",
            self.base_ref,
            self.preserved,
            self.behind,
            self.trunk,
            self.files.join(", ")
        )
    }
}

impl std::error::Error for ContinuationConflict {}

async fn checked(
    shell: &dyn ShellGateway,
    cwd: &Path,
    command: &str,
    args: &[&str],
) -> Result<String> {
    let result = shell.run(cwd, command, args, None, None).await?;
    if !result.success {
        bail!(
            "{} {} failed ({}): {}",
            command,
            args.join(" "),
            result.exit_code,
            result.stderr.trim()
        );
    }
    Ok(result.stdout.trim().to_string())
}

async fn ref_exists_locally(shell: &dyn ShellGateway, cwd: &Path, reference: &str) -> Result<bool> {
    let result = shell
        .run(cwd, "git", &["rev-parse", "--verify", "--quiet", reference], None, None)
        .await?;
    Ok(result.success)
}

/// The exact commit `reference` names, so a later fetch that rewrites
/// `FETCH_HEAD` cannot change what preparation acts on.
async fn commit_of(shell: &dyn ShellGateway, cwd: &Path, reference: &str) -> Result<String> {
    checked(
        shell,
        cwd,
        "git",
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )
    .await
}

/// Whether `ancestor` is reachable from `descendant`.
async fn is_ancestor(
    shell: &dyn ShellGateway,
    cwd: &Path,
    ancestor: &str,
    descendant: &str,
) -> Result<bool> {
    let result = shell
        .run(cwd, "git", &["merge-base", "--is-ancestor", ancestor, descendant], None, None)
        .await?;
    match result.exit_code {
        0 => Ok(true),
        1 => Ok(false),
        code => bail!("git merge-base --is-ancestor failed ({code}): {}", result.stderr.trim()),
    }
}

/// The branch ref a preservation bundle carries.
///
/// Resume reads the ref out of the artifact rather than relying on the bundle's
/// `HEAD`, so it does not depend on how the bundle was written. Bundles created
/// before `preserve` recorded `HEAD` hold only `refs/heads/<task branch>`, and a
/// bare `git fetch <bundle>` against one dies with "couldn't find remote ref
/// HEAD" — reading the ref keeps the work already sitting in `~/.foundry/preserved`
/// recoverable.
async fn bundle_branch_ref(shell: &dyn ShellGateway, repo: &Path, bundle: &str) -> Result<String> {
    let heads = checked(shell, repo, "git", &["bundle", "list-heads", bundle]).await?;
    heads
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .find(|name| name.starts_with("refs/heads/"))
        .map(str::to_string)
        .with_context(|| format!("preservation bundle carries no branch ref: {bundle}"))
}

/// The commit a continuation ref (local branch or commit, remote ref, or
/// `bundle:<path>`) resolves to, fetching it into the object store when it is
/// not local.
async fn continuation_commit(
    shell: &dyn ShellGateway,
    repo: &Path,
    base_ref: &str,
) -> Result<String> {
    if let Some(bundle) = base_ref.strip_prefix("bundle:") {
        let reference = bundle_branch_ref(shell, repo, bundle).await?;
        checked(shell, repo, "git", &["fetch", bundle, &reference]).await?;
        commit_of(shell, repo, "FETCH_HEAD").await
    } else if ref_exists_locally(shell, repo, base_ref).await? {
        commit_of(shell, repo, base_ref).await
    } else {
        checked(shell, repo, "git", &["fetch", "origin", base_ref]).await?;
        commit_of(shell, repo, "FETCH_HEAD").await
    }
}

/// The registered trunk a continuation is measured against: freshly fetched
/// `origin/<branch>`, or the local branch when origin cannot be reached.
///
/// The fetch takes no tags: measuring trunk needs only the branch, and a tag
/// configuration that would clobber a local tag must not fail it into the
/// fallback. The fallback keeps a bundle continuation working when origin is
/// down, which is when bundles get written. The local branch is what finalize
/// lands onto, so it is a truthful trunk for that run.
async fn continuation_trunk(shell: &dyn ShellGateway, repo: &Path, branch: &str) -> Result<String> {
    match checked(shell, repo, "git", &["fetch", "--no-tags", "origin", branch]).await {
        Ok(_) => commit_of(shell, repo, "FETCH_HEAD").await,
        Err(error) => {
            tracing::warn!(
                %error,
                %branch,
                "could not fetch trunk for a continuation; measuring it against the local branch"
            );
            commit_of(shell, repo, &format!("refs/heads/{branch}")).await
        }
    }
}

/// Create a fresh branch and worktree from the current remote base (or a
/// preserved continuation ref) without touching the registered checkout.
///
/// A continuation starts from current trunk plus its preserved work: when the
/// preserved commit does not already contain trunk, it is rebased onto trunk
/// in the new workspace before any agent session. The preservation ref itself
/// is never rewritten; only the new workspace branch moves. A rebase that
/// conflicts is refused with a typed [`ContinuationConflict`] (reachable
/// through `anyhow::Error::downcast_ref`) and leaves nothing behind.
///
/// `workspace_id` names the workspace. Callers derive it from
/// [`crate::workspace`] — campaign cycles use an id that encodes the campaign
/// and cycle, so a cancellation can find an orphaned worktree without having
/// observed the run that created it; everything else uses the run id.
pub(crate) async fn prepare_task_workspace(
    shell: &dyn ShellGateway,
    entry: &ProjectEntry,
    workspace_id: &str,
    base_ref: Option<&str>,
) -> Result<TaskWorkspace> {
    let (path, _) = task_workspace_paths(&entry.name, workspace_id);
    ensure_disk_room(entry, &path, foundry_sdk::disk::DiskThreshold::from_env())?;
    prepare_task_workspace_unchecked(shell, entry, workspace_id, base_ref).await
}

/// Refuse to create a worktree on a filesystem that is low on space: a task
/// that runs out part-way leaves a half-written worktree and fails at
/// finalize ("Out of diskspace").
fn ensure_disk_room(
    entry: &ProjectEntry,
    worktree: &Path,
    threshold: foundry_sdk::disk::DiskThreshold,
) -> Result<()> {
    foundry_sdk::disk::ensure_room(
        &[
            PathBuf::from(&entry.path),
            worktree.to_path_buf(),
            foundry_sdk::paths::foundry_home(),
        ],
        threshold,
    )
    .map_err(|reason| anyhow::anyhow!(reason))
}

async fn prepare_task_workspace_unchecked(
    shell: &dyn ShellGateway,
    entry: &ProjectEntry,
    workspace_id: &str,
    base_ref: Option<&str>,
) -> Result<TaskWorkspace> {
    let repo = Path::new(&entry.path);
    let (path, branch) = task_workspace_paths(&entry.name, workspace_id);

    if path.exists() {
        bail!("task worktree already exists: {}", path.display());
    }
    std::fs::create_dir_all(path.parent().context("worktree path has no parent")?)
        .with_context(|| format!("create task worktree parent for {}", path.display()))?;

    let (source, trunk) = if let Some(reference) = base_ref {
        let preserved = continuation_commit(shell, repo, reference).await?;
        let trunk = continuation_trunk(shell, repo, &entry.branch).await?;
        (preserved, trunk)
    } else {
        checked(shell, repo, "git", &["fetch", "origin", &entry.branch]).await?;
        let trunk = commit_of(shell, repo, "FETCH_HEAD").await?;
        (trunk.clone(), trunk)
    };

    let path_text = path.to_string_lossy().to_string();
    checked(shell, repo, "git", &["worktree", "add", "-b", &branch, &path_text, &source]).await?;

    let actual = checked(shell, &path, "git", &["rev-parse", "--show-toplevel"]).await?;
    let expected = path
        .canonicalize()
        .with_context(|| format!("canonicalize task worktree {}", path.display()))?;
    let actual = PathBuf::from(actual)
        .canonicalize()
        .context("canonicalize git-reported task worktree")?;
    if actual != expected {
        bail!(
            "task worktree confinement check failed: expected {}, git reported {}",
            expected.display(),
            actual.display()
        );
    }

    if let Some(reference) = base_ref
        && !is_ancestor(shell, repo, &trunk, &source).await?
        && let Err(error) = rebase_onto_trunk(shell, &path, reference, &source, &trunk).await
    {
        discard_refused_workspace(shell, repo, &path_text, &branch).await;
        return Err(error);
    }
    let commit = commit_of(shell, &path, "HEAD").await?;

    Ok(TaskWorkspace {
        path,
        branch,
        base: WorkspaceBase { commit, trunk },
    })
}

/// Replay the preserved work onto `trunk` inside the new workspace.
///
/// A conflict is aborted and returned as a [`ContinuationConflict`]; any other
/// rebase failure is an ordinary error.
async fn rebase_onto_trunk(
    shell: &dyn ShellGateway,
    worktree: &Path,
    base_ref: &str,
    preserved: &str,
    trunk: &str,
) -> Result<()> {
    let behind = checked(
        shell,
        worktree,
        "git",
        &["rev-list", "--count", &format!("{preserved}..{trunk}")],
    )
    .await?
    .parse::<u64>()
    .context("parse the count of trunk commits a continuation lacks")?;
    let rebase = shell.run(worktree, "git", &["rebase", trunk], None, None).await?;
    if rebase.success {
        return Ok(());
    }
    let files = checked(shell, worktree, "git", &["diff", "--name-only", "--diff-filter=U"])
        .await
        .unwrap_or_default();
    // Best-effort: the workspace is discarded next whether or not the abort
    // succeeds; the abort only keeps that removal from tripping on a rebase
    // in progress.
    crate::workspace::run_best_effort(shell, worktree, &["rebase", "--abort"]).await;
    let files: Vec<String> = files.lines().map(str::to_string).collect();
    if files.is_empty() {
        bail!(
            "rebasing the continuation {base_ref} ({preserved}) onto trunk ({trunk}) failed: {}",
            rebase.stderr.trim()
        );
    }
    Err(ContinuationConflict {
        base_ref: base_ref.to_string(),
        preserved: preserved.to_string(),
        trunk: trunk.to_string(),
        behind,
        files,
    }
    .into())
}

/// Remove a workspace preparation created and then refused. It holds no agent
/// work: only the preserved commits, which stay at their own ref.
async fn discard_refused_workspace(
    shell: &dyn ShellGateway,
    repo: &Path,
    worktree: &str,
    branch: &str,
) {
    // Owner removal: preparation discards the workspace it just created and
    // refused, before any agent session wrote to it.
    crate::workspace::run_best_effort(shell, repo, &["worktree", "remove", "--force", worktree])
        .await;
    crate::workspace::run_best_effort(shell, repo, &["branch", "-D", branch]).await;
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::pin::Pin;
    use std::process::Command;
    use std::time::Duration;

    use anyhow::Result;
    use foundry_sdk::gateway::{CommandResult, ShellGateway};
    use foundry_sdk::registry::{ActionFlags, ProjectEntry, Stack};

    use super::prepare_task_workspace;
    use crate::blocks::finalize_task::FinalizeTask;
    use crate::gateway::fakes::FakeShellGateway;
    use foundry_sdk::event::Event;
    use foundry_sdk::payload::{LoopContext, TaskReviewedPayload, TaskVerdict};
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    fn clean_git_env(command: &mut Command) {
        command.env("GIT_CONFIG_GLOBAL", "/dev/null");
        command.env_remove("GIT_CONFIG_COUNT");
        command.env_remove("GIT_CONFIG_PARAMETERS");
        for index in 0..8 {
            command.env_remove(format!("GIT_CONFIG_KEY_{index}"));
            command.env_remove(format!("GIT_CONFIG_VALUE_{index}"));
        }
    }

    struct CleanProcessShellGateway;

    impl ShellGateway for CleanProcessShellGateway {
        fn run<'a>(
            &'a self,
            working_dir: &'a Path,
            command: &'a str,
            args: &'a [&'a str],
            env: Option<&'a [(String, String)]>,
            _timeout: Option<Duration>,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<CommandResult>> + Send + 'a>> {
            Box::pin(async move {
                let mut child = Command::new(command);
                child.current_dir(working_dir).args(args);
                clean_git_env(&mut child);
                if let Some(env) = env {
                    child.envs(env.iter().map(|(k, v)| (k, v)));
                }
                let output = child.output()?;
                Ok(CommandResult {
                    stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                    exit_code: output.status.code().unwrap_or(1),
                    success: output.status.success(),
                })
            })
        }
    }

    fn git(cwd: &Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        command.args(args).current_dir(cwd);
        clean_git_env(&mut command);
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn test_entry(name: &str, path: &Path) -> ProjectEntry {
        ProjectEntry {
            name: name.to_string(),
            path: path.display().to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: ActionFlags::default(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: vec![],
            update_policy: None,
        }
    }

    fn task_trigger(project: &str, worktree: &Path, branch: &str, verdict: TaskVerdict) -> Event {
        Event::new(
            foundry_sdk::event::EventType::TaskReviewed,
            project.to_string(),
            Throttle::Full,
            Event::serialize_payload(&TaskReviewedPayload {
                project: project.to_string(),
                objective: "ship change".to_string(),
                review: "ok".to_string(),
                gate_results: vec![],
                verdict,
                context: LoopContext {
                    task_worktree: Some(worktree.display().to_string()),
                    task_branch: Some(branch.to_string()),
                    ..LoopContext::default()
                },
            })
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn prepare_task_workspace_uses_landed_commit_without_remote_branch() {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote.git");
        let remote_url = format!("file://{}", remote.display());
        let checkout = dir.path().join("checkout");
        let worktree = dir.path().join("worktree");

        git(dir.path(), &["init", "--bare", remote.to_str().unwrap()]);
        git(dir.path(), &["init", "-b", "main", checkout.to_str().unwrap()]);
        git(&checkout, &["config", "user.email", "foundry-test@example.com"]);
        git(&checkout, &["config", "user.name", "Foundry Test"]);
        std::fs::write(checkout.join("README.md"), "base\n").unwrap();
        git(&checkout, &["add", "README.md"]);
        git(&checkout, &["commit", "-m", "initial"]);
        git(&checkout, &["remote", "add", "origin", &remote_url]);
        let _ = Command::new("git")
            .current_dir(&checkout)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["config", "--unset-all", "remote.origin.pushurl"])
            .status();
        git(&checkout, &["remote", "set-url", "--push", "origin", &remote_url]);
        git(&checkout, &["push", "-u", "origin", "main"]);
        git(
            &checkout,
            &[
                "worktree",
                "add",
                "-b",
                "foundry-task/landed-cycle",
                worktree.to_str().unwrap(),
                "main",
            ],
        );
        std::fs::write(worktree.join("README.md"), "base\nlanded change\n").unwrap();
        git(&worktree, &["add", "README.md"]);
        git(&worktree, &["commit", "-m", "branch commit"]);
        let expected_commit = git(&worktree, &["rev-parse", "HEAD"]);

        let registry =
            super::super::test_helpers::registry_with_entry(test_entry("foundry", &checkout));
        let finalize =
            FinalizeTask::with_gateways(registry, std::sync::Arc::new(CleanProcessShellGateway));
        let finalized = finalize
            .execute(&task_trigger(
                "foundry",
                &worktree,
                "foundry-task/landed-cycle",
                TaskVerdict::Complete,
            ))
            .await
            .unwrap();
        let landed_ref =
            finalized.events[0].payload["preservation_ref"].as_str().unwrap().to_string();
        assert_eq!(landed_ref, expected_commit);
        // Model the ref being removed by the ledger's later settlement. This
        // test exercises continuation from a commit after the remote ref is gone.
        git(&checkout, &["push", "origin", "--delete", "foundry-task/landed-cycle"]);
        assert!(
            git(
                &checkout,
                &[
                    "ls-remote",
                    "--heads",
                    "origin",
                    "foundry-task/landed-cycle"
                ]
            )
            .is_empty(),
            "temporary landed branch should be gone from origin"
        );

        let next = prepare_task_workspace(
            &CleanProcessShellGateway,
            &test_entry("foundry", &checkout),
            "evt_123456789abc",
            Some(&landed_ref),
        )
        .await
        .unwrap();
        assert_eq!(git(&next.path, &["rev-parse", "HEAD"]), expected_commit);
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), expected_commit);
        // Test fixture owns the worktree it created.
        git(&checkout, &["worktree", "remove", next.path.to_str().unwrap()]);
        git(&checkout, &["branch", "-d", &next.branch]);
    }

    #[tokio::test]
    async fn prepare_task_workspace_fetches_non_landed_preservation_branch() {
        let shell = FakeShellGateway::sequence(vec![
            crate::shell::CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 1,
                success: false,
            },
            crate::shell::CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            crate::shell::CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            crate::shell::CommandResult {
                stdout: std::env::temp_dir().display().to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let dir = tempfile::tempdir().unwrap();
        let entry = test_entry("foundry", dir.path());
        let _ = prepare_task_workspace(
            &*shell,
            &entry,
            "evt_abcdef123456",
            Some("foundry-task/preserved"),
        )
        .await;
        let invocations = shell.invocations();
        assert_eq!(
            invocations[0].args,
            vec!["rev-parse", "--verify", "--quiet", "foundry-task/preserved"]
        );
        assert_eq!(invocations[1].args, vec!["fetch", "origin", "foundry-task/preserved"]);
    }

    /// Build a checkout whose `origin` does not exist on disk, so every push
    /// fails and `preserve` is forced down the bundle fallback, plus a task
    /// worktree carrying one commit worth preserving.
    fn repo_with_unpushable_task_branch(
        dir: &Path,
        branch: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf, String) {
        let checkout = dir.join("checkout");
        let worktree = dir.join("worktree");
        let absent_remote = format!("file://{}", dir.join("absent.git").display());
        git(dir, &["init", "-b", "main", checkout.to_str().unwrap()]);
        git(&checkout, &["config", "user.email", "foundry-test@example.com"]);
        git(&checkout, &["config", "user.name", "Foundry Test"]);
        std::fs::write(checkout.join("README.md"), "base\n").unwrap();
        git(&checkout, &["add", "README.md"]);
        git(&checkout, &["commit", "-m", "initial"]);
        git(&checkout, &["remote", "add", "origin", &absent_remote]);
        git(
            &checkout,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                worktree.to_str().unwrap(),
                "main",
            ],
        );
        std::fs::write(worktree.join("README.md"), "base\npreserved slice\n").unwrap();
        git(&worktree, &["add", "README.md"]);
        git(&worktree, &["commit", "-m", "preserved slice"]);
        let head = git(&worktree, &["rev-parse", "HEAD"]);
        (checkout, worktree, head)
    }

    /// The whole point of the bundle fallback is that the next cycle can carry
    /// the work forward. Bundles that recorded only the task branch advertised
    /// no HEAD, so the resume fetch died with "couldn't find remote ref HEAD"
    /// and escalated two client campaigns as `runner_error`.
    #[tokio::test]
    async fn work_preserved_to_a_bundle_resumes_into_the_next_task_workspace() {
        let project = "foundry-bundle-resume";
        let branch = "foundry-task/bundle-resume";
        let dir = tempfile::tempdir().unwrap();
        let (checkout, worktree, preserved_commit) =
            repo_with_unpushable_task_branch(dir.path(), branch);

        let registry =
            super::super::test_helpers::registry_with_entry(test_entry(project, &checkout));
        let finalize =
            FinalizeTask::with_gateways(registry, std::sync::Arc::new(CleanProcessShellGateway));
        let finalized = finalize
            .execute(&task_trigger(
                project,
                &worktree,
                branch,
                TaskVerdict::Remainder {
                    gaps: vec!["unfinished".to_string()],
                },
            ))
            .await
            .unwrap();

        let preservation_ref =
            finalized.events[0].payload["preservation_ref"].as_str().unwrap().to_string();
        let bundle =
            preservation_ref.strip_prefix("bundle:").expect("push failed, expected bundle");
        assert!(Path::new(bundle).exists(), "preservation bundle was not written");
        assert!(
            git(&checkout, &["bundle", "list-heads", bundle]).contains(" HEAD"),
            "bundle must advertise HEAD so it can also be cloned or fetched by hand"
        );

        let next = prepare_task_workspace(
            &CleanProcessShellGateway,
            &test_entry(project, &checkout),
            "evt_bundleresume",
            Some(&preservation_ref),
        )
        .await
        .unwrap();

        assert_eq!(git(&next.path, &["rev-parse", "HEAD"]), preserved_commit);
        assert!(
            std::fs::read_to_string(next.path.join("README.md"))
                .unwrap()
                .contains("preserved slice"),
            "preserved work did not reach the next cycle's worktree"
        );

        // Test fixture owns the worktree it created.
        git(&checkout, &["worktree", "remove", next.path.to_str().unwrap()]);
        git(&checkout, &["branch", "-D", &next.branch]);
        // Test fixture owns these preservation artifacts; this is not a worktree removal.
        let _ = std::fs::remove_dir_all(foundry_sdk::paths::preserved_dir().join(project));
        let _ = std::fs::remove_dir(next.path.parent().unwrap());
    }

    /// Bundles already sitting in `~/.foundry/preserved` hold real client work
    /// and predate the HEAD fix, so resume must not depend on their format.
    #[tokio::test]
    async fn legacy_bundle_without_head_still_resumes() {
        let branch = "foundry-task/legacy-bundle";
        let dir = tempfile::tempdir().unwrap();
        let (checkout, worktree, preserved_commit) =
            repo_with_unpushable_task_branch(dir.path(), branch);
        let bundle = dir.path().join("legacy.bundle");
        let bundle_text = bundle.display().to_string();
        git(&worktree, &["bundle", "create", &bundle_text, branch]);
        assert!(
            !git(&checkout, &["bundle", "list-heads", &bundle_text]).contains(" HEAD"),
            "fixture must reproduce the HEAD-less bundles already on disk"
        );

        let next = prepare_task_workspace(
            &CleanProcessShellGateway,
            &test_entry("foundry-legacy-bundle", &checkout),
            "evt_legacybundle",
            Some(&format!("bundle:{bundle_text}")),
        )
        .await
        .unwrap();

        assert_eq!(git(&next.path, &["rev-parse", "HEAD"]), preserved_commit);

        // Test fixture owns the worktree it created.
        git(&checkout, &["worktree", "remove", next.path.to_str().unwrap()]);
        git(&checkout, &["branch", "-D", &next.branch]);
        let _ = std::fs::remove_dir(next.path.parent().unwrap());
    }

    use crate::blocks::test_helpers::git_repo;

    /// A registered checkout whose trunk moved after a task branch was
    /// preserved from it. `preserved_change` and `trunk_change` are
    /// `(file, contents)` pairs; the same file makes them conflict.
    struct MovedTrunk {
        repo: git_repo::Repo,
        preserved_branch: String,
        preserved: String,
        trunk: String,
    }

    fn moved_trunk(
        branch: &str,
        preserved_change: (&str, &str),
        trunk_change: (&str, &str),
    ) -> MovedTrunk {
        let repo = git_repo::repo();
        git_repo::git(&repo.work, &["checkout", "-q", "-b", branch]);
        git_repo::commit(&repo.work, preserved_change.0, preserved_change.1, "preserved slice");
        let preserved = git_repo::git(&repo.work, &["rev-parse", "HEAD"]);
        git_repo::git(&repo.work, &["checkout", "-q", "main"]);
        git_repo::commit(&repo.work, trunk_change.0, trunk_change.1, "trunk moved on");
        git_repo::git(&repo.work, &["push", "-q", "origin", "main"]);
        let trunk = git_repo::git(&repo.work, &["rev-parse", "HEAD"]);
        MovedTrunk {
            repo,
            preserved_branch: branch.to_string(),
            preserved,
            trunk,
        }
    }

    /// Test fixture owns the worktree and branch it created.
    fn remove_fixture_workspace(checkout: &Path, workspace: &super::TaskWorkspace) {
        git_repo::git(
            checkout,
            &[
                "worktree",
                "remove",
                "--force",
                workspace.path.to_str().unwrap(),
            ],
        );
        git_repo::git(checkout, &["branch", "-D", &workspace.branch]);
        let _ = std::fs::remove_dir(workspace.path.parent().unwrap());
    }

    /// The mojentic-sw cycle 15 case, resolved at the start: trunk moved after
    /// the preserved cycle, so the next cycle starts from trunk plus that
    /// work, and the preservation ref is left exactly where it was.
    #[tokio::test]
    async fn a_continuation_behind_trunk_starts_from_trunk_plus_its_preserved_work() {
        let fixture = moved_trunk(
            "foundry-task/rebase-clean",
            ("feature.txt", "preserved work\n"),
            ("trunk.txt", "trunk work\n"),
        );
        let checkout = &fixture.repo.work;

        let workspace = prepare_task_workspace(
            &CleanProcessShellGateway,
            &test_entry("foundry-continuation-clean", checkout),
            "evt_contclean01",
            Some(&fixture.preserved_branch),
        )
        .await
        .unwrap();

        let head = git(&workspace.path, &["rev-parse", "HEAD"]);
        assert_eq!(git(&workspace.path, &["rev-parse", "HEAD^"]), fixture.trunk);
        assert_eq!(git(&workspace.path, &["log", "-1", "--format=%s"]), "preserved slice");
        assert!(workspace.path.join("feature.txt").exists(), "preserved work carried");
        assert!(workspace.path.join("trunk.txt").exists(), "trunk work present");
        assert_eq!(
            git(checkout, &["rev-parse", &fixture.preserved_branch]),
            fixture.preserved,
            "the preservation ref must not be rewritten"
        );
        assert_eq!(workspace.base.commit, head);
        assert_eq!(workspace.base.trunk, fixture.trunk);

        remove_fixture_workspace(checkout, &workspace);
    }

    /// A continuation that already contains trunk starts exactly where it was
    /// preserved, as it always did.
    #[tokio::test]
    async fn an_up_to_date_continuation_starts_exactly_on_its_preserved_commit() {
        let repo = git_repo::repo();
        let checkout = &repo.work;
        let trunk = git(checkout, &["rev-parse", "HEAD"]);
        git(checkout, &["checkout", "-q", "-b", "foundry-task/current"]);
        git_repo::commit(checkout, "feature.txt", "preserved\n", "preserved slice");
        let preserved = git(checkout, &["rev-parse", "HEAD"]);
        git(checkout, &["checkout", "-q", "main"]);

        let workspace = prepare_task_workspace(
            &CleanProcessShellGateway,
            &test_entry("foundry-continuation-current", checkout),
            "evt_contcurrent",
            Some("foundry-task/current"),
        )
        .await
        .unwrap();

        assert_eq!(git(&workspace.path, &["rev-parse", "HEAD"]), preserved);
        assert_eq!(workspace.base.commit, preserved);
        assert_eq!(workspace.base.trunk, trunk);

        remove_fixture_workspace(checkout, &workspace);
    }

    /// Preserved work that conflicts with trunk cannot be carried by an agent
    /// that may not rewrite history, so it is refused before any session,
    /// naming the files, with nothing left behind and the preserved ref intact.
    #[tokio::test]
    async fn a_conflicting_continuation_is_refused_before_any_session_and_leaves_nothing_behind() {
        let fixture = moved_trunk(
            "foundry-task/rebase-conflict",
            ("README.md", "preserved edit\n"),
            ("README.md", "trunk edit\n"),
        );
        let checkout = &fixture.repo.work;
        let entry = test_entry("foundry-continuation-conflict", checkout);
        let (path, branch) =
            crate::workspace::task_workspace_paths(&entry.name, "evt_contconflict");

        let Err(error) = prepare_task_workspace(
            &CleanProcessShellGateway,
            &entry,
            "evt_contconflict",
            Some(&fixture.preserved_branch),
        )
        .await
        else {
            panic!("a conflicting continuation must be refused");
        };

        let refusal = error
            .downcast_ref::<super::ContinuationConflict>()
            .expect("the refusal is typed");
        assert_eq!(
            refusal,
            &super::ContinuationConflict {
                base_ref: fixture.preserved_branch.clone(),
                preserved: fixture.preserved.clone(),
                trunk: fixture.trunk.clone(),
                behind: 1,
                files: vec!["README.md".to_string()],
            }
        );
        let message = error.to_string();
        assert!(message.contains("before any agent session"), "{message}");
        assert!(message.contains("README.md"), "{message}");
        assert!(!path.exists(), "the refused worktree must be removed");
        assert!(
            git(checkout, &["branch", "--list", &branch]).is_empty(),
            "the refused workspace branch must be deleted"
        );
        assert_eq!(
            git(checkout, &["rev-parse", &fixture.preserved_branch]),
            fixture.preserved,
            "the preservation ref must be untouched"
        );
        assert_eq!(git(checkout, &["status", "--porcelain"]), "");
        let _ = std::fs::remove_dir(path.parent().unwrap());
    }

    #[test]
    fn a_low_disk_refuses_the_worktree_before_anything_is_written() {
        use foundry_sdk::disk::DiskThreshold;
        let dir = tempfile::tempdir().unwrap();
        let entry = crate::blocks::test_helpers::project_entry("p", dir.path().to_str().unwrap());
        let worktree = dir.path().join("worktrees/p/abc");
        let off = DiskThreshold {
            min_free_bytes: 0,
            min_free_percent: 0,
        };
        assert!(super::ensure_disk_room(&entry, &worktree, off).is_ok());
        let impossible = DiskThreshold {
            min_free_bytes: u64::MAX,
            min_free_percent: 101,
        };
        let err = super::ensure_disk_room(&entry, &worktree, impossible).unwrap_err();
        assert!(err.to_string().starts_with("insufficient disk: "), "{err}");
        assert!(!worktree.exists());
    }
}
