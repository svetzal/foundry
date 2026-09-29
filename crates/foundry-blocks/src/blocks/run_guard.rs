//! Foundry's checks on an agent run whose commits Foundry pushes.
//!
//! Every coding agent that can leave commits in a checkout Foundry later
//! pushes from (maintain, iterate, task, vulnerability and pipeline
//! remediation) runs between two calls here:
//!
//! 1. [`capture_run_base`] before the agent: `HEAD` and `origin/<branch>`.
//! 2. [`guard_agent_run`] after it: the direct-push check (`push_guard`) and
//!    the suppression check (`suppression_guard`), both against that start.
//!
//! A tripped check turns the agent's outcome into a failure whose reason
//! starts with "needs review: ". A failed run is never green, and
//! `Commit and Push` keeps a failed run's commits local, so nothing it made
//! is pushed and no release chain follows.

use std::path::{Path, PathBuf};

use foundry_sdk::gateway::AgentFailureMetadata;
use foundry_sdk::payload::RunBase;
use foundry_sdk::registry::ProjectEntry;

use crate::gateway::{AgentOutcome, ShellGateway};

pub(crate) use super::push_guard::capture_run_base;

/// What a run does when its agent added an advisory suppression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnSuppression {
    /// Fail the attempt and let the workflow's retry loop try again: the retry
    /// prompt tells the agent to remove the suppression and upgrade. Used by
    /// maintain and iterate, which retry through `Route Gate Result`.
    Retry,
    /// Fail the run and stop for a person (`needs_review` set). Used where no
    /// retry would see the failure: remediation, and tasks, whose reviewer
    /// must not be asked to accept a suppression.
    Review,
}

/// Where a run is checked: the checkout and the branch Foundry pushes.
pub(crate) struct GuardedRun<'a> {
    pub(crate) project: &'a str,
    pub(crate) path: &'a Path,
    pub(crate) branch: &'a str,
    pub(crate) events_dir: &'a Path,
}

/// Foundry's checks on an agent run, after the agent and before anything is
/// pushed.
///
/// - An agent that pushed directly fails the run and needs review; nothing a
///   retry does can undo the push, so the failure stops the workflow.
/// - A run that added an advisory suppression fails; `on_suppression` says
///   whether it may be retried or needs review.
///
/// Both compare against `base`, where the run started, so commits the agent
/// already pushed are still checked. Without a recorded base the suppression
/// check falls back to `origin/<branch>` and the push check is skipped.
pub(crate) async fn guard_agent_run(
    shell: &dyn ShellGateway,
    run: &GuardedRun<'_>,
    base: Option<&RunBase>,
    on_suppression: OnSuppression,
    outcome: AgentOutcome,
) -> AgentOutcome {
    if matches!(outcome, AgentOutcome::Unavailable { .. }) {
        // The agent never ran, so it pushed and changed nothing.
        return outcome;
    }
    let pushed = match base {
        Some(base) => {
            super::push_guard::detect_direct_push(shell, run.path, run.branch, base).await
        }
        None => None,
    };
    let check_suppressions = pushed.is_some() || matches!(outcome, AgentOutcome::Success { .. });
    let found = if check_suppressions {
        let diff_base = suppression_diff_base(shell, run.path, run.branch, base).await;
        super::suppression_guard::run_suppressions(
            shell,
            run.path,
            &diff_base,
            run.project,
            run.events_dir,
        )
        .await
    } else {
        Vec::new()
    };
    if let Some(push) = pushed {
        let reason = super::push_guard::direct_push_reason(run.branch, &push, &found);
        tracing::warn!(project = %run.project, %reason, "agent pushed directly");
        return needs_review(outcome, reason);
    }
    if found.is_empty() {
        return outcome;
    }
    let reason = super::suppression_guard::needs_review(&found);
    tracing::warn!(project = %run.project, %reason, "agent run added advisory suppressions");
    match on_suppression {
        OnSuppression::Retry => AgentOutcome::AgentFailed {
            stderr: reason,
            failure: None,
        },
        OnSuppression::Review => needs_review(outcome, reason),
    }
}

/// Run a remediation agent session between the guard's two checks.
///
/// Remediation (a vulnerability or a failing pipeline) works in the
/// registered checkout and has no retry loop, so any tripped check stops it
/// for review. `session` is the agent invocation; it is not started until
/// the run's start has been recorded.
pub(crate) async fn guard_remediation(
    shell: &dyn ShellGateway,
    entry: &ProjectEntry,
    session: impl Future<Output = AgentOutcome>,
) -> AgentOutcome {
    let path = PathBuf::from(&entry.path);
    let base = capture_run_base(shell, &path, &entry.branch).await;
    let outcome = session.await;
    let events_dir = foundry_sdk::paths::events_dir();
    let run = GuardedRun {
        project: &entry.name,
        path: &path,
        branch: &entry.branch,
        events_dir: &events_dir,
    };
    guard_agent_run(shell, &run, base.as_ref(), OnSuppression::Review, outcome).await
}

/// Fail `outcome` and mark it as needing review, keeping any provider
/// failure metadata the agent reported.
fn needs_review(outcome: AgentOutcome, reason: String) -> AgentOutcome {
    let failure = match outcome {
        AgentOutcome::AgentFailed {
            failure: Some(failure),
            ..
        } => failure,
        _ => AgentFailureMetadata::default(),
    };
    AgentOutcome::AgentFailed {
        stderr: reason.clone(),
        failure: Some(failure.with_needs_review(reason)),
    }
}

/// The revision the suppression diff starts from.
///
/// The commit `HEAD` and `origin/<branch>` shared when the run started, so
/// the diff covers the agent's commits and also commits an earlier failed run
/// left unpushed in the checkout: `Commit and Push` pushes every commit ahead
/// of the remote, and a suppression a previous run was stopped for must not
/// ride out with the next green run. Falls back to the recorded `HEAD`, and
/// to `origin/<branch>` when nothing was recorded.
async fn suppression_diff_base(
    shell: &dyn ShellGateway,
    dir: &Path,
    branch: &str,
    base: Option<&RunBase>,
) -> String {
    let Some(base) = base else {
        return format!("origin/{branch}");
    };
    let Some(origin) = base.origin.as_deref() else {
        return base.head.clone();
    };
    match shell.run(dir, "git", &["merge-base", &base.head, origin], None, None).await {
        Ok(r) if r.success && !r.stdout.trim().is_empty() => r.stdout.trim().to_string(),
        Ok(r) => {
            // Best-effort: unrelated histories or a pruned object; the
            // recorded HEAD still covers everything this run's agent did.
            tracing::warn!(%branch, stderr = %r.stderr.trim(), "no merge base; checking from the run's HEAD");
            base.head.clone()
        }
        Err(e) => {
            // Best-effort: as above, logged so it can be investigated.
            tracing::warn!(%branch, error = %e, "git merge-base failed; checking from the run's HEAD");
            base.head.clone()
        }
    }
}
