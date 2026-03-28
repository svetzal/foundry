// Phase 3 iterate blocks — not yet registered in the engine (item 2 handles wiring).
#[allow(dead_code)]
mod assess_project;
mod audit;
#[allow(dead_code)]
mod check_charter;
#[allow(dead_code)]
mod create_plan;
mod execute_maintain;
#[allow(dead_code)]
mod execute_plan;
mod generate_summary;
mod git_ops;
mod greet;
mod hone_common;
mod hone_iterate;
mod hone_maintain;
mod install;
mod release;
mod remediate;
mod resolve_gates;
mod retry_execution;
mod route_gate_result;
mod route_project;
mod route_validation_result;
mod run_preflight_gates;
mod run_verify_gates;
mod scan;
mod summarize_result;
#[allow(dead_code)]
mod triage_assessment;
mod validate;

// Phase 3 iterate blocks — unregistered until item 2 wires them into the engine.
#[allow(unused_imports)]
pub use assess_project::AssessProject;
pub use audit::{AuditMainBranch, AuditReleaseTag};
#[allow(unused_imports)]
pub use check_charter::CheckCharter;
#[allow(unused_imports)]
pub use create_plan::CreatePlan;
pub use execute_maintain::ExecuteMaintain;
#[allow(unused_imports)]
pub use execute_plan::ExecutePlan;
pub use generate_summary::GenerateSummary;
pub use git_ops::CommitAndPush;
pub use greet::{ComposeGreeting, DeliverGreeting};
pub use hone_iterate::RunHoneIterate;
// RunHoneMaintain is unregistered (Phase 2) but kept for Phase 4 cleanup.
#[allow(unused_imports)]
pub use hone_maintain::RunHoneMaintain;
pub use install::InstallLocally;
pub use release::{CutRelease, WatchPipeline};
pub use remediate::RemediateVulnerability;
pub use resolve_gates::ResolveGates;
pub use retry_execution::RetryExecution;
pub use route_gate_result::RouteGateResult;
pub use route_project::RouteProjectWorkflow;
pub use route_validation_result::RouteValidationResult;
pub use run_preflight_gates::RunPreflightGates;
pub use run_verify_gates::RunVerifyGates;
pub use scan::ScanDependencies;
pub use summarize_result::SummarizeResult;
#[allow(unused_imports)]
pub use triage_assessment::TriageAssessment;
pub use validate::ValidateProject;

#[cfg(test)]
mod maintain_chain_test;
