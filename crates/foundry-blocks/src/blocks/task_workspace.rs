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
    } else {
        let remote_ref = base_ref.unwrap_or(&entry.branch);
        checked(shell, repo, "git", &["fetch", "origin", remote_ref]).await?;
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
