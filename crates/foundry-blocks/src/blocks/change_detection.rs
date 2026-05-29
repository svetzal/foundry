use std::path::Path;

use crate::gateway::ShellGateway;

/// Capture the project's HEAD SHA before agent invocation so post-execution
/// change detection can compare against a stable pre-execution snapshot.
///
/// Callers should invoke this immediately before running the agent. Returns
/// `None` for non-git directories, repositories with no commits yet, or any
/// other failure — in which case [`detect_post_execution_changes`] falls back
/// to working-tree-only detection via `git status --porcelain`.
pub(crate) async fn capture_pre_execution_sha(
    shell: &dyn ShellGateway,
    project_path: &Path,
) -> Option<String> {
    match shell.run(project_path, "git", &["rev-parse", "HEAD"], None, None).await {
        Ok(r) if r.success => {
            let sha = r.stdout.trim().to_string();
            if sha.is_empty() { None } else { Some(sha) }
        }
        Ok(r) => {
            tracing::warn!(stderr = %r.stderr, "git rev-parse HEAD failed; pre-execution sha unavailable");
            None
        }
        Err(e) => {
            tracing::warn!(error = %e, "git rev-parse HEAD errored; pre-execution sha unavailable");
            None
        }
    }
}

/// Detect whether agent execution produced changes in `project_path`, returning
/// `(changes_detected, files_changed)`.
///
/// When `pre_execution_sha` is provided (the normal path), runs
/// `git diff --name-only <sha>` which captures **both committed and
/// uncommitted** changes since the agent started. This is the right signal
/// because agents (especially Claude Code) commonly commit their work, which
/// would leave `git status --porcelain` empty even when substantial changes
/// have landed in HEAD.
///
/// On `git diff` failure (e.g. the pre-SHA was orphaned by a force-push) or
/// when no `pre_execution_sha` is available, falls back to
/// `git status --porcelain` (working-tree-only detection).
///
/// On total failure (non-git directory, missing git binary, etc.) logs a
/// warning and returns `(false, vec![])` so the calling block can still emit
/// its event normally.
pub(crate) async fn detect_post_execution_changes(
    shell: &dyn ShellGateway,
    project_path: &Path,
    pre_execution_sha: Option<&str>,
) -> (bool, Vec<String>) {
    if let Some(sha) = pre_execution_sha {
        match shell.run(project_path, "git", &["diff", "--name-only", sha], None, None).await {
            Ok(r) if r.success => {
                let files: Vec<String> =
                    r.stdout.lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
                return (!files.is_empty(), files);
            }
            Ok(r) => {
                tracing::warn!(
                    stderr = %r.stderr,
                    pre_sha = %sha,
                    "git diff against pre-execution sha failed; falling back to porcelain"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    pre_sha = %sha,
                    "git diff against pre-execution sha errored; falling back to porcelain"
                );
            }
        }
    }
    match shell.run(project_path, "git", &["status", "--porcelain"], None, None).await {
        Ok(r) if r.success => {
            // Porcelain v1 format: XY<space><path>. The first 3 bytes (XY + space)
            // are always ASCII, so slicing at byte 3 is safe for any filename.
            let files: Vec<String> =
                r.stdout.lines().filter(|l| l.len() >= 4).map(|l| l[3..].to_string()).collect();
            (!files.is_empty(), files)
        }
        Ok(r) => {
            tracing::warn!(stderr = %r.stderr, "git status failed; reporting no changes");
            (false, vec![])
        }
        Err(e) => {
            tracing::warn!(error = %e, "git status errored; reporting no changes");
            (false, vec![])
        }
    }
}

/// Returns `true` if `path` is an auxiliary worktree path that should not count
/// as a meaningful working-tree change (e.g. `.claude/worktrees/…`).
fn is_auxiliary_path(p: &str) -> bool {
    p.starts_with(".claude/worktrees/")
}

/// Returns `true` when the file list represents only auxiliary changes (or is
/// empty) and therefore should be treated as a silent no-op.
pub(crate) fn only_auxiliary_changes(files: &[String]) -> bool {
    files.is_empty() || files.iter().all(|f| is_auxiliary_path(f))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::{capture_pre_execution_sha, detect_post_execution_changes, is_auxiliary_path};

    // --- is_auxiliary_path ---

    #[test]
    fn auxiliary_path_returns_true() {
        assert!(is_auxiliary_path(".claude/worktrees/abc/foo.md"));
        assert!(is_auxiliary_path(".claude/worktrees/"));
    }

    #[test]
    fn non_auxiliary_path_returns_false() {
        assert!(!is_auxiliary_path("src/main.rs"));
        assert!(!is_auxiliary_path("Cargo.toml"));
        assert!(!is_auxiliary_path(".claude/settings.json"));
    }

    // --- capture_pre_execution_sha ---

    #[tokio::test]
    async fn capture_pre_execution_sha_returns_trimmed_sha_on_success() {
        let shell = FakeShellGateway::always(CommandResult {
            stdout: "abc123def456\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let sha = capture_pre_execution_sha(&*shell, Path::new("/tmp")).await;
        assert_eq!(sha.as_deref(), Some("abc123def456"));

        let invs = shell.invocations();
        assert_eq!(invs.len(), 1);
        assert_eq!(invs[0].command, "git");
        assert_eq!(invs[0].args, vec!["rev-parse", "HEAD"]);
    }

    #[tokio::test]
    async fn capture_pre_execution_sha_returns_none_on_empty_stdout() {
        let shell = FakeShellGateway::success();
        let sha = capture_pre_execution_sha(&*shell, Path::new("/tmp")).await;
        assert!(sha.is_none(), "empty stdout must yield None (not Some(\"\"))");
    }

    #[tokio::test]
    async fn capture_pre_execution_sha_returns_none_on_failure() {
        let shell = FakeShellGateway::failure("fatal: not a git repository");
        let sha = capture_pre_execution_sha(&*shell, Path::new("/tmp")).await;
        assert!(sha.is_none());
    }

    // --- detect_post_execution_changes: pre-SHA aware ---

    #[tokio::test]
    async fn detect_with_pre_sha_uses_git_diff_and_returns_committed_files() {
        // This reproduces the production regression: the agent committed its work,
        // leaving a clean working tree. `git status --porcelain` returns empty, but
        // `git diff --name-only <pre_sha>` correctly lists the committed paths.
        let shell = FakeShellGateway::always(CommandResult {
            stdout: "src/lib.rs\nCargo.toml\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let (changed, files) =
            detect_post_execution_changes(&*shell, Path::new("/tmp"), Some("abc123")).await;

        assert!(changed, "diff returning paths must report changes_detected=true");
        assert_eq!(files, vec!["src/lib.rs".to_string(), "Cargo.toml".to_string()]);

        let invs = shell.invocations();
        assert_eq!(invs.len(), 1);
        assert_eq!(invs[0].command, "git");
        assert_eq!(invs[0].args, vec!["diff", "--name-only", "abc123"]);
    }

    #[tokio::test]
    async fn detect_with_pre_sha_empty_diff_reports_no_changes() {
        let shell = FakeShellGateway::success();
        let (changed, files) =
            detect_post_execution_changes(&*shell, Path::new("/tmp"), Some("abc123")).await;
        assert!(!changed);
        assert!(files.is_empty());
    }

    #[tokio::test]
    async fn detect_with_pre_sha_falls_back_to_porcelain_on_diff_error() {
        // git diff may fail if pre_sha is unknown (e.g. force-pushed). Fall back to
        // working-tree-only detection so we still produce a useful signal.
        let shell = FakeShellGateway::sequence(vec![
            // git diff --name-only <sha> — fails (bad revision)
            CommandResult {
                stdout: String::new(),
                stderr: "fatal: bad revision 'abc123'".to_string(),
                exit_code: 128,
                success: false,
            },
            // git status --porcelain — succeeds, shows uncommitted change
            CommandResult {
                stdout: "M  src/main.rs\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let (changed, files) =
            detect_post_execution_changes(&*shell, Path::new("/tmp"), Some("abc123")).await;
        assert!(changed);
        assert_eq!(files, vec!["src/main.rs".to_string()]);

        let invs = shell.invocations();
        assert_eq!(invs.len(), 2);
        assert_eq!(invs[0].args, vec!["diff", "--name-only", "abc123"]);
        assert_eq!(invs[1].args, vec!["status", "--porcelain"]);
    }

    #[tokio::test]
    async fn detect_with_none_pre_sha_uses_porcelain_directly() {
        // When pre-sha capture earlier returned None, skip the diff attempt entirely
        // and use porcelain (preserves legacy behaviour for non-git contexts).
        let shell = FakeShellGateway::always(CommandResult {
            stdout: "M  src/foo.rs\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let (changed, files) =
            detect_post_execution_changes(&*shell, Path::new("/tmp"), None).await;
        assert!(changed);
        assert_eq!(files, vec!["src/foo.rs".to_string()]);

        let invs = shell.invocations();
        assert_eq!(invs.len(), 1);
        assert_eq!(invs[0].args, vec!["status", "--porcelain"]);
    }
}
