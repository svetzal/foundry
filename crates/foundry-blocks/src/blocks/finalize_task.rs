use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gates::{GateDefinition, GateResult};
use foundry_sdk::payload::{
    LandBlocked, LoopContext, TaskReviewedPayload, TaskRunCompletedPayload, TaskVerdict,
    TrunkArrival, WorkspaceBase,
};
use foundry_sdk::registry::{ProjectEntry, Registry};
use foundry_sdk::task_block::{BlockKind, TaskBlock};

use crate::gateway::{ProcessShellGateway, ShellGateway};
use crate::git_hook_timeout::{GitCommandTimedOut, GitHookTimeout};
use crate::workspace::{
    PROOF_FILE, checked, checked_with_hook_timeout, clean_foundry_dir, commit_worktree,
    git_failure_detail, preserve, remove_workspace, run, run_best_effort, unlanded_foundry_files,
};

use super::SimulatedSuccess;

task_block_new! {
    /// Commits every task result before terminal state, pushes non-complete work
    /// to a durable preservation ref (or bundle), and fast-forwards complete
    /// work onto the project's trunk branch — rebasing it onto trunk and
    /// re-running the required gates first when trunk moved during the run.
    ///
    /// A landing that is still blocked never rewrites the reviewer's verdict:
    /// the result keeps it and records a typed `land_blocked` reason instead.
    pub struct FinalizeTask {
        shell: ShellGateway = ProcessShellGateway
    }
}

/// Keep the original proof and a portable copy whose log paths resolve inside
/// the archive. Invalid proof is still evidence of a failed task.
fn archive_proof(worktree: &Path, destination: &Path) -> Result<Option<String>> {
    let source = worktree.join(PROOF_FILE);
    let bytes = match std::fs::read(&source) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    std::fs::create_dir_all(destination)?;
    std::fs::write(destination.join("original-proof.json"), &bytes)?;
    let mut proof: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(proof) => proof,
        Err(error) => {
            // Best-effort: malformed evidence already yields a review defect;
            // retain its exact bytes rather than blocking preservation.
            tracing::warn!(%error, "archiving malformed early proof without interpreting logs");
            std::fs::write(destination.join("proof.json"), bytes)?;
            return Ok(Some(destination.to_string_lossy().into_owned()));
        }
    };
    for field in ["rejecting", "corrected"] {
        if let Some(log) = proof[field]["log"].as_str() {
            let source = worktree.join(log);
            match std::fs::read(&source) {
                Ok(bytes) => {
                    let name = format!("{field}.log");
                    std::fs::write(destination.join(&name), bytes)?;
                    proof[field]["log"] = serde_json::Value::String(name);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // Best-effort: absent logs are part of the review defect;
                    // preserve the proof without inventing their contents.
                    tracing::warn!(%error, path = %source.display(), "early proof log is absent during archival");
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    std::fs::write(destination.join("proof.json"), serde_json::to_vec_pretty(&proof)?)?;
    Ok(Some(destination.to_string_lossy().into_owned()))
}

/// A fresh per-attempt evidence directory under the run's trace. Proof and
/// the unlanded-file mirror of one finalization share it.
fn attempt_evidence_dir(trace_id: &str) -> PathBuf {
    foundry_sdk::paths::foundry_home()
        .join("evidence")
        .join(trace_id)
        .join(uuid::Uuid::new_v4().to_string())
}

/// Subdirectory of a per-attempt evidence directory that mirrors the files
/// under `.foundry` that did not land, at their repository-relative paths.
const UNCOMMITTED_ARCHIVE_DIR: &str = "worktree";

/// Copy `files` (repository-relative) from `worktree` into `destination`,
/// keeping their relative paths. Returns `destination` when anything was
/// copied.
///
/// This is the raw record of what cleanup removes or restores, proof included,
/// so nothing under `.foundry` disappears unrecorded. `archive_proof`
/// separately keeps the interpreted, portable copy of the proof.
fn archive_uncommitted(
    worktree: &Path,
    files: &[String],
    destination: &Path,
) -> Result<Option<String>> {
    if files.is_empty() {
        return Ok(None);
    }
    for relative in files {
        let source = worktree.join(relative);
        let target = destination.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        copy_preserving_symlink(&source, &target)?;
    }
    Ok(Some(destination.to_string_lossy().into_owned()))
}

/// Copy a file, recreating a symlink as a symlink rather than following it,
/// so a link to a directory or to something outside the worktree is recorded
/// as what it was.
fn copy_preserving_symlink(source: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    if std::fs::symlink_metadata(source)?.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(source)?, target)?;
        return Ok(());
    }
    std::fs::copy(source, target)?;
    Ok(())
}

async fn branch_has_deliverable(
    shell: &dyn ShellGateway,
    checkout: &Path,
    base_branch: &str,
    task_branch: &str,
) -> Result<bool> {
    let count = checked(
        shell,
        checkout,
        &[
            "rev-list",
            "--count",
            &format!("{base_branch}..{task_branch}"),
        ],
    )
    .await?;
    Ok(count != "0")
}

/// Why one landing attempt did not fast-forward trunk.
enum LandAttemptError {
    /// Trunk is no longer an ancestor of the task branch: it moved while the
    /// task ran. Recoverable by rebasing.
    TrunkMoved,
    /// Anything else. Not retried.
    Blocked(LandBlocked, String),
}

/// The typed reason for a git error during landing: a hook-running command
/// still running at its derived limit is [`LandBlocked::GitHookTimedOut`],
/// anything else [`LandBlocked::GitFailed`].
fn git_land_blocked(error: &anyhow::Error) -> LandBlocked {
    if error.downcast_ref::<GitCommandTimedOut>().is_some() {
        LandBlocked::GitHookTimedOut
    } else {
        LandBlocked::GitFailed
    }
}

/// One attempt to fast-forward trunk onto the task branch.
///
/// Trunk movement is detected before the merge, so a moved trunk leaves the
/// registered checkout exactly as the pull left it.
///
/// The pull, merge and push can run the repository's hooks, so each runs
/// under the limit derived from the required gates of the tree it runs in
/// ([`GitHookTimeout::for_tree`]). After the fast-forward merge the checkout
/// holds the task's tree, so the push's limit comes from the task's gates.
async fn land_on_trunk(
    shell: &dyn ShellGateway,
    checkout: &Path,
    base_branch: &str,
    task_branch: &str,
    push_enabled: bool,
) -> Result<(), LandAttemptError> {
    let git_failed = |error: anyhow::Error| {
        LandAttemptError::Blocked(git_land_blocked(&error), error.to_string())
    };
    let dirty = checked(shell, checkout, &["status", "--porcelain"]).await.map_err(git_failed)?;
    if !dirty.is_empty() {
        return Err(LandAttemptError::Blocked(
            LandBlocked::CheckoutNotReady,
            "registered checkout is dirty; preserved task branch instead of risking user work"
                .to_string(),
        ));
    }
    let current = checked(shell, checkout, &["branch", "--show-current"])
        .await
        .map_err(git_failed)?;
    if current != base_branch {
        return Err(LandAttemptError::Blocked(
            LandBlocked::CheckoutNotReady,
            format!("registered checkout is on '{current}', expected '{base_branch}'"),
        ));
    }
    if push_enabled {
        checked_with_hook_timeout(
            shell,
            checkout,
            &["pull", "--ff-only", "origin", base_branch],
            &GitHookTimeout::for_tree(checkout),
        )
        .await
        .map_err(git_failed)?;
    }
    let ancestry = run(shell, checkout, &["merge-base", "--is-ancestor", base_branch, task_branch])
        .await
        .map_err(git_failed)?;
    match ancestry.exit_code {
        0 => {}
        1 => return Err(LandAttemptError::TrunkMoved),
        code => {
            return Err(LandAttemptError::Blocked(
                LandBlocked::GitFailed,
                format!(
                    "git merge-base --is-ancestor failed ({code}): {}",
                    git_failure_detail(&ancestry.stderr)
                ),
            ));
        }
    }
    checked_with_hook_timeout(
        shell,
        checkout,
        &["merge", "--ff-only", task_branch],
        &GitHookTimeout::for_tree(checkout),
    )
    .await
    .map_err(git_failed)?;
    if push_enabled {
        checked_with_hook_timeout(
            shell,
            checkout,
            &["push", "origin", base_branch],
            &GitHookTimeout::for_tree(checkout),
        )
        .await
        .map_err(git_failed)?;
    }
    Ok(())
}

/// Most rebases one finalize may attempt: the first after trunk is found to
/// have moved, and one more only if it moved again during the gate re-run.
const MAX_REBASES: usize = 2;

/// What happened when finalize tried to put landing-eligible work on trunk.
struct Landing {
    /// `None` when the work landed.
    blocked: Option<(LandBlocked, String)>,
    /// Trunk commits the task branch lacked, oldest first.
    arrivals: Vec<TrunkArrival>,
    /// Why a rebase was needed, when one was.
    drift: Option<StartDrift>,
}

/// Why the task branch lacked trunk when finalize came to land it, read from
/// the start the run recorded ([`WorkspaceBase`]).
///
/// Reporting every rebase as "trunk advanced during the run" sent operators and
/// campaigns after a race that never happened when the run had simply started
/// from a stale continuation base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartDrift {
    /// The run started on the trunk it observed, so trunk moved after it
    /// started.
    MovedDuringRun,
    /// The run started this many commits behind the trunk it observed.
    BehindAtStart(u64),
    /// The run recorded no start, so the cause cannot be told.
    Unrecorded,
}

impl StartDrift {
    async fn of(
        shell: &dyn ShellGateway,
        checkout: &Path,
        start: Option<&WorkspaceBase>,
    ) -> Result<Self> {
        let Some(start) = start else {
            return Ok(Self::Unrecorded);
        };
        let ancestry =
            run(shell, checkout, &["merge-base", "--is-ancestor", &start.trunk, &start.commit])
                .await?;
        match ancestry.exit_code {
            0 => Ok(Self::MovedDuringRun),
            1 => {
                let range = format!("{}..{}", start.commit, start.trunk);
                let behind = checked(shell, checkout, &["rev-list", "--count", &range]).await?;
                Ok(Self::BehindAtStart(behind.parse()?))
            }
            code => anyhow::bail!(
                "git merge-base --is-ancestor failed ({code}): {}",
                git_failure_detail(&ancestry.stderr)
            ),
        }
    }

    /// The typed reason and its wording for a rebase that conflicted.
    fn conflict(self) -> (LandBlocked, String) {
        match self {
            Self::MovedDuringRun => (
                LandBlocked::TrunkMovedConflict,
                "trunk advanced during the run and rebasing onto it conflicted".to_string(),
            ),
            Self::BehindAtStart(behind) => (
                LandBlocked::StaleBaseConflict,
                format!(
                    "the run started {behind} commit(s) behind trunk and rebasing onto trunk conflicted"
                ),
            ),
            Self::Unrecorded => (
                LandBlocked::TrunkMovedConflict,
                "trunk is ahead of the task branch and rebasing onto it conflicted (the run \
                 recorded no start, so whether trunk moved during the run is unknown)"
                    .to_string(),
            ),
        }
    }

    /// The typed reason and its wording for a clean rebase whose required
    /// gates then failed.
    fn gates_failed(self) -> (LandBlocked, String) {
        match self {
            Self::MovedDuringRun => {
                (LandBlocked::TrunkMovedGatesFailed, "trunk advanced during the run".to_string())
            }
            Self::BehindAtStart(behind) => (
                LandBlocked::StaleBaseGatesFailed,
                format!("the run started {behind} commit(s) behind trunk"),
            ),
            Self::Unrecorded => (
                LandBlocked::TrunkMovedGatesFailed,
                "trunk is ahead of the task branch (the run recorded no start, so whether \
                 trunk moved during the run is unknown)"
                    .to_string(),
            ),
        }
    }

    /// How the landed summary describes the trunk commits rebased onto.
    fn arrivals(self) -> &'static str {
        match self {
            Self::MovedDuringRun => "that arrived during the run",
            Self::BehindAtStart(_) => "that the run's starting base lacked",
            Self::Unrecorded => "that trunk held beyond the task branch",
        }
    }
}

/// Land the task branch, rebasing it onto trunk when trunk moved during the
/// run.
///
/// A moved trunk is ordinary on a shared repository and says nothing about
/// the work. The branch is rebased inside the task worktree, the required
/// gates are re-run on the rebased tree, and landing is retried. A conflicting
/// rebase or a red required gate stops there and the reviewed branch is
/// restored untouched, so what is preserved is exactly what was reviewed.
async fn land_with_rebase(
    shell: &dyn ShellGateway,
    checkout: &Path,
    worktree: &Path,
    base_branch: &str,
    task_branch: &str,
    push_enabled: bool,
    start: Option<&WorkspaceBase>,
) -> Landing {
    let mut arrivals = Vec::new();
    let mut reviewed_head: Option<String> = None;
    let mut drift: Option<StartDrift> = None;
    let mut rebases = 0;
    let blocked = loop {
        let blocked =
            match land_on_trunk(shell, checkout, base_branch, task_branch, push_enabled).await {
                Ok(()) => break None,
                Err(LandAttemptError::Blocked(reason, detail)) => (reason, detail),
                Err(LandAttemptError::TrunkMoved) => {
                    match trunk_arrivals(shell, checkout, base_branch, task_branch).await {
                        Ok(arrived) => arrivals.extend(arrived),
                        Err(error) => break Some((LandBlocked::GitFailed, error.to_string())),
                    }
                    if rebases == MAX_REBASES {
                        break Some((
                            LandBlocked::TrunkMovedRepeatedly,
                            format!("trunk moved again after {MAX_REBASES} rebases"),
                        ));
                    }
                    rebases += 1;
                    if reviewed_head.is_none() {
                        match checked(shell, worktree, &["rev-parse", "HEAD"]).await {
                            Ok(head) => reviewed_head = Some(head),
                            Err(error) => break Some((LandBlocked::GitFailed, error.to_string())),
                        }
                    }
                    // Only the first rebase can be owed to the start; trunk
                    // moving again after it is movement during the run.
                    let cause = match drift {
                        Some(StartDrift::Unrecorded) => StartDrift::Unrecorded,
                        Some(_) => StartDrift::MovedDuringRun,
                        None => match StartDrift::of(shell, checkout, start).await {
                            Ok(cause) => cause,
                            Err(error) => break Some((LandBlocked::GitFailed, error.to_string())),
                        },
                    };
                    drift.get_or_insert(cause);
                    match rebase_and_verify(shell, worktree, base_branch, cause).await {
                        Ok(()) => continue,
                        Err(blocked) => blocked,
                    }
                }
            };
        break Some(blocked);
    };
    if blocked.is_some()
        && let Some(head) = reviewed_head
    {
        restore_reviewed_head(shell, worktree, &head).await;
    }
    Landing {
        blocked,
        arrivals,
        drift,
    }
}

/// Commits on trunk that the task branch does not contain, oldest first.
async fn trunk_arrivals(
    shell: &dyn ShellGateway,
    checkout: &Path,
    base_branch: &str,
    task_branch: &str,
) -> Result<Vec<TrunkArrival>> {
    let log = checked(
        shell,
        checkout,
        &[
            "log",
            "--reverse",
            "--format=%H%x09%s",
            &format!("{task_branch}..{base_branch}"),
        ],
    )
    .await?;
    Ok(log
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let (commit, subject) = line.split_once('\t').unwrap_or((line, ""));
            TrunkArrival {
                commit: commit.to_string(),
                subject: subject.to_string(),
            }
        })
        .collect())
}

/// Rebase the task branch onto trunk inside its worktree, then re-run the
/// project's required gates on the rebased tree.
async fn rebase_and_verify(
    shell: &dyn ShellGateway,
    worktree: &Path,
    base_branch: &str,
    cause: StartDrift,
) -> Result<(), (LandBlocked, String)> {
    let rebase = run(shell, worktree, &["rebase", base_branch])
        .await
        .map_err(|error| (LandBlocked::GitFailed, error.to_string()))?;
    if !rebase.success {
        // Best-effort: the rebase already failed and the caller restores the
        // reviewed head; an abort that fails leaves nothing that restore does
        // not also reset.
        run_best_effort(shell, worktree, &["rebase", "--abort"]).await;
        let (reason, why) = cause.conflict();
        return Err((reason, format!("{why}: {}", git_failure_detail(&rebase.stderr))));
    }
    let failed = rerun_required_gates(shell, worktree, cause).await?;
    if failed.is_empty() {
        Ok(())
    } else {
        let (reason, why) = cause.gates_failed();
        Err((
            reason,
            format!(
                "{why}; the rebase was clean but required gate(s) failed on the rebased tree: {}",
                failed.join(", ")
            ),
        ))
    }
}

/// Re-run the required gates in `worktree`, returning the names of those that
/// failed, each with the paths of its full logs when they were written.
///
/// Gates are read from the rebased tree, so a gate definition trunk changed is
/// the one that runs. Fix commands are not run: a fix would edit the tree
/// after the commit being landed, so only a gate that passes as-is counts.
async fn rerun_required_gates(
    shell: &dyn ShellGateway,
    worktree: &Path,
    cause: StartDrift,
) -> Result<Vec<String>, (LandBlocked, String)> {
    let reason = cause.gates_failed().0;
    let gates: Vec<GateDefinition> = crate::gate_file::read_gates(worktree)
        .map_err(|error| (reason, format!("could not read the rebased tree's gates: {error}")))?
        .into_iter()
        .filter(|gate| gate.required)
        .map(|gate| GateDefinition {
            fix_command: None,
            ..gate
        })
        .collect();
    let run = crate::gate_runner::run_gates(&gates, worktree, shell)
        .await
        .map_err(|error| (reason, error.to_string()))?;
    Ok(run
        .results
        .into_iter()
        .filter(|gate| !gate.passed)
        .map(|gate| crate::gate_runner::describe_failed_gate(&gate))
        .collect())
}

/// Put the task branch back on the head the reviewer saw, so the preserved
/// branch matches what was already pushed.
async fn restore_reviewed_head(shell: &dyn ShellGateway, worktree: &Path, head: &str) {
    // Best-effort: the reviewed head is already durable on the preservation
    // ref pushed before landing; a failed local reset leaves only a rebased
    // local branch in a worktree that is about to be removed.
    run_best_effort(shell, worktree, &["reset", "--hard", head]).await;
}

fn enforce_gate_truth(payload: &TaskReviewedPayload) -> TaskVerdict {
    if payload.verdict.is_complete()
        && payload.gate_results.iter().any(|gate| gate.required && !gate.passed)
    {
        TaskVerdict::Defect {
            diagnosis: "reviewer returned complete while a required mechanical gate failed"
                .to_string(),
        }
    } else {
        payload.verdict.clone()
    }
}

/// Whether a reviewed task may be integrated into trunk.
///
/// `Complete` lands unconditionally, as it always has.
///
/// `Remainder` is the reviewer's term for a finite list of missing work on a
/// *converging* implementation — sound work that simply is not finished.
/// Withholding it accumulates a long-lived divergent branch, which is both a
/// trunk-based-development violation and the mechanism that strands whole
/// campaigns: nothing merges until some later cycle happens to return
/// `Complete`. It therefore lands too, but only against positive green
/// evidence — at least one required gate ran and every required gate passed —
/// so trunk is never made red by unfinished work. A `Remainder` with no
/// required gate to vouch for it stays preserved.
///
/// `Defect`, `BlockedOnDecision`, and `RunnerError` never land.
fn may_land(verdict: &TaskVerdict, gate_results: &[GateResult]) -> bool {
    match verdict {
        TaskVerdict::Complete => true,
        TaskVerdict::Remainder { .. } => {
            let mut required = gate_results.iter().filter(|gate| gate.required).peekable();
            required.peek().is_some() && required.all(|gate| gate.passed)
        }
        TaskVerdict::Defect { .. }
        | TaskVerdict::BlockedOnRecovery { .. }
        | TaskVerdict::BlockedOnDecision { .. }
        | TaskVerdict::RunnerError { .. } => false,
    }
}

fn task_location(context: &LoopContext) -> Result<(PathBuf, &str), String> {
    let worktree = context
        .task_worktree
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| "task finalization missing worktree".to_string())?;
    let branch = context
        .task_branch
        .as_deref()
        .ok_or_else(|| "task finalization missing branch".to_string())?;
    Ok((worktree, branch))
}

fn task_summary(
    verdict: &TaskVerdict,
    landed: bool,
    arrivals: usize,
    drift: Option<StartDrift>,
) -> String {
    let summary = match (landed, verdict) {
        (true, TaskVerdict::Remainder { gaps }) => {
            format!("converging task landed on trunk with {} gap(s) carried forward", gaps.len())
        }
        (true, _) => "task completed, reviewed, and landed".to_string(),
        (false, verdict) if verdict.is_complete() => {
            "task completed, reviewed, and required no landing".to_string()
        }
        (false, _) => "task stopped with a typed non-complete verdict; work preserved".to_string(),
    };
    if landed && arrivals > 0 {
        let which = drift.unwrap_or(StartDrift::MovedDuringRun).arrivals();
        format!("{summary} after rebasing onto {arrivals} trunk commit(s) {which}")
    } else {
        summary
    }
}

/// The summary for landing-eligible work kept off trunk. Names the reviewer's
/// verdict, which stands, and the typed reason landing was blocked.
fn blocked_summary(verdict: &TaskVerdict, reason: LandBlocked, detail: &str) -> String {
    format!(
        "{} work preserved but could not land on trunk ({reason}): {detail}",
        verdict.tag()
    )
}

fn run_completed(
    project: &str,
    landed: bool,
    summary: String,
    preservation_ref: Option<String>,
    verdict: TaskVerdict,
    context: LoopContext,
) -> TaskRunCompletedPayload {
    TaskRunCompletedPayload {
        project: project.to_string(),
        success: verdict.is_complete(),
        landed,
        summary,
        preservation_ref,
        land_blocked: None,
        proof_evidence: None,
        uncommitted_artifacts: None,
        trunk_arrivals: Vec::new(),
        verdict,
        context,
        failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
    }
}

/// A run that could not be finalized at all: nothing about the work is known.
fn runner_error(project: &str, detail: String, context: LoopContext) -> TaskRunCompletedPayload {
    run_completed(
        project,
        false,
        detail.clone(),
        None,
        TaskVerdict::RunnerError { detail },
        context,
    )
}

fn terminal_result(
    throttle: foundry_sdk::throttle::Throttle,
    result: &TaskRunCompletedPayload,
) -> anyhow::Result<foundry_sdk::task_block::TaskBlockResult> {
    super::emit_event_result(
        format!("{}: {}", result.project, result.summary),
        result.success,
        EventType::TaskRunCompleted,
        &result.project,
        throttle,
        result,
    )
}

async fn landed_commit_ref(shell: &dyn ShellGateway, checkout: &Path) -> Result<String> {
    checked(shell, checkout, &["rev-parse", "HEAD"]).await
}

/// What committing and preserving a task worktree produced.
struct Committed {
    /// The task branch carries commits trunk does not.
    deliverable: bool,
    /// Where non-landed work is durable: a branch, or `bundle:{path}`.
    preservation_ref: Option<String>,
    /// Where the unlanded `.foundry` files were archived, when there were any.
    uncommitted_artifacts: Option<String>,
}

async fn commit_and_preserve_if_needed(
    shell: &dyn ShellGateway,
    entry: &ProjectEntry,
    worktree: &Path,
    project: &str,
    branch: &str,
    verdict: &TaskVerdict,
    evidence: &Path,
) -> Result<Committed> {
    let _committed = commit_worktree(shell, worktree, project).await?;
    let checkout = Path::new(&entry.path);
    let deliverable = branch_has_deliverable(shell, checkout, &entry.branch, branch).await?;
    let preservation_ref = if deliverable || !verdict.is_complete() {
        Some(preserve(shell, worktree, project, branch).await?)
    } else {
        None
    };
    // Nothing under .foundry landed except deletions of tracked files. What
    // is left there (new and ignored files, tracked files the agent modified)
    // is about to be removed or restored so the worktree can be removed.
    // Archive it first; an archival failure stops here, before anything is
    // discarded.
    let unlanded = unlanded_foundry_files(shell, worktree).await?;
    let files = unlanded.all();
    let count = files.len();
    let source = worktree.to_path_buf();
    let destination = evidence.join(UNCOMMITTED_ARCHIVE_DIR);
    let uncommitted_artifacts =
        tokio::task::spawn_blocking(move || archive_uncommitted(&source, &files, &destination))
            .await??;
    if let Some(archive) = &uncommitted_artifacts {
        tracing::debug!(%project, count, %archive, "archived unlanded .foundry files before cleanup");
    }
    clean_foundry_dir(shell, worktree, &unlanded).await?;
    Ok(Committed {
        deliverable,
        preservation_ref,
        uncommitted_artifacts,
    })
}

impl SimulatedSuccess for FinalizeTask {
    type Outcome = TaskRunCompletedPayload;

    fn simulate(&self, trigger: &Event) -> TaskRunCompletedPayload {
        let payload = trigger.parse_payload::<TaskReviewedPayload>().unwrap_or_else(|error| {
            TaskReviewedPayload {
                project: trigger.project.clone(),
                objective: String::new(),
                review: String::new(),
                gate_results: vec![],
                verdict: TaskVerdict::RunnerError {
                    detail: error.to_string(),
                },
                context: LoopContext::default(),
                failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
            }
        });
        let verdict = enforce_gate_truth(&payload);
        TaskRunCompletedPayload {
            project: trigger.project.clone(),
            success: verdict.is_complete(),
            landed: false,
            summary: "dry-run task finalization".to_string(),
            preservation_ref: None,
            land_blocked: None,
            proof_evidence: None,
            uncommitted_artifacts: None,
            trunk_arrivals: Vec::new(),
            verdict,
            context: payload.context,
            failure: payload.failure,
        }
    }

    fn success_events(&self, trigger: &Event, outcome: &TaskRunCompletedPayload) -> Vec<Event> {
        vec![super::event_from_infallible_payload(
            EventType::TaskRunCompleted,
            &trigger.project,
            trigger.throttle,
            outcome,
        )]
    }
}

impl TaskBlock for FinalizeTask {
    task_block_meta! {
        name: "Finalize Task",
        kind: Mutator,
        sinks_on: [TaskReviewed],
    }

    dry_run_via_simulation!();

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let payload = parse_payload!(trigger, TaskReviewedPayload);
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let trace_id = trigger.trace_id.clone().unwrap_or_else(|| trigger.id.clone());
        let registry = Arc::clone(&self.registry);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let entry = super::read_registry(&registry)?
                .find_project(&project)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("project '{project}' not found"))?;
            // The provider failure that ended execution rides through to the
            // run's result, so the campaign can tell a run that never started.
            let carried = payload.failure.clone();
            let finish = |mut result: TaskRunCompletedPayload| {
                result.failure = carried.clone();
                terminal_result(throttle, &result)
            };
            let mut context = payload.context.clone();
            let (worktree, branch) = match task_location(&context) {
                Ok(location) => location,
                Err(detail) => {
                    return finish(runner_error(&project, detail, context));
                }
            };
            let checkout = Path::new(&entry.path);

            // Archival must succeed before any commit or worktree cleanup.
            let evidence = attempt_evidence_dir(&trace_id);
            let (source, destination) = (worktree.clone(), evidence.clone());
            let proof_evidence =
                tokio::task::spawn_blocking(move || archive_proof(&source, &destination)).await??;

            let verdict = enforce_gate_truth(&payload);
            let Committed {
                deliverable,
                preservation_ref,
                uncommitted_artifacts,
            } = match commit_and_preserve_if_needed(
                &*shell, &entry, &worktree, &project, branch, &verdict, &evidence,
            )
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    let mut result = runner_error(&project, error.to_string(), context);
                    result.proof_evidence = proof_evidence;
                    return finish(result);
                }
            };

            let mut arrivals = Vec::new();
            let mut drift = None;
            let landed = if may_land(&verdict, &payload.gate_results) && deliverable {
                let landing = land_with_rebase(
                    &*shell,
                    checkout,
                    &worktree,
                    &entry.branch,
                    branch,
                    entry.actions.push,
                    context.workspace_base.as_ref(),
                )
                .await;
                arrivals = landing.arrivals;
                drift = landing.drift;
                if let Some((reason, detail)) = landing.blocked {
                    // FinalizeTask owns the task workspace it is finalizing.
                    remove_workspace(&*shell, checkout, &worktree).await;
                    let mut result = run_completed(
                        &project,
                        false,
                        blocked_summary(&verdict, reason, &detail),
                        preservation_ref,
                        verdict,
                        context,
                    );
                    // The reviewer's verdict stands; the run still did not
                    // deliver, so it is not a success until reconciled.
                    result.success = false;
                    result.land_blocked = Some(reason);
                    result.trunk_arrivals = arrivals;
                    result.proof_evidence = proof_evidence;
                    result.uncommitted_artifacts = uncommitted_artifacts;
                    return finish(result);
                }
                true
            } else {
                false
            };
            context.task_preservation_ref.clone_from(&preservation_ref);
            let preservation_ref = if landed {
                Some(landed_commit_ref(&*shell, checkout).await?)
            } else {
                preservation_ref
            };

            // Ref deletion belongs to durable ledger settlement, which checks
            // ownership and fresh trunk proof and records each cleanup result.
            // FinalizeTask owns the task workspace it is finalizing.
            remove_workspace(&*shell, checkout, &worktree).await;
            let summary = task_summary(&verdict, landed, arrivals.len(), drift);
            let mut result =
                run_completed(&project, landed, summary, preservation_ref, verdict, context);
            result.trunk_arrivals = arrivals;
            result.proof_evidence = proof_evidence;
            result.uncommitted_artifacts = uncommitted_artifacts;
            finish(result)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::pin::Pin;
    use std::process::Command;
    use std::time::Duration;

    use anyhow::Result;
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::gates::GateResult;
    use foundry_sdk::gateway::{CommandResult, ShellGateway};
    use foundry_sdk::payload::{LoopContext, TaskReviewedPayload, TaskVerdict, WorkspaceBase};
    use foundry_sdk::registry::{ActionFlags, ProjectEntry, Stack};
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use super::super::test_helpers::git_repo::{CleanProcessShellGateway, git};
    use super::{FinalizeTask, enforce_gate_truth, may_land};

    /// Delegates to `CleanProcessShellGateway` for every command except
    /// worktree cleanup commands, which it fails outright. Used to
    /// prove that a failed best-effort cleanup does not fail the task.
    struct CleanupFailsShellGateway;

    impl ShellGateway for CleanupFailsShellGateway {
        fn run<'a>(
            &'a self,
            working_dir: &'a Path,
            command: &'a str,
            args: &'a [&'a str],
            env: Option<&'a [(String, String)]>,
            timeout: Option<Duration>,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<CommandResult>> + Send + 'a>> {
            let is_cleanup = args.first() == Some(&"worktree") && args.get(1) == Some(&"remove");
            if is_cleanup {
                return Box::pin(async move {
                    Ok(CommandResult {
                        stdout: String::new(),
                        stderr: "simulated cleanup failure".to_string(),
                        exit_code: 1,
                        success: false,
                    })
                });
            }
            CleanProcessShellGateway.run(working_dir, command, args, env, timeout)
        }
    }

    #[tokio::test]
    async fn cleanup_failure_is_logged_but_does_not_fail_an_already_landed_task() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/cleanup-fails";
        let (checkout, worktree, branch_head) = repo_with_task_branch(dir.path(), branch);

        let registry = super::super::test_helpers::registry_with_entry(test_entry(&checkout));
        let block =
            FinalizeTask::with_gateways(registry, std::sync::Arc::new(CleanupFailsShellGateway));
        let trigger = task_trigger(&worktree, branch, TaskVerdict::Complete);

        let result = block.execute(&trigger).await.unwrap();

        assert!(
            result.success,
            "a failed best-effort cleanup must not fail an already-landed task"
        );
        assert_eq!(result.events[0].payload["landed"], true);
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), branch_head);
    }

    #[test]
    fn complete_verdict_cannot_override_failed_required_gate() {
        let payload = TaskReviewedPayload {
            project: "p".to_string(),
            objective: "do it".to_string(),
            review: String::new(),
            gate_results: vec![GateResult {
                name: "test".to_string(),
                command: "cargo test".to_string(),
                passed: false,
                required: true,
                output: "failed".to_string(),
                exit_code: 1,
                duration_ms: None,
                fix_applied: false,
                logs: Vec::new(),
            }],
            verdict: TaskVerdict::Complete,
            context: LoopContext::default(),
            failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
        };
        assert!(matches!(enforce_gate_truth(&payload), TaskVerdict::Defect { .. }));
    }

    fn task_trigger(worktree: &std::path::Path, branch: &str, verdict: TaskVerdict) -> Event {
        Event::new(
            EventType::TaskReviewed,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&TaskReviewedPayload {
                project: "p".to_string(),
                objective: "finish".to_string(),
                review: String::new(),
                gate_results: vec![],
                verdict,
                context: LoopContext {
                    task_worktree: Some(worktree.to_string_lossy().to_string()),
                    task_branch: Some(branch.to_string()),
                    ..LoopContext::default()
                },
                failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
            })
            .unwrap(),
        )
    }

    fn required_gate(passed: bool) -> GateResult {
        GateResult {
            name: "test".to_string(),
            command: "cargo test".to_string(),
            passed,
            required: true,
            output: String::new(),
            exit_code: i32::from(!passed),
            duration_ms: None,
            fix_applied: false,
            logs: Vec::new(),
        }
    }

    fn task_trigger_with_gates(
        worktree: &std::path::Path,
        branch: &str,
        verdict: TaskVerdict,
        gate_results: Vec<GateResult>,
    ) -> Event {
        Event::new(
            EventType::TaskReviewed,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&TaskReviewedPayload {
                project: "p".to_string(),
                objective: "finish".to_string(),
                review: String::new(),
                gate_results,
                verdict,
                context: LoopContext {
                    task_worktree: Some(worktree.to_string_lossy().to_string()),
                    task_branch: Some(branch.to_string()),
                    ..LoopContext::default()
                },
                failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
            })
            .unwrap(),
        )
    }

    /// Build a real checkout + origin + task worktree carrying one commit.
    fn repo_with_task_branch(
        dir: &std::path::Path,
        branch: &str,
    ) -> (std::path::PathBuf, std::path::PathBuf, String) {
        let remote = dir.join("remote.git");
        let remote_url = format!("file://{}", remote.display());
        let checkout = dir.join("checkout");
        let worktree = dir.join("worktree");
        git(dir, &["init", "--bare", remote.to_str().unwrap()]);
        git(dir, &["init", "-b", "main", checkout.to_str().unwrap()]);
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
                branch,
                worktree.to_str().unwrap(),
                "main",
            ],
        );
        std::fs::write(worktree.join("README.md"), "base\nconverging slice\n").unwrap();
        git(&worktree, &["add", "README.md"]);
        git(&worktree, &["commit", "-m", "converging slice"]);
        let head = git(&worktree, &["rev-parse", "HEAD"]);
        (checkout, worktree, head)
    }

    /// A converging implementation with green required gates integrates rather
    /// than accumulating a divergent branch.
    #[tokio::test]
    async fn converging_remainder_with_green_required_gates_lands_on_trunk() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/converging-test";
        let (checkout, worktree, branch_head) = repo_with_task_branch(dir.path(), branch);

        let registry = super::super::test_helpers::registry_with_entry(test_entry(&checkout));
        let block =
            FinalizeTask::with_gateways(registry, std::sync::Arc::new(CleanProcessShellGateway));
        let trigger = task_trigger_with_gates(
            &worktree,
            branch,
            TaskVerdict::Remainder {
                gaps: vec![
                    "queue-explain projection".to_string(),
                    "CHANGELOG".to_string(),
                ],
            },
            vec![required_gate(true)],
        );

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(result.events[0].payload["landed"], true);
        assert_eq!(
            result.events[0].payload["summary"],
            "converging task landed on trunk with 2 gap(s) carried forward"
        );
        // preservation_ref becomes the trunk commit, so the next campaign cycle
        // bases off trunk and sees no divergent accumulation.
        assert_eq!(result.events[0].payload["preservation_ref"], branch_head);
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), branch_head);
        assert!(
            std::fs::read_to_string(checkout.join("README.md"))
                .unwrap()
                .contains("converging slice"),
            "converging work did not reach trunk"
        );
        // The gaps must survive into the campaign's next formation prompt.
        assert_eq!(result.events[0].payload["gaps"][0], "queue-explain projection");
        assert!(!worktree.exists(), "merged worktree should be removed");
        assert!(
            !git(&checkout, &["ls-remote", "--heads", "origin", branch]).is_empty(),
            "origin ref stays until ledger settlement"
        );
    }

    /// Green-gate evidence is the whole safety mechanism: without it, unfinished
    /// work must not reach trunk.
    #[tokio::test]
    async fn remainder_with_a_failed_required_gate_stays_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/red-gate-test";
        let (checkout, worktree, _) = repo_with_task_branch(dir.path(), branch);
        let trunk_before = git(&checkout, &["rev-parse", "HEAD"]);

        let registry = super::super::test_helpers::registry_with_entry(test_entry(&checkout));
        let block =
            FinalizeTask::with_gateways(registry, std::sync::Arc::new(CleanProcessShellGateway));
        let trigger = task_trigger_with_gates(
            &worktree,
            branch,
            TaskVerdict::Remainder {
                gaps: vec!["still red".to_string()],
            },
            vec![required_gate(false)],
        );

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(result.events[0].payload["landed"], false);
        assert_eq!(result.events[0].payload["preservation_ref"], branch);
        assert_eq!(
            git(&checkout, &["rev-parse", "HEAD"]),
            trunk_before,
            "a red required gate must not move trunk"
        );
        assert!(
            !std::fs::read_to_string(checkout.join("README.md"))
                .unwrap()
                .contains("converging slice"),
            "unverified work leaked onto trunk"
        );
        assert!(
            !git(&checkout, &["ls-remote", "--heads", "origin", branch]).is_empty(),
            "unlanded work must stay preserved on its branch"
        );
    }

    #[test]
    fn landing_policy_admits_only_converging_work_with_green_required_evidence() {
        let remainder = TaskVerdict::Remainder {
            gaps: vec!["gap".to_string()],
        };
        assert!(may_land(&TaskVerdict::Complete, &[]));
        assert!(may_land(&remainder, &[required_gate(true)]));
        // No required gate ran: nothing vouches for the unfinished work.
        assert!(!may_land(&remainder, &[]));
        assert!(!may_land(&remainder, &[required_gate(false)]));
        assert!(!may_land(&remainder, &[required_gate(true), required_gate(false)]));
        assert!(!may_land(
            &TaskVerdict::Defect {
                diagnosis: "bad".to_string()
            },
            &[required_gate(true)]
        ));
        assert!(!may_land(
            &TaskVerdict::BlockedOnDecision {
                finding: "f".to_string(),
                options: vec![]
            },
            &[required_gate(true)]
        ));
        assert!(!may_land(
            &TaskVerdict::RunnerError {
                detail: "d".to_string()
            },
            &[required_gate(true)]
        ));
    }

    fn test_entry(path: &std::path::Path) -> ProjectEntry {
        ProjectEntry {
            name: "p".to_string(),
            path: path.to_string_lossy().to_string(),
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
            audit_exceptions: Vec::new(),
            update_policy: None,
        }
    }

    #[tokio::test]
    async fn noncomplete_task_commits_and_pushes_before_removing_worktree() {
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
                "foundry-task/preserve-test",
                worktree.to_str().unwrap(),
                "main",
            ],
        );
        std::fs::write(worktree.join("remainder.txt"), "valuable work\n").unwrap();

        let registry = super::super::test_helpers::registry_with_entry(test_entry(&checkout));
        let block =
            FinalizeTask::with_gateways(registry, std::sync::Arc::new(CleanProcessShellGateway));
        let trigger = task_trigger(
            &worktree,
            "foundry-task/preserve-test",
            TaskVerdict::Remainder {
                gaps: vec!["one gap".to_string()],
            },
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events[0].event_type, EventType::TaskRunCompleted);
        assert_eq!(result.events[0].payload["preservation_ref"], "foundry-task/preserve-test");
        assert_eq!(result.events[0].payload["landed"], false);
        assert!(!worktree.exists(), "disposable worktree should be removed after durable push");
        let refs = git(
            &checkout,
            &[
                "ls-remote",
                "--heads",
                "origin",
                "foundry-task/preserve-test",
            ],
        );
        assert!(!refs.is_empty(), "preservation branch was not pushed");
        assert!(!checkout.join("remainder.txt").exists(), "non-complete work leaked onto main");
    }

    #[tokio::test]
    async fn complete_task_lands_clean_branch_that_is_ahead_of_trunk() {
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
                "foundry-task/landed-test",
                worktree.to_str().unwrap(),
                "main",
            ],
        );
        std::fs::write(worktree.join("README.md"), "base\nbranch change\n").unwrap();
        git(&worktree, &["add", "README.md"]);
        git(&worktree, &["commit", "-m", "branch commit"]);
        let branch_head = git(&worktree, &["rev-parse", "HEAD"]);

        let registry = super::super::test_helpers::registry_with_entry(test_entry(&checkout));
        let block =
            FinalizeTask::with_gateways(registry, std::sync::Arc::new(CleanProcessShellGateway));
        let trigger = task_trigger(&worktree, "foundry-task/landed-test", TaskVerdict::Complete);

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["landed"], true);
        assert_eq!(result.events[0].payload["summary"], "task completed, reviewed, and landed");
        assert_eq!(result.events[0].payload["preservation_ref"], branch_head);
        assert!(!worktree.exists(), "landed worktree should be removed");
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), branch_head);
        assert!(
            !git(&checkout, &["ls-remote", "--heads", "origin", "foundry-task/landed-test"])
                .is_empty(),
            "origin ref stays until ledger settlement"
        );
        assert!(checkout.join("README.md").exists());
        assert!(
            std::fs::read_to_string(checkout.join("README.md"))
                .unwrap()
                .contains("branch change")
        );
    }

    #[tokio::test]
    async fn complete_task_with_no_deliverable_reports_no_landing() {
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
                "foundry-task/noop-test",
                worktree.to_str().unwrap(),
                "main",
            ],
        );

        let head_before = git(&checkout, &["rev-parse", "HEAD"]);
        let registry = super::super::test_helpers::registry_with_entry(test_entry(&checkout));
        let block =
            FinalizeTask::with_gateways(registry, std::sync::Arc::new(CleanProcessShellGateway));
        let trigger = task_trigger(&worktree, "foundry-task/noop-test", TaskVerdict::Complete);

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["landed"], false);
        assert_eq!(
            result.events[0].payload["summary"],
            "task completed, reviewed, and required no landing"
        );
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), head_before);
        assert!(!worktree.exists(), "no-op worktree should still be removed");
    }

    #[tokio::test]
    async fn complete_task_that_cannot_land_is_preserved_and_reported_truthfully() {
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
                "foundry-task/fail-land-test",
                worktree.to_str().unwrap(),
                "main",
            ],
        );
        std::fs::write(worktree.join("README.md"), "base\nbranch change\n").unwrap();
        git(&worktree, &["add", "README.md"]);
        git(&worktree, &["commit", "-m", "branch commit"]);
        std::fs::write(checkout.join("unrelated.txt"), "dirty\n").unwrap();

        let registry = super::super::test_helpers::registry_with_entry(test_entry(&checkout));
        let block =
            FinalizeTask::with_gateways(registry, std::sync::Arc::new(CleanProcessShellGateway));
        let trigger = task_trigger(&worktree, "foundry-task/fail-land-test", TaskVerdict::Complete);

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        let payload = &result.events[0].payload;
        assert_eq!(payload["landed"], false);
        assert_eq!(
            payload["summary"],
            "complete work preserved but could not land on trunk (checkout_not_ready): \
             registered checkout is dirty; preserved task branch instead of risking user work"
        );
        assert_eq!(
            payload["verdict"], "complete",
            "a blocked landing must not rewrite the verdict"
        );
        assert_eq!(payload["land_blocked"], "checkout_not_ready");
        assert_eq!(result.events[0].payload["preservation_ref"], "foundry-task/fail-land-test");
        assert!(!worktree.exists(), "failed landing should still remove the disposable worktree");
        assert!(
            git(
                &checkout,
                &[
                    "ls-remote",
                    "--heads",
                    "origin",
                    "foundry-task/fail-land-test"
                ]
            )
            .contains("foundry-task/fail-land-test")
        );
        assert_eq!(git(&checkout, &["rev-parse", "HEAD"]), git(&checkout, &["rev-parse", "main"]));
    }

    // --- trunk moved during the run -------------------------------------

    /// A shared repository whose trunk another session can push to while the
    /// task runs: `origin`, the registered checkout, a second clone standing
    /// in for the other session, and a task worktree carrying one commit.
    struct SharedTrunk {
        checkout: std::path::PathBuf,
        worktree: std::path::PathBuf,
        other: std::path::PathBuf,
        /// What workspace preparation recorded: the task started on trunk.
        start: WorkspaceBase,
    }

    fn shared_trunk(dir: &Path, branch: &str, required_gate: &str) -> SharedTrunk {
        let remote = dir.join("remote.git");
        let remote_url = format!("file://{}", remote.display());
        let checkout = dir.join("checkout");
        let worktree = dir.join("worktree");
        let other = dir.join("other");
        git(dir, &["init", "--bare", remote.to_str().unwrap()]);
        git(dir, &["init", "-b", "main", checkout.to_str().unwrap()]);
        git(&checkout, &["config", "user.email", "foundry-test@example.com"]);
        git(&checkout, &["config", "user.name", "Foundry Test"]);
        std::fs::write(checkout.join("README.md"), "base\n").unwrap();
        let gates = serde_json::json!({
            "gates": [{"name": "rebased", "command": required_gate, "required": true}]
        });
        std::fs::write(checkout.join(".hone-gates.json"), gates.to_string()).unwrap();
        git(&checkout, &["add", "README.md", ".hone-gates.json"]);
        git(&checkout, &["commit", "-m", "initial"]);
        git(&checkout, &["remote", "add", "origin", &remote_url]);
        let _ = Command::new("git")
            .current_dir(&checkout)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(["config", "--unset-all", "remote.origin.pushurl"])
            .status();
        git(&checkout, &["remote", "set-url", "--push", "origin", &remote_url]);
        git(&checkout, &["push", "-u", "origin", "main"]);
        git(dir, &["clone", "-b", "main", &remote_url, other.to_str().unwrap()]);
        git(&other, &["config", "user.email", "other-session@example.com"]);
        git(&other, &["config", "user.name", "Other Session"]);
        git(&other, &["remote", "set-url", "--push", "origin", &remote_url]);
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
        let started_on = git(&worktree, &["rev-parse", "HEAD"]);
        std::fs::write(worktree.join("README.md"), "base\ntask change\n").unwrap();
        git(&worktree, &["add", "README.md"]);
        git(&worktree, &["commit", "-m", "task change"]);
        SharedTrunk {
            checkout,
            worktree,
            other,
            start: WorkspaceBase {
                commit: started_on.clone(),
                trunk: started_on,
            },
        }
    }

    /// Another session pushes a commit to trunk.
    fn advance_trunk(other: &Path, file: &str, contents: &str, message: &str) -> String {
        std::fs::write(other.join(file), contents).unwrap();
        git(other, &["add", file]);
        git(other, &["commit", "-m", message]);
        git(other, &["push", "origin", "main"]);
        git(other, &["rev-parse", "HEAD"])
    }

    fn pushing_entry(checkout: &Path) -> ProjectEntry {
        let mut entry = test_entry(checkout);
        entry.actions.push = true;
        entry
    }

    async fn finalize_complete(shared: &SharedTrunk, branch: &str) -> serde_json::Value {
        finalize_complete_from(shared, branch, Some(shared.start.clone())).await
    }

    /// Finalize a reviewed-complete task whose preparation recorded `start`.
    async fn finalize_complete_from(
        shared: &SharedTrunk,
        branch: &str,
        start: Option<WorkspaceBase>,
    ) -> serde_json::Value {
        let registry =
            super::super::test_helpers::registry_with_entry(pushing_entry(&shared.checkout));
        let block =
            FinalizeTask::with_gateways(registry, std::sync::Arc::new(CleanProcessShellGateway));
        let mut trigger = task_trigger(&shared.worktree, branch, TaskVerdict::Complete);
        if let Some(start) = start {
            trigger.payload["workspace_base"] = serde_json::to_value(start).unwrap();
        }
        let result = block.execute(&trigger).await.unwrap();
        let payload = result.events[0].payload.clone();
        assert_eq!(result.success, payload["land_blocked"].is_null());
        payload
    }

    fn origin_main(checkout: &Path) -> String {
        git(checkout, &["ls-remote", "origin", "refs/heads/main"])
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_string()
    }

    /// Trunk unchanged: the payload carries no rebase evidence at all, so the
    /// wire shape and summary are exactly what they were.
    #[tokio::test]
    async fn unchanged_trunk_lands_exactly_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/unchanged-trunk";
        let log = dir.path().join("gate-log");
        let shared = shared_trunk(dir.path(), branch, &format!("echo ran >> {}", log.display()));
        let task_head = git(&shared.worktree, &["rev-parse", "HEAD"]);

        let payload = finalize_complete(&shared, branch).await;

        assert_eq!(payload["landed"], true);
        assert_eq!(payload["summary"], "task completed, reviewed, and landed");
        assert_eq!(payload["preservation_ref"], task_head);
        assert!(payload.get("land_blocked").is_none());
        assert!(payload.get("trunk_arrivals").is_none());
        assert!(!log.exists(), "gates must not be re-run when trunk did not move");
        assert_eq!(origin_main(&shared.checkout), task_head);
    }

    /// The ops-01 incident of 2026-10-25: the registered checkout's pre-push
    /// hook wrote a whole validation run to git's stderr and the push failed.
    /// The run's summary is stored in its event and read by campaign
    /// formation, so it carries git's final lines, not the hook's flood.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_push_failure_behind_a_noisy_hook_keeps_the_summary_bounded() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/noisy-pre-push";
        let shared = shared_trunk(dir.path(), branch, "true");
        let hook = shared.checkout.join(".git/hooks/pre-push");
        std::fs::write(
            &hook,
            "#!/bin/sh\n\
             yes 'validate: test output line that a hook prints for every case it runs' \
             | head -n 6000 >&2\n\
             echo 'client_loop: send disconnect: Broken pipe' >&2\n\
             exit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

        let payload = finalize_complete(&shared, branch).await;

        assert_eq!(payload["landed"], false);
        assert_eq!(payload["land_blocked"], "git_failed");
        let summary = payload["summary"].as_str().unwrap();
        assert!(summary.contains("git push origin main failed"), "{summary}");
        assert!(summary.contains("client_loop: send disconnect: Broken pipe"), "{summary}");
        assert!(summary.contains("failed to push some refs"), "{summary}");
        assert!(summary.contains("bytes omitted"), "{summary}");
        assert!(
            summary.len() < crate::workspace::GIT_FAILURE_DETAIL_BYTES + 512,
            "{} bytes",
            summary.len()
        );
    }

    /// The ops-01 incident: two documentation commits reached trunk while a
    /// reviewed-complete task ran. The task is rebased, re-verified and landed.
    #[tokio::test]
    async fn non_conflicting_trunk_movement_is_rebased_reverified_and_landed() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/trunk-moved-clean";
        let log = dir.path().join("gate-log");
        // The gate records that it ran and that it saw the rebased tree.
        let gate = format!("test -f NOTES.md && echo ran >> {}", log.display());
        let shared = shared_trunk(dir.path(), branch, &gate);
        let first = advance_trunk(&shared.other, "NOTES.md", "notes\n", "docs: add notes");
        let second = advance_trunk(&shared.other, "GUIDE.md", "guide\n", "docs: add guide");

        let payload = finalize_complete(&shared, branch).await;

        assert_eq!(payload["landed"], true);
        assert_eq!(payload["verdict"], "complete");
        assert!(payload.get("land_blocked").is_none());
        assert_eq!(
            payload["summary"],
            "task completed, reviewed, and landed after rebasing onto 2 trunk commit(s) that \
             arrived during the run"
        );
        assert_eq!(payload["trunk_arrivals"][0]["commit"], first);
        assert_eq!(payload["trunk_arrivals"][0]["subject"], "docs: add notes");
        assert_eq!(payload["trunk_arrivals"][1]["commit"], second);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "ran\n", "required gate re-ran once");

        let trunk = git(&shared.checkout, &["rev-parse", "HEAD"]);
        assert_eq!(payload["preservation_ref"], trunk);
        assert_eq!(origin_main(&shared.checkout), trunk);
        assert_eq!(git(&shared.checkout, &["log", "-1", "--format=%s"]), "task change");
        assert!(shared.checkout.join("NOTES.md").exists());
        assert!(
            std::fs::read_to_string(shared.checkout.join("README.md"))
                .unwrap()
                .contains("task change")
        );
        assert!(!shared.worktree.exists());
        assert!(
            !git(&shared.checkout, &["ls-remote", "--heads", "origin", branch]).is_empty(),
            "origin ref stays until ledger settlement"
        );
    }

    /// A conflicting trunk commit cannot be reconciled mechanically: the work
    /// is preserved as reviewed, and the verdict stays `complete`.
    #[tokio::test]
    async fn conflicting_trunk_movement_preserves_complete_work_with_a_typed_reason() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/trunk-moved-conflict";
        let log = dir.path().join("gate-log");
        let shared = shared_trunk(dir.path(), branch, &format!("echo ran >> {}", log.display()));
        let task_head = git(&shared.worktree, &["rev-parse", "HEAD"]);
        let arrived =
            advance_trunk(&shared.other, "README.md", "base\ntrunk change\n", "edit readme");

        let payload = finalize_complete(&shared, branch).await;

        assert_eq!(payload["landed"], false);
        assert_eq!(payload["success"], false);
        assert_eq!(payload["verdict"], "complete");
        assert_eq!(payload["land_blocked"], "trunk_moved_conflict");
        assert!(
            payload["summary"].as_str().unwrap().starts_with(
                "complete work preserved but could not land on trunk (trunk_moved_conflict): \
                 trunk advanced during the run and rebasing onto it conflicted"
            ),
            "{}",
            payload["summary"]
        );
        assert_eq!(payload["trunk_arrivals"][0]["commit"], arrived);
        assert_eq!(payload["preservation_ref"], branch);
        assert!(!log.exists(), "no gate runs on a rebase that conflicted");
        assert_eq!(origin_main(&shared.checkout), arrived, "trunk must not move");
        assert_eq!(
            git(&shared.checkout, &["rev-parse", branch]),
            task_head,
            "the preserved branch must be the reviewed commit"
        );
        assert!(
            git(&shared.checkout, &["ls-remote", "--heads", "origin", branch]).contains(&task_head)
        );
    }

    /// The production mojentic-sw case: the run started from a continuation
    /// base that already lacked trunk, and nothing reached trunk while it ran.
    /// That is a stale start, not a race, and the reason must say so.
    #[tokio::test]
    async fn a_run_that_started_behind_trunk_reports_a_stale_base_not_a_moved_trunk() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/stale-base-conflict";
        let shared = shared_trunk(dir.path(), branch, "true");
        let task_head = git(&shared.worktree, &["rev-parse", "HEAD"]);
        // Trunk was already here when the run started; it never moved after.
        let trunk = advance_trunk(&shared.other, "README.md", "base\ntrunk change\n", "edit");
        let start = WorkspaceBase {
            commit: shared.start.commit.clone(),
            trunk,
        };

        let payload = finalize_complete_from(&shared, branch, Some(start)).await;

        assert_eq!(payload["landed"], false);
        assert_eq!(payload["verdict"], "complete");
        assert_eq!(payload["land_blocked"], "stale_base_conflict");
        let summary = payload["summary"].as_str().unwrap();
        assert!(
            summary.starts_with(
                "complete work preserved but could not land on trunk (stale_base_conflict): \
                 the run started 1 commit(s) behind trunk and rebasing onto trunk conflicted"
            ),
            "{summary}"
        );
        assert!(!summary.contains("during the run"), "{summary}");
        assert_eq!(git(&shared.checkout, &["rev-parse", branch]), task_head);
    }

    /// A clean rebase after a stale start lands, and the summary does not
    /// call the commits it lacked arrivals during the run.
    #[tokio::test]
    async fn a_stale_start_whose_rebase_is_clean_lands_without_claiming_trunk_moved() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/stale-base-clean";
        let shared = shared_trunk(dir.path(), branch, "true");
        let trunk = advance_trunk(&shared.other, "NOTES.md", "notes\n", "docs: add notes");
        let start = WorkspaceBase {
            commit: shared.start.commit.clone(),
            trunk,
        };

        let payload = finalize_complete_from(&shared, branch, Some(start)).await;

        assert_eq!(payload["landed"], true, "{payload}");
        assert_eq!(
            payload["summary"],
            "task completed, reviewed, and landed after rebasing onto 1 trunk commit(s) that \
             the run's starting base lacked"
        );
    }

    /// A run that recorded no start (it began before Foundry recorded one)
    /// keeps the stable tag, but its detail does not assert a race it cannot
    /// know about.
    #[tokio::test]
    async fn a_run_with_no_recorded_start_does_not_claim_trunk_moved_during_it() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/unrecorded-start";
        let shared = shared_trunk(dir.path(), branch, "true");
        advance_trunk(&shared.other, "README.md", "base\ntrunk change\n", "edit readme");

        let payload = finalize_complete_from(&shared, branch, None).await;

        assert_eq!(payload["land_blocked"], "trunk_moved_conflict");
        let summary = payload["summary"].as_str().unwrap();
        assert!(summary.contains("whether trunk moved during the run is unknown"), "{summary}");
        assert!(!summary.contains("trunk advanced during the run"), "{summary}");
    }

    /// A clean rebase is not enough: a required gate that goes red on the
    /// rebased tree keeps the work off trunk.
    #[tokio::test]
    async fn a_required_gate_failing_after_rebase_preserves_the_work() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/trunk-moved-red";
        // Passes on the reviewed tree, fails once trunk's new file is present.
        let shared = shared_trunk(dir.path(), branch, "test ! -f BREAKS.md");
        let task_head = git(&shared.worktree, &["rev-parse", "HEAD"]);
        let arrived = advance_trunk(&shared.other, "BREAKS.md", "x\n", "add breaking file");

        let payload = finalize_complete(&shared, branch).await;

        assert_eq!(payload["landed"], false);
        assert_eq!(payload["verdict"], "complete");
        assert_eq!(payload["land_blocked"], "trunk_moved_gates_failed");
        assert!(
            payload["summary"].as_str().unwrap().contains("rebased"),
            "{}",
            payload["summary"]
        );
        assert_eq!(payload["trunk_arrivals"][0]["commit"], arrived);
        assert_eq!(payload["preservation_ref"], branch);
        assert_eq!(origin_main(&shared.checkout), arrived, "trunk must not move");
        assert_eq!(
            git(&shared.checkout, &["rev-parse", branch]),
            task_head,
            "the preserved branch must be restored to the reviewed commit"
        );
    }

    /// Trunk moved again while the gates re-ran: one more fetch, rebase and
    /// re-verify, then land.
    #[tokio::test]
    async fn trunk_moving_again_during_the_gate_rerun_gets_one_more_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/trunk-moved-twice";
        let mark = dir.path().join("pushed-once");
        let other = dir.path().join("other");
        // The first gate run is where the other session pushes again.
        let gate = format!(
            "if [ ! -f {mark} ]; then touch {mark} && cd {other} && echo again > AGAIN.md \
             && git add AGAIN.md && git commit -qm again && git push -q origin main; fi",
            mark = mark.display(),
            other = other.display(),
        );
        let shared = shared_trunk(dir.path(), branch, &gate);
        advance_trunk(&shared.other, "NOTES.md", "notes\n", "docs: add notes");

        let payload = finalize_complete(&shared, branch).await;

        assert_eq!(payload["landed"], true, "{payload}");
        assert_eq!(payload["trunk_arrivals"].as_array().unwrap().len(), 2);
        assert_eq!(payload["trunk_arrivals"][1]["subject"], "again");
        assert!(shared.checkout.join("AGAIN.md").exists());
        assert_eq!(git(&shared.checkout, &["log", "-1", "--format=%s"]), "task change");
    }

    /// The retry is bounded: a trunk that keeps moving is not chased forever.
    #[tokio::test]
    async fn a_trunk_that_keeps_moving_is_preserved_after_two_rebases() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/trunk-keeps-moving";
        let other = dir.path().join("other");
        let gate = format!(
            "cd {other} && date +%s%N >> MOVING.md && git add MOVING.md \
             && git commit -qm moving && git push -q origin main",
            other = other.display(),
        );
        let shared = shared_trunk(dir.path(), branch, &gate);
        let task_head = git(&shared.worktree, &["rev-parse", "HEAD"]);
        advance_trunk(&shared.other, "NOTES.md", "notes\n", "docs: add notes");

        let payload = finalize_complete(&shared, branch).await;

        assert_eq!(payload["landed"], false);
        assert_eq!(payload["verdict"], "complete");
        assert_eq!(payload["land_blocked"], "trunk_moved_repeatedly");
        // The first arrival, then one per gate re-run.
        assert_eq!(payload["trunk_arrivals"].as_array().unwrap().len(), 3);
        assert_eq!(git(&shared.checkout, &["rev-parse", branch]), task_head);
    }

    /// A project change outside `.foundry`, which must land in the task commit
    /// whatever happens under `.foundry`.
    fn write_project_change(worktree: &Path) {
        std::fs::create_dir_all(worktree.join("docs")).unwrap();
        std::fs::write(worktree.join("docs/notes.md"), "kept\n").unwrap();
    }

    /// The paths the commit at `rev` changed, with their git status letter.
    fn commit_changes(checkout: &Path, rev: &str) -> String {
        git(checkout, &["show", "--name-status", "--format=", rev])
    }

    fn tracked_under_foundry(checkout: &Path, rev: &str) -> String {
        git(checkout, &["ls-tree", "-r", "--name-only", rev, "--", ".foundry"])
    }

    fn finalize_block(checkout: &Path) -> FinalizeTask {
        let registry = super::super::test_helpers::registry_with_entry(test_entry(checkout));
        FinalizeTask::with_gateways(registry, std::sync::Arc::new(CleanProcessShellGateway))
    }

    fn complete_trigger(worktree: &Path, branch: &str) -> Event {
        task_trigger_with_gates(worktree, branch, TaskVerdict::Complete, vec![required_gate(true)])
    }

    /// The archive of unlanded `.foundry` files a finalization recorded.
    fn unlanded_mirror(payload: &serde_json::Value) -> &Path {
        Path::new(payload["uncommitted_artifacts"].as_str().unwrap_or_else(|| {
            panic!("finalize must record where the unlanded files went: {payload}")
        }))
    }

    /// Commit `files` on the task branch, so HEAD tracks them before the agent
    /// runs, as a project with legacy `.foundry` content would.
    fn track_on_task_branch(worktree: &Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let path = worktree.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        git(worktree, &["add", "--force", ".foundry"]);
        git(worktree, &["commit", "-m", "legacy .foundry content"]);
    }

    /// Proof never lands, and neither does an edit to a tracked `.foundry`
    /// file, even when the agent staged both. Both stay retrievable.
    #[tokio::test]
    async fn early_proof_and_tracked_dot_foundry_edits_stay_off_trunk_and_remain_retrievable() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/proof-artifacts";
        let (checkout, worktree, _) = repo_with_task_branch(dir.path(), branch);
        track_on_task_branch(&worktree, &[(".foundry/project.json", "original")]);
        std::fs::write(worktree.join(".foundry/project.json"), "updated").unwrap();
        std::fs::create_dir(worktree.join(".foundry/logs")).unwrap();
        std::fs::write(worktree.join(".foundry/logs/red.log"), "rejection").unwrap();
        std::fs::write(worktree.join(".foundry/logs/green.log"), "correction").unwrap();
        let proof = serde_json::json!({"kind":"behavioral","source_change":"README.md",
            "rejecting":{"command":"probe","exit_code":1,"log":".foundry/logs/red.log"},
            "corrected":{"command":"probe","exit_code":0,"log":".foundry/logs/green.log"}});
        std::fs::write(worktree.join(".foundry/proof.json"), proof.to_string()).unwrap();
        write_project_change(&worktree);
        // Even accidentally staged .foundry files must not enter the commit.
        git(&worktree, &["add", ".foundry"]);
        let trigger = complete_trigger(&worktree, branch);
        let result = finalize_block(&checkout).execute(&trigger).await.unwrap();
        let payload = &result.events[0].payload;
        assert_eq!(payload["landed"], true, "{payload}");
        assert_eq!(commit_changes(&checkout, "HEAD"), "A\tdocs/notes.md");
        assert_eq!(tracked_under_foundry(&checkout, "HEAD"), ".foundry/project.json");
        assert_eq!(git(&checkout, &["show", "HEAD:.foundry/project.json"]), "original");
        assert_eq!(
            std::fs::read_to_string(checkout.join(".foundry/project.json")).unwrap(),
            "original"
        );
        assert!(!worktree.exists(), "archived artifacts must not prevent cleanup");
        let evidence = Path::new(payload["proof_evidence"].as_str().unwrap());
        assert!(
            evidence.starts_with(
                foundry_sdk::paths::foundry_home()
                    .join("evidence")
                    .join(trigger.trace_id.as_ref().unwrap_or(&trigger.id))
            )
        );
        assert_eq!(
            std::fs::read_to_string(evidence.join("original-proof.json")).unwrap(),
            proof.to_string()
        );
        let archived: serde_json::Value =
            serde_json::from_slice(&std::fs::read(evidence.join("proof.json")).unwrap()).unwrap();
        assert_eq!(
            std::fs::read_to_string(evidence.join(archived["rejecting"]["log"].as_str().unwrap()))
                .unwrap(),
            "rejection"
        );
        assert_eq!(
            std::fs::read_to_string(evidence.join(archived["corrected"]["log"].as_str().unwrap()))
                .unwrap(),
            "correction"
        );
        // The raw mirror of what did not land sits beside the interpreted
        // proof archive, in the same attempt directory, and holds every
        // .foundry file at its worktree path, the agent's edit included.
        let mirror = unlanded_mirror(payload);
        assert_eq!(mirror, evidence.join("worktree"));
        assert_eq!(
            std::fs::read_to_string(mirror.join(".foundry/proof.json")).unwrap(),
            proof.to_string()
        );
        assert_eq!(
            std::fs::read_to_string(mirror.join(".foundry/logs/red.log")).unwrap(),
            "rejection"
        );
        assert_eq!(
            std::fs::read_to_string(mirror.join(".foundry/project.json")).unwrap(),
            "updated"
        );
        assert!(!mirror.join("docs/notes.md").exists(), "landed files are not archived");
        // Test fixture owns this proof archive; this is not a worktree removal.
        std::fs::remove_dir_all(evidence).unwrap();
    }

    /// Evidence a brief asks for under `.foundry/<topic>/`.
    fn write_agent_evidence(worktree: &Path) {
        std::fs::create_dir_all(worktree.join(".foundry/armature/shots")).unwrap();
        std::fs::write(worktree.join(".foundry/armature/result.json"), "{\"ok\":true}").unwrap();
        std::fs::write(worktree.join(".foundry/armature/shots/a.png"), "png bytes").unwrap();
    }

    fn assert_agent_evidence_archived(mirror: &Path) {
        assert_eq!(
            std::fs::read_to_string(mirror.join(".foundry/armature/result.json")).unwrap(),
            "{\"ok\":true}"
        );
        assert_eq!(
            std::fs::read_to_string(mirror.join(".foundry/armature/shots/a.png")).unwrap(),
            "png bytes"
        );
    }

    /// The 2026-10-10 Roost incident: a task wrote its evidence under
    /// `.foundry/armature/` and none of it survived finalization. It still
    /// does not land, because `.foundry/` is Foundry scratch, but it is
    /// archived rather than destroyed.
    #[tokio::test]
    async fn new_dot_foundry_files_stay_off_trunk_and_are_archived() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/agent-evidence-archived";
        let (checkout, worktree, _) = repo_with_task_branch(dir.path(), branch);
        write_agent_evidence(&worktree);
        write_project_change(&worktree);

        let result = finalize_block(&checkout)
            .execute(&complete_trigger(&worktree, branch))
            .await
            .unwrap();
        let payload = &result.events[0].payload;

        assert_eq!(payload["landed"], true, "{payload}");
        assert_eq!(commit_changes(&checkout, "HEAD"), "A\tdocs/notes.md");
        assert!(tracked_under_foundry(&checkout, "HEAD").is_empty(), "scratch landed on trunk");
        assert!(!worktree.exists());
        let mirror = unlanded_mirror(payload);
        assert_agent_evidence_archived(mirror);
        // Test fixture owns this evidence archive; this is not a worktree removal.
        std::fs::remove_dir_all(mirror.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn new_dot_foundry_files_stay_off_the_preserved_branch_and_are_archived() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/agent-evidence-preserved";
        let (checkout, worktree, _) = repo_with_task_branch(dir.path(), branch);
        write_agent_evidence(&worktree);
        write_project_change(&worktree);

        let trigger = task_trigger(
            &worktree,
            branch,
            TaskVerdict::Defect {
                diagnosis: "not yet".to_string(),
            },
        );
        let result = finalize_block(&checkout).execute(&trigger).await.unwrap();
        let payload = &result.events[0].payload;

        assert_eq!(payload["landed"], false);
        assert_eq!(payload["preservation_ref"], branch);
        assert_eq!(commit_changes(&checkout, branch), "A\tdocs/notes.md");
        assert!(
            tracked_under_foundry(&checkout, branch).is_empty(),
            "scratch landed on the preserved branch"
        );
        assert!(
            tracked_under_foundry(&checkout, "main").is_empty(),
            "defect work leaked to trunk"
        );
        let mirror = unlanded_mirror(payload);
        assert_agent_evidence_archived(mirror);
        // Test fixture owns this evidence archive; this is not a worktree removal.
        std::fs::remove_dir_all(mirror.parent().unwrap()).unwrap();
    }

    /// An agent's edit to a tracked `.foundry` file does not land: trunk keeps
    /// HEAD's content, the archive keeps the edit, and the restored file does
    /// not hold the worktree open.
    #[tokio::test]
    async fn an_edit_to_a_tracked_dot_foundry_file_is_archived_not_landed() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/tracked-edit";
        let (checkout, worktree, _) = repo_with_task_branch(dir.path(), branch);
        track_on_task_branch(&worktree, &[(".foundry/x.json", "head")]);
        std::fs::write(worktree.join(".foundry/x.json"), "agent").unwrap();
        write_project_change(&worktree);

        let result = finalize_block(&checkout)
            .execute(&complete_trigger(&worktree, branch))
            .await
            .unwrap();
        let payload = &result.events[0].payload;

        assert_eq!(payload["landed"], true, "{payload}");
        assert_eq!(commit_changes(&checkout, "HEAD"), "A\tdocs/notes.md");
        assert_eq!(git(&checkout, &["show", "HEAD:.foundry/x.json"]), "head");
        assert!(!worktree.exists(), "the restored file must not block worktree removal");
        let mirror = unlanded_mirror(payload);
        assert_eq!(std::fs::read_to_string(mirror.join(".foundry/x.json")).unwrap(), "agent");
        // Test fixture owns this evidence archive; this is not a worktree removal.
        std::fs::remove_dir_all(mirror.parent().unwrap()).unwrap();
    }

    /// Deleting tracked `.foundry` content is how a task cleans legacy files
    /// out of a project, so that deletion is the one `.foundry` change that
    /// lands, in the same commit as the project change.
    #[tokio::test]
    async fn deleting_a_tracked_dot_foundry_file_lands_on_trunk() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/tracked-delete";
        let (checkout, worktree, _) = repo_with_task_branch(dir.path(), branch);
        track_on_task_branch(
            &worktree,
            &[
                (".foundry/old.json", "legacy"),
                (".foundry/kept.json", "kept"),
            ],
        );
        // Staged by the agent, so finalize must restage it after resetting.
        git(&worktree, &["rm", "--quiet", ".foundry/old.json"]);
        write_project_change(&worktree);

        let result = finalize_block(&checkout)
            .execute(&complete_trigger(&worktree, branch))
            .await
            .unwrap();
        let payload = &result.events[0].payload;

        assert_eq!(payload["landed"], true, "{payload}");
        assert_eq!(commit_changes(&checkout, "HEAD"), "D\t.foundry/old.json\nA\tdocs/notes.md");
        assert_eq!(tracked_under_foundry(&checkout, "HEAD"), ".foundry/kept.json");
        assert!(payload.get("uncommitted_artifacts").is_none(), "nothing was left: {payload}");
        assert!(!worktree.exists());
    }

    /// A repository that tracks proof scratch (Foundry's own does) gets the
    /// same treatment as any tracked `.foundry` file: edits and new files are
    /// archived, deletions land.
    #[tokio::test]
    async fn tracked_proof_scratch_keeps_head_content_and_only_its_deletions_land() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/tracked-scratch";
        let (checkout, worktree, _) = repo_with_task_branch(dir.path(), branch);
        track_on_task_branch(
            &worktree,
            &[
                (".foundry/proof.json", "{}"),
                (".foundry/logs/old.log", "old"),
            ],
        );
        std::fs::write(worktree.join(".foundry/proof.json"), "{\"kind\":\"direct\"}").unwrap();
        std::fs::remove_file(worktree.join(".foundry/logs/old.log")).unwrap();
        std::fs::write(worktree.join(".foundry/logs/new.log"), "new").unwrap();
        write_project_change(&worktree);

        let result = finalize_block(&checkout)
            .execute(&complete_trigger(&worktree, branch))
            .await
            .unwrap();
        let payload = &result.events[0].payload;

        assert_eq!(payload["landed"], true, "{payload}");
        assert_eq!(commit_changes(&checkout, "HEAD"), "D\t.foundry/logs/old.log\nA\tdocs/notes.md");
        assert_eq!(tracked_under_foundry(&checkout, "HEAD"), ".foundry/proof.json");
        assert_eq!(git(&checkout, &["show", "HEAD:.foundry/proof.json"]), "{}");
        assert!(!worktree.exists());
        let mirror = unlanded_mirror(payload);
        assert_eq!(
            std::fs::read_to_string(mirror.join(".foundry/proof.json")).unwrap(),
            "{\"kind\":\"direct\"}"
        );
        assert_eq!(std::fs::read_to_string(mirror.join(".foundry/logs/new.log")).unwrap(), "new");
        // Test fixture owns this evidence archive; this is not a worktree removal.
        std::fs::remove_dir_all(mirror.parent().unwrap()).unwrap();
    }

    /// A gitignored file cannot land, but cleanup must not destroy it silently.
    #[tokio::test]
    async fn gitignored_dot_foundry_files_are_archived_rather_than_destroyed() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/ignored-evidence";
        let (checkout, worktree, _) = repo_with_task_branch(dir.path(), branch);
        std::fs::write(worktree.join(".gitignore"), ".foundry/runtime/\n").unwrap();
        std::fs::create_dir_all(worktree.join(".foundry/runtime/nested")).unwrap();
        std::fs::write(worktree.join(".foundry/runtime/nested/session.log"), "runtime state")
            .unwrap();

        let trigger = complete_trigger(&worktree, branch);
        let result = finalize_block(&checkout).execute(&trigger).await.unwrap();
        let payload = &result.events[0].payload;

        assert_eq!(payload["landed"], true, "{payload}");
        assert_eq!(commit_changes(&checkout, "HEAD"), "A\t.gitignore");
        assert!(tracked_under_foundry(&checkout, "HEAD").is_empty(), "ignored file landed");
        assert!(!worktree.exists(), "archival must not prevent cleanup");
        let archive = unlanded_mirror(payload);
        let attempt = archive.parent().unwrap();
        assert!(
            attempt.starts_with(
                foundry_sdk::paths::foundry_home()
                    .join("evidence")
                    .join(trigger.trace_id.as_ref().unwrap_or(&trigger.id))
            ),
            "{}",
            archive.display()
        );
        assert_eq!(
            std::fs::read_to_string(archive.join(".foundry/runtime/nested/session.log")).unwrap(),
            "runtime state"
        );
        // Test fixture owns this evidence archive; this is not a worktree removal.
        std::fs::remove_dir_all(attempt).unwrap();
    }

    /// Commit a `.gitignore` that ignores the whole of `.foundry/` on the task
    /// branch, as a project that keeps Foundry scratch out of git does.
    fn ignore_dot_foundry_on_task_branch(worktree: &Path) {
        std::fs::write(worktree.join(".gitignore"), ".foundry/\n").unwrap();
        git(worktree, &["add", ".gitignore"]);
        git(worktree, &["commit", "-m", "ignore .foundry"]);
    }

    /// The 2026-10-11 Bedrock incident: once a repository ignored `.foundry/`,
    /// staging with a `.foundry` exclude pathspec made git exit 1 ("paths are
    /// ignored"), so every task failed at commit. Ignoring `.foundry` must
    /// not stop the project change landing, nor let scratch land.
    #[tokio::test]
    async fn an_ignored_dot_foundry_does_not_stop_the_project_change_landing() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/ignored-dot-foundry";
        let (checkout, worktree, _) = repo_with_task_branch(dir.path(), branch);
        ignore_dot_foundry_on_task_branch(&worktree);
        write_agent_evidence(&worktree);
        write_project_change(&worktree);

        let result = finalize_block(&checkout)
            .execute(&complete_trigger(&worktree, branch))
            .await
            .unwrap();
        let payload = &result.events[0].payload;

        assert_eq!(payload["landed"], true, "{payload}");
        assert_eq!(commit_changes(&checkout, "HEAD"), "A\tdocs/notes.md");
        assert!(tracked_under_foundry(&checkout, "HEAD").is_empty(), "scratch landed on trunk");
        assert!(!worktree.exists());
        let mirror = unlanded_mirror(payload);
        assert_agent_evidence_archived(mirror);
        // Test fixture owns this evidence archive; this is not a worktree removal.
        std::fs::remove_dir_all(mirror.parent().unwrap()).unwrap();
    }

    /// An ignored `.foundry` that HEAD still partly tracks: the deletion of a
    /// tracked file lands, while an edit to another and a file the agent
    /// force-staged do not.
    #[tokio::test]
    async fn an_ignored_but_partly_tracked_dot_foundry_lands_only_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/ignored-partly-tracked";
        let (checkout, worktree, _) = repo_with_task_branch(dir.path(), branch);
        track_on_task_branch(
            &worktree,
            &[
                (".foundry/old.json", "legacy"),
                (".foundry/kept.json", "kept"),
            ],
        );
        ignore_dot_foundry_on_task_branch(&worktree);
        std::fs::remove_file(worktree.join(".foundry/old.json")).unwrap();
        std::fs::write(worktree.join(".foundry/kept.json"), "agent").unwrap();
        std::fs::write(worktree.join(".foundry/forced.json"), "forced").unwrap();
        git(&worktree, &["add", "--force", ".foundry/forced.json"]);
        write_project_change(&worktree);

        let result = finalize_block(&checkout)
            .execute(&complete_trigger(&worktree, branch))
            .await
            .unwrap();
        let payload = &result.events[0].payload;

        assert_eq!(payload["landed"], true, "{payload}");
        assert_eq!(commit_changes(&checkout, "HEAD"), "D\t.foundry/old.json\nA\tdocs/notes.md");
        assert_eq!(tracked_under_foundry(&checkout, "HEAD"), ".foundry/kept.json");
        assert_eq!(git(&checkout, &["show", "HEAD:.foundry/kept.json"]), "kept");
        assert!(!worktree.exists());
        let mirror = unlanded_mirror(payload);
        assert_eq!(std::fs::read_to_string(mirror.join(".foundry/kept.json")).unwrap(), "agent");
        assert_eq!(std::fs::read_to_string(mirror.join(".foundry/forced.json")).unwrap(), "forced");
        // Test fixture owns this evidence archive; this is not a worktree removal.
        std::fs::remove_dir_all(mirror.parent().unwrap()).unwrap();
    }

    /// A tracked `.foundry` file the agent replaced with a directory: the
    /// file's deletion lands, the directory's new content does not.
    #[tokio::test]
    async fn a_tracked_dot_foundry_file_replaced_by_a_directory_lands_as_a_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/file-to-directory";
        let (checkout, worktree, _) = repo_with_task_branch(dir.path(), branch);
        track_on_task_branch(&worktree, &[(".foundry/state", "file")]);
        std::fs::remove_file(worktree.join(".foundry/state")).unwrap();
        std::fs::create_dir(worktree.join(".foundry/state")).unwrap();
        std::fs::write(worktree.join(".foundry/state/inner.json"), "inner").unwrap();
        write_project_change(&worktree);

        let result = finalize_block(&checkout)
            .execute(&complete_trigger(&worktree, branch))
            .await
            .unwrap();
        let payload = &result.events[0].payload;

        assert_eq!(payload["landed"], true, "{payload}");
        assert_eq!(commit_changes(&checkout, "HEAD"), "D\t.foundry/state\nA\tdocs/notes.md");
        assert!(tracked_under_foundry(&checkout, "HEAD").is_empty(), "scratch landed on trunk");
        assert!(!worktree.exists());
        let mirror = unlanded_mirror(payload);
        assert_eq!(
            std::fs::read_to_string(mirror.join(".foundry/state/inner.json")).unwrap(),
            "inner"
        );
        // Test fixture owns this evidence archive; this is not a worktree removal.
        std::fs::remove_dir_all(mirror.parent().unwrap()).unwrap();
    }

    // --- git hooks inside push: the derived limit --------------------------

    /// Stands in for a repository hook that makes selected git commands slow.
    ///
    /// A matching command waits `duration` under the timeout it was given,
    /// exactly as the real gateway bounds a child process, and fails with the
    /// real gateway's timeout error when the wait exceeds it. Otherwise it
    /// runs the real git command. Every matching command's timeout is
    /// recorded. Run under paused tokio time so the wait costs nothing.
    struct SlowHookShellGateway {
        is_slow: fn(&[&str]) -> bool,
        duration: Duration,
        timeouts: std::sync::Mutex<Vec<(String, Option<Duration>)>>,
    }

    impl SlowHookShellGateway {
        fn new(is_slow: fn(&[&str]) -> bool, duration: Duration) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                is_slow,
                duration,
                timeouts: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn timeouts(&self) -> Vec<(String, Option<Duration>)> {
            self.timeouts.lock().unwrap().clone()
        }
    }

    impl ShellGateway for SlowHookShellGateway {
        fn run<'a>(
            &'a self,
            working_dir: &'a Path,
            command: &'a str,
            args: &'a [&'a str],
            env: Option<&'a [(String, String)]>,
            timeout: Option<Duration>,
        ) -> Pin<Box<dyn std::future::Future<Output = Result<CommandResult>> + Send + 'a>> {
            Box::pin(async move {
                if command == "git" && (self.is_slow)(args) {
                    self.timeouts.lock().unwrap().push((args.join(" "), timeout));
                    let limit = timeout.unwrap_or(crate::shell::DEFAULT_TIMEOUT);
                    anyhow::Context::with_context(
                        tokio::time::timeout(limit, tokio::time::sleep(self.duration)).await,
                        || {
                            format!(
                                "command timed out after {:.1}s: {command}",
                                limit.as_secs_f64()
                            )
                        },
                    )?;
                }
                CleanProcessShellGateway.run(working_dir, command, args, env, timeout).await
            })
        }
    }

    fn is_landing_push(args: &[&str]) -> bool {
        args == ["push", "origin", "main"]
    }

    fn is_preservation_push(args: &[&str]) -> bool {
        args.starts_with(&["push", "-u", "origin"])
    }

    /// Two required gates of 20 minutes each, and an optional one the hook
    /// limit must ignore, committed on the task branch.
    fn hook_gates_on_task_branch(worktree: &Path) {
        let gates = serde_json::json!({"gates": [
            {"name": "lint", "command": "true", "required": true, "timeout": 1200},
            {"name": "test", "command": "true", "required": true, "timeout": 1200},
            {"name": "e2e", "command": "true", "required": false, "timeout": 90000}
        ]});
        std::fs::write(worktree.join(".hone-gates.json"), gates.to_string()).unwrap();
        git(worktree, &["add", ".hone-gates.json"]);
        git(worktree, &["commit", "-m", "gate timeouts"]);
    }

    /// The limit those gates give on this host.
    fn hook_gates_limit() -> Duration {
        Duration::from_secs(2_400)
            .mul_f64(crate::gate_runner::host_gate_timeout_scale())
            .min(crate::git_hook_timeout::GIT_HOOK_TIMEOUT_CEILING)
    }

    async fn finalize_with(
        shared: &SharedTrunk,
        branch: &str,
        shell: std::sync::Arc<SlowHookShellGateway>,
        verdict: TaskVerdict,
    ) -> serde_json::Value {
        let registry =
            super::super::test_helpers::registry_with_entry(pushing_entry(&shared.checkout));
        let block = FinalizeTask::with_gateways(registry, shell);
        let mut trigger =
            task_trigger_with_gates(&shared.worktree, branch, verdict, vec![required_gate(true)]);
        trigger.payload["workspace_base"] = serde_json::to_value(shared.start.clone()).unwrap();
        let result = block.execute(&trigger).await.unwrap();
        result.events[0].payload.clone()
    }

    /// The ops-01 incident of 2026-10-26: a pre-push hook that ran the
    /// project's whole validation outlasted the shell's five-minute default,
    /// and landing-eligible work stayed off trunk. A push that takes longer
    /// than five minutes but less than the project's required gates allow
    /// now lands.
    #[tokio::test(start_paused = true)]
    async fn a_push_whose_hook_outlasts_the_shell_default_but_not_the_gates_lands() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/slow-pre-push";
        let shared = shared_trunk(dir.path(), branch, "true");
        hook_gates_on_task_branch(&shared.worktree);
        let task_head = git(&shared.worktree, &["rev-parse", "HEAD"]);
        let push = Duration::from_secs(301);
        assert!(push > crate::shell::DEFAULT_TIMEOUT && push < hook_gates_limit());
        let shell = SlowHookShellGateway::new(is_landing_push, push);

        let payload = finalize_with(
            &shared,
            branch,
            std::sync::Arc::clone(&shell),
            TaskVerdict::Remainder {
                gaps: vec!["docs".to_string()],
            },
        )
        .await;

        assert_eq!(payload["landed"], true, "{payload}");
        assert!(payload.get("land_blocked").is_none(), "{payload}");
        assert_eq!(origin_main(&shared.checkout), task_head);
        assert_eq!(
            shell.timeouts(),
            [("push origin main".to_string(), Some(hook_gates_limit()))],
            "the landing push runs under the limit derived from the task's required gates"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_push_still_running_at_the_derived_limit_is_a_typed_blocked_landing() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/hung-pre-push";
        let shared = shared_trunk(dir.path(), branch, "true");
        hook_gates_on_task_branch(&shared.worktree);
        let trunk_before = origin_main(&shared.checkout);
        let shell =
            SlowHookShellGateway::new(is_landing_push, hook_gates_limit() + Duration::from_secs(1));

        let payload =
            finalize_with(&shared, branch, std::sync::Arc::clone(&shell), TaskVerdict::Complete)
                .await;

        assert_eq!(payload["landed"], false);
        assert_eq!(payload["land_blocked"], "git_hook_timed_out");
        assert_eq!(payload["verdict"], "complete", "a blocked landing keeps the verdict");
        assert_eq!(payload["preservation_ref"], branch, "the work stays on its task branch");
        assert!(
            !git(&shared.checkout, &["ls-remote", "--heads", "origin", branch]).is_empty(),
            "the preserved branch reached origin"
        );
        assert_eq!(origin_main(&shared.checkout), trunk_before, "trunk did not move");
        let summary = payload["summary"].as_str().unwrap();
        assert!(
            summary.starts_with(
                "complete work preserved but could not land on trunk (git_hook_timed_out): \
                 git push origin main was still running when it reached its"
            ),
            "{summary}"
        );
        assert!(summary.contains("git hook (pre-push, for a push) was most likely still running"));
        assert!(
            summary.contains(
                "The limit is the sum of the 2 required gate timeout(s) in .hone-gates.json"
            ),
            "{summary}"
        );
    }

    /// Preservation's push runs the same hook. Still running at its limit, it
    /// falls back to a bundle instead of failing the whole finalize.
    #[tokio::test(start_paused = true)]
    async fn a_preservation_push_still_running_at_its_limit_falls_back_to_a_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let branch = "foundry-task/hung-preservation-push";
        let shared = shared_trunk(dir.path(), branch, "true");
        hook_gates_on_task_branch(&shared.worktree);
        let shell = SlowHookShellGateway::new(
            is_preservation_push,
            hook_gates_limit() + Duration::from_secs(1),
        );

        let payload = finalize_with(
            &shared,
            branch,
            std::sync::Arc::clone(&shell),
            TaskVerdict::Defect {
                diagnosis: "not yet".to_string(),
            },
        )
        .await;

        assert_eq!(payload["verdict"], "defect", "not a runner error: {payload}");
        assert_eq!(payload["diagnosis"], "not yet");
        let preserved = payload["preservation_ref"].as_str().unwrap();
        let bundle = preserved.strip_prefix("bundle:").expect("preserved as a bundle");
        assert_eq!(
            shell.timeouts(),
            [(format!("push -u origin {branch}"), Some(hook_gates_limit()))]
        );
        assert!(git(&shared.checkout, &["bundle", "list-heads", bundle]).contains(branch));
        // Test fixture owns this preservation bundle.
        std::fs::remove_file(bundle).unwrap();
    }
}
