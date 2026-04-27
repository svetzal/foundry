#[macro_use]
mod macros;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::RemediationCompletedPayload;
use foundry_core::task_block::TaskBlockResult;
use foundry_core::throttle::Throttle;

use crate::gateway::AgentResponse;

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
    response: anyhow::Result<AgentResponse>,
    cve: Option<String>,
    pipeline_fix: Option<bool>,
    success_label: &str,
    failure_label: &str,
) -> TaskBlockResult {
    let (raw_output, exit_code, success, summary) = match response {
        Ok(r) => {
            let s = r.success;
            let out = format!("{}\n{}", r.stdout, r.stderr).trim().to_string();
            let summary = if s {
                "remediation completed".to_string()
            } else {
                let first_line = r.stderr.lines().next().unwrap_or("agent failed");
                format!("remediation failed: {first_line}")
            };
            (Some(out), Some(r.exit_code), s, summary)
        }
        Err(err) => {
            tracing::warn!(error = %err, "agent not available or failed to spawn");
            (None, None, false, format!("agent unavailable: {err}"))
        }
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
        exit_code,
        audit_artifacts: vec![],
    }
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
