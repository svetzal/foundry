//! Keeps an agent's commits local until Foundry's own checks have run.
//!
//! Foundry pushes a run's commits in `Commit and Push`, after the verify gates
//! and, for maintenance, after the suppression guard. On 2026-09-29 the
//! mojentic-kt maintain agent pushed its own commit before any of that ran:
//! the suppression guard then diffed against an `origin/<branch>` that already
//! held the agent's work, and saw nothing to check.
//!
//! Three layers close that gap:
//!
//! - **The prompt** tells the agent to commit locally and never push
//!   ([`COMMIT_LOCALLY_RULE`], [`FOUNDRY_OWNS_GIT_RULE`]).
//! - **Prevention:** the agent session runs with `remote.origin.pushurl`
//!   pointed at an unusable URL ([`push_disabled_environment`]). A plain
//!   `git push` from the agent, or from any tool it starts, fails. Foundry's
//!   own push runs outside the agent's environment and is unaffected.
//! - **Detection:** a maintain run records where it started
//!   ([`capture_maintain_base`]) and afterwards checks whether
//!   `origin/<branch>` moved to commits the checkout holds
//!   ([`detect_direct_push`]). It fetches first, so a push to an explicit URL
//!   that bypassed the push URL is still seen. A run that pushed fails and
//!   needs review; the suppression guard still runs over the pushed commits
//!   because it compares against the recorded start, not the remote.

use std::path::Path;

use foundry_sdk::payload::MaintainBase;

use crate::gateway::ShellGateway;

/// The push URL an agent session sees for `origin`. Git rejects the scheme,
/// so a push fails before it reaches the network.
pub(crate) const PUSH_DISABLED_URL: &str = "foundry://agent-push-disabled";

/// Git rule for prompts whose commits Foundry pushes (maintenance,
/// remediation): commits are fine, pushes are not.
pub(crate) const COMMIT_LOCALLY_RULE: &str = "\
Git rules: commit your work locally if you want to, but never push, force-push, tag, or \
change remotes. Foundry checks your commits and pushes them itself after its checks pass. \
Pushing is disabled for this session, and a run that pushes directly fails and needs review.\n";

/// Git rule for prompts where Foundry commits as well as pushes (iterate and
/// task execution).
pub(crate) const FOUNDRY_OWNS_GIT_RULE: &str = "\
- Foundry owns Git finalization for this run. Do NOT commit, push, merge, rebase, tag, or \
modify refs. Repository guidance that normally requires a commit or push does not apply \
inside this Foundry task worktree. Leave the completed changes in the working tree for \
Foundry to review and finalize.\n";

/// Environment for an agent session that must not push through `origin`.
///
/// Inherited by every process the agent starts. Git reads `GIT_CONFIG_*` as
/// command-line configuration, so it overrides the repository's own
/// `remote.origin.pushurl` without writing to the repository.
pub(crate) fn push_disabled_environment() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
        ("GIT_CONFIG_KEY_0".to_string(), "remote.origin.pushurl".to_string()),
        ("GIT_CONFIG_VALUE_0".to_string(), PUSH_DISABLED_URL.to_string()),
    ]
}

/// Commits an agent pushed to `origin/<branch>` during its session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirectPush {
    /// `origin/<branch>` now.
    pub(crate) to: String,
    /// The pushed commits, oldest first, as `<short sha> <subject>`.
    pub(crate) commits: Vec<String>,
}

async fn git_line(shell: &dyn ShellGateway, dir: &Path, args: &[&str]) -> Option<String> {
    match shell.run(dir, "git", args, None, None).await {
        Ok(r) if r.success => Some(r.stdout.trim().to_string()).filter(|s| !s.is_empty()),
        Ok(_) => None,
        Err(e) => {
            // Best-effort: callers treat an unreadable ref as "unknown" and
            // fall back; the error is logged so it can be investigated.
            tracing::warn!(error = %e, ?args, "git could not run");
            None
        }
    }
}

fn remote_ref(branch: &str) -> String {
    format!("refs/remotes/origin/{branch}")
}

/// Record where a maintain run starts: `HEAD` and `origin/<branch>`.
/// `None` when `HEAD` cannot be read (not a Git checkout, or no commits).
pub(crate) async fn capture_maintain_base(
    shell: &dyn ShellGateway,
    dir: &Path,
    branch: &str,
) -> Option<MaintainBase> {
    let head = git_line(shell, dir, &["rev-parse", "HEAD"]).await?;
    let origin = git_line(shell, dir, &["rev-parse", "--verify", "-q", &remote_ref(branch)]).await;
    Some(MaintainBase { head, origin })
}

/// Whether the remote moved during the session to commits the checkout holds.
///
/// `origin_now` reachable from `HEAD` means the new remote commits are the
/// checkout's own: the agent pushed them. A remote that moved to commits the
/// checkout does not hold was pushed by someone else, and `Commit and Push`
/// integrates those as it always has.
pub(crate) fn pushed_directly(
    origin_before: Option<&str>,
    origin_now: Option<&str>,
    now_reachable_from_head: bool,
) -> bool {
    match (origin_before, origin_now) {
        (_, None) => false,
        (Some(before), Some(now)) if before == now => false,
        _ => now_reachable_from_head,
    }
}

/// Detect a push the agent made during its session.
///
/// Fetches `origin/<branch>` first so a push that bypassed the tracking ref
/// (an explicit URL) is seen; a failed fetch falls back to the local ref.
pub(crate) async fn detect_direct_push(
    shell: &dyn ShellGateway,
    dir: &Path,
    branch: &str,
    base: &MaintainBase,
) -> Option<DirectPush> {
    match shell.run(dir, "git", &["fetch", "-q", "origin", branch], None, None).await {
        Ok(r) if r.success => {}
        // Best-effort: without the fetch the local tracking ref is compared,
        // which still sees an ordinary `git push`; only an explicit-URL push
        // would be missed, and that is logged here.
        Ok(r) => tracing::warn!(
            %branch,
            stderr = %r.stderr.trim(),
            "could not fetch before the direct-push check; comparing the local tracking ref"
        ),
        Err(e) => tracing::warn!(
            %branch,
            error = %e,
            "could not fetch before the direct-push check; comparing the local tracking ref"
        ),
    }
    let remote = remote_ref(branch);
    let now = git_line(shell, dir, &["rev-parse", "--verify", "-q", &remote]).await;
    let reachable = match now.as_deref() {
        Some(now) => shell
            .run(dir, "git", &["merge-base", "--is-ancestor", now, "HEAD"], None, None)
            .await
            .is_ok_and(|r| r.success),
        None => false,
    };
    if !pushed_directly(base.origin.as_deref(), now.as_deref(), reachable) {
        return None;
    }
    let to = now?;
    let from = base.origin.as_deref().unwrap_or(&base.head);
    let range = format!("{from}..{to}");
    let commits = git_line(shell, dir, &["log", "--reverse", "--format=%h %s", &range])
        .await
        .map(|out| out.lines().map(str::to_string).collect())
        .unwrap_or_default();
    Some(DirectPush { to, commits })
}

/// The needs-review reason for a run whose agent pushed directly.
pub(crate) fn direct_push_reason(
    branch: &str,
    push: &DirectPush,
    suppressions: &[String],
) -> String {
    let short = push.to.get(..7).unwrap_or(&push.to);
    let commits = if push.commits.is_empty() {
        format!("origin/{branch} now at {short}")
    } else {
        format!("{} commit(s): {}", push.commits.len(), push.commits.join("; "))
    };
    let checked = if suppressions.is_empty() {
        "Foundry's suppression check ran over them and found no new suppressions".to_string()
    } else {
        format!(
            "they add advisory suppressions, which maintenance must never do: {}",
            suppressions.join("; ")
        )
    };
    format!(
        "needs review: agent pushed directly to origin/{branch} before Foundry's checks \
         ({commits}); {checked}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unmoved_remote_is_not_a_push() {
        assert!(!pushed_directly(Some("a"), Some("a"), true));
    }

    #[test]
    fn a_remote_moved_to_the_checkouts_own_commits_is_a_push() {
        assert!(pushed_directly(Some("a"), Some("b"), true));
    }

    #[test]
    fn a_remote_moved_by_someone_else_is_not_the_agents_push() {
        assert!(!pushed_directly(Some("a"), Some("b"), false));
    }

    #[test]
    fn a_new_remote_branch_holding_the_checkouts_commits_is_a_push() {
        assert!(pushed_directly(None, Some("b"), true));
    }

    #[test]
    fn an_unreadable_remote_is_not_a_push() {
        assert!(!pushed_directly(Some("a"), None, true));
    }

    #[test]
    fn push_disabled_environment_overrides_the_origin_push_url() {
        let env = push_disabled_environment();
        assert!(env.contains(&("GIT_CONFIG_COUNT".to_string(), "1".to_string())));
        assert!(
            env.contains(&("GIT_CONFIG_KEY_0".to_string(), "remote.origin.pushurl".to_string()))
        );
        assert!(env.contains(&("GIT_CONFIG_VALUE_0".to_string(), PUSH_DISABLED_URL.to_string())));
    }

    #[test]
    fn reason_names_the_commits_and_the_suppression_result() {
        let push = DirectPush {
            to: "3c77fa9aaaaaaaa".to_string(),
            commits: vec!["3c77fa9 chore(deps): Kover 0.9.9 -> 0.9.10".to_string()],
        };
        let clean = direct_push_reason("main", &push, &[]);
        assert!(
            clean.starts_with("needs review: agent pushed directly to origin/main"),
            "{clean}"
        );
        assert!(clean.contains("3c77fa9 chore(deps): Kover"), "{clean}");
        assert!(clean.contains("found no new suppressions"), "{clean}");
        let dirty = direct_push_reason("main", &push, &["CVE-2026-1".to_string()]);
        assert!(dirty.contains("CVE-2026-1"), "{dirty}");
    }

    #[test]
    fn the_rules_forbid_pushing() {
        assert!(COMMIT_LOCALLY_RULE.contains("never push"));
        assert!(FOUNDRY_OWNS_GIT_RULE.contains("Do NOT commit, push"));
    }
}
