use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gates::{GateDefinition, GateResult};
use foundry_sdk::payload::{
    LandBlocked, LoopContext, TaskReviewedPayload, TaskRunCompletedPayload, TaskVerdict,
    TrunkArrival,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock};

use crate::gateway::{ProcessShellGateway, ShellGateway};
use crate::workspace::{
    checked, commit_worktree, preserve, remove_workspace, run, run_best_effort,
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

/// One attempt to fast-forward trunk onto the task branch.
///
/// Trunk movement is detected before the merge, so a moved trunk leaves the
/// registered checkout exactly as the pull left it.
async fn land_on_trunk(
    shell: &dyn ShellGateway,
    checkout: &Path,
    base_branch: &str,
    task_branch: &str,
    push_enabled: bool,
) -> Result<(), LandAttemptError> {
    let git_failed =
        |error: anyhow::Error| LandAttemptError::Blocked(LandBlocked::GitFailed, error.to_string());
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
        checked(shell, checkout, &["pull", "--ff-only", "origin", base_branch])
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
                format!("git merge-base --is-ancestor failed ({code}): {}", ancestry.stderr.trim()),
            ));
        }
    }
    checked(shell, checkout, &["merge", "--ff-only", task_branch])
        .await
        .map_err(git_failed)?;
    if push_enabled {
        checked(shell, checkout, &["push", "origin", base_branch])
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
    /// Trunk commits that arrived during the run, oldest first.
    arrivals: Vec<TrunkArrival>,
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
) -> Landing {
    let mut arrivals = Vec::new();
    let mut reviewed_head: Option<String> = None;
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
                    match rebase_and_verify(shell, worktree, base_branch).await {
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
    Landing { blocked, arrivals }
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
) -> Result<(), (LandBlocked, String)> {
    let rebase = run(shell, worktree, &["rebase", base_branch])
        .await
        .map_err(|error| (LandBlocked::GitFailed, error.to_string()))?;
    if !rebase.success {
        // Best-effort: the rebase already failed and the caller restores the
        // reviewed head; an abort that fails leaves nothing that restore does
        // not also reset.
        run_best_effort(shell, worktree, &["rebase", "--abort"]).await;
        return Err((
            LandBlocked::TrunkMovedConflict,
            format!(
                "trunk advanced during the run and rebasing onto it conflicted: {}",
                rebase.stderr.trim()
            ),
        ));
    }
    let failed = rerun_required_gates(shell, worktree).await?;
    if failed.is_empty() {
        Ok(())
    } else {
        Err((
            LandBlocked::TrunkMovedGatesFailed,
            format!(
                "trunk advanced during the run; the rebase was clean but required gate(s) failed on the rebased tree: {}",
                failed.join(", ")
            ),
        ))
    }
}

/// Re-run the required gates in `worktree`, returning the names of those that
/// failed.
///
/// Gates are read from the rebased tree, so a gate definition trunk changed is
/// the one that runs. Fix commands are not run: a fix would edit the tree
/// after the commit being landed, so only a gate that passes as-is counts.
async fn rerun_required_gates(
    shell: &dyn ShellGateway,
    worktree: &Path,
) -> Result<Vec<String>, (LandBlocked, String)> {
    let gates: Vec<GateDefinition> = crate::gate_file::read_gates(worktree)
        .map_err(|error| {
            (
                LandBlocked::TrunkMovedGatesFailed,
                format!("could not read the rebased tree's gates: {error}"),
            )
        })?
        .into_iter()
        .filter(|gate| gate.required)
        .map(|gate| GateDefinition {
            fix_command: None,
            ..gate
        })
        .collect();
    let run = crate::gate_runner::run_gates(&gates, worktree, shell)
        .await
        .map_err(|error| (LandBlocked::TrunkMovedGatesFailed, error.to_string()))?;
    Ok(run
        .results
        .into_iter()
        .filter(|gate| !gate.passed)
        .map(|gate| gate.name)
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

async fn cleanup_landed_branch(
    shell: &dyn ShellGateway,
    checkout: &Path,
    worktree: &Path,
    branch: &str,
) {
    remove_workspace(shell, checkout, worktree).await;
    // Best-effort: the branch has already been fast-forward merged onto
    // trunk; an undeleted local or remote branch is cosmetic and must not
    // fail an already-landed task.
    run_best_effort(shell, checkout, &["branch", "-d", branch]).await;
    run_best_effort(shell, checkout, &["push", "origin", "--delete", branch]).await;
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

fn task_summary(verdict: &TaskVerdict, landed: bool, arrivals: usize) -> String {
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
        format!(
            "{summary} after rebasing onto {arrivals} trunk commit(s) that arrived during the run"
        )
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
        trunk_arrivals: Vec::new(),
        verdict,
        context,
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

async fn commit_and_preserve_if_needed(
    shell: &dyn ShellGateway,
    checkout: &Path,
    worktree: &Path,
    project: &str,
    base_branch: &str,
    branch: &str,
    verdict: &TaskVerdict,
) -> Result<(bool, Option<String>)> {
    let _committed = commit_worktree(shell, worktree, project).await?;
    let deliverable = branch_has_deliverable(shell, checkout, base_branch, branch).await?;
    let reference = if deliverable || !verdict.is_complete() {
        Some(preserve(shell, worktree, project, branch).await?)
    } else {
        None
    };
    Ok((deliverable, reference))
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
            trunk_arrivals: Vec::new(),
            verdict,
            context: payload.context,
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
        let registry = Arc::clone(&self.registry);
        let shell = Arc::clone(&self.shell);

        Box::pin(async move {
            let entry = super::read_registry(&registry)?
                .find_project(&project)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("project '{project}' not found"))?;
            let context = payload.context.clone();
            let (worktree, branch) = match task_location(&context) {
                Ok(location) => location,
                Err(detail) => {
                    return terminal_result(throttle, &runner_error(&project, detail, context));
                }
            };
            let checkout = Path::new(&entry.path);

            let verdict = enforce_gate_truth(&payload);
            let (deliverable, preservation_ref) = match commit_and_preserve_if_needed(
                &*shell,
                checkout,
                &worktree,
                &project,
                &entry.branch,
                branch,
                &verdict,
            )
            .await
            {
                Ok(result) => result,
                Err(error) => {
                    return terminal_result(
                        throttle,
                        &runner_error(&project, error.to_string(), context),
                    );
                }
            };

            let mut arrivals = Vec::new();
            let landed = if may_land(&verdict, &payload.gate_results) && deliverable {
                let landing = land_with_rebase(
                    &*shell,
                    checkout,
                    &worktree,
                    &entry.branch,
                    branch,
                    entry.actions.push,
                )
                .await;
                arrivals = landing.arrivals;
                if let Some((reason, detail)) = landing.blocked {
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
                    return terminal_result(throttle, &result);
                }
                true
            } else {
                false
            };
            let preservation_ref = if landed {
                Some(landed_commit_ref(&*shell, checkout).await?)
            } else {
                preservation_ref
            };

            if success_needs_cleanup(&verdict, landed) {
                cleanup_landed_branch(&*shell, checkout, &worktree, branch).await;
            } else {
                remove_workspace(&*shell, checkout, &worktree).await;
            }
            let summary = task_summary(&verdict, landed, arrivals.len());
            let mut result =
                run_completed(&project, landed, summary, preservation_ref, verdict, context);
            result.trunk_arrivals = arrivals;
            terminal_result(throttle, &result)
        })
    }
}

/// A branch is disposable once it is merged, or once a complete run proved it
/// carried nothing to merge. Anything still holding unmerged work is preserved.
fn success_needs_cleanup(verdict: &TaskVerdict, landed: bool) -> bool {
    landed || verdict.is_complete()
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
    use foundry_sdk::payload::{LoopContext, TaskReviewedPayload, TaskVerdict};
    use foundry_sdk::registry::{ActionFlags, ProjectEntry, Stack};
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use super::super::test_helpers::git_repo::{CleanProcessShellGateway, git};
    use super::{FinalizeTask, enforce_gate_truth, may_land};

    /// Delegates to `CleanProcessShellGateway` for every command except
    /// worktree/branch cleanup commands, which it fails outright. Used to
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
            let is_cleanup = (args.first() == Some(&"worktree") && args.get(1) == Some(&"remove"))
                || (args.first() == Some(&"branch") && args.get(1) == Some(&"-d"))
                || (args.first() == Some(&"push") && args.get(2) == Some(&"--delete"));
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
            }],
            verdict: TaskVerdict::Complete,
            context: LoopContext::default(),
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
            git(&checkout, &["ls-remote", "--heads", "origin", branch]).is_empty(),
            "merged task branch should be deleted from origin"
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
            git(&checkout, &["ls-remote", "--heads", "origin", "foundry-task/landed-test"])
                .is_empty(),
            "landed task branch should be deleted from origin"
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
        std::fs::write(worktree.join("README.md"), "base\ntask change\n").unwrap();
        git(&worktree, &["add", "README.md"]);
        git(&worktree, &["commit", "-m", "task change"]);
        SharedTrunk {
            checkout,
            worktree,
            other,
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
        let registry =
            super::super::test_helpers::registry_with_entry(pushing_entry(&shared.checkout));
        let block =
            FinalizeTask::with_gateways(registry, std::sync::Arc::new(CleanProcessShellGateway));
        let trigger = task_trigger(&shared.worktree, branch, TaskVerdict::Complete);
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
            git(&shared.checkout, &["ls-remote", "--heads", "origin", branch]).is_empty(),
            "the landed task branch should be deleted from origin"
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
                "complete work preserved but could not land on trunk (trunk_moved_conflict)"
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
}
