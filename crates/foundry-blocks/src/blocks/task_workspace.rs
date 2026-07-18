use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use foundry_sdk::registry::ProjectEntry;

use crate::gateway::ShellGateway;

pub(crate) struct TaskWorkspace {
    pub path: PathBuf,
    pub branch: String,
}

fn slug(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
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

/// Create a fresh branch and worktree from the current remote base (or a
/// preserved continuation ref) without touching the registered checkout.
pub(crate) async fn prepare_task_workspace(
    shell: &dyn ShellGateway,
    entry: &ProjectEntry,
    run_id: &str,
    base_ref: Option<&str>,
) -> Result<TaskWorkspace> {
    let repo = Path::new(&entry.path);
    let short_id = run_id.trim_start_matches("evt_").chars().take(12).collect::<String>();
    let branch = format!("foundry-task/{}-{short_id}", slug(&entry.name));
    let path = foundry_sdk::paths::worktrees_dir().join(slug(&entry.name)).join(&short_id);

    if path.exists() {
        bail!("task worktree already exists: {}", path.display());
    }
    std::fs::create_dir_all(path.parent().context("worktree path has no parent")?)
        .with_context(|| format!("create task worktree parent for {}", path.display()))?;

    let source = if let Some(bundle) = base_ref.and_then(|r| r.strip_prefix("bundle:")) {
        checked(shell, repo, "git", &["fetch", bundle]).await?;
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

    fn test_entry(path: &Path) -> ProjectEntry {
        ProjectEntry {
            name: "foundry".to_string(),
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

    fn task_trigger(worktree: &Path, branch: &str, verdict: TaskVerdict) -> Event {
        Event::new(
            foundry_sdk::event::EventType::TaskReviewed,
            "foundry".to_string(),
            Throttle::Full,
            Event::serialize_payload(&TaskReviewedPayload {
                project: "foundry".to_string(),
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

        let registry = super::super::test_helpers::registry_with_entry(test_entry(&checkout));
        let finalize =
            FinalizeTask::with_gateways(registry, std::sync::Arc::new(CleanProcessShellGateway));
        let finalized = finalize
            .execute(&task_trigger(&worktree, "foundry-task/landed-cycle", TaskVerdict::Complete))
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
            &test_entry(&checkout),
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
        let entry = test_entry(dir.path());
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
}
