//! Keeping a registered checkout in step with `origin/<branch>`.
//!
//! The nightly per-project chain works directly in the registered checkout
//! (`entry.path`), not in a disposable worktree. Before any work begins,
//! [`sync_checkout`] brings that checkout up to the remote tip by fast-forward
//! only, and refuses — without touching anything — when the tree is dirty or
//! the branch has diverged. Before pushing, [`integrate_remote_before_push`]
//! absorbs any remote movement that happened during the run, rebasing the
//! local commit only when that applies cleanly. Nothing here ever forces.
//!
//! The decision logic ([`parse_divergence`], [`plan_sync`]) is pure; the async
//! functions are a thin imperative shell over the [`ShellGateway`].

use std::path::Path;

use foundry_sdk::payload::GitSyncFailure;

use crate::gateway::ShellGateway;

/// Position of local `HEAD` relative to `origin/<branch>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Divergence {
    /// Commits on `HEAD` that are not on the remote tip.
    pub ahead: u32,
    /// Commits on the remote tip that are not on `HEAD`.
    pub behind: u32,
}

/// What the pre-work sync must do, derived purely from a [`Divergence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SyncPlan {
    /// Nothing to fast-forward (local is level with, or only ahead of, the remote).
    UpToDate,
    /// Local is strictly behind; fast-forward this many commits.
    FastForward(u32),
    /// Both sides moved; a fast-forward is impossible.
    Diverged,
}

/// Outcome of [`sync_checkout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SyncOutcome {
    /// The checkout is level with (or ahead of) the remote. `fast_forwarded`
    /// is the number of commits applied — or, under dry run, that would be.
    Synced { fast_forwarded: u32 },
    /// The sync was refused; the checkout was left exactly as found.
    Refused {
        failure: GitSyncFailure,
        detail: String,
    },
}

/// Outcome of [`integrate_remote_before_push`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PrePushSync {
    /// The local branch now contains the remote tip; pushing is a fast-forward.
    /// `rebased` is true when the remote had moved and the local commits were
    /// replayed onto it, so they are no longer the commits the gates verified.
    Ready { rebased: bool },
    /// Pushing must not happen; the local commit stays on the local branch.
    Refused {
        failure: GitSyncFailure,
        detail: String,
    },
}

/// Parse `git rev-list --left-right --count HEAD...origin/<branch>` output
/// (`"<ahead>\t<behind>"`).
pub(super) fn parse_divergence(stdout: &str) -> Option<Divergence> {
    let mut counts = stdout.split_whitespace().map(str::parse::<u32>);
    let ahead = counts.next()?.ok()?;
    let behind = counts.next()?.ok()?;
    if counts.next().is_some() {
        return None;
    }
    Some(Divergence { ahead, behind })
}

/// Decide the pre-work sync action for a divergence.
pub(super) fn plan_sync(divergence: Divergence) -> SyncPlan {
    match divergence {
        Divergence { behind: 0, .. } => SyncPlan::UpToDate,
        Divergence { ahead: 0, behind } => SyncPlan::FastForward(behind),
        Divergence { .. } => SyncPlan::Diverged,
    }
}

fn refused(failure: GitSyncFailure, detail: impl Into<String>) -> SyncOutcome {
    SyncOutcome::Refused {
        failure,
        detail: detail.into(),
    }
}

/// Measure `HEAD` against `remote_ref`. `None` when the ref cannot be resolved.
async fn measure_divergence(
    shell: &dyn ShellGateway,
    path: &Path,
    remote_ref: &str,
) -> anyhow::Result<Option<Divergence>> {
    let range = format!("HEAD...{remote_ref}");
    let result = shell
        .run(path, "git", &["rev-list", "--left-right", "--count", &range], None, None)
        .await?;
    Ok(if result.success {
        parse_divergence(&result.stdout)
    } else {
        None
    })
}

/// Bring the checkout at `path` up to `origin/<branch>` by fast-forward only.
///
/// Assumes the caller has already verified that `branch` is checked out.
///
/// 1. Refuses with [`GitSyncFailure::DirtyTree`] when `git status --porcelain`
///    is non-empty.
/// 2. Runs `git fetch origin <branch>` (skipped under `dry_run`); a failed
///    fetch refuses with [`GitSyncFailure::RemoteUnavailable`].
/// 3. Refuses with [`GitSyncFailure::Diverged`] when local and remote have
///    both moved.
/// 4. Otherwise runs `git merge --ff-only origin/<branch>` (skipped under
///    `dry_run`) and reports how many commits were fast-forwarded.
///
/// Under `dry_run` nothing is mutated — not even remote-tracking refs — so
/// the reported count is measured against the last-fetched `origin/<branch>`.
///
/// Every refusal leaves the checkout exactly as found. Spawn-level failures
/// and a failing `git status` propagate as `Err`.
pub(super) async fn sync_checkout(
    shell: &dyn ShellGateway,
    path: &Path,
    branch: &str,
    dry_run: bool,
) -> anyhow::Result<SyncOutcome> {
    let status = shell.run(path, "git", &["status", "--porcelain"], None, None).await?;
    if !status.success {
        anyhow::bail!("git status failed: {}", status.stderr.trim());
    }
    if !status.stdout.trim().is_empty() {
        return Ok(refused(
            GitSyncFailure::DirtyTree,
            "working tree has uncommitted changes; refusing to sync or work on it",
        ));
    }

    if !dry_run {
        let fetch = shell.run(path, "git", &["fetch", "origin", branch], None, None).await?;
        if !fetch.success {
            return Ok(refused(
                GitSyncFailure::RemoteUnavailable,
                format!("git fetch origin {branch} failed: {}", fetch.stderr.trim()),
            ));
        }
    }

    let remote_ref = format!("origin/{branch}");
    let Some(divergence) = measure_divergence(shell, path, &remote_ref).await? else {
        return Ok(refused(
            GitSyncFailure::RemoteUnavailable,
            format!("could not resolve {remote_ref}"),
        ));
    };

    match plan_sync(divergence) {
        SyncPlan::UpToDate => Ok(SyncOutcome::Synced { fast_forwarded: 0 }),
        SyncPlan::Diverged => Ok(refused(
            GitSyncFailure::Diverged,
            format!(
                "{branch} is {} commit(s) ahead of and {} commit(s) behind {remote_ref}; \
                 fast-forward impossible",
                divergence.ahead, divergence.behind
            ),
        )),
        SyncPlan::FastForward(count) if dry_run => Ok(SyncOutcome::Synced {
            fast_forwarded: count,
        }),
        SyncPlan::FastForward(count) => {
            let merge =
                shell.run(path, "git", &["merge", "--ff-only", &remote_ref], None, None).await?;
            if merge.success {
                Ok(SyncOutcome::Synced {
                    fast_forwarded: count,
                })
            } else {
                // `--ff-only` refuses atomically, so the checkout is untouched.
                Ok(refused(
                    GitSyncFailure::Diverged,
                    format!("git merge --ff-only {remote_ref} failed: {}", merge.stderr.trim()),
                ))
            }
        }
    }
}

/// How many commits local `HEAD` has that `origin/<branch>` does not, measured
/// against the last-fetched remote-tracking ref (no network). `None` when the
/// ref cannot be resolved.
pub(super) async fn commits_ahead(
    shell: &dyn ShellGateway,
    path: &Path,
    branch: &str,
) -> anyhow::Result<Option<u32>> {
    let remote_ref = format!("origin/{branch}");
    Ok(measure_divergence(shell, path, &remote_ref).await?.map(|d| d.ahead))
}

/// Make the local branch contain the current remote tip before pushing.
///
/// Runs `git fetch origin <branch>` then `git merge --ff-only origin/<branch>`
/// (a no-op when the remote has not moved). When the remote did move, the
/// local commit is replayed with `git rebase origin/<branch>`. A conflicting
/// rebase is aborted and refused with
/// [`GitSyncFailure::PushRejectedDiverged`], leaving the commit on the local
/// branch for a human. A failed fetch refuses with
/// [`GitSyncFailure::RemoteUnavailable`].
pub(super) async fn integrate_remote_before_push(
    shell: &dyn ShellGateway,
    path: &Path,
    project: &str,
    branch: &str,
) -> anyhow::Result<PrePushSync> {
    let fetch = shell.run(path, "git", &["fetch", "origin", branch], None, None).await?;
    if !fetch.success {
        return Ok(PrePushSync::Refused {
            failure: GitSyncFailure::RemoteUnavailable,
            detail: format!("git fetch origin {branch} failed: {}", fetch.stderr.trim()),
        });
    }

    let remote_ref = format!("origin/{branch}");
    let ff = shell.run(path, "git", &["merge", "--ff-only", &remote_ref], None, None).await?;
    if ff.success {
        return Ok(PrePushSync::Ready { rebased: false });
    }

    tracing::info!(%project, %remote_ref, "remote moved during run; rebasing local commit");
    let rebase = shell.run(path, "git", &["rebase", &remote_ref], None, None).await?;
    if rebase.success {
        return Ok(PrePushSync::Ready { rebased: true });
    }

    let abort = shell.run(path, "git", &["rebase", "--abort"], None, None).await?;
    if !abort.success {
        // Surfaced rather than absorbed: the refusal below already fails the
        // push, and this warning tells the human the checkout needs attention.
        tracing::warn!(%project, stderr = %abort.stderr.trim(), "git rebase --abort failed");
    }
    Ok(PrePushSync::Refused {
        failure: GitSyncFailure::PushRejectedDiverged,
        detail: format!("rebase onto {remote_ref} did not apply cleanly: {}", rebase.stderr.trim()),
    })
}

#[cfg(test)]
mod tests {
    use crate::gateway::fakes::FakeShellGateway;
    use crate::shell::CommandResult;

    use super::*;

    fn ok(stdout: &str) -> CommandResult {
        CommandResult {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    fn fail(stderr: &str) -> CommandResult {
        CommandResult {
            stdout: String::new(),
            stderr: stderr.to_string(),
            exit_code: 1,
            success: false,
        }
    }

    fn git_calls(shell: &FakeShellGateway) -> Vec<String> {
        shell.invocations().into_iter().map(|i| i.args.join(" ")).collect()
    }

    /// Git subcommands that change the checkout, refs, or index.
    const MUTATING: [&str; 5] = ["fetch", "merge", "rebase", "add", "commit"];

    fn assert_no_mutations(shell: &FakeShellGateway) {
        for call in git_calls(shell) {
            let sub = call.split_whitespace().next().unwrap_or_default();
            assert!(!MUTATING.contains(&sub), "unexpected mutating git call: git {call}");
        }
    }

    // -- pure core --

    #[test]
    fn parse_divergence_reads_ahead_and_behind() {
        assert_eq!(
            parse_divergence("2\t5\n"),
            Some(Divergence {
                ahead: 2,
                behind: 5
            })
        );
    }

    #[test]
    fn parse_divergence_rejects_malformed_output() {
        assert_eq!(parse_divergence(""), None);
        assert_eq!(parse_divergence("3"), None);
        assert_eq!(parse_divergence("a\tb"), None);
        assert_eq!(parse_divergence("1\t2\t3"), None);
    }

    #[test]
    fn plan_sync_classifies_every_position() {
        assert_eq!(
            plan_sync(Divergence {
                ahead: 0,
                behind: 0
            }),
            SyncPlan::UpToDate
        );
        assert_eq!(
            plan_sync(Divergence {
                ahead: 3,
                behind: 0
            }),
            SyncPlan::UpToDate
        );
        assert_eq!(
            plan_sync(Divergence {
                ahead: 0,
                behind: 7
            }),
            SyncPlan::FastForward(7)
        );
        assert_eq!(
            plan_sync(Divergence {
                ahead: 1,
                behind: 1
            }),
            SyncPlan::Diverged
        );
    }

    // -- sync_checkout (scripted shell) --

    #[tokio::test]
    async fn clean_and_behind_fast_forwards_n_commits() {
        let shell = FakeShellGateway::sequence(vec![
            ok(""),         // status --porcelain
            ok(""),         // fetch origin main
            ok("0\t4\n"),   // rev-list: 0 ahead, 4 behind
            ok("Updating"), // merge --ff-only
        ]);
        let outcome = sync_checkout(shell.as_ref(), Path::new("/p"), "main", false).await.unwrap();

        assert_eq!(outcome, SyncOutcome::Synced { fast_forwarded: 4 });
        assert_eq!(
            git_calls(&shell),
            [
                "status --porcelain",
                "fetch origin main",
                "rev-list --left-right --count HEAD...origin/main",
                "merge --ff-only origin/main",
            ]
        );
    }

    #[tokio::test]
    async fn clean_and_level_does_not_merge() {
        let shell = FakeShellGateway::sequence(vec![ok(""), ok(""), ok("0\t0\n")]);
        let outcome = sync_checkout(shell.as_ref(), Path::new("/p"), "main", false).await.unwrap();

        assert_eq!(outcome, SyncOutcome::Synced { fast_forwarded: 0 });
        assert!(!git_calls(&shell).iter().any(|c| c.starts_with("merge")));
    }

    #[tokio::test]
    async fn dirty_tree_is_refused_without_mutations() {
        let shell = FakeShellGateway::sequence(vec![ok(" M src/lib.rs\n")]);
        let outcome = sync_checkout(shell.as_ref(), Path::new("/p"), "main", false).await.unwrap();

        assert!(matches!(
            outcome,
            SyncOutcome::Refused {
                failure: GitSyncFailure::DirtyTree,
                ..
            }
        ));
        assert_eq!(git_calls(&shell), ["status --porcelain"]);
        assert_no_mutations(&shell);
    }

    #[tokio::test]
    async fn diverged_is_refused_without_touching_the_checkout() {
        let shell = FakeShellGateway::sequence(vec![ok(""), ok(""), ok("2\t3\n")]);
        let outcome = sync_checkout(shell.as_ref(), Path::new("/p"), "main", false).await.unwrap();

        let SyncOutcome::Refused { failure, detail } = outcome else {
            panic!("expected refusal, got {outcome:?}");
        };
        assert_eq!(failure, GitSyncFailure::Diverged);
        assert!(detail.contains("2 commit(s) ahead"), "{detail}");
        // Only the fetch touched remote-tracking refs; no merge, no rebase.
        let calls = git_calls(&shell);
        assert!(!calls.iter().any(|c| c.starts_with("merge") || c.starts_with("rebase")));
    }

    #[tokio::test]
    async fn failed_ff_merge_is_refused_as_diverged() {
        let shell = FakeShellGateway::sequence(vec![
            ok(""),
            ok(""),
            ok("0\t1\n"),
            fail("fatal: Not possible to fast-forward, aborting."),
        ]);
        let outcome = sync_checkout(shell.as_ref(), Path::new("/p"), "main", false).await.unwrap();

        assert!(matches!(
            outcome,
            SyncOutcome::Refused {
                failure: GitSyncFailure::Diverged,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn failed_fetch_is_refused_as_remote_unavailable() {
        let shell = FakeShellGateway::sequence(vec![ok(""), fail("could not read from remote")]);
        let outcome = sync_checkout(shell.as_ref(), Path::new("/p"), "main", false).await.unwrap();

        assert!(matches!(
            outcome,
            SyncOutcome::Refused {
                failure: GitSyncFailure::RemoteUnavailable,
                ..
            }
        ));
        assert_eq!(git_calls(&shell).len(), 2);
    }

    #[tokio::test]
    async fn unresolvable_remote_ref_is_refused_as_remote_unavailable() {
        let shell = FakeShellGateway::sequence(vec![ok(""), ok(""), fail("unknown revision")]);
        let outcome = sync_checkout(shell.as_ref(), Path::new("/p"), "main", false).await.unwrap();

        assert!(matches!(
            outcome,
            SyncOutcome::Refused {
                failure: GitSyncFailure::RemoteUnavailable,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn failing_status_propagates() {
        let shell = FakeShellGateway::sequence(vec![fail("not a git repository")]);
        let err = sync_checkout(shell.as_ref(), Path::new("/p"), "main", false).await.unwrap_err();
        assert!(err.to_string().contains("git status failed"));
    }

    #[tokio::test]
    async fn dry_run_reports_would_fast_forward_without_mutations() {
        let shell = FakeShellGateway::sequence(vec![ok(""), ok("0\t6\n")]);
        let outcome = sync_checkout(shell.as_ref(), Path::new("/p"), "main", true).await.unwrap();

        assert_eq!(outcome, SyncOutcome::Synced { fast_forwarded: 6 });
        assert_eq!(
            git_calls(&shell),
            [
                "status --porcelain",
                "rev-list --left-right --count HEAD...origin/main"
            ]
        );
        assert_no_mutations(&shell);
    }

    #[tokio::test]
    async fn dry_run_still_reports_dirty_tree() {
        let shell = FakeShellGateway::sequence(vec![ok("?? new.txt\n")]);
        let outcome = sync_checkout(shell.as_ref(), Path::new("/p"), "main", true).await.unwrap();

        assert!(matches!(
            outcome,
            SyncOutcome::Refused {
                failure: GitSyncFailure::DirtyTree,
                ..
            }
        ));
        assert_no_mutations(&shell);
    }

    // -- integrate_remote_before_push (scripted shell) --

    #[tokio::test]
    async fn pre_push_remote_unmoved_is_ready_without_rebase() {
        let shell = FakeShellGateway::sequence(vec![ok(""), ok("Already up to date.")]);
        let outcome = integrate_remote_before_push(shell.as_ref(), Path::new("/p"), "proj", "main")
            .await
            .unwrap();

        assert_eq!(outcome, PrePushSync::Ready { rebased: false });
        assert_eq!(git_calls(&shell), ["fetch origin main", "merge --ff-only origin/main"]);
    }

    #[tokio::test]
    async fn pre_push_remote_moved_and_rebase_clean_is_ready() {
        let shell = FakeShellGateway::sequence(vec![
            ok(""),
            fail("fatal: Not possible to fast-forward, aborting."),
            ok("Successfully rebased"),
        ]);
        let outcome = integrate_remote_before_push(shell.as_ref(), Path::new("/p"), "proj", "main")
            .await
            .unwrap();

        assert_eq!(outcome, PrePushSync::Ready { rebased: true });
        assert_eq!(
            git_calls(&shell),
            [
                "fetch origin main",
                "merge --ff-only origin/main",
                "rebase origin/main"
            ]
        );
    }

    #[tokio::test]
    async fn pre_push_rebase_conflict_aborts_and_refuses() {
        let shell = FakeShellGateway::sequence(vec![
            ok(""),
            fail("fatal: Not possible to fast-forward, aborting."),
            fail("CONFLICT (content): Merge conflict in src/lib.rs"),
            ok(""),
        ]);
        let outcome = integrate_remote_before_push(shell.as_ref(), Path::new("/p"), "proj", "main")
            .await
            .unwrap();

        let PrePushSync::Refused { failure, detail } = outcome else {
            panic!("expected refusal, got {outcome:?}");
        };
        assert_eq!(failure, GitSyncFailure::PushRejectedDiverged);
        assert!(detail.contains("CONFLICT"), "{detail}");
        assert_eq!(git_calls(&shell).last().map(String::as_str), Some("rebase --abort"));
    }

    #[tokio::test]
    async fn pre_push_failed_fetch_refuses_as_remote_unavailable() {
        let shell = FakeShellGateway::sequence(vec![fail("network down")]);
        let outcome = integrate_remote_before_push(shell.as_ref(), Path::new("/p"), "proj", "main")
            .await
            .unwrap();

        assert!(matches!(
            outcome,
            PrePushSync::Refused {
                failure: GitSyncFailure::RemoteUnavailable,
                ..
            }
        ));
        assert_eq!(git_calls(&shell), ["fetch origin main"]);
    }
}
