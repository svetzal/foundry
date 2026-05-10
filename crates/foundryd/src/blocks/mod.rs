#[macro_use]
mod macros;

use std::path::{Path, PathBuf};
use std::time::Duration;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::{ExecutionCompletedPayload, LoopContext, RemediationCompletedPayload};
use foundry_core::task_block::TaskBlockResult;
use foundry_core::throttle::Throttle;
use foundry_core::workflow::WorkflowType;

use crate::gateway::{
    AgentAccess, AgentCapability, AgentGateway, AgentOutcome, AgentRequest, ShellGateway,
};

/// Bundles the three fields every block `execute()` extracts from the trigger event.
///
/// Use [`TriggerContext::from_trigger`] to populate it, then destructure immediately:
/// ```ignore
/// let TriggerContext { project, throttle, payload } = TriggerContext::from_trigger(trigger);
/// ```
pub(super) struct TriggerContext {
    pub project: String,
    pub throttle: foundry_core::throttle::Throttle,
    pub payload: serde_json::Value,
}

impl TriggerContext {
    pub fn from_trigger(trigger: &foundry_core::event::Event) -> Self {
        Self {
            project: trigger.project.clone(),
            throttle: trigger.throttle,
            payload: trigger.payload.clone(),
        }
    }
}

/// Configuration for an agent invocation within a task block.
///
/// Pass to [`invoke_agent`] to handle request construction, invocation,
/// outcome conversion, and tracing.
pub(super) struct AgentBlockSpec {
    pub prompt: String,
    pub working_dir: PathBuf,
    pub access: AgentAccess,
    pub capability: AgentCapability,
    pub agent_file: Option<PathBuf>,
    pub timeout: Duration,
}

/// Invoke an agent with the given spec, returning the outcome.
///
/// Encapsulates `AgentRequest` construction, `AgentGateway::invoke`,
/// `AgentOutcome::from_response`, and tracing.
pub(super) async fn invoke_agent(
    agent: &dyn AgentGateway,
    spec: AgentBlockSpec,
    trace_label: &str,
    project: &str,
) -> AgentOutcome {
    let request = AgentRequest {
        prompt: spec.prompt,
        working_dir: spec.working_dir,
        access: spec.access,
        capability: spec.capability,
        agent_file: spec.agent_file,
        timeout: spec.timeout,
    };
    tracing::info!(project = %project, "{trace_label}: invoking agent");
    let response = agent.invoke(&request).await;
    let outcome = AgentOutcome::from_response(response);
    if let AgentOutcome::Unavailable { ref error } = outcome {
        tracing::warn!(project = %project, error = %error, "{trace_label}: agent unavailable");
    }
    outcome
}

/// Look up a project in the registry, returning the entry or a not-found failure result.
///
/// Replaces the two-phase pattern of cloning `Option<ProjectEntry>` before `Box::pin`
/// and then unwrapping inside the async block.
fn require_project(
    registry: &foundry_core::registry::Registry,
    project: &str,
) -> Result<foundry_core::registry::ProjectEntry, TaskBlockResult> {
    registry.find_project(project).cloned().ok_or_else(|| {
        tracing::warn!(project = %project, "project not found in registry");
        TaskBlockResult::project_not_found(project)
    })
}

/// Emit a single-event success result with a serialized payload.
///
/// Eliminates the three-line boilerplate of `serialize_payload` → `Event::new` →
/// `TaskBlockResult::success` that appears in blocks whose happy path emits
/// exactly one event with `success = true` and no `raw_output` or `exit_code`.
pub(super) fn emit_result(
    summary: String,
    event_type: EventType,
    project: &str,
    throttle: Throttle,
    payload: &impl serde::Serialize,
) -> anyhow::Result<TaskBlockResult> {
    let event_payload = Event::serialize_payload(payload)?;
    Ok(TaskBlockResult::success(
        summary,
        vec![Event::new(
            event_type,
            project.to_string(),
            throttle,
            event_payload,
        )],
    ))
}

/// Build a `TaskBlockResult` for an agent-driven remediation, handling the
/// response match, tracing, payload serialization, and summary formatting.
///
/// `success_label` and `failure_label` are the prefix for `TaskBlockResult.summary`
/// (e.g. "Remediated CVE-2026-1234" / "Remediation of CVE-2026-1234 failed").
pub(super) fn build_agent_remediation_result(
    project: &str,
    throttle: Throttle,
    outcome: AgentOutcome,
    cve: Option<String>,
    pipeline_fix: Option<bool>,
    success_label: &str,
    failure_label: &str,
) -> TaskBlockResult {
    let (raw_output, success, summary) = match outcome {
        AgentOutcome::Success { stdout } => {
            let out = stdout.trim().to_string();
            (Some(out), true, "remediation completed".to_string())
        }
        AgentOutcome::AgentFailed { stderr } => {
            let first_line = stderr.lines().next().unwrap_or("agent failed");
            let summary = format!("remediation failed: {first_line}");
            (Some(stderr), false, summary)
        }
        AgentOutcome::Unavailable { error } => (None, false, format!("agent unavailable: {error}")),
    };

    tracing::info!(
        project = %project,
        success = success,
        summary = %summary,
        "remediation completed"
    );

    let event_payload = Event::serialize_payload(&RemediationCompletedPayload {
        cve,
        success,
        summary: Some(summary.clone()),
        dry_run: None,
        pipeline_fix,
    })
    .expect("RemediationCompletedPayload is infallibly serializable");

    TaskBlockResult {
        events: vec![Event::new(
            EventType::RemediationCompleted,
            project.to_string(),
            throttle,
            event_payload,
        )],
        success,
        summary: if success {
            format!("{success_label}: {summary}")
        } else {
            format!("{failure_label}: {summary}")
        },
        raw_output,
        exit_code: None,
        audit_artifacts: vec![],
    }
}

/// Run `git status --porcelain` in `project_path` and return whether the
/// working tree has uncommitted changes, along with the list of affected paths.
///
/// On any error (non-git directory, missing git binary, etc.) logs a warning
/// and returns `(false, vec![])` so that the calling block can still emit its
/// event normally.
pub(super) async fn detect_post_execution_changes(
    shell: &dyn ShellGateway,
    project_path: &Path,
) -> (bool, Vec<String>) {
    match shell.run(project_path, "git", &["status", "--porcelain"], None, None).await {
        Ok(r) if r.success => {
            // Porcelain v1 format: XY<space><path>. The first 3 bytes (XY + space)
            // are always ASCII, so slicing at byte 3 is safe for any filename.
            let files: Vec<String> =
                r.stdout.lines().filter(|l| l.len() >= 4).map(|l| l[3..].to_string()).collect();
            (!files.is_empty(), files)
        }
        Ok(r) => {
            tracing::warn!(stderr = %r.stderr, "git status failed; reporting no changes");
            (false, vec![])
        }
        Err(e) => {
            tracing::warn!(error = %e, "git status errored; reporting no changes");
            (false, vec![])
        }
    }
}

/// Returns `true` if `path` is an auxiliary worktree path that should not count
/// as a meaningful working-tree change (e.g. `.claude/worktrees/…`).
fn is_auxiliary_path(p: &str) -> bool {
    p.starts_with(".claude/worktrees/")
}

/// Returns `true` when the file list represents only auxiliary changes (or is
/// empty) and therefore should be treated as a silent no-op.
fn only_auxiliary_changes(files: &[String]) -> bool {
    files.is_empty() || files.iter().all(|f| is_auxiliary_path(f))
}

/// Build a `TaskBlockResult` for an agent-driven execution step, handling the
/// response match, output trimming to 200 lines, tracing, `LoopContext` extraction,
/// `ExecutionCompletedPayload` serialization, and `TaskBlockResult` construction.
///
/// `success_label` is the base text for the summary, e.g. "plan execution",
/// "maintenance", or "retry 2".  `retry_count` is forwarded into the payload
/// when present (retry flow only).
///
/// For the `Iterate` workflow only: if the agent exits 0 but produces no
/// meaningful working-tree changes, the result is overridden to `success: false`
/// with a "silent no-op" summary.  The `Maintain` workflow is unaffected.
#[allow(clippy::too_many_arguments)]
pub(super) fn build_agent_execution_result(
    project: &str,
    workflow: WorkflowType,
    outcome: AgentOutcome,
    trigger_payload: &serde_json::Value,
    throttle: Throttle,
    success_label: &str,
    retry_count: Option<u64>,
    changes_detected: bool,
    files_changed: Vec<String>,
) -> TaskBlockResult {
    let (raw_output, mut success, mut summary, execution_output) = match outcome {
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
            (Some(out), true, format!("{success_label} completed"), exec_out)
        }
        AgentOutcome::AgentFailed { stderr } => {
            let first_line = stderr.lines().next().unwrap_or("agent failed").to_string();
            (Some(stderr), false, format!("{success_label} failed: {first_line}"), None)
        }
        AgentOutcome::Unavailable { error } => {
            (None, false, format!("agent unavailable: {error}"), None)
        }
    };

    // For iterate only: an agent that exits 0 but makes no meaningful changes is
    // a silent no-op — treat it as a failure so the retry loop can fire.
    if workflow == WorkflowType::Iterate
        && success
        && (!changes_detected || only_auxiliary_changes(&files_changed))
    {
        tracing::info!(
            project = %project,
            changes_detected = changes_detected,
            files_changed = ?files_changed,
            "iterate agent produced no meaningful changes — overriding to failure"
        );
        success = false;
        summary = "agent did not modify any files (silent no-op)".to_string();
    }

    tracing::info!(project = %project, success = success, "{success_label} completed");

    let context = LoopContext::extract_from(trigger_payload);
    let event_payload = Event::serialize_payload(&ExecutionCompletedPayload {
        project: project.to_string(),
        workflow: workflow.to_string(),
        success,
        summary: summary.clone(),
        execution_output,
        dry_run: None,
        retry_count,
        changes_detected: Some(changes_detected),
        files_changed,
        context,
    })
    .expect("ExecutionCompletedPayload is infallibly serializable");

    TaskBlockResult {
        events: vec![Event::new(
            EventType::ExecutionCompleted,
            project.to_string(),
            throttle,
            event_payload,
        )],
        success,
        summary: format!("{project}: {summary}"),
        raw_output,
        exit_code: None,
        audit_artifacts: vec![],
    }
}

#[cfg(test)]
mod mod_tests {
    use foundry_core::throttle::Throttle;
    use foundry_core::workflow::WorkflowType;

    use crate::gateway::AgentOutcome;

    use super::{build_agent_execution_result, is_auxiliary_path};

    fn trigger_payload() -> serde_json::Value {
        serde_json::json!({ "project": "p", "workflow": "iterate" })
    }

    // --- is_auxiliary_path ---

    #[test]
    fn auxiliary_path_returns_true() {
        assert!(is_auxiliary_path(".claude/worktrees/abc/foo.md"));
        assert!(is_auxiliary_path(".claude/worktrees/"));
    }

    #[test]
    fn non_auxiliary_path_returns_false() {
        assert!(!is_auxiliary_path("src/main.rs"));
        assert!(!is_auxiliary_path("Cargo.toml"));
        assert!(!is_auxiliary_path(".claude/settings.json"));
    }

    // --- iterate: clean tree → failure override ---

    #[test]
    fn iterate_clean_tree_overrides_to_failure() {
        let result = build_agent_execution_result(
            "proj",
            WorkflowType::Iterate,
            AgentOutcome::Success {
                stdout: "done".to_string(),
            },
            &trigger_payload(),
            Throttle::Full,
            "plan execution",
            None,
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

    // --- iterate: real changes → success unchanged ---

    #[test]
    fn iterate_dirty_tree_remains_success() {
        let result = build_agent_execution_result(
            "proj",
            WorkflowType::Iterate,
            AgentOutcome::Success {
                stdout: "done".to_string(),
            },
            &trigger_payload(),
            Throttle::Full,
            "plan execution",
            None,
            true,
            vec!["src/lib.rs".to_string()],
        );

        assert!(result.success, "expected success but got failure");
        assert_eq!(result.events[0].payload["success"], true);
    }

    // --- maintain: clean tree → NOT overridden ---

    #[test]
    fn maintain_clean_tree_remains_success() {
        let payload = serde_json::json!({ "project": "p", "workflow": "maintain" });
        let result = build_agent_execution_result(
            "proj",
            WorkflowType::Maintain,
            AgentOutcome::Success {
                stdout: "done".to_string(),
            },
            &payload,
            Throttle::Full,
            "maintenance",
            None,
            false, // clean tree
            vec![],
        );

        assert!(result.success, "maintain workflow must NOT override to failure on clean tree");
        assert_eq!(result.events[0].payload["success"], true);
    }

    // --- iterate: only auxiliary files → treated as no-op ---

    #[test]
    fn iterate_aux_only_changes_treated_as_clean() {
        let result = build_agent_execution_result(
            "proj",
            WorkflowType::Iterate,
            AgentOutcome::Success {
                stdout: "done".to_string(),
            },
            &trigger_payload(),
            Throttle::Full,
            "plan execution",
            None,
            true, // changes_detected=true but only aux files
            vec![".claude/worktrees/abc/foo".to_string()],
        );

        assert!(!result.success, "all-auxiliary file list must trigger failure override");
        assert!(result.summary.contains("silent no-op"));
    }
}

/// Produce the single simulated-success `ExecutionCompleted` event used by
/// `dry_run_events()` across all three execution blocks.
///
/// Encapsulates `LoopContext::extract_from`, `ExecutionCompletedPayload` construction,
/// `trigger.with_payload()`, and the infallibility `.expect()`.
pub(super) fn dry_run_execution_event(
    trigger: &Event,
    workflow: WorkflowType,
    retry_count: Option<u64>,
) -> Vec<Event> {
    let context = LoopContext::extract_from(&trigger.payload);
    vec![
        trigger
            .with_payload(
                EventType::ExecutionCompleted,
                &ExecutionCompletedPayload {
                    project: trigger.project.clone(),
                    workflow: workflow.to_string(),
                    success: true,
                    summary: String::new(),
                    execution_output: None,
                    dry_run: Some(true),
                    retry_count,
                    changes_detected: None,
                    files_changed: vec![],
                    context,
                },
            )
            .expect("ExecutionCompletedPayload is infallibly serializable"),
    ]
}

/// Build the quality-gates context paragraph included in agent prompts.
///
/// Returns a formatted section if `gates` is `Some`, otherwise an empty string.
pub(super) fn format_gates_context(gates: Option<&serde_json::Value>) -> String {
    if let Some(gates) = gates {
        format!(
            "\n\nThe following quality gates must pass after your changes:\n{}",
            serde_json::to_string_pretty(gates).unwrap_or_default()
        )
    } else {
        String::new()
    }
}

/// Produce the single simulated-success `RemediationCompleted` event used by
/// `dry_run_events()` in `remediate` and `remediate_pipeline`.
pub(super) fn dry_run_remediation_event(
    trigger: &Event,
    cve: Option<String>,
    pipeline_fix: Option<bool>,
) -> Vec<Event> {
    let summary = cve.as_ref().map(|_| String::new());
    let payload = Event::serialize_payload(&RemediationCompletedPayload {
        cve,
        success: true,
        summary,
        dry_run: Some(true),
        pipeline_fix,
    })
    .expect("RemediationCompletedPayload is infallibly serializable");
    vec![Event::new(
        EventType::RemediationCompleted,
        trigger.project.clone(),
        trigger.throttle,
        payload,
    )]
}

/// Match an agent outcome into text output plus a success flag, or return a failure result.
///
/// On `Unavailable`, returns `Err(TaskBlockResult::failure(...))`.
/// On `AgentFailed`, logs a warning and returns `Ok((fallback_text, false))`.
/// On `Success`, returns `Ok((trimmed_stdout, true))`.
pub(super) fn match_agent_text_outcome(
    outcome: AgentOutcome,
    project: &str,
    trace_label: &str,
) -> Result<(String, bool), TaskBlockResult> {
    match outcome {
        AgentOutcome::Success { stdout } => Ok((stdout.trim().to_string(), true)),
        AgentOutcome::AgentFailed { stderr } => {
            tracing::warn!(project = %project, stderr = %stderr, "{trace_label} failed");
            Ok((format!("{trace_label} failed: {stderr}"), false))
        }
        AgentOutcome::Unavailable { error } => {
            Err(TaskBlockResult::failure(format!("agent unavailable: {error}")))
        }
    }
}

/// Emit a single-event success result with a raw JSON payload, without serialization.
///
/// Use for stub/fallback paths that already have a `serde_json::Value` payload
/// (e.g., no-repo results) and don't need typed payload serialization.
pub(super) fn stub_event_result(
    summary: impl Into<String>,
    event_type: EventType,
    project: String,
    throttle: Throttle,
    payload: serde_json::Value,
) -> TaskBlockResult {
    TaskBlockResult::success(summary, vec![Event::new(event_type, project, throttle, payload)])
}

/// Serialize `payload` and construct a `TaskBlockResult` for a gate-run event.
///
/// Absorbs the `serialize_payload().expect(...)` boilerplate shared by every
/// gate result builder, delegating final construction to [`build_gate_block_result`].
fn build_gate_result_from_payload(
    project: &str,
    event_type: EventType,
    success: bool,
    label: &str,
    throttle: Throttle,
    payload: &impl serde::Serialize,
) -> TaskBlockResult {
    let event_payload =
        Event::serialize_payload(payload).expect("gate result payload is infallibly serializable");
    build_gate_block_result(project, event_type, success, label, throttle, event_payload)
}

/// Construct a `TaskBlockResult` for a gate-run event.
fn build_gate_block_result(
    project: &str,
    event_type: EventType,
    success: bool,
    label: &str,
    throttle: Throttle,
    event_payload: serde_json::Value,
) -> TaskBlockResult {
    TaskBlockResult {
        events: vec![Event::new(
            event_type,
            project.to_string(),
            throttle,
            event_payload,
        )],
        success,
        summary: if success {
            format!("{project}: {label} passed")
        } else {
            format!("{project}: {label} failed")
        },
        raw_output: None,
        exit_code: None,
        audit_artifacts: vec![],
    }
}

mod assess_project;
mod audit_main_branch;
mod audit_release_tag;
mod check_charter;
mod check_pipeline;
mod cleanup_branches;
mod create_plan;
mod direct_prompt;
mod execute_maintain;
mod execute_plan;
mod generate_summary;
mod git_ops;
mod greet;
mod install;
mod release;
mod remediate;
mod remediate_pipeline;
mod resolve_gates;
mod retry_execution;
mod route_gate_result;
mod route_project;
mod route_validation_result;
mod run_preflight_gates;
mod run_verify_gates;
mod scan;
mod scout_drift;
mod strategic_assess;
mod strategic_loop;
mod summarize_result;
mod triage_assessment;
mod validate;
mod watch_pipeline;

pub use assess_project::AssessProject;
pub use audit_main_branch::AuditMainBranch;
pub use audit_release_tag::AuditReleaseTag;
pub use check_charter::CheckCharter;
pub use check_pipeline::CheckPipeline;
pub use cleanup_branches::CleanupBranches;
pub use create_plan::CreatePlan;
pub use direct_prompt::DirectPrompt;
pub use execute_maintain::ExecuteMaintain;
pub use execute_plan::ExecutePlan;
pub use generate_summary::GenerateSummary;
pub use git_ops::CommitAndPush;
pub use greet::{ComposeGreeting, DeliverGreeting};
pub use install::InstallLocally;
pub use release::{cut_release_step, execute_release_step};
pub use remediate::RemediateVulnerability;
pub use remediate_pipeline::RemediatePipeline;
pub use resolve_gates::ResolveGates;
pub use retry_execution::RetryExecution;
pub use route_gate_result::RouteGateResult;
pub use route_project::RouteProjectWorkflow;
pub use route_validation_result::RouteValidationResult;
pub use run_preflight_gates::RunPreflightGates;
pub use run_verify_gates::RunVerifyGates;
pub use scan::ScanDependencies;
pub use scout_drift::ScoutDrift;
pub use strategic_assess::StrategicAssessor;
pub use strategic_loop::StrategicLoopController;
pub use summarize_result::SummarizeResult;
pub use triage_assessment::TriageAssessment;
pub use validate::ValidateProject;
pub use watch_pipeline::WatchPipeline;

#[cfg(test)]
mod test_helpers;

#[cfg(test)]
mod iterate_chain_test;
#[cfg(test)]
mod maintain_chain_test;
#[cfg(test)]
mod prompt_chain_test;
#[cfg(test)]
mod release_chain_test;
#[cfg(test)]
mod strategic_chain_test;
