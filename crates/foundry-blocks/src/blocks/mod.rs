#[macro_use]
mod macros;

use foundry_core::task_block::TaskBlockResult;

mod agent_helpers;
mod change_detection;
mod event_builders;
mod execution;

pub(super) use agent_helpers::{
    AgentBlockSpec, chain_agent_provider, invoke_agent, invoke_coding_agent,
    match_agent_text_outcome, parse_agent_json, parse_agent_provider,
};
pub(super) use event_builders::{
    build_agent_remediation_result, build_gate_result_from_payload, dry_run_execution_event,
    dry_run_remediation_event, emit_event_result, emit_result, format_gates_context,
    stub_event_result,
};
pub(super) use execution::{ExecutionContext, execute_agent_block};

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

mod assess_project;
mod audit_main_branch;
mod audit_release_tag;
mod check_charter;
mod check_pipeline;
mod cleanup_branches;
mod complete_project_run;
mod create_plan;
mod direct_prompt;
mod execute_maintain;
mod execute_plan;
mod generate_summary;
mod git_ops;
mod greet;
mod install;
mod observe_commits;
mod observe_events;
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
mod summarize_commits;
mod summarize_events;
mod summarize_result;
mod triage_assessment;
mod validate;
mod watch_pipeline;
mod write_commit_digest;
mod write_ops_digest;

pub use assess_project::AssessProject;
pub use audit_main_branch::AuditMainBranch;
pub use audit_release_tag::AuditReleaseTag;
pub use check_charter::CheckCharter;
pub use check_pipeline::CheckPipeline;
pub use cleanup_branches::CleanupBranches;
pub use complete_project_run::CompleteProjectRun;
pub use create_plan::CreatePlan;
pub use direct_prompt::DirectPrompt;
pub use execute_maintain::ExecuteMaintain;
pub use execute_plan::ExecutePlan;
pub use generate_summary::GenerateSummary;
pub use git_ops::CommitAndPush;
pub use greet::{ComposeGreeting, DeliverGreeting};
pub use install::InstallLocally;
pub use observe_commits::ObserveCommits;
pub use observe_events::ObserveEvents;
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
pub use summarize_commits::SummarizeCommits;
pub use summarize_events::SummarizeEvents;
pub use summarize_result::SummarizeResult;
pub use triage_assessment::TriageAssessment;
pub use validate::ValidateProject;
pub use watch_pipeline::WatchPipeline;
pub use write_commit_digest::WriteCommitDigest;
pub use write_ops_digest::WriteOpsDigest;

// Per-block unit-test fixtures (Engine-free). The full-chain integration tests
// and their Engine-based register helper live in the host crate (foundryd) to
// avoid a blocks↔engine dependency cycle. See `foundryd::chain_tests`.
#[cfg(test)]
mod test_helpers;
