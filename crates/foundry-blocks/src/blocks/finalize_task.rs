use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use foundry_sdk::event::{Event, EventType};
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
    checked(shell, worktree, &["bundle", "create", &bundle_text, branch]).await?;
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

async fn remove_workspace(shell: &dyn ShellGateway, checkout: &Path, worktree: &Path) {
    let worktree_text = worktree.to_string_lossy().to_string();
    let _ = run(shell, checkout, &["worktree", "remove", &worktree_text]).await;
}

async fn cleanup_landed_branch(
    shell: &dyn ShellGateway,
    checkout: &Path,
    worktree: &Path,
    branch: &str,
) {
    remove_workspace(shell, checkout, worktree).await;
    let _ = run(shell, checkout, &["branch", "-d", branch]).await;
    let _ = run(shell, checkout, &["push", "origin", "--delete", branch]).await;
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

fn terminal_result(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
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
            summary,
            preservation_ref,
            verdict,
            context,
        },
    )
}

async fn commit_and_preserve_if_needed(
    shell: &dyn ShellGateway,
    worktree: &Path,
    project: &str,
    branch: &str,
    verdict: &TaskVerdict,
) -> Result<(bool, Option<String>)> {
    let changed = commit_worktree(shell, worktree, project).await?;
    let reference = if changed || !verdict.is_complete() {
        Some(preserve(shell, worktree, project, branch).await?)
    } else {
        None
    };
    Ok((changed, reference))
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
            let worktree = context
                .task_worktree
                .as_deref()
                .map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("task finalization missing worktree"))?;
            let branch = context
                .task_branch
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("task finalization missing branch"))?;
            let checkout = Path::new(&entry.path);

            let verdict = enforce_gate_truth(&payload);
            let (changed, preservation_ref) =
                match commit_and_preserve_if_needed(&*shell, &worktree, &project, branch, &verdict)
                    .await
                {
                    Ok(result) => result,
                    Err(error) => {
                        return terminal_result(
                            &project,
                            throttle,
                            error.to_string(),
                            None,
                            TaskVerdict::RunnerError {
                                detail: error.to_string(),
                            },
                            context,
                        );
                    }
                };

            if verdict.is_complete()
                && changed
                && let Err(error) =
                    land_on_trunk(&*shell, checkout, &entry.branch, branch, entry.actions.push)
                        .await
            {
                remove_workspace(&*shell, checkout, &worktree).await;
                return terminal_result(
                    &project,
                    throttle,
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

            let success = verdict.is_complete();
            if success {
                cleanup_landed_branch(&*shell, checkout, &worktree, branch).await;
            } else {
                remove_workspace(&*shell, checkout, &worktree).await;
            }
            let summary = if success {
                "task completed, reviewed, and landed".to_string()
            } else {
                "task stopped with a typed non-complete verdict; work preserved".to_string()
            };
            terminal_result(&project, throttle, summary, preservation_ref, verdict, context)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::gates::GateResult;
    use foundry_sdk::payload::{LoopContext, TaskReviewedPayload, TaskVerdict};
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use super::{FinalizeTask, enforce_gate_truth};

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
        let output = Command::new("git").current_dir(cwd).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    #[tokio::test]
    async fn noncomplete_task_commits_and_pushes_before_removing_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let remote = dir.path().join("remote.git");
        let checkout = dir.path().join("checkout");
        let worktree = dir.path().join("worktree");
        git(dir.path(), &["init", "--bare", remote.to_str().unwrap()]);
        git(dir.path(), &["init", "-b", "main", checkout.to_str().unwrap()]);
        git(&checkout, &["config", "user.email", "foundry-test@example.com"]);
        git(&checkout, &["config", "user.name", "Foundry Test"]);
        std::fs::write(checkout.join("README.md"), "base\n").unwrap();
        git(&checkout, &["add", "README.md"]);
        git(&checkout, &["commit", "-m", "initial"]);
        git(&checkout, &["remote", "add", "origin", remote.to_str().unwrap()]);
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

        let registry =
            super::super::test_helpers::registry_with_project("p", checkout.to_str().unwrap());
        let block = FinalizeTask::new(registry);
        let trigger = Event::new(
            EventType::TaskReviewed,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&TaskReviewedPayload {
                project: "p".to_string(),
                objective: "finish".to_string(),
                review: "one gap".to_string(),
                gate_results: vec![],
                verdict: TaskVerdict::Remainder {
                    gaps: vec!["one gap".to_string()],
                },
                context: LoopContext {
                    task_worktree: Some(worktree.to_string_lossy().to_string()),
                    task_branch: Some("foundry-task/preserve-test".to_string()),
                    ..LoopContext::default()
                },
            })
            .unwrap(),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert_eq!(result.events[0].event_type, EventType::TaskRunCompleted);
        assert_eq!(result.events[0].payload["preservation_ref"], "foundry-task/preserve-test");
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
}
