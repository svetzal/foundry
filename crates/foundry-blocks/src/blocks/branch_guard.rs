//! After the maintain agent finishes, put the checkout back on its configured
//! branch.
//!
//! An agent sometimes creates a working branch, commits there, and stops. The
//! next nightly's validation then refuses the checkout ("wrong branch") and
//! the project goes unmaintained until a human notices. This guard runs right
//! after the agent:
//!
//! - on the configured branch: nothing to do;
//! - on another branch (or a detached `HEAD`) that fast-forwards the
//!   configured branch: move the configured branch to it, check it out, and
//!   delete the agent's branch;
//! - otherwise: report failure with the reason, and leave the checkout for a
//!   human.
//!
//! Nothing here rewrites history or forces anything: the configured branch
//! only ever moves forward, and the agent branch is deleted with `-d`, which
//! Git refuses unless it is merged.

use std::path::Path;

use crate::gateway::ShellGateway;

/// What the guard found and did.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum BranchGuard {
    /// Already on the configured branch.
    OnBranch,
    /// The branch could not be read (not a Git checkout, or Git failed); the
    /// guard did nothing. Validation checks the branch before the next run.
    Unknown(String),
    /// The configured branch was fast-forwarded to the agent's commits and
    /// checked out. `from` is where the agent left `HEAD`.
    Restored { from: String },
    /// The checkout is not on the configured branch and could not be put
    /// back safely. The run must fail with this reason.
    Failed(String),
}

/// Make sure the checkout at `path` is on `branch` after an agent run.
pub(super) async fn restore_configured_branch(
    shell: &dyn ShellGateway,
    path: &Path,
    branch: &str,
) -> BranchGuard {
    let current =
        match shell.run(path, "git", &["rev-parse", "--abbrev-ref", "HEAD"], None, None).await {
            Ok(r) if r.success => r.stdout.lines().next().unwrap_or("").trim().to_string(),
            Ok(r) => return BranchGuard::Unknown(r.stderr.trim().to_string()),
            Err(e) => return BranchGuard::Unknown(e.to_string()),
        };
    if current.is_empty() {
        return BranchGuard::Unknown("git reported no branch".to_string());
    }
    if current == branch {
        return BranchGuard::OnBranch;
    }
    let from = if current == "HEAD" {
        "a detached HEAD".to_string()
    } else {
        current.clone()
    };

    // Exit 0: `branch` is an ancestor of HEAD, so moving it is a fast-forward.
    // Exit 1: it is not. Anything else: Git could not tell.
    match shell
        .run(path, "git", &["merge-base", "--is-ancestor", branch, "HEAD"], None, None)
        .await
    {
        Ok(r) if r.exit_code == 0 => {}
        Ok(r) if r.exit_code == 1 => {
            return BranchGuard::Failed(format!(
                "the agent left the checkout on {from}, which does not fast-forward {branch}; \
                 left for a human"
            ));
        }
        Ok(r) => {
            return BranchGuard::Failed(format!(
                "the agent left the checkout on {from}; could not compare it with {branch}: {}",
                r.stderr.trim()
            ));
        }
        Err(e) => {
            return BranchGuard::Failed(format!(
                "the agent left the checkout on {from}; could not compare it with {branch}: {e}"
            ));
        }
    }

    // Move the configured branch (not checked out, so `-f` only updates the
    // ref) and switch to it. HEAD's commit does not change, so any uncommitted
    // work the agent left stays in the working tree for the commit step.
    for args in [
        vec!["branch", "-f", branch, "HEAD"],
        vec!["checkout", branch],
    ] {
        match shell.run(path, "git", &args, None, None).await {
            Ok(r) if r.success => {}
            Ok(r) => {
                return BranchGuard::Failed(format!(
                    "the agent left the checkout on {from}; git {} failed: {}",
                    args.join(" "),
                    r.stderr.trim()
                ));
            }
            Err(e) => {
                return BranchGuard::Failed(format!(
                    "the agent left the checkout on {from}; git {} failed: {e}",
                    args.join(" ")
                ));
            }
        }
    }

    if current != "HEAD" {
        match shell.run(path, "git", &["branch", "-d", &current], None, None).await {
            Ok(r) if r.success => {}
            // Best-effort: the checkout is already back on the configured
            // branch with the agent's commits; a leftover (merged) branch name
            // blocks nothing. Logged so it can be cleaned up.
            Ok(r) => {
                tracing::warn!(branch = %current, stderr = %r.stderr.trim(), "could not delete agent branch");
            }
            Err(e) => {
                tracing::warn!(branch = %current, error = %e, "could not delete agent branch");
            }
        }
    }

    tracing::warn!(from = %from, %branch, "agent left the configured branch; fast-forwarded it back");
    BranchGuard::Restored { from }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use crate::gateway::ProcessShellGateway;

    use super::{BranchGuard, restore_configured_branch};

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn commit(dir: &Path, file: &str, message: &str) {
        std::fs::write(dir.join(file), message).unwrap();
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", message]);
    }

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "-b", "main"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Test"]);
        commit(dir.path(), "README.md", "init");
        dir
    }

    fn branches(dir: &Path) -> Vec<String> {
        git(dir, &["branch", "--format=%(refname:short)"])
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[tokio::test]
    async fn on_the_configured_branch_nothing_happens() {
        let dir = repo();
        let guard = restore_configured_branch(&ProcessShellGateway, dir.path(), "main").await;
        assert_eq!(guard, BranchGuard::OnBranch);
    }

    #[tokio::test]
    async fn agent_branch_that_fast_forwards_is_merged_back_and_deleted() {
        // reaction_new on 2026-09-24: the agent created a branch and committed there.
        let dir = repo();
        git(dir.path(), &["checkout", "-q", "-b", "chore/dependency-update-2026-09-24"]);
        commit(dir.path(), "mix.lock", "Update deps");
        let agent_tip = git(dir.path(), &["rev-parse", "HEAD"]);

        let guard = restore_configured_branch(&ProcessShellGateway, dir.path(), "main").await;

        assert_eq!(
            guard,
            BranchGuard::Restored {
                from: "chore/dependency-update-2026-09-24".to_string()
            }
        );
        assert_eq!(git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
        assert_eq!(git(dir.path(), &["rev-parse", "main"]), agent_tip, "main carries the commit");
        assert_eq!(branches(dir.path()), ["main"], "agent branch deleted");
    }

    #[tokio::test]
    async fn uncommitted_work_survives_the_move_back() {
        let dir = repo();
        git(dir.path(), &["checkout", "-q", "-b", "agent-work"]);
        commit(dir.path(), "a.txt", "committed");
        std::fs::write(dir.path().join("README.md"), "left uncommitted").unwrap();

        let guard = restore_configured_branch(&ProcessShellGateway, dir.path(), "main").await;

        assert!(matches!(guard, BranchGuard::Restored { .. }), "{guard:?}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("README.md")).unwrap(),
            "left uncommitted"
        );
    }

    #[tokio::test]
    async fn detached_head_ahead_of_the_branch_is_fast_forwarded() {
        let dir = repo();
        git(dir.path(), &["checkout", "-q", "--detach"]);
        commit(dir.path(), "b.txt", "detached work");

        let guard = restore_configured_branch(&ProcessShellGateway, dir.path(), "main").await;

        assert_eq!(
            guard,
            BranchGuard::Restored {
                from: "a detached HEAD".to_string()
            }
        );
        assert_eq!(git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]), "main");
    }

    #[tokio::test]
    async fn diverged_agent_branch_fails_with_a_clear_reason_and_is_left_alone() {
        let dir = repo();
        git(dir.path(), &["checkout", "-q", "-b", "agent-work"]);
        commit(dir.path(), "a.txt", "agent");
        git(dir.path(), &["checkout", "-q", "main"]);
        commit(dir.path(), "c.txt", "main moved");
        git(dir.path(), &["checkout", "-q", "agent-work"]);

        let guard = restore_configured_branch(&ProcessShellGateway, dir.path(), "main").await;

        let BranchGuard::Failed(reason) = guard else {
            panic!("diverged branch must fail: {guard:?}");
        };
        assert!(
            reason.contains("agent-work") && reason.contains("does not fast-forward main"),
            "{reason}"
        );
        assert_eq!(
            git(dir.path(), &["rev-parse", "--abbrev-ref", "HEAD"]),
            "agent-work",
            "untouched"
        );
        assert_eq!(branches(dir.path()), ["agent-work", "main"]);
    }

    #[tokio::test]
    async fn not_a_git_checkout_is_unknown_not_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let guard = restore_configured_branch(&ProcessShellGateway, dir.path(), "main").await;
        assert!(matches!(guard, BranchGuard::Unknown(_)), "{guard:?}");
    }
}
