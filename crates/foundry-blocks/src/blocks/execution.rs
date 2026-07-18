use std::path::{Path, PathBuf};

use foundry_sdk::gateway::AgentFailureMetadata;
use foundry_sdk::payload::{ExecutionCompletedPayload, LoopContext};
use foundry_sdk::registry::ProjectEntry;
use foundry_sdk::task_block::TaskBlockResult;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::workflow::WorkflowType;

use crate::gateway::{AgentGateway, AgentOutcome, ShellGateway};

use super::agent_helpers::{CodingAgentSpec, invoke_coding_agent};
use super::change_detection::{
    capture_pre_execution_sha, detect_post_execution_changes, only_auxiliary_changes,
};

/// Bundles the execution-context parameters shared across
/// [`build_agent_execution_result`], [`build_execution_outcome`], and
/// [`execute_agent_block`].
pub(crate) struct ExecutionContext<'a> {
    pub project: &'a str,
    pub workflow: WorkflowType,
    pub payload: &'a serde_json::Value,
    pub throttle: Throttle,
    pub label: &'a str,
    pub retry_count: Option<u64>,
    pub correction_needed: bool,
}

/// Build a `TaskBlockResult` for an agent-driven execution step, handling the
/// response match, output trimming to 200 lines, tracing, `LoopContext` extraction,
/// `ExecutionCompletedPayload` serialization, and `TaskBlockResult` construction.
///
/// `ctx.label` is the base text for the summary, e.g. "plan execution",
/// "maintenance", or "retry 2".  `ctx.retry_count` is forwarded into the payload
/// when present (retry flow only).
///
/// For the `Iterate` workflow only: if the agent exits 0 but produces no
/// meaningful working-tree changes, the result is overridden to `success: false`
/// with a "silent no-op" summary — unless `ctx.correction_needed` is `false`, in
/// which case a clean tree is a legitimate no-op (plan agent said no work needed)
/// and the result remains `success: true`.  The `Maintain` workflow is unaffected.
pub(crate) fn build_agent_execution_result(
    ctx: &ExecutionContext<'_>,
    outcome: AgentOutcome,
    changes_detected: bool,
    files_changed: Vec<String>,
) -> TaskBlockResult {
    let project = ctx.project;
    let workflow = ctx.workflow;
    let trigger_payload = ctx.payload;
    let throttle = ctx.throttle;
    let success_label = ctx.label;
    let retry_count = ctx.retry_count;
    let correction_needed = ctx.correction_needed;
    let (raw_output, mut success, mut summary, execution_output, failure) = match outcome {
        AgentOutcome::Success { stdout } => {
            let out = stdout.trim().to_string();
            let lines: Vec<&str> = out.lines().collect();
            let start = lines.len().saturating_sub(200);
            let trimmed_output = lines[start..].join("\n");
            let exec_out = if trimmed_output.is_empty() {
                None
            } else {
                Some(trimmed_output)
            };
            (Some(out), true, format!("{success_label} completed"), exec_out, None)
        }
        AgentOutcome::AgentFailed { stderr, failure } => {
            let summary = failure
                .as_ref()
                .filter(|failure| failure.is_terminal_provider_failure())
                .map_or_else(
                    || {
                        let first_line =
                            stderr.lines().next().unwrap_or("agent failed").to_string();
                        format!("{success_label} failed: {first_line}")
                    },
                    AgentFailureMetadata::execution_summary,
                );
            (Some(stderr), false, summary, None, failure)
        }
        AgentOutcome::Unavailable { error } => {
            (None, false, format!("agent unavailable: {error}"), None, None)
        }
    };

    // For iterate only: a clean (or all-auxiliary) working tree after agent
    // execution is either a silent flake or a legitimate no-op, depending on
    // what the plan agent told us.
    if workflow == WorkflowType::Iterate
        && success
        && (!changes_detected || only_auxiliary_changes(&files_changed))
    {
        if correction_needed {
            tracing::info!(
                project = %project,
                changes_detected = changes_detected,
                files_changed = ?files_changed,
                "iterate agent produced no meaningful changes — overriding to failure"
            );
            success = false;
            summary = "agent did not modify any files (silent no-op)".to_string();
        } else {
            tracing::info!(
                project = %project,
                "iterate plan concluded no correction needed; clean tree is a legitimate no-op"
            );
            summary = "no correction needed — codebase satisfies assessed principle".to_string();
        }
    }

    tracing::info!(project = %project, success = success, "{success_label} completed");

    let context = LoopContext::extract_from(trigger_payload);
    TaskBlockResult {
        events: vec![super::execution_completed_event(
            project,
            throttle,
            &ExecutionCompletedPayload {
                project: project.to_string(),
                workflow: workflow.to_string(),
                success,
                summary: summary.clone(),
                execution_output,
                dry_run: None,
                retry_count,
                changes_detected: Some(changes_detected),
                files_changed,
                failure: failure.unwrap_or_default(),
                context,
            },
        )],
        success,
        summary: format!("{project}: {summary}"),
        raw_output,
        ..Default::default()
    }
}

/// Detect post-execution changes and build an `ExecutionCompleted` result.
///
/// Combines [`detect_post_execution_changes`] and [`build_agent_execution_result`]
/// into a single call, eliminating the pattern duplicated by
/// `ExecutePlan`, `ExecuteMaintain`, and `RetryExecution`.
///
/// `pre_execution_sha` is the HEAD SHA captured immediately before agent
/// invocation (via [`capture_pre_execution_sha`]); pass `None` only when no
/// snapshot was taken.
pub(crate) async fn build_execution_outcome(
    shell: &dyn ShellGateway,
    project_path: &Path,
    ctx: &ExecutionContext<'_>,
    outcome: AgentOutcome,
    pre_execution_sha: Option<String>,
) -> foundry_sdk::task_block::TaskBlockResult {
    let (changes_detected, files_changed) =
        detect_post_execution_changes(shell, project_path, pre_execution_sha.as_deref()).await;
    build_agent_execution_result(ctx, outcome, changes_detected, files_changed)
}

/// Execute the common agent-driven body shared by `ExecutePlan`, `ExecuteMaintain`,
/// and `RetryExecution`: resolve the project path and agent file, capture the
/// pre-execution HEAD SHA, invoke the coding agent, and build the result.
pub(crate) async fn execute_agent_block(
    agent: &dyn AgentGateway,
    shell: &dyn ShellGateway,
    entry: &ProjectEntry,
    ctx: &ExecutionContext<'_>,
    prompt: String,
) -> TaskBlockResult {
    let project_path = PathBuf::from(&entry.path);
    let agent_file = super::resolve_agent_file(&entry.agent);
    let provider = super::chain_agent_provider(ctx.payload);
    let pre_sha = capture_pre_execution_sha(shell, &project_path).await;
    let outcome = invoke_coding_agent(
        agent,
        ctx.project,
        CodingAgentSpec {
            working_dir: project_path.clone(),
            prompt,
            agent_file,
            provider,
            env: execution_environment(ctx.workflow),
            timeout: entry.timeout(),
        },
        ctx.label,
    )
    .await;
    build_execution_outcome(shell, &project_path, ctx, outcome, pre_sha).await
}

/// Prevent an isolated task executor from pushing through the checkout's
/// configured `origin`. The environment is inherited by shell commands spawned
/// by every supported CLI agent, while Foundry's reviewer and finalizer run
/// outside it and retain normal Git access.
fn execution_environment(workflow: WorkflowType) -> Vec<(String, String)> {
    if workflow != WorkflowType::Task {
        return Vec::new();
    }

    vec![
        ("GIT_CONFIG_COUNT".to_string(), "1".to_string()),
        ("GIT_CONFIG_KEY_0".to_string(), "remote.origin.pushurl".to_string()),
        (
            "GIT_CONFIG_VALUE_0".to_string(),
            "foundry://task-executor-push-disabled".to_string(),
        ),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::workflow::WorkflowType;

    use crate::gateway::fakes::FakeShellGateway;
    use crate::gateway::{AgentFailureKind, AgentFailureMetadata, AgentOutcome, AgentProvider};
    use crate::shell::CommandResult;

    use super::{
        ExecutionContext, build_agent_execution_result, build_execution_outcome,
        execution_environment,
    };

    fn trigger_payload() -> serde_json::Value {
        serde_json::json!({ "project": "p", "workflow": "iterate" })
    }

    #[test]
    fn task_execution_disables_origin_push_for_agent_process() {
        let env = execution_environment(WorkflowType::Task);

        assert_eq!(env.len(), 3);
        assert!(env.contains(&("GIT_CONFIG_COUNT".to_string(), "1".to_string())));
        assert!(
            env.contains(&("GIT_CONFIG_KEY_0".to_string(), "remote.origin.pushurl".to_string()))
        );
        assert!(env.contains(&(
            "GIT_CONFIG_VALUE_0".to_string(),
            "foundry://task-executor-push-disabled".to_string()
        )));
    }

    #[test]
    fn non_task_execution_does_not_override_git_remote() {
        assert!(execution_environment(WorkflowType::Iterate).is_empty());
        assert!(execution_environment(WorkflowType::Maintain).is_empty());
    }

    // --- iterate: clean tree → failure override ---

    #[test]
    fn iterate_clean_tree_overrides_to_failure() {
        let payload = trigger_payload();
        let ctx = ExecutionContext {
            project: "proj",
            workflow: WorkflowType::Iterate,
            payload: &payload,
            throttle: Throttle::Full,
            label: "plan execution",
            retry_count: None,
            correction_needed: true,
        };
        let result = build_agent_execution_result(
            &ctx,
            AgentOutcome::Success {
                stdout: "done".to_string(),
            },
            false, // changes_detected = false
            vec![],
        );

        assert!(!result.success, "expected failure but got success");
        assert!(
            result.summary.contains("silent no-op"),
            "expected 'silent no-op' in summary, got: {}",
            result.summary
        );
        // Payload must also reflect the override
        assert_eq!(result.events[0].payload["success"], false);
        assert!(
            result.events[0].payload["summary"]
                .as_str()
                .unwrap_or("")
                .contains("silent no-op")
        );
    }

    // --- iterate: clean tree, correction_needed=false → legitimate no-op remains success ---

    #[test]
    fn iterate_clean_tree_no_correction_needed_remains_success() {
        let payload = trigger_payload();
        let ctx = ExecutionContext {
            project: "proj",
            workflow: WorkflowType::Iterate,
            payload: &payload,
            throttle: Throttle::Full,
            label: "plan execution",
            retry_count: None,
            correction_needed: false,
        };
        let result = build_agent_execution_result(
            &ctx,
            AgentOutcome::Success {
                stdout: "Reviewed; no changes needed.".to_string(),
            },
            false, // changes_detected = false
            vec![],
        );

        assert!(result.success, "expected success for legitimate no-op");
        assert!(
            result.summary.contains("no correction needed"),
            "expected 'no correction needed' in summary, got: {}",
            result.summary
        );
        assert_eq!(result.events[0].payload["success"], true);
    }

    // --- iterate: real changes → success unchanged ---

    #[test]
    fn iterate_dirty_tree_remains_success() {
        let payload = trigger_payload();
        let ctx = ExecutionContext {
            project: "proj",
            workflow: WorkflowType::Iterate,
            payload: &payload,
            throttle: Throttle::Full,
            label: "plan execution",
            retry_count: None,
            correction_needed: true,
        };
        let result = build_agent_execution_result(
            &ctx,
            AgentOutcome::Success {
                stdout: "done".to_string(),
            },
            true,
            vec!["src/lib.rs".to_string()],
        );

        assert!(result.success, "expected success but got failure");
        assert_eq!(result.events[0].payload["success"], true);
    }

    #[test]
    fn terminal_provider_failure_uses_account_limit_summary_and_payload() {
        let payload = trigger_payload();
        let ctx = ExecutionContext {
            project: "proj",
            workflow: WorkflowType::Iterate,
            payload: &payload,
            throttle: Throttle::Full,
            label: "plan execution",
            retry_count: None,
            correction_needed: true,
        };
        let failure = AgentFailureMetadata::new(AgentProvider::Claude)
            .terminal(AgentFailureKind::AccountLimit)
            .with_api_error_status(429)
            .with_message(
                "You've hit your monthly spend limit - raise it at claude.ai/settings/usage",
            );

        let result = build_agent_execution_result(
            &ctx,
            AgentOutcome::AgentFailed {
                stderr: String::new(),
                failure: Some(failure),
            },
            false,
            vec![],
        );

        assert!(!result.success);
        assert_eq!(
            result.events[0].payload["summary"],
            "agent account limit reached: You've hit your monthly spend limit - raise it at claude.ai/settings/usage"
        );
        assert_eq!(result.events[0].payload["failure_kind"], "account_limit");
        assert_eq!(result.events[0].payload["terminal"], true);
        assert_eq!(result.events[0].payload["api_error_status"], 429);
    }

    // --- maintain: clean tree → NOT overridden ---

    #[test]
    fn maintain_clean_tree_remains_success() {
        let payload = serde_json::json!({ "project": "p", "workflow": "maintain" });
        let ctx = ExecutionContext {
            project: "proj",
            workflow: WorkflowType::Maintain,
            payload: &payload,
            throttle: Throttle::Full,
            label: "maintenance",
            retry_count: None,
            correction_needed: true,
        };
        let result = build_agent_execution_result(
            &ctx,
            AgentOutcome::Success {
                stdout: "done".to_string(),
            },
            false, // clean tree
            vec![],
        );

        assert!(result.success, "maintain workflow must NOT override to failure on clean tree");
        assert_eq!(result.events[0].payload["success"], true);
    }

    // --- iterate: only auxiliary files → treated as no-op ---

    #[test]
    fn iterate_aux_only_changes_treated_as_clean() {
        let payload = trigger_payload();
        let ctx = ExecutionContext {
            project: "proj",
            workflow: WorkflowType::Iterate,
            payload: &payload,
            throttle: Throttle::Full,
            label: "plan execution",
            retry_count: None,
            correction_needed: true,
        };
        let result = build_agent_execution_result(
            &ctx,
            AgentOutcome::Success {
                stdout: "done".to_string(),
            },
            true, // changes_detected=true but only aux files
            vec![".claude/worktrees/abc/foo".to_string()],
        );

        assert!(!result.success, "all-auxiliary file list must trigger failure override");
        assert!(result.summary.contains("silent no-op"));
    }

    // --- build_execution_outcome: regression test for the production bug ---

    #[tokio::test]
    async fn build_execution_outcome_with_committed_changes_succeeds() {
        // Reproduces 2026-05-10 production bug: agent runs iterate, applies the plan,
        // commits and pushes, leaving the working tree clean. Pre-fix the detector
        // saw an empty `git status --porcelain` and mis-flagged this as a silent
        // no-op. Post-fix `git diff --name-only <pre_sha>` finds the committed
        // changes and the run succeeds.
        let shell = FakeShellGateway::always(CommandResult {
            stdout: "src/lib.rs\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let payload = trigger_payload();
        let ctx = ExecutionContext {
            project: "proj",
            workflow: WorkflowType::Iterate,
            payload: &payload,
            throttle: Throttle::Full,
            label: "plan execution",
            retry_count: None,
            correction_needed: true,
        };
        let result = build_execution_outcome(
            &*shell,
            Path::new("/tmp"),
            &ctx,
            AgentOutcome::Success {
                stdout: "done; commit pushed to origin/main".to_string(),
            },
            Some("abc123".to_string()),
        )
        .await;

        assert!(
            result.success,
            "iterate with committed changes must succeed, not be flagged silent no-op (got summary: {})",
            result.summary
        );
        assert!(
            !result.summary.contains("silent no-op"),
            "must not be flagged silent no-op when files changed since pre_sha"
        );
        assert_eq!(result.events[0].payload["changes_detected"], true);
    }
}
