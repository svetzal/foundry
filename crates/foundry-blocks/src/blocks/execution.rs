use std::path::{Path, PathBuf};

use foundry_sdk::gateway::{AgentFailureMetadata, AgentProvider};
use foundry_sdk::payload::{
    ExecutionCompletedPayload, ExecutionSessionFailure, LoopContext, RunBase,
};
use foundry_sdk::registry::ProjectEntry;
use foundry_sdk::task_block::TaskBlockResult;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::workflow::WorkflowType;

use crate::gateway::{AgentGateway, AgentOutcome, ShellGateway, Sleeper};

use super::agent_helpers::{CodingAgentSpec, invoke_coding_agent};
use super::branch_guard::BranchGuard;
use super::change_detection::{
    capture_pre_execution_sha, changes_project_files, detect_post_execution_changes,
    only_auxiliary_changes,
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

/// How the agent session itself ended, before Foundry's own checks after it.
///
/// Those checks can turn a finished session into a failed run (iterate's
/// silent no-op, a suppression to retry, a branch that could not be put
/// back), so the run's outcome alone cannot say whether the session failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionEnd {
    /// The agent finished its turn.
    Finished,
    /// The provider reported a failure, or the agent could not run at all.
    Failed,
}

impl SessionEnd {
    /// How the session that produced `outcome` ended. Pass the outcome the
    /// gateway returned, before any post-agent guard rewrote it.
    pub(crate) fn of(outcome: &AgentOutcome) -> Self {
        match outcome {
            AgentOutcome::Success { .. } => Self::Finished,
            AgentOutcome::AgentFailed { .. } | AgentOutcome::Unavailable { .. } => Self::Failed,
        }
    }
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
///
/// When `session` is [`SessionEnd::Failed`], the payload carries an
/// [`ExecutionSessionFailure`] with the summary and whether the session
/// changed project files, so review can settle a session that did no work
/// as a runner error instead of asking for its proof.
pub(crate) fn build_agent_execution_result(
    ctx: &ExecutionContext<'_>,
    outcome: AgentOutcome,
    session: SessionEnd,
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
            // A classified provider failure (terminal, or provider
            // unavailable) names itself: "provider unavailable: <message>".
            let summary =
                failure.as_ref().filter(|failure| failure.failure_kind.is_some()).map_or_else(
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

    let session_failure = (session == SessionEnd::Failed).then(|| ExecutionSessionFailure {
        message: summary.clone(),
        left_work: changes_detected && changes_project_files(&files_changed),
    });
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
                session_failure,
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
/// snapshot was taken. `session` is how the agent session ended, taken from
/// the gateway's outcome before any guard rewrote it.
pub(crate) async fn build_execution_outcome(
    shell: &dyn ShellGateway,
    project_path: &Path,
    ctx: &ExecutionContext<'_>,
    outcome: AgentOutcome,
    session: SessionEnd,
    pre_execution_sha: Option<String>,
) -> foundry_sdk::task_block::TaskBlockResult {
    let (changes_detected, files_changed) =
        detect_post_execution_changes(shell, project_path, pre_execution_sha.as_deref()).await;
    build_agent_execution_result(ctx, outcome, session, changes_detected, files_changed)
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
    sleeper: &dyn Sleeper,
    entry: &ProjectEntry,
    ctx: &ExecutionContext<'_>,
    prompt: String,
) -> TaskBlockResult {
    execute_agent_block_in_store(
        agent,
        shell,
        sleeper,
        entry,
        ctx,
        prompt,
        &foundry_sdk::paths::campaigns_path(),
    )
    .await
}

async fn execute_agent_block_in_store(
    agent: &dyn AgentGateway,
    shell: &dyn ShellGateway,
    sleeper: &dyn Sleeper,
    entry: &ProjectEntry,
    ctx: &ExecutionContext<'_>,
    prompt: String,
    store_path: &Path,
) -> TaskBlockResult {
    match initiative_stop_reason(ctx.payload, store_path) {
        Ok(Some(reason)) => return stopped_before_agent(ctx, reason),
        Err(error) => {
            return stopped_before_agent(
                ctx,
                format!("initiative execution guard failed: {error:#}"),
            );
        }
        Ok(None) => {}
    }
    let project_path = PathBuf::from(&entry.path);
    let agent_file = super::resolve_agent_file(&entry.agent);
    let provider =
        super::chain_stage_provider(ctx.payload, foundry_sdk::gateway::AgentStage::Execution);
    let prompt = super::with_initiative_context(&prompt, ctx.payload);
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
    }
    if ctx.payload.get("campaign").and_then(serde_json::Value::as_str).is_some()
        || ctx.payload.get("initiative").and_then(serde_json::Value::as_str).is_some()
    {
        env.push(("FOUNDRY_WRITABLE_ROOT".into(), project_path.to_string_lossy().into()));
    }
    let timeout = limits.map_or(entry.timeout(), |_| std::time::Duration::ZERO);
    let outcome = invoke_while_provider_unavailable(
        agent,
        sleeper,
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
    let session = SessionEnd::of(&outcome);
    let Some(on_suppression) = on_suppression else {
        return build_execution_outcome(shell, &project_path, ctx, outcome, session, pre_sha).await;
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
    build_execution_outcome(shell, &project_path, &ctx, outcome, session, pre_sha).await
}

/// The pauses before each new attempt when the provider refused the agent
/// session as unavailable (a model at capacity, an overloaded or rate-limited
/// API). Four attempts in all, the last one about seven and a half minutes
/// after the first; an outage that outlasts them settles the run as
/// `provider unavailable` and is left to the caller (a campaign re-dispatches
/// the same objective later).
pub(crate) const PROVIDER_RETRY_DELAYS: [std::time::Duration; 3] = [
    std::time::Duration::from_secs(30),
    std::time::Duration::from_secs(120),
    std::time::Duration::from_secs(300),
];

fn provider_unavailable(outcome: &AgentOutcome) -> bool {
    matches!(
        outcome,
        AgentOutcome::AgentFailed { failure: Some(failure), .. } if failure.is_provider_unavailable()
    )
}

/// Invoke the coding agent, and while the provider refuses the session as
/// unavailable, pause through `sleeper` and try again, up to
/// [`PROVIDER_RETRY_DELAYS`]. A refusal that outlasts every pause comes back
/// as the last attempt's outcome, its message noting the attempts.
async fn invoke_while_provider_unavailable(
    agent: &dyn AgentGateway,
    sleeper: &dyn Sleeper,
    project: &str,
    spec: CodingAgentSpec,
    label: &str,
) -> AgentOutcome {
    let mut attempts = 1;
    let provider = spec.provider;
    loop {
        let outcome = recognise_provider_unavailable(
            invoke_coding_agent(agent, project, spec.clone(), label).await,
            provider,
        );
        if !provider_unavailable(&outcome) {
            return outcome;
        }
        let Some(pause) = PROVIDER_RETRY_DELAYS.get(attempts - 1) else {
            return note_attempts(outcome, attempts);
        };
        tracing::warn!(
            project,
            attempt = attempts,
            pause_secs = pause.as_secs(),
            "agent provider unavailable; trying again after a pause"
        );
        sleeper.sleep(*pause).await;
        attempts += 1;
    }
}

/// The session's outcome, classified as provider unavailable when the gateway
/// left it unclassified but its own words say so.
///
/// Gateways classify what they recognise in their CLI's output. A failure
/// they did not classify (a message with no failure kind, or none at all),
/// and an agent that could not run (`Unavailable`, which carries no failure
/// metadata), reach here unclassified. When that text names a model at
/// capacity, an overload or a rate limit ([`classify_provider_failure`]),
/// the session is treated as the provider refusing it: the retries below
/// apply, the run ends `provider unavailable`, and a campaign repeats the
/// cycle instead of reading it as a failed objective. Only that transient
/// kind is taken from here; terminal account states stay the gateways'
/// call, made on their structured output.
///
/// [`classify_provider_failure`]: foundry_sdk::gateway::classify_provider_failure
fn recognise_provider_unavailable(
    outcome: AgentOutcome,
    provider: Option<AgentProvider>,
) -> AgentOutcome {
    match outcome {
        AgentOutcome::AgentFailed { stderr, failure }
            if failure.as_ref().is_none_or(|failure| failure.failure_kind.is_none()) =>
        {
            let wording = failure
                .as_ref()
                .and_then(|failure| failure.message.as_deref())
                .filter(|message| !message.trim().is_empty())
                .unwrap_or(&stderr);
            let failure =
                provider_unavailable_failure(wording, provider, failure.as_ref()).or(failure);
            AgentOutcome::AgentFailed { stderr, failure }
        }
        AgentOutcome::Unavailable { error } => {
            match provider_unavailable_failure(&error, provider, None) {
                Some(failure) => AgentOutcome::AgentFailed {
                    stderr: error,
                    failure: Some(failure),
                },
                None => AgentOutcome::Unavailable { error },
            }
        }
        other => other,
    }
}

/// `known` with the provider-unavailable classification of `wording`, or
/// `None` when the wording does not name one.
fn provider_unavailable_failure(
    wording: &str,
    provider: Option<AgentProvider>,
    known: Option<&AgentFailureMetadata>,
) -> Option<AgentFailureMetadata> {
    let provider = known.and_then(|known| known.provider).or(provider);
    let classified = foundry_sdk::gateway::classify_provider_failure(
        provider.unwrap_or_default(),
        known.and_then(|known| known.api_error_status),
        wording,
        None,
        None,
    )
    .filter(AgentFailureMetadata::is_provider_unavailable)?;
    let mut failure = known.cloned().unwrap_or_default();
    failure.provider = provider;
    failure.failure_kind = classified.failure_kind;
    failure.terminal = classified.terminal;
    failure.api_error_status = classified.api_error_status;
    failure.message = classified.message;
    Some(failure)
}

/// Add the number of attempts to a provider-unavailable failure's message.
fn note_attempts(outcome: AgentOutcome, attempts: usize) -> AgentOutcome {
    match outcome {
        AgentOutcome::AgentFailed {
            stderr,
            failure: Some(mut failure),
        } => {
            let message = failure.message.take().unwrap_or_default();
            failure.message = Some(format!("{} ({attempts} attempts)", message.trim()));
            AgentOutcome::AgentFailed {
                stderr,
                failure: Some(failure),
            }
        }
        other => other,
    }
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

/// Refuse queued work after its initiative ends, before invoking an agent.
fn initiative_stop_reason(
    payload: &serde_json::Value,
    store_path: &Path,
) -> anyhow::Result<Option<String>> {
    use anyhow::Context as _;
    use foundry_sdk::{campaign::CampaignStore, initiative::InitiativeStatus};
    let Some(name) = payload.get("initiative").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let store = CampaignStore::load(store_path)
        .context("cannot read initiative state before agent execution")?;
    let initiative = store
        .find_initiative(name)
        .with_context(|| format!("initiative '{name}' is missing before agent execution"))?;
    Ok(match initiative.status {
        InitiativeStatus::Completed | InitiativeStatus::Cancelled => Some(format!(
            "initiative '{name}' is {}; queued member task skipped before agent execution",
            initiative.status
        )),
        _ => None,
    })
}

fn stopped_before_agent(ctx: &ExecutionContext<'_>, reason: String) -> TaskBlockResult {
    use foundry_sdk::{
        event::EventType,
        payload::{TaskRunCompletedPayload, TaskVerdict},
    };
    let summary = format!("{}: {reason}", ctx.project);
    let payload = TaskRunCompletedPayload {
        project: ctx.project.into(),
        success: false,
        landed: false,
        summary: reason.clone(),
        preservation_ref: None,
        land_blocked: None,
        trunk_arrivals: Vec::new(),
        proof_evidence: None,
        uncommitted_artifacts: None,
        verdict: TaskVerdict::RunnerError { detail: reason },
        context: LoopContext::extract_from(ctx.payload),
        failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
    };
    TaskBlockResult {
        success: false,
        summary,
        events: vec![super::event_from_infallible_payload(
            EventType::TaskRunCompleted,
            ctx.project,
            ctx.throttle,
            &payload,
        )],
        ..TaskBlockResult::default()
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
        ExecutionContext, SessionEnd, build_agent_execution_result, build_execution_outcome,
        execution_environment,
    };

    const CAPACITY: &str = "Selected model is at capacity. Please try a different model.";

    /// Run the execution body as `ExecutePlan` does, for an unguarded
    /// workflow, over `agent` and a recording sleeper.
    async fn execute_with(
        agent: &crate::gateway::fakes::FakeAgentGateway,
        sleeper: &crate::gateway::fakes::RecordingSleeper,
    ) -> foundry_sdk::task_block::TaskBlockResult {
        let shell = crate::blocks::test_helpers::passing_shell();
        let entry = crate::blocks::test_helpers::project_entry("p", "/repo/worktree");
        let payload =
            serde_json::json!({"project":"p", "workflow":"prompt", "agent_provider":"codex"});
        let context = ExecutionContext {
            project: "p",
            trace_id: None,
            workflow: WorkflowType::Prompt,
            payload: &payload,
            throttle: Throttle::Full,
            label: "execution",
            retry_count: None,
            correction_needed: true,
        };
        super::execute_agent_block(agent, &*shell, sleeper, &entry, &context, "work".into()).await
    }

    /// Scenario 1 of the 2026-10-19 outage: the provider is at capacity once,
    /// then serves the session. Execution pauses and tries again, and the run
    /// carries on as if nothing happened.
    #[tokio::test]
    async fn a_provider_at_capacity_is_retried_after_a_pause_and_the_run_succeeds() {
        use crate::gateway::AgentResponse;
        use crate::gateway::fakes::{FakeAgentGateway, RecordingSleeper};
        let agent = FakeAgentGateway::sequence(vec![
            AgentResponse::provider_unavailable(AgentProvider::Codex, CAPACITY),
            AgentResponse::success("done"),
        ]);
        let sleeper = RecordingSleeper::new();

        let result = execute_with(&agent, &sleeper).await;

        assert!(result.success, "{}", result.summary);
        assert_eq!(agent.invocations().len(), 2);
        assert_eq!(sleeper.pauses(), vec![std::time::Duration::from_secs(30)]);
        assert_eq!(result.events[0].payload["success"], true);
        assert!(result.events[0].payload.get("failure_kind").is_none());
    }

    /// Scenario 2: the provider stays at capacity. Execution tries four times,
    /// pausing 30 s, 2 min and 5 min, then reports the run as provider
    /// unavailable with the failure kind on the event.
    #[tokio::test]
    async fn a_provider_that_stays_unavailable_ends_the_run_as_provider_unavailable() {
        use crate::gateway::AgentResponse;
        use crate::gateway::fakes::{FakeAgentGateway, RecordingSleeper};
        let agent = FakeAgentGateway::always(AgentResponse::provider_unavailable(
            AgentProvider::Codex,
            CAPACITY,
        ));
        let sleeper = RecordingSleeper::new();

        let result = execute_with(&agent, &sleeper).await;

        assert!(!result.success);
        assert_eq!(agent.invocations().len(), 4);
        assert_eq!(sleeper.pauses(), super::PROVIDER_RETRY_DELAYS.to_vec());
        let payload = &result.events[0].payload;
        assert_eq!(payload["failure_kind"], "provider_unavailable");
        assert!(payload.get("terminal").is_none(), "not terminal: {payload}");
        assert_eq!(payload["summary"], format!("provider unavailable: {CAPACITY} (4 attempts)"));
    }

    /// A gateway that did not classify capacity wording, and an agent that
    /// could not run with that wording in its error, are both treated as the
    /// provider refusing the session: retried after the same pauses, then
    /// ended `provider unavailable` with the failure kind on the event.
    #[tokio::test]
    async fn capacity_wording_the_gateway_left_unclassified_is_provider_unavailable() {
        use crate::gateway::AgentResponse;
        use crate::gateway::fakes::{FakeAgentGateway, RecordingSleeper};
        for agent in [
            FakeAgentGateway::always(AgentResponse::failure(CAPACITY)),
            FakeAgentGateway::unavailable(CAPACITY),
        ] {
            let sleeper = RecordingSleeper::new();

            let result = execute_with(&agent, &sleeper).await;

            assert!(!result.success);
            assert_eq!(agent.invocations().len(), 4);
            assert_eq!(sleeper.pauses(), super::PROVIDER_RETRY_DELAYS.to_vec());
            let payload = &result.events[0].payload;
            assert_eq!(payload["failure_kind"], "provider_unavailable", "{payload}");
            assert_eq!(payload["provider"], "codex", "{payload}");
            assert!(payload.get("terminal").is_none(), "not terminal: {payload}");
            assert_eq!(
                payload["summary"],
                format!("provider unavailable: {CAPACITY} (4 attempts)")
            );
        }
    }

    /// Unclassified failures whose wording names no outage keep their own
    /// summaries and are not retried.
    #[tokio::test]
    async fn other_unclassified_failures_are_left_as_they_are() {
        use crate::gateway::fakes::{FakeAgentGateway, RecordingSleeper};
        for (agent, summary) in [
            (
                FakeAgentGateway::failure("unexpected error event"),
                "execution failed: unexpected error event",
            ),
            (
                FakeAgentGateway::unavailable("codex stream produced no output for 600s"),
                "agent unavailable: codex stream produced no output for 600s",
            ),
        ] {
            let sleeper = RecordingSleeper::new();

            let result = execute_with(&agent, &sleeper).await;

            assert_eq!(agent.invocations().len(), 1);
            assert_eq!(sleeper.pauses(), Vec::<std::time::Duration>::new());
            let payload = &result.events[0].payload;
            assert!(payload.get("failure_kind").is_none(), "{payload}");
            assert_eq!(payload["summary"], summary);
            assert_eq!(payload["session_failure"]["message"], summary);
        }
    }

    /// The execution result says when the agent session itself failed, with
    /// its summary and whether it changed project files. A session that
    /// finished carries no such signal, including iterate's silent no-op,
    /// which Foundry fails after a session that finished.
    #[test]
    fn a_failed_session_is_signalled_with_whether_it_left_work() {
        let payload = trigger_payload();
        let ctx = |workflow| ExecutionContext {
            trace_id: None,
            project: "proj",
            workflow,
            payload: &payload,
            throttle: Throttle::Full,
            label: "plan execution",
            retry_count: None,
            correction_needed: true,
        };
        let failed = || AgentOutcome::AgentFailed {
            stderr: "boom".into(),
            failure: None,
        };
        let signal = |result: &foundry_sdk::task_block::TaskBlockResult| {
            result.events[0].payload.get("session_failure").cloned()
        };

        let scratch_only = build_agent_execution_result(
            &ctx(WorkflowType::Task),
            failed(),
            SessionEnd::Failed,
            true,
            vec![
                ".foundry/session.log".into(),
                ".claude/worktrees/x/y".into(),
            ],
        );
        assert_eq!(
            signal(&scratch_only),
            Some(serde_json::json!({"message": "plan execution failed: boom", "left_work": false}))
        );

        let edited = build_agent_execution_result(
            &ctx(WorkflowType::Task),
            failed(),
            SessionEnd::Failed,
            true,
            vec!["src/lib.rs".into()],
        );
        assert_eq!(signal(&edited).unwrap()["left_work"], true);

        let silent_no_op = build_agent_execution_result(
            &ctx(WorkflowType::Iterate),
            AgentOutcome::Success {
                stdout: "done".into(),
            },
            SessionEnd::Finished,
            false,
            vec![],
        );
        assert_eq!(silent_no_op.events[0].payload["success"], false);
        assert_eq!(signal(&silent_no_op), None);
    }

    /// Only a provider refusal is retried: an ordinary agent failure and a
    /// terminal account state each end the run on the first attempt.
    #[tokio::test]
    async fn other_failures_are_not_retried() {
        use crate::gateway::AgentResponse;
        use crate::gateway::fakes::{FakeAgentGateway, RecordingSleeper};
        for response in [
            AgentResponse::failure("compile error"),
            AgentResponse::terminal_failure(
                AgentProvider::Claude,
                AgentFailureKind::AccountLimit,
                "monthly spend limit",
            ),
        ] {
            let agent = FakeAgentGateway::always(response);
            let sleeper = RecordingSleeper::new();
            let result = execute_with(&agent, &sleeper).await;
            assert!(!result.success);
            assert_eq!(agent.invocations().len(), 1);
            assert_eq!(sleeper.pauses(), [] as [std::time::Duration; 0]);
        }
    }

    #[tokio::test]
    async fn executor_receives_its_provider_and_shared_brief_identity() {
        use crate::{blocks::test_helpers, gateway::fakes::FakeAgentGateway};
        let agent = FakeAgentGateway::success_with("done");
        let shell = test_helpers::passing_shell();
        let entry = test_helpers::project_entry("p", "/repo/worktree");
        let payload = serde_json::json!({"agent_provider":"claude", "providers":{"formation":"claude", "execution":"codex"},
            "initiative_brief":{"path":"/brief/BRIEF.md", "hash":"abc123"},
            "initiative_findings":[{"id":"fnd_1", "text":"shared failure diagnosis"}]});
        let context = ExecutionContext {
            project: "p",
            trace_id: None,
            workflow: WorkflowType::Prompt,
            payload: &payload,
            throttle: Throttle::Full,
            label: "execution",
            retry_count: None,
            correction_needed: true,
        };
        super::execute_agent_block(
            &*agent,
            &*shell,
            &crate::gateway::fakes::RecordingSleeper::default(),
            &entry,
            &context,
            "fix the diagnosis".into(),
        )
        .await;
        let requests = agent.invocations();
        assert_eq!(requests[0].provider, Some(AgentProvider::Codex));
        assert!(requests[0].prompt.contains("/brief/BRIEF.md"));
        assert!(requests[0].prompt.contains("abc123"));
        assert!(requests[0].prompt.contains("shared failure diagnosis"));
    }

    #[tokio::test]
    async fn standalone_initiative_executor_enforces_its_selected_worktree() {
        use crate::{blocks::test_helpers, gateway::fakes::FakeAgentGateway};
        let directory = tempfile::tempdir().unwrap();
        let store = directory.path().join("store.json");
        std::fs::write(&store,serde_json::json!({"initiatives":[{"name":"i","outcome":"work","projects":["p"],"done_evidence":[],"status":"active"}]}).to_string()).unwrap();
        let agent = FakeAgentGateway::success_with("done");
        let shell = test_helpers::passing_shell();
        let entry = test_helpers::project_entry("p", "/repo/isolated-task");
        let payload =
            serde_json::json!({"initiative":"i","agent_provider":"claude","campaign_limits":{}});
        let context = ExecutionContext {
            project: "p",
            trace_id: None,
            workflow: WorkflowType::Task,
            payload: &payload,
            throttle: Throttle::Full,
            label: "execution",
            retry_count: None,
            correction_needed: true,
        };
        super::execute_agent_block_in_store(
            &*agent,
            &*shell,
            &crate::gateway::fakes::RecordingSleeper::default(),
            &entry,
            &context,
            "work".into(),
            &store,
        )
        .await;
        assert_eq!(
            agent.invocations()[0]
                .env
                .iter()
                .find(|(key, _)| key == "FOUNDRY_WRITABLE_ROOT")
                .unwrap()
                .1,
            "/repo/isolated-task"
        );
    }

    #[tokio::test]
    async fn terminal_or_unreadable_initiatives_settle_without_invoking_a_provider() {
        use crate::{blocks::test_helpers, gateway::fakes::FakeAgentGateway};
        use foundry_sdk::event::EventType;
        let directory = tempfile::tempdir().unwrap();
        let store_path = directory.path().join("campaigns.json");
        for state in [
            "completed",
            "cancelled",
            "invalid-json",
            "missing",
            "active",
        ] {
            if state == "invalid-json" {
                std::fs::write(&store_path, "{").unwrap();
            } else {
                std::fs::write(&store_path, serde_json::json!({"initiatives": if state == "missing" { serde_json::json!([]) } else { serde_json::json!([{"name":"i", "outcome":"done", "projects":["p"], "done_evidence":[], "status":state}]) }}).to_string()).unwrap();
            }
            let agent = FakeAgentGateway::success_with("must not run");
            let shell = test_helpers::passing_shell();
            let entry = test_helpers::project_entry("p", "/repo/worktree");
            let payload =
                serde_json::json!({"initiative":"i", "campaign":"c", "campaign_limits":{}});
            let context = ExecutionContext {
                project: "p",
                trace_id: None,
                workflow: WorkflowType::Task,
                payload: &payload,
                throttle: Throttle::Full,
                label: "execution",
                retry_count: None,
                correction_needed: true,
            };
            let result = super::execute_agent_block_in_store(
                &*agent,
                &*shell,
                &crate::gateway::fakes::RecordingSleeper::default(),
                &entry,
                &context,
                "must not run".into(),
                &store_path,
            )
            .await;
            if state == "active" {
                assert_eq!(
                    agent.invocations().len(),
                    1,
                    "active initiative must reach its provider"
                );
                continue;
            }
            assert!(agent.invocations().is_empty());
            assert_eq!(result.events[0].event_type, EventType::TaskRunCompleted);
            assert_eq!(result.events[0].payload["verdict"], "runner_error");
            assert_eq!(result.events[0].payload["initiative"], "i");
            assert!(!result.success);
            if matches!(state, "completed" | "cancelled") {
                assert!(result.summary.contains(state));
            } else {
                assert!(result.summary.contains("guard failed"));
            }
        }
    }

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
        assert!(
            execution_environment(WorkflowType::Prompt).is_empty(),
            "expected `execution_environment(WorkflowType::Prompt)` to be empty: {:?}",
            execution_environment(WorkflowType::Prompt)
        );
        assert!(
            execution_environment(WorkflowType::Scout).is_empty(),
            "expected `execution_environment(WorkflowType::Scout)` to be empty: {:?}",
            execution_environment(WorkflowType::Scout)
        );
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
            SessionEnd::Finished,
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
            SessionEnd::Finished,
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
            SessionEnd::Finished,
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
            SessionEnd::Failed,
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
            SessionEnd::Failed,
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
            SessionEnd::Failed,
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
            SessionEnd::Finished,
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
            SessionEnd::Finished,
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
            SessionEnd::Finished,
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
                &crate::gateway::fakes::RecordingSleeper::default(),
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
                &crate::gateway::fakes::RecordingSleeper::default(),
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
                &crate::gateway::fakes::RecordingSleeper::default(),
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
                &crate::gateway::fakes::RecordingSleeper::default(),
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
                &crate::gateway::fakes::RecordingSleeper::default(),
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
