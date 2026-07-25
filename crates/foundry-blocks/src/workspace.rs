//! Task workspace mechanics: where a task's worktree and branch live, and how
//! its work is committed, preserved, or thrown away.
//!
//! These were private to `blocks::finalize_task` and `blocks::task_workspace`
//! until cancellation needed them too. `FinalizeTask` disposes of a workspace
//! at the *end* of a healthy task; `DisposeCampaignWork` disposes of one that
//! was orphaned when an operator killed the run mid-flight. Both need the same
//! primitives, and — more importantly — both need to agree on exactly which
//! path and branch a workspace id names.
//!
//! [`task_workspace_paths`] is that single agreement. Deriving the path in one
//! place and the branch in another is how disposal silently starts missing
//! worktrees, so every caller goes through here.
//!
//! Landing policy (what may reach trunk, and how) deliberately stays in
//! `blocks::finalize_task` — that is judgement about work, not mechanics of
//! where it sits.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

use crate::gateway::ShellGateway;

/// Reduce a project or campaign name to something safe in a path and a git ref.
pub(crate) fn slug(value: &str) -> String {
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

/// The worktree path and branch name a `(project, workspace_id)` pair denotes.
///
/// The single source of truth for this derivation. `prepare_task_workspace`
/// creates what this returns; `DisposeCampaignWork` finds what this returns.
/// If the two ever computed it separately they would drift, and disposal would
/// quietly leave orphaned worktrees on disk.
pub(crate) fn task_workspace_paths(project_name: &str, workspace_id: &str) -> (PathBuf, String) {
    task_workspace_paths_in(&foundry_sdk::paths::worktrees_dir(), project_name, workspace_id)
}

/// [`task_workspace_paths`] against an explicit worktrees root.
///
/// Exists so callers that already hold a root — and tests, which must not
/// mutate the process environment to steer `worktrees_dir()` — resolve paths
/// through the same derivation rather than rebuilding it.
pub(crate) fn task_workspace_paths_in(
    worktrees_root: &Path,
    project_name: &str,
    workspace_id: &str,
) -> (PathBuf, String) {
    let project = slug(project_name);
    let path = worktrees_root.join(&project).join(workspace_id);
    let branch = format!("foundry-task/{project}-{workspace_id}");
    (path, branch)
}

/// How many run-id characters disambiguate a re-dispatched cycle.
const RUN_FRAGMENT_LEN: usize = 6;

/// The workspace id for one campaign cycle: `{campaign}-c{cycle}-{run fragment}`.
///
/// The run fragment keeps a re-dispatched cycle from colliding with the
/// worktree its earlier attempt left behind, which `prepare_task_workspace`
/// would otherwise refuse outright.
pub(crate) fn campaign_workspace_id(campaign: &str, cycle: u64, run_id: &str) -> String {
    let fragment: String =
        run_id.trim_start_matches("evt_").chars().take(RUN_FRAGMENT_LEN).collect();
    format!("{}-c{cycle}-{fragment}", slug(campaign))
}

/// Whether `workspace_id` is a cycle of `campaign`.
///
/// Cancellation knows the campaign but not which cycle was in flight, nor the
/// run fragment, so disposal has to recognise the campaign's worktrees on
/// disk. It parses rather than prefix-matches deliberately: campaign `ship`
/// and campaign `ship-c2` produce ids sharing the prefix `ship-c`, so a
/// `starts_with` would let one campaign's cancellation delete the other's
/// work. Requiring a numeric cycle and a dash-free fragment separates them —
/// `ship-c2-c1-999999` (campaign `ship-c2`, cycle 1) fails to parse as a cycle
/// of `ship` because `c1-999999` is not a run fragment.
pub(crate) fn is_campaign_workspace(campaign: &str, workspace_id: &str) -> bool {
    let Some(rest) = workspace_id.strip_prefix(&format!("{}-c", slug(campaign))) else {
        return false;
    };
    let Some((cycle, fragment)) = rest.split_once('-') else {
        return false;
    };
    !cycle.is_empty()
        && cycle.chars().all(|c| c.is_ascii_digit())
        && !fragment.is_empty()
        && fragment.chars().all(|c| c.is_ascii_alphanumeric())
}

/// The workspace id for a task that no campaign owns.
pub(crate) fn run_workspace_id(run_id: &str) -> String {
    run_id.trim_start_matches("evt_").chars().take(12).collect()
}

pub(crate) async fn run(
    shell: &dyn ShellGateway,
    cwd: &Path,
    args: &[&str],
) -> Result<crate::shell::CommandResult> {
    shell.run(cwd, "git", args, None, None).await
}

pub(crate) async fn checked(shell: &dyn ShellGateway, cwd: &Path, args: &[&str]) -> Result<String> {
    let result = run(shell, cwd, args).await?;
    if !result.success {
        bail!("git {} failed ({}): {}", args.join(" "), result.exit_code, result.stderr.trim());
    }
    Ok(result.stdout.trim().to_string())
}

/// Run a git command whose outcome is cleanup, not correctness: log a failure
/// (spawn error or non-zero exit) rather than discarding it.
pub(crate) async fn run_best_effort(shell: &dyn ShellGateway, cwd: &Path, args: &[&str]) {
    match run(shell, cwd, args).await {
        Ok(result) if !result.success => {
            tracing::warn!(
                git_args = %args.join(" "),
                exit_code = result.exit_code,
                stderr = %result.stderr.trim(),
                "best-effort cleanup command failed"
            );
        }
        Err(e) => {
            tracing::warn!(git_args = %args.join(" "), error = %e, "best-effort cleanup command failed to run");
        }
        Ok(_) => {}
    }
}

/// Commit whatever the agent left in the worktree. `Ok(false)` when the tree
/// was already clean.
pub(crate) async fn commit_worktree(
    shell: &dyn ShellGateway,
    worktree: &Path,
    project: &str,
) -> Result<bool> {
    let status = checked(shell, worktree, &["status", "--porcelain"]).await?;
    if status.is_empty() {
        return Ok(false);
    }
    checked(shell, worktree, &["add", "-A"]).await?;
    checked(shell, worktree, &["commit", "-m", &format!("feat({project}): automated task")])
        .await?;
    Ok(true)
}

/// Push the task branch somewhere durable, falling back to a local bundle.
///
/// Returns the ref by which the work can be recovered: a branch name, or
/// `bundle:{path}`.
pub(crate) async fn preserve(
    shell: &dyn ShellGateway,
    worktree: &Path,
    project: &str,
    branch: &str,
) -> Result<String> {
    let push = run(shell, worktree, &["push", "-u", "origin", branch]).await?;
    if push.success {
        return Ok(branch.to_string());
    }

    let dir = foundry_sdk::paths::preserved_dir().join(project);
    std::fs::create_dir_all(&dir)?;
    let bundle = dir.join(format!("{}.bundle", branch.replace('/', "-")));
    let bundle_text = bundle.to_string_lossy().to_string();
    // Record HEAD alongside the branch. A bundle carrying only
    // `refs/heads/<branch>` advertises no HEAD, so neither `git clone` nor a
    // bare `git fetch` can resolve it — which is what made this fallback a
    // dead end for anyone, human or machine, trying to recover the work.
    checked(shell, worktree, &["bundle", "create", &bundle_text, "HEAD", branch]).await?;
    Ok(format!("bundle:{bundle_text}"))
}

pub(crate) async fn remove_workspace(shell: &dyn ShellGateway, checkout: &Path, worktree: &Path) {
    let worktree_text = worktree.to_string_lossy().to_string();
    // Best-effort: a task already landed (or was preserved) by the time we
    // reach here; a leaked worktree is cleaned up on a later run and must
    // not fail an already-completed task.
    run_best_effort(shell, checkout, &["worktree", "remove", &worktree_text]).await;
}

/// Tear down a worktree and its branch without preserving anything.
///
/// Used only when the operator explicitly asked to discard. The remote branch
/// is deliberately left alone: if a previous cycle already pushed one, it is
/// the audit trail for work that did reach a durable ref, and a discard of
/// *uncommitted* work has no business deleting it.
pub(crate) async fn discard_workspace(
    shell: &dyn ShellGateway,
    checkout: &Path,
    worktree: &Path,
    branch: &str,
) {
    let worktree_text = worktree.to_string_lossy().to_string();
    run_best_effort(shell, checkout, &["worktree", "remove", "--force", &worktree_text]).await;
    run_best_effort(shell, checkout, &["branch", "-D", branch]).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_lowercases_and_replaces_non_alphanumerics() {
        assert_eq!(slug("Foundry CLI"), "foundry-cli");
        assert_eq!(slug("_leading-and-trailing_"), "leading-and-trailing");
        assert_eq!(slug("backend-ort"), "backend-ort");
    }

    #[test]
    fn workspace_paths_agree_between_path_and_branch() {
        let (path, branch) = task_workspace_paths("Backend ORT", "abc123");
        assert!(path.ends_with("backend-ort/abc123"));
        assert_eq!(branch, "foundry-task/backend-ort-abc123");
    }

    #[test]
    fn every_cycle_of_a_campaign_is_recognised_as_its_own() {
        let cycle_one = campaign_workspace_id("Ship Billing", 1, "evt_abcdef123456");
        let cycle_two = campaign_workspace_id("Ship Billing", 2, "evt_999999888888");

        assert_eq!(cycle_one, "ship-billing-c1-abcdef");
        assert_eq!(cycle_two, "ship-billing-c2-999999");
        assert!(is_campaign_workspace("Ship Billing", &cycle_one));
        assert!(is_campaign_workspace("ship-billing", &cycle_two));
    }

    /// The case that makes this a parser instead of a `starts_with`: campaign
    /// `ship` and campaign `ship-c2` produce ids sharing the prefix `ship-c`.
    /// Cancelling one must never dispose of the other's work.
    #[test]
    fn a_campaign_never_claims_a_similarly_named_campaigns_workspace() {
        let ship = campaign_workspace_id("ship", 2, "evt_abcdef123456");
        let ship_c2 = campaign_workspace_id("ship-c2", 1, "evt_999999888888");

        assert_eq!(ship, "ship-c2-abcdef");
        assert_eq!(ship_c2, "ship-c2-c1-999999");

        assert!(is_campaign_workspace("ship", &ship));
        assert!(is_campaign_workspace("ship-c2", &ship_c2));
        assert!(!is_campaign_workspace("ship", &ship_c2));
        assert!(!is_campaign_workspace("ship-c2", &ship));
    }

    #[test]
    fn a_non_campaign_workspace_is_never_claimed() {
        // Plain task worktrees are named from the run id alone.
        assert!(!is_campaign_workspace("ship", &run_workspace_id("evt_abcdef123456")));
        assert!(!is_campaign_workspace("ship", "ship-cabc-123456"));
        assert!(!is_campaign_workspace("ship", "ship-c1"));
        assert!(!is_campaign_workspace("ship", "ship-c1-"));
    }

    /// A re-dispatched cycle must not collide with the worktree its earlier
    /// attempt left behind — `prepare_task_workspace` bails on an existing path.
    #[test]
    fn a_re_dispatched_cycle_gets_a_distinct_id() {
        let first = campaign_workspace_id("c", 3, "evt_aaaaaaaaaaaa");
        let second = campaign_workspace_id("c", 3, "evt_bbbbbbbbbbbb");
        assert_ne!(first, second);
        assert!(first.starts_with("c-c3-"));
        assert!(second.starts_with("c-c3-"));
    }

    #[test]
    fn non_campaign_workspace_id_is_the_run_id_fragment() {
        assert_eq!(run_workspace_id("evt_abcdef1234567890"), "abcdef123456");
        assert_eq!(run_workspace_id("short"), "short");
    }
}
