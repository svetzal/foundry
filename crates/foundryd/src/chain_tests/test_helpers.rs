//! Chain-test fixtures that wire real blocks into a `foundry_engine::Engine`.
//!
//! Only the helpers the chain tests actually use live here; the Engine-free
//! per-block fixtures live in `foundry_blocks::blocks::test_helpers`.
#![allow(dead_code)]

use std::sync::{Arc, RwLock};

use foundry_engine::engine::Engine;
use foundry_sdk::gateway::fakes::{FakeAgentGateway, FakeShellGateway};
use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};

use foundry_blocks::gateway::{AgentGateway, AgentResponse, ShellGateway};
use foundry_blocks::shell::CommandResult;

/// Build a registry containing a single standard test project entry.
pub fn registry_with_project(name: &str, path: &str) -> Arc<RwLock<Registry>> {
    Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: name.to_string(),
            path: path.to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: ActionFlags::default(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
        }],
    }))
}

/// Build a shell gateway that always returns a successful, empty result.
pub fn passing_shell() -> Arc<dyn ShellGateway> {
    FakeShellGateway::always(CommandResult {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        success: true,
    })
}

/// Build an agent gateway that returns each string in `responses` as a
/// successful agent response, in sequence.
pub fn sequenced_agent(responses: Vec<&str>) -> Arc<dyn AgentGateway> {
    let agent_responses: Vec<AgentResponse> =
        responses.into_iter().map(AgentResponse::success).collect();
    FakeAgentGateway::sequence(agent_responses)
}

/// Create a temporary project directory with standard files for chain tests.
///
/// Creates `CHARTER.md` (100 × "a") and `.hone-gates.json` with a single `fmt` gate.
/// Caller must hold the returned `TempDir` for the test duration.
pub fn test_project_dir() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(dir.path().join("CHARTER.md"), "a".repeat(100)).unwrap();
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
    )
    .unwrap();
    dir
}

/// Create a temporary project directory without a CHARTER.md (charter check will fail).
pub fn test_project_dir_no_charter() -> tempfile::TempDir {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
    )
    .unwrap();
    dir
}

/// Register the standard iterate-chain blocks into `engine`.
///
/// Registers: `CheckCharter`, `ResolveGates`, `RunPreflightGates`,
/// `AssessProject`, `TriageAssessment`, `CreatePlan`, `ExecutePlan`,
/// `RunVerifyGates`, `RouteGateResult`, `RetryExecution`, `SummarizeResult`.
///
/// The `shell` gateway is shared by ALL shell-using blocks, including the
/// execution blocks (`ExecutePlan`, `RetryExecution`), so change detection
/// uses the fake shell rather than spawning a real `git` process.
///
/// Chain-specific blocks (e.g. `ExecuteMaintain`, `CommitAndPush`) must be
/// registered separately by the caller after this call.
pub fn register_iterate_chain(
    engine: &mut Engine,
    shell: Arc<dyn ShellGateway>,
    agent: Arc<dyn AgentGateway>,
    registry: Arc<RwLock<Registry>>,
) {
    use foundry_blocks::blocks as b;
    engine.register(Box::new(b::CheckCharter::new(registry.clone())));
    engine.register(Box::new(b::ResolveGates::new(registry.clone())));
    engine.register(Box::new(b::RunPreflightGates::new(shell.clone(), registry.clone())));
    engine.register(Box::new(b::AssessProject::new(agent.clone(), registry.clone())));
    engine.register(Box::new(b::TriageAssessment::new(agent.clone(), registry.clone())));
    engine.register(Box::new(b::CreatePlan::new(agent.clone(), registry.clone())));
    engine.register(Box::new(b::ExecutePlan::with_gateways(
        agent.clone(),
        registry.clone(),
        shell.clone(),
    )));
    engine.register(Box::new(b::RunVerifyGates::new(shell.clone(), registry.clone())));
    engine.register(Box::new(b::RouteGateResult));
    engine.register(Box::new(b::RetryExecution::with_gateways(
        agent.clone(),
        registry.clone(),
        shell,
    )));
    engine.register(Box::new(b::SummarizeResult::new(agent, registry)));
}
