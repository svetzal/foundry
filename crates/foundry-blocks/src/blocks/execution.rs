use std::path::{Path, PathBuf};

use foundry_sdk::gateway::AgentFailureMetadata;
use foundry_sdk::payload::{ExecutionCompletedPayload, LoopContext, RunBase};
use foundry_sdk::registry::ProjectEntry;
use foundry_sdk::task_block::TaskBlockResult;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::workflow::WorkflowType;

use crate::gateway::{AgentGateway, AgentOutcome, ShellGateway};

use super::agent_helpers::{CodingAgentSpec, invoke_coding_agent};
use super::branch_guard::BranchGuard;
use super::change_detection::{
    capture_pre_execution_sha, detect_post_execution_changes, only_auxiliary_changes,
};
use super::run_guard::{GuardedRun, OnSuppression, capture_run_base, guard_agent_run};

/// Bundles the execution-context parameters shared across
/// [`build_agent_execution_result`], [`build_execution_outcome`], and
/// [`execute_agent_block`].
pub(crate) struct ExecutionContext<'a> {
    pub project: &'a str,
    /// Trace this execution belongs to, forwarded into the agent session so
    /// its token spend lands in the right workflow.
    pub trace_id: Option<String>,
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
                        // Prefer a provider-reported failure message (e.g. codex's
                        // parsed `turn.failed`/`error` diagnostic) over stderr's
                        // first line: some CLIs (codex) write the same harmless,
                        // content-free line to stderr on every run regardless of
                        // outcome, so stderr alone can be worse than useless for a
                        // non-terminal failure.
                        let first_line = failure
                            .as_ref()
                            .and_then(|f| f.message.as_deref())
                            .map(str::trim)
                            .filter(|m| !m.is_empty())
                            .map_or_else(
                                || stderr.lines().next().unwrap_or("agent failed").to_string(),
                                str::to_string,
                            );
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
///
/// For every workflow whose commits Foundry pushes (maintain, iterate, task)
/// it also records where the run started (a retry inherits the first
/// attempt's `run_base`) and, after the agent, runs the direct-push check and
/// the suppression guard against that start (`run_guard`). Maintain also
/// puts the checkout back on its configured branch first.
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
    let on_suppression = suppression_policy(ctx.workflow);
    let run_base = match on_suppression {
        Some(_) => run_start(shell, &project_path, &entry.branch, ctx).await,
        None => None,
    };
    let pre_sha = capture_pre_execution_sha(shell, &project_path).await;
    let limits = LoopContext::extract_from(ctx.payload).campaign_limits;
    let mut env = execution_environment(ctx.workflow);
    env.push(("FOUNDRY_AGENT_STAGE".into(), "execution".into()));
    if let Some(campaign) = ctx.payload.get("campaign").and_then(serde_json::Value::as_str) {
        env.push(("FOUNDRY_CAMPAIGN".into(), campaign.into()));
        env.push(("FOUNDRY_WRITABLE_ROOT".into(), project_path.to_string_lossy().into()));
    }
    let timeout = limits.map_or(entry.timeout(), |l| {
        entry.timeout().min(std::time::Duration::from_secs(l.execution_seconds))
    });
    let outcome = invoke_coding_agent(
        agent,
        ctx.project,
        CodingAgentSpec {
            working_dir: project_path.clone(),
            prompt,
            agent_file,
            provider,
            env,
            timeout,
            trace_id: ctx.trace_id.clone(),
        },
        ctx.label,
    )
    .await;
    let Some(on_suppression) = on_suppression else {
        return build_execution_outcome(shell, &project_path, ctx, outcome, pre_sha).await;
    };
    let outcome = if ctx.workflow == WorkflowType::Maintain {
        guard_configured_branch(shell, &project_path, &entry.branch, outcome).await
    } else {
        outcome
    };
    let events_dir = foundry_sdk::paths::events_dir();
    let run = GuardedRun {
        project: &entry.name,
        path: &project_path,
        branch: &entry.branch,
        events_dir: &events_dir,
    };
    let outcome = guard_agent_run(shell, &run, run_base.as_ref(), on_suppression, outcome).await;
    // Carry the run's start forward so a retry checks against the same commit.
    let mut payload = ctx.payload.clone();
    if let (Some(base), Some(object)) = (&run_base, payload.as_object_mut()) {
        #[allow(
            clippy::expect_used,
            reason = "RunBase is two strings and infallibly serializable"
        )]
        let value = serde_json::to_value(base).expect("RunBase is infallibly serializable");
        object.insert("run_base".to_string(), value);
    }
    let ctx = ExecutionContext {
        payload: &payload,
        trace_id: ctx.trace_id.clone(),
        ..*ctx
    };
    build_execution_outcome(shell, &project_path, &ctx, outcome, pre_sha).await
}

/// Which workflows Foundry guards after the agent, and what a suppression
/// does to the run. `None` for workflows whose agent Foundry does not push
/// after. Maintain and iterate retry through `Route Gate Result`; a task has
/// no retry loop, and its reviewer must not be the one to accept a
/// suppression, so it stops for review.
fn suppression_policy(workflow: WorkflowType) -> Option<OnSuppression> {
    match workflow {
        WorkflowType::Maintain | WorkflowType::Iterate => Some(OnSuppression::Retry),
        WorkflowType::Task => Some(OnSuppression::Review),
        _ => None,
    }
}

/// Where this run started. A retry inherits the first attempt's start from
/// the chain; a first attempt records a fresh one, so a start carried along
/// by a loop's later iteration is never reused.
async fn run_start(
    shell: &dyn ShellGateway,
    project_path: &Path,
    branch: &str,
    ctx: &ExecutionContext<'_>,
) -> Option<RunBase> {
    let inherited = ctx.retry_count.and_then(|_| LoopContext::extract_from(ctx.payload).run_base);
    match inherited {
        Some(base) => Some(base),
        None => capture_run_base(shell, project_path, branch).await,
    }
}

/// Put the checkout back on its configured branch after the maintain agent,
/// or turn the run into a failure that says why it could not be.
///
/// Change detection runs afterwards and still sees the agent's work: it diffs
/// against the pre-agent SHA, and the fast-forward leaves `HEAD` on the same
/// commit.
async fn guard_configured_branch(
    shell: &dyn ShellGateway,
    project_path: &Path,
    branch: &str,
    outcome: AgentOutcome,
) -> AgentOutcome {
    match super::branch_guard::restore_configured_branch(shell, project_path, branch).await {
        BranchGuard::OnBranch | BranchGuard::Restored { .. } => outcome,
        BranchGuard::Unknown(reason) => {
            // Best-effort: the branch cannot be read (not a Git checkout, or
            // Git failed). Validation checks the branch before the next run,
            // so nothing is lost by not guarding here.
            tracing::warn!(%reason, "could not read the branch after the agent; branch guard skipped");
            outcome
        }
        BranchGuard::Failed(reason) => match outcome {
            AgentOutcome::AgentFailed { stderr, failure } => AgentOutcome::AgentFailed {
                stderr: format!("{reason}\n{stderr}"),
                failure,
            },
            AgentOutcome::Success { .. } => AgentOutcome::AgentFailed {
                stderr: reason,
                failure: None,
            },
            // The agent never ran, so it did not move the branch; its own
            // failure is the one to report.
            unavailable @ AgentOutcome::Unavailable { .. } => {
                tracing::warn!(%reason, "checkout is off its configured branch");
                unavailable
            }
        },
    }
}

/// Keep the agent from pushing through the checkout's `origin` in every
/// workflow whose commits Foundry pushes itself (see `push_guard`). The
/// environment is inherited by shell commands spawned by every supported CLI
/// agent, while Foundry's reviewer, finalizer and `Commit and Push` run
/// outside it and retain normal Git access.
fn execution_environment(workflow: WorkflowType) -> Vec<(String, String)> {
    if suppression_policy(workflow).is_some() {
        super::push_guard::push_disabled_environment()
    } else {
        Vec::new()
    }
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
    fn task_maintain_and_iterate_disable_origin_push_for_the_agent() {
        for workflow in [
            WorkflowType::Task,
            WorkflowType::Maintain,
            WorkflowType::Iterate,
        ] {
            let env = execution_environment(workflow);
            assert!(
                env.contains(&(
                    "GIT_CONFIG_KEY_0".to_string(),
                    "remote.origin.pushurl".to_string()
                )),
                "{workflow} must run the agent with pushing disabled"
            );
            assert!(env.contains(&(
                "GIT_CONFIG_VALUE_0".to_string(),
                crate::blocks::push_guard::PUSH_DISABLED_URL.to_string()
            )));
        }
    }

    #[test]
    fn workflows_foundry_does_not_push_keep_the_remote() {
        assert!(execution_environment(WorkflowType::Prompt).is_empty());
        assert!(execution_environment(WorkflowType::Scout).is_empty());
    }

    // --- iterate: clean tree → failure override ---

    #[test]
    fn iterate_clean_tree_overrides_to_failure() {
        let payload = trigger_payload();
        let ctx = ExecutionContext {
            trace_id: None,
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
            trace_id: None,
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
            trace_id: None,
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
            trace_id: None,
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

    /// A non-terminal failure (no `AgentFailureKind` classification — e.g.
    /// codex's parsed `turn.failed` diagnostic for an invalid model id) must
    /// still surface its `message` in the summary rather than falling back to
    /// stderr. Regression test for the 2026-09-30 `parite` incident, where
    /// codex's stderr is always the harmless "Reading additional input from
    /// stdin..." probe line regardless of outcome, so the old stderr-first-line
    /// fallback produced a summary with no diagnostic value.
    #[test]
    fn non_terminal_failure_with_a_message_uses_it_over_stderr() {
        let payload = trigger_payload();
        let ctx = ExecutionContext {
            trace_id: None,
            project: "proj",
            workflow: WorkflowType::Task,
            payload: &payload,
            throttle: Throttle::Full,
            label: "plan execution",
            retry_count: None,
            correction_needed: true,
        };
        let failure = AgentFailureMetadata::new(AgentProvider::Codex).with_message(
            "The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account.",
        );

        let result = build_agent_execution_result(
            &ctx,
            AgentOutcome::AgentFailed {
                stderr: "Reading additional input from stdin...".to_string(),
                failure: Some(failure),
            },
            false,
            vec![],
        );

        assert!(!result.success);
        assert_eq!(
            result.summary,
            "proj: plan execution failed: The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account."
        );
    }

    /// With no failure metadata at all (or a message-less one), the summary
    /// still falls back to stderr's first line — unchanged prior behavior for
    /// providers that don't populate `failure`.
    #[test]
    fn no_failure_metadata_falls_back_to_stderr_first_line() {
        let payload = trigger_payload();
        let ctx = ExecutionContext {
            trace_id: None,
            project: "proj",
            workflow: WorkflowType::Task,
            payload: &payload,
            throttle: Throttle::Full,
            label: "plan execution",
            retry_count: None,
            correction_needed: true,
        };

        let result = build_agent_execution_result(
            &ctx,
            AgentOutcome::AgentFailed {
                stderr: "boom\nsecond line".to_string(),
                failure: None,
            },
            false,
            vec![],
        );

        assert!(!result.success);
        assert_eq!(result.summary, "proj: plan execution failed: boom");
    }

    // --- maintain: clean tree → NOT overridden ---

    #[test]
    fn maintain_clean_tree_remains_success() {
        let payload = serde_json::json!({ "project": "p", "workflow": "maintain" });
        let ctx = ExecutionContext {
            trace_id: None,
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
            trace_id: None,
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
            trace_id: None,
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

    /// The maintain workflow's guard: a suppression is retried, a push stops.
    async fn guard_maintain_run(
        shell: &dyn crate::gateway::ShellGateway,
        path: &Path,
        entry: &foundry_sdk::registry::ProjectEntry,
        events_dir: &Path,
        base: Option<&foundry_sdk::payload::RunBase>,
        outcome: AgentOutcome,
    ) -> AgentOutcome {
        let run = crate::blocks::run_guard::GuardedRun {
            project: &entry.name,
            path,
            branch: &entry.branch,
            events_dir,
        };
        crate::blocks::run_guard::guard_agent_run(
            shell,
            &run,
            base,
            crate::blocks::run_guard::OnSuppression::Retry,
            outcome,
        )
        .await
    }

    #[tokio::test]
    async fn a_maintain_run_that_adds_a_suppression_fails_and_needs_review() {
        let diff = "+++ b/.supply-chain-allow.json\n+  {\"cve\": \"CVE-2026-64941\", \"reason\": \"no fix\"}\n";
        let shell = FakeShellGateway::always(CommandResult {
            stdout: diff.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let dir = tempfile::tempdir().unwrap();
        let entry = crate::blocks::test_helpers::project_entry(
            "ops-visualizer",
            dir.path().to_str().unwrap(),
        );
        let ok = crate::gateway::AgentOutcome::Success {
            stdout: "done".to_string(),
        };
        let outcome = guard_maintain_run(&*shell, dir.path(), &entry, dir.path(), None, ok).await;
        let crate::gateway::AgentOutcome::AgentFailed { stderr, .. } = outcome else {
            panic!("expected a failure");
        };
        assert!(stderr.starts_with("needs review: "), "{stderr}");
        assert!(stderr.contains("CVE-2026-64941"));
        let calls = shell.invocations();
        assert_eq!(
            calls[0].args,
            ["diff", "-U0", "--no-color", "origin/main"],
            "without a recorded start the guard falls back to origin/<branch>"
        );
    }

    #[tokio::test]
    async fn a_maintain_run_without_suppressions_is_unchanged() {
        let shell = FakeShellGateway::always(CommandResult {
            stdout: "+++ b/mix.lock\n+  \"jason\": {:hex, :jason, \"1.4.5\"},\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let dir = tempfile::tempdir().unwrap();
        let entry = crate::blocks::test_helpers::project_entry("p", dir.path().to_str().unwrap());
        let ok = crate::gateway::AgentOutcome::Success {
            stdout: "done".to_string(),
        };
        let outcome = guard_maintain_run(&*shell, dir.path(), &entry, dir.path(), None, ok).await;
        assert!(matches!(outcome, crate::gateway::AgentOutcome::Success { .. }));
    }

    // --- real Git: the agent pushes before Foundry's checks ---

    mod direct_push {
        use std::process::Command;

        use foundry_sdk::payload::RunBase;
        use foundry_sdk::throttle::Throttle;
        use foundry_sdk::workflow::WorkflowType;

        use crate::blocks::push_guard::{capture_run_base, push_disabled_environment};
        use crate::gateway::AgentOutcome;
        use crate::gateway::fakes::FakeAgentGateway;

        use super::super::{ExecutionContext, execute_agent_block};
        use super::guard_maintain_run;

        use crate::blocks::test_helpers::git_repo::{
            CleanProcessShellGateway, Repo, commit, git, repo,
        };

        const SUPPRESSION: &str =
            "[\n  {\"cve\": \"CVE-2026-64941\", \"reason\": \"no fix available\"}\n]\n";

        async fn guard(repo: &Repo, base: Option<&RunBase>, outcome: AgentOutcome) -> AgentOutcome {
            let shell = CleanProcessShellGateway;
            let entry = crate::blocks::test_helpers::project_entry(
                "mojentic-kt",
                repo.work.to_str().unwrap(),
            );
            let events = tempfile::tempdir().unwrap();
            guard_maintain_run(&shell, &repo.work, &entry, events.path(), base, outcome).await
        }

        fn success() -> AgentOutcome {
            AgentOutcome::Success {
                stdout: "done".to_string(),
            }
        }

        #[tokio::test]
        async fn a_suppression_the_agent_pushed_is_still_caught_and_the_run_needs_review() {
            let repo = repo();
            let base = capture_run_base(&CleanProcessShellGateway, &repo.work, "main")
                .await
                .expect("base recorded");
            // The agent commits a suppression and pushes it, as mojentic-kt's
            // agent pushed its own commit on 2026-09-29.
            commit(&repo.work, ".supply-chain-allow.json", SUPPRESSION, "chore: allow advisory");
            git(&repo.work, &["push", "-q", "origin", "main"]);

            let outcome = guard(&repo, Some(&base), success()).await;

            let AgentOutcome::AgentFailed { stderr, failure } = outcome else {
                panic!("a direct push must fail the run");
            };
            assert!(
                stderr.starts_with("needs review: agent pushed directly to origin/main"),
                "{stderr}"
            );
            assert!(stderr.contains("chore: allow advisory"), "names the pushed commit: {stderr}");
            assert!(
                stderr.contains("CVE-2026-64941"),
                "the suppression check still ran over the pushed commit: {stderr}"
            );
            let failure = failure.expect("failure metadata");
            assert!(failure.needs_review.is_some(), "the run stops for review, no retry");
            assert!(!failure.is_terminal_provider_failure());
        }

        #[tokio::test]
        async fn a_clean_direct_push_still_needs_review() {
            let repo = repo();
            let base =
                capture_run_base(&CleanProcessShellGateway, &repo.work, "main").await.unwrap();
            commit(
                &repo.work,
                "build.gradle.kts",
                "kover 0.9.10",
                "chore(deps): Kover 0.9.9 -> 0.9.10",
            );
            git(&repo.work, &["push", "-q", "origin", "main"]);

            let AgentOutcome::AgentFailed { stderr, failure } =
                guard(&repo, Some(&base), success()).await
            else {
                panic!("a direct push must fail the run");
            };
            assert!(stderr.contains("Kover 0.9.9 -> 0.9.10"), "{stderr}");
            assert!(stderr.contains("found no new suppressions"), "{stderr}");
            assert!(failure.unwrap().needs_review.is_some());
        }

        #[tokio::test]
        async fn a_push_to_an_explicit_url_is_seen_after_the_fetch() {
            let repo = repo();
            let base =
                capture_run_base(&CleanProcessShellGateway, &repo.work, "main").await.unwrap();
            commit(&repo.work, "a.txt", "a", "chore: bump");
            // Bypasses origin's push URL and does not move the tracking ref.
            git(&repo.work, &["push", "-q", repo.remote.to_str().unwrap(), "main"]);

            let outcome = guard(&repo, Some(&base), success()).await;
            let AgentOutcome::AgentFailed { stderr, .. } = outcome else {
                panic!("an explicit-URL push must fail the run");
            };
            assert!(stderr.contains("pushed directly"), "{stderr}");
        }

        #[tokio::test]
        async fn local_commits_are_not_a_push_and_a_local_suppression_is_retried() {
            let repo = repo();
            let base =
                capture_run_base(&CleanProcessShellGateway, &repo.work, "main").await.unwrap();
            commit(&repo.work, ".supply-chain-allow.json", SUPPRESSION, "chore: allow advisory");

            let AgentOutcome::AgentFailed { stderr, failure } =
                guard(&repo, Some(&base), success()).await
            else {
                panic!("a suppression fails the run");
            };
            assert!(stderr.starts_with("needs review: this run added"), "{stderr}");
            assert!(failure.is_none(), "not a review stop: a retry may remove the suppression");
        }

        #[tokio::test]
        async fn a_retry_checks_against_the_runs_start_not_its_own() {
            let repo = repo();
            let base =
                capture_run_base(&CleanProcessShellGateway, &repo.work, "main").await.unwrap();
            // Attempt 1 committed a suppression locally and failed; the retry
            // starts after it. The guard must still see it.
            commit(&repo.work, ".supply-chain-allow.json", SUPPRESSION, "chore: allow advisory");
            let payload = serde_json::json!({
                "project": "mojentic-kt",
                "workflow": "maintain",
                "run_base": serde_json::to_value(&base).unwrap(),
            });
            let ctx = ExecutionContext {
                trace_id: None,
                project: "mojentic-kt",
                workflow: WorkflowType::Maintain,
                payload: &payload,
                throttle: Throttle::Full,
                label: "retry 1",
                retry_count: Some(1),
                correction_needed: true,
            };
            let entry = crate::blocks::test_helpers::project_entry(
                "mojentic-kt",
                repo.work.to_str().unwrap(),
            );
            let agent = FakeAgentGateway::success();
            let result = execute_agent_block(
                &*agent,
                &CleanProcessShellGateway,
                &entry,
                &ctx,
                String::new(),
            )
            .await;
            assert!(!result.success, "{}", result.summary);
            assert!(result.summary.contains("CVE-2026-64941"), "{}", result.summary);
            assert_eq!(
                result.events[0].payload["run_base"]["head"], base.head,
                "the run's start is carried forward unchanged"
            );
        }

        #[tokio::test]
        async fn the_first_attempt_records_the_runs_start_for_retries() {
            let repo = repo();
            let head = git(&repo.work, &["rev-parse", "HEAD"]);
            let payload = serde_json::json!({ "project": "p", "workflow": "maintain" });
            let ctx = ExecutionContext {
                trace_id: None,
                project: "p",
                workflow: WorkflowType::Maintain,
                payload: &payload,
                throttle: Throttle::Full,
                label: "maintenance",
                retry_count: None,
                correction_needed: true,
            };
            let entry =
                crate::blocks::test_helpers::project_entry("p", repo.work.to_str().unwrap());
            let agent = FakeAgentGateway::success();
            let result = execute_agent_block(
                &*agent,
                &CleanProcessShellGateway,
                &entry,
                &ctx,
                String::new(),
            )
            .await;
            assert!(result.success, "{}", result.summary);
            let recorded = &result.events[0].payload["run_base"];
            assert_eq!(recorded["head"], head);
            assert_eq!(recorded["origin"], head);
        }

        fn allowlisting_agent()
        -> std::sync::Arc<crate::blocks::test_helpers::ActingAgent<fn(&std::path::Path)>> {
            fn act(dir: &std::path::Path) {
                std::fs::write(dir.join(".supply-chain-allow.json"), SUPPRESSION).unwrap();
            }
            std::sync::Arc::new(crate::blocks::test_helpers::ActingAgent {
                act: act as fn(&std::path::Path),
            })
        }

        fn context(
            workflow: WorkflowType,
            payload: &serde_json::Value,
            retry_count: Option<u64>,
        ) -> ExecutionContext<'_> {
            ExecutionContext {
                trace_id: None,
                project: "p",
                workflow,
                payload,
                throttle: Throttle::Full,
                label: "plan execution",
                retry_count,
                correction_needed: true,
            }
        }

        #[tokio::test]
        async fn an_iterate_agent_that_adds_a_suppression_fails_the_attempt() {
            let repo = repo();
            let entry =
                crate::blocks::test_helpers::project_entry("p", repo.work.to_str().unwrap());
            let payload = serde_json::json!({ "project": "p", "workflow": "iterate" });
            let ctx = context(WorkflowType::Iterate, &payload, None);

            let result = execute_agent_block(
                &*allowlisting_agent(),
                &CleanProcessShellGateway,
                &entry,
                &ctx,
                String::new(),
            )
            .await;

            assert!(!result.success, "{}", result.summary);
            let payload = &result.events[0].payload;
            assert!(payload["summary"].as_str().unwrap().contains("CVE-2026-64941"), "{payload}");
            assert!(
                payload.get("needs_review").is_none(),
                "iterate retries: the retry is told to remove it"
            );
        }

        #[tokio::test]
        async fn a_task_agent_that_adds_a_suppression_stops_for_review() {
            let repo = repo();
            let entry =
                crate::blocks::test_helpers::project_entry("p", repo.work.to_str().unwrap());
            let payload = serde_json::json!({ "project": "p", "workflow": "task" });
            let ctx = context(WorkflowType::Task, &payload, None);

            let result = execute_agent_block(
                &*allowlisting_agent(),
                &CleanProcessShellGateway,
                &entry,
                &ctx,
                String::new(),
            )
            .await;

            assert!(!result.success, "{}", result.summary);
            let reason = result.events[0].payload["needs_review"].as_str().expect("needs_review");
            assert!(reason.contains("CVE-2026-64941"), "{reason}");
        }

        #[tokio::test]
        async fn a_suppression_an_earlier_run_left_unpushed_is_caught_by_the_next_run() {
            let repo = repo();
            // An earlier run was stopped for this commit, so it stayed local.
            commit(&repo.work, ".supply-chain-allow.json", SUPPRESSION, "chore: allow advisory");
            let base =
                capture_run_base(&CleanProcessShellGateway, &repo.work, "main").await.unwrap();
            // This run's agent changes something unrelated.
            commit(&repo.work, "a.txt", "a", "chore: tidy");

            let outcome = guard(&repo, Some(&base), success()).await;

            let AgentOutcome::AgentFailed { stderr, .. } = outcome else {
                panic!("Commit and Push would push the stranded suppression; the run must fail");
            };
            assert!(stderr.contains("CVE-2026-64941"), "{stderr}");
        }

        #[tokio::test]
        async fn a_first_attempt_records_its_own_start_even_when_one_is_carried() {
            let repo = repo();
            let head = git(&repo.work, &["rev-parse", "HEAD"]);
            // A loop carries the previous iteration's context forward.
            let payload = serde_json::json!({
                "project": "p",
                "workflow": "iterate",
                "run_base": { "head": "0000000000000000000000000000000000000000" },
            });
            let ctx = context(WorkflowType::Iterate, &payload, None);
            let entry =
                crate::blocks::test_helpers::project_entry("p", repo.work.to_str().unwrap());
            let agent = FakeAgentGateway::success();

            let result = execute_agent_block(
                &*agent,
                &CleanProcessShellGateway,
                &entry,
                &ctx,
                String::new(),
            )
            .await;

            assert_eq!(result.events[0].payload["run_base"]["head"], head);
        }

        #[test]
        fn the_push_disabled_environment_blocks_a_plain_git_push() {
            let repo = repo();
            commit(&repo.work, "a.txt", "a", "local only");
            let out = Command::new("git")
                .current_dir(&repo.work)
                .args(["push", "-q", "origin", "main"])
                .envs(push_disabled_environment())
                .output()
                .unwrap();
            assert!(!out.status.success(), "the agent's push must fail");
            let remote_head = git(&repo.remote, &["rev-parse", "main"]);
            let local_head = git(&repo.work, &["rev-parse", "HEAD"]);
            assert_ne!(remote_head, local_head, "nothing reached origin");
            // Foundry's own push, outside the agent's environment, still works.
            git(&repo.work, &["push", "-q", "origin", "main"]);
            assert_eq!(git(&repo.remote, &["rev-parse", "main"]), local_head);
        }
    }
}
