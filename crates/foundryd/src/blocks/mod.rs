#[macro_use]
mod macros;

use foundry_core::event::{Event, EventType};
use foundry_core::task_block::TaskBlockResult;
use foundry_core::throttle::Throttle;
use foundry_core::workflow::WorkflowType;

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

/// Serialize a slice of gate results to JSON values using the `Serialize` derive.
fn gate_results_to_json(results: &[foundry_core::gates::GateResult]) -> Vec<serde_json::Value> {
    results.iter().filter_map(|r| serde_json::to_value(r).ok()).collect()
}

/// Build the shared base payload for any gate-run event.
fn build_gate_run_payload(
    project: &str,
    workflow: WorkflowType,
    run_result: &foundry_core::gates::GatesRunResult,
) -> serde_json::Value {
    serde_json::json!({
        "project": project,
        "workflow": workflow,
        "all_passed": run_result.all_passed,
        "required_passed": run_result.required_passed,
        "results": gate_results_to_json(&run_result.results),
    })
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
mod audit;
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

pub use assess_project::AssessProject;
pub use audit::{AuditMainBranch, AuditReleaseTag};
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
pub use release::{CutRelease, WatchPipeline};
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

#[cfg(test)]
mod test_helpers;

#[cfg(test)]
mod iterate_chain_test;
#[cfg(test)]
mod maintain_chain_test;
#[cfg(test)]
mod prompt_chain_test;
#[cfg(test)]
mod strategic_chain_test;
