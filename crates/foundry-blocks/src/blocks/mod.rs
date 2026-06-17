#[macro_use]
mod macros;

use std::sync::{Arc, RwLock, RwLockReadGuard};

use foundry_sdk::task_block::TaskBlockResult;

mod agent_helpers;
mod change_detection;
mod digest_io;
mod event_builders;
mod execution;

pub(super) use agent_helpers::{
    AgentBlockSpec, CodingAgentSpec, chain_agent_provider, extract_json, invoke_agent,
    invoke_coding_agent, match_agent_text_outcome, parse_agent_json, parse_agent_provider,
};
pub(super) use digest_io::{today_dated_path, write_digest_and_emit};
pub(super) use event_builders::{
    build_agent_remediation_result, build_gate_result_from_payload, dry_run_execution_event,
    dry_run_remediation_event, emit_event_result, emit_result, event_from_infallible_payload,
    event_from_payload, format_gates_context, stub_event_result,
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
    pub throttle: foundry_sdk::throttle::Throttle,
    pub payload: serde_json::Value,
}

impl TriggerContext {
    pub fn from_trigger(trigger: &foundry_sdk::event::Event) -> Self {
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
    registry: &foundry_sdk::registry::Registry,
    project: &str,
) -> Result<foundry_sdk::registry::ProjectEntry, TaskBlockResult> {
    registry.find_project(project).cloned().ok_or_else(|| {
        tracing::warn!(project = %project, "project not found in registry");
        TaskBlockResult::project_not_found(project)
    })
}

/// Acquire a read guard on the registry, returning a descriptive error instead of panicking
/// when the lock is poisoned.
///
/// Lock poisoning occurs when a writer panicked while holding the write guard. Using this
/// helper instead of `.expect("registry lock poisoned")` means the daemon continues serving
/// other events rather than aborting on the affected block.
fn read_registry(
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
) -> anyhow::Result<RwLockReadGuard<'_, foundry_sdk::registry::Registry>> {
    registry.read().map_err(|_| {
        anyhow::anyhow!(
            "registry lock poisoned: a prior writer panicked while holding the write lock"
        )
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
mod remediate_supply_chain;
mod resolve_gates;
mod retry_execution;
mod route_gate_result;
mod route_project;
mod route_validation_result;
mod run_preflight_gates;
mod run_verify_gates;
mod scan;
mod scan_supply_chain;
mod scout_drift;
mod strategic_assess;
mod strategic_loop;
mod summarize_commits;
mod summarize_events;
mod summarize_result;
mod triage_assessment;
pub mod triage_core;
mod triage_maintenance;
mod validate;
mod watch_pipeline;
mod write_commit_digest;
mod write_ops_digest;
mod write_supply_chain_digest;
mod write_triage_digest;

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
pub use remediate_supply_chain::RemediateSupplyChain;
pub use resolve_gates::ResolveGates;
pub use retry_execution::RetryExecution;
pub use route_gate_result::RouteGateResult;
pub use route_project::RouteProjectWorkflow;
pub use route_validation_result::RouteValidationResult;
pub use run_preflight_gates::RunPreflightGates;
pub use run_verify_gates::RunVerifyGates;
pub use scan::ScanDependencies;
pub use scan_supply_chain::ScanSupplyChain;
pub use scout_drift::ScoutDrift;
pub use strategic_assess::StrategicAssessor;
pub use strategic_loop::StrategicLoopController;
pub use summarize_commits::SummarizeCommits;
pub use summarize_events::SummarizeEvents;
pub use summarize_result::SummarizeResult;
pub use triage_assessment::TriageAssessment;
pub use triage_maintenance::TriageMaintenance;
pub use validate::ValidateProject;
pub use watch_pipeline::WatchPipeline;
pub use write_commit_digest::WriteCommitDigest;
pub use write_ops_digest::WriteOpsDigest;
pub use write_supply_chain_digest::WriteSupplyChainDigest;
pub use write_triage_digest::WriteTriageDigest;

// Per-block unit-test fixtures (Engine-free). The full-chain integration tests
// and their Engine-based register helper live in the host crate (foundryd) to
// avoid a blocks↔engine dependency cycle. See `foundryd::chain_tests`.
#[cfg(test)]
mod test_helpers;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::registry::Registry;

    use super::read_registry;

    fn empty_registry() -> Arc<RwLock<Registry>> {
        Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        }))
    }

    #[test]
    fn read_registry_returns_err_when_poisoned() {
        let registry = empty_registry();
        let r2 = Arc::clone(&registry);
        let _ = std::thread::spawn(move || {
            let _guard = r2.write().unwrap();
            panic!("intentional poison");
        })
        .join();
        let result = read_registry(&registry);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("poisoned"));
    }
}
