use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gates::GateResult;
use foundry_sdk::payload::{
    LoopContext, TaskReviewedPayload, TaskRunCompletedPayload, TaskVerdict,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock};

use crate::gateway::{ProcessShellGateway, ShellGateway};

use super::SimulatedSuccess;

task_block_new! {
    /// Commits every task result before terminal state, pushes non-complete work
    /// to a durable preservation ref (or bundle), and fast-forwards complete
    /// work onto the project's trunk branch.
    pub struct FinalizeTask {
        shell: ShellGateway = ProcessShellGateway
    }
}

async fn run(
    shell: &dyn ShellGateway,
    cwd: &Path,
    args: &[&str],
) -> Result<crate::shell::CommandResult> {
    shell.run(cwd, "git", args, None, None).await
}

async fn checked(shell: &dyn ShellGateway, cwd: &Path, args: &[&str]) -> Result<String> {
    let result = run(shell, cwd, args).await?;
    if !result.success {
        bail!("git {} failed ({}): {}", args.join(" "), result.exit_code, result.stderr.trim());
    }
    Ok(result.stdout.trim().to_string())
}

async fn commit_worktree(shell: &dyn ShellGateway, worktree: &Path, project: &str) -> Result<bool> {
    let status = checked(shell, worktree, &["status", "--porcelain"]).await?;
    if status.is_empty() {
        return Ok(false);
    }
    checked(shell, worktree, &["add", "-A"]).await?;
    checked(shell, worktree, &["commit", "-m", &format!("feat({project}): automated task")])
        .await?;
    Ok(true)
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

async fn preserve(
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

async fn land_on_trunk(
    shell: &dyn ShellGateway,
    checkout: &Path,
    base_branch: &str,
    task_branch: &str,
    push_enabled: bool,
) -> Result<()> {
    let dirty = checked(shell, checkout, &["status", "--porcelain"]).await?;
    if !dirty.is_empty() {
        bail!("registered checkout is dirty; preserved task branch instead of risking user work");
    }
    let current = checked(shell, checkout, &["branch", "--show-current"]).await?;
    if current != base_branch {
        bail!("registered checkout is on '{current}', expected '{base_branch}'");
    }
    if push_enabled {
        checked(shell, checkout, &["pull", "--ff-only", "origin", base_branch]).await?;
    }
    checked(shell, checkout, &["merge", "--ff-only", task_branch]).await?;
    if push_enabled {
        checked(shell, checkout, &["push", "origin", base_branch]).await?;
    }
    Ok(())
}

/// Run a git command whose outcome is cleanup, not correctness: log a failure
/// (spawn error or non-zero exit) rather than discarding it.
async fn run_best_effort(shell: &dyn ShellGateway, cwd: &Path, args: &[&str]) {
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

async fn remove_workspace(shell: &dyn ShellGateway, checkout: &Path, worktree: &Path) {
    let worktree_text = worktree.to_string_lossy().to_string();
    // Best-effort: a task already landed (or was preserved) by the time we
    // reach here; a leaked worktree is cleaned up on a later run and must
    // not fail an already-completed task.
    run_best_effort(shell, checkout, &["worktree", "remove", &worktree_text]).await;
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

fn task_summary(verdict: &TaskVerdict, landed: bool) -> String {
    match (landed, verdict) {
        (true, TaskVerdict::Remainder { gaps }) => {
            format!("converging task landed on trunk with {} gap(s) carried forward", gaps.len())
        }
        (true, _) => "task completed, reviewed, and landed".to_string(),
        (false, verdict) if verdict.is_complete() => {
            "task completed, reviewed, and required no landing".to_string()
        }
        (false, _) => "task stopped with a typed non-complete verdict; work preserved".to_string(),
    }
}

fn terminal_result(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    landed: bool,
    summary: String,
    preservation_ref: Option<String>,
    verdict: TaskVerdict,
    context: LoopContext,
) -> anyhow::Result<foundry_sdk::task_block::TaskBlockResult> {
    super::emit_event_result(
        format!("{project}: {summary}"),
        verdict.is_complete(),
        EventType::TaskRunCompleted,
        project,
        throttle,
        &TaskRunCompletedPayload {
            project: project.to_string(),
            success: verdict.is_complete(),
            landed,
            summary,
            preservation_ref,
            verdict,
            context,
        },
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
                    return terminal_result(
                        &project,
                        throttle,
                        false,
                        detail.clone(),
                        None,
                        TaskVerdict::RunnerError { detail },
                        context,
                    );
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
                        &project,
                        throttle,
                        false,
                        error.to_string(),
                        None,
                        TaskVerdict::RunnerError {
                            detail: error.to_string(),
                        },
                        context,
                    );
                }
            };

            let landed = if may_land(&verdict, &payload.gate_results) && deliverable {
                if let Err(error) =
                    land_on_trunk(&*shell, checkout, &entry.branch, branch, entry.actions.push)
                        .await
                {
                    remove_workspace(&*shell, checkout, &worktree).await;
                    return terminal_result(
                        &project,
                        throttle,
                        false,
                        "complete work preserved but could not land on trunk".to_string(),
                        preservation_ref,
                        TaskVerdict::Defect {
                            diagnosis: format!(
                                "task passed review but could not land on trunk: {error}"
                            ),
                        },
                        context,
                    );
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
            let summary = task_summary(&verdict, landed);
            terminal_result(&project, throttle, landed, summary, preservation_ref, verdict, context)
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

    use super::{FinalizeTask, enforce_gate_truth, may_land};

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

    fn git(cwd: &std::path::Path, args: &[&str]) -> String {
        let mut command = Command::new("git");
        command.current_dir(cwd).args(args);
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
        assert_eq!(result.events[0].payload["landed"], false);
        assert_eq!(
            result.events[0].payload["summary"],
            "complete work preserved but could not land on trunk"
        );
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
}
