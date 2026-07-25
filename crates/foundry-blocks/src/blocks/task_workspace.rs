use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use foundry_sdk::registry::ProjectEntry;

use crate::gateway::ShellGateway;
use crate::workspace::task_workspace_paths;

pub(crate) struct TaskWorkspace {
    pub path: PathBuf,
    pub branch: String,
}

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

/// Create a fresh branch and worktree from the current remote base (or a
/// preserved continuation ref) without touching the registered checkout.
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
    let repo = Path::new(&entry.path);
    let (path, branch) = task_workspace_paths(&entry.name, workspace_id);

    if path.exists() {
        bail!("task worktree already exists: {}", path.display());
    }
    std::fs::create_dir_all(path.parent().context("worktree path has no parent")?)
        .with_context(|| format!("create task worktree parent for {}", path.display()))?;

    let source = if let Some(bundle) = base_ref.and_then(|r| r.strip_prefix("bundle:")) {
        let reference = bundle_branch_ref(shell, repo, bundle).await?;
        checked(shell, repo, "git", &["fetch", bundle, &reference]).await?;
        "FETCH_HEAD".to_string()
    } else if let Some(reference) = base_ref {
        if ref_exists_locally(shell, repo, reference).await? {
            reference.to_string()
        } else {
            checked(shell, repo, "git", &["fetch", "origin", reference]).await?;
            "FETCH_HEAD".to_string()
        }
    } else {
        checked(shell, repo, "git", &["fetch", "origin", &entry.branch]).await?;
        "FETCH_HEAD".to_string()
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

    Ok(TaskWorkspace { path, branch })
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

        git(&checkout, &["worktree", "remove", next.path.to_str().unwrap()]);
        git(&checkout, &["branch", "-D", &next.branch]);
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

        git(&checkout, &["worktree", "remove", next.path.to_str().unwrap()]);
        git(&checkout, &["branch", "-D", &next.branch]);
        let _ = std::fs::remove_dir(next.path.parent().unwrap());
    }
}
