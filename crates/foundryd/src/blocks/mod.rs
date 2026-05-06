#[macro_use]
mod macros;

use std::path::PathBuf;
use std::time::Duration;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::{ExecutionCompletedPayload, LoopContext, RemediationCompletedPayload};
use foundry_core::task_block::TaskBlockResult;
use foundry_core::throttle::Throttle;
use foundry_core::workflow::WorkflowType;

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentOutcome, AgentRequest};

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

/// Serialize a slice of gate results to JSON values using the `Serialize` derive.
pub(super) fn gate_results_to_json(
    results: &[foundry_core::gates::GateResult],
) -> Vec<serde_json::Value> {
    results.iter().filter_map(|r| serde_json::to_value(r).ok()).collect()
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

/// Build a `TaskBlockResult` for an agent-driven execution step, handling the
/// response match, output trimming to 200 lines, tracing, `LoopContext` extraction,
/// `ExecutionCompletedPayload` serialization, and `TaskBlockResult` construction.
///
/// `success_label` is the base text for the summary, e.g. "plan execution",
/// "maintenance", or "retry 2".  `retry_count` is forwarded into the payload
/// when present (retry flow only).
pub(super) fn build_agent_execution_result(
    project: &str,
    workflow: WorkflowType,
    outcome: AgentOutcome,
    trigger_payload: &serde_json::Value,
    throttle: Throttle,
    success_label: &str,
    retry_count: Option<u64>,
) -> TaskBlockResult {
    let (raw_output, success, summary, execution_output) = match outcome {
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
