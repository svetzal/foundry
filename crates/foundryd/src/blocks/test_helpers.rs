//! Shared test fixtures for task block unit tests.
//!
//! Gated with `#[cfg(test)]` — this module is only compiled during testing.
#![allow(dead_code)]

use std::sync::{Arc, RwLock};

use foundry_core::event::{Event, EventType};
use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
use foundry_core::throttle::Throttle;

use crate::engine::Engine;
use crate::gateway::fakes::{FakeAgentGateway, FakeShellGateway};
use crate::gateway::{AgentGateway, AgentResponse, ShellGateway};
use crate::shell::CommandResult;

/// Build a registry containing a single standard test project entry.
///
/// Uses `Stack::Rust`, agent `"claude"`, and `ActionFlags::default()`.
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

/// Build a registry containing a single project entry with custom fields via a pre-built entry.
pub fn registry_with_entry(entry: ProjectEntry) -> Arc<RwLock<Registry>> {
    Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![entry],
    }))
}

/// Build a standard test project entry with default fields.
pub fn project_entry(name: &str, path: &str) -> ProjectEntry {
    ProjectEntry {
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
    }
}

/// Build a standard test project entry with custom install config.
pub fn project_entry_with_install(
    name: &str,
    path: &str,
    install: Option<foundry_core::registry::InstallConfig>,
) -> ProjectEntry {
    ProjectEntry {
        install,
        ..project_entry(name, path)
    }
}

/// Build a test event with the given type, project name, and payload.
pub fn make_trigger(event_type: EventType, project: &str, payload: serde_json::Value) -> Event {
    Event::new(event_type, project.to_string(), Throttle::Full, payload)
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
    let agent_responses: Vec<AgentResponse> = responses
        .into_iter()
        .map(|s| AgentResponse {
            stdout: s.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        })
        .collect();
    FakeAgentGateway::sequence(agent_responses)
}

/// Build a standard test project entry with a custom agent.
pub fn project_entry_with_agent(name: &str, path: &str, agent: &str) -> ProjectEntry {
    ProjectEntry {
        agent: agent.to_string(),
        ..project_entry(name, path)
    }
}

/// Build a standard test project entry with a custom repo.
pub fn project_entry_with_repo(name: &str, path: &str, repo: &str) -> ProjectEntry {
    ProjectEntry {
        repo: repo.to_string(),
        ..project_entry(name, path)
    }
}

/// Build a project entry with optional AGENTS.md in a temporary directory.
///
/// Returns `(ProjectEntry, Option<TempDir>)`. The caller must hold the `TempDir`
/// for the duration of the test to keep the directory alive.
pub fn project_entry_with_agents_md(
    name: &str,
    has_agents_md: bool,
) -> (ProjectEntry, Option<tempfile::TempDir>) {
    if has_agents_md {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "# Agent guidance").unwrap();
        let entry = ProjectEntry {
            path: dir.path().to_str().unwrap().to_string(),
            ..project_entry(name, "/nonexistent")
        };
        (entry, Some(dir))
    } else {
        (project_entry(name, "/nonexistent/path"), None)
    }
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

/// Assert that a block returns a not-found failure when the trigger project is not in the registry.
///
/// Constructs a trigger with event type `event_type` and project `"unknown-project"`,
/// executes the block, and asserts `!result.success && result.events.is_empty()`.
pub async fn assert_missing_project_fails(
    block: &dyn foundry_core::task_block::TaskBlock,
    event_type: foundry_core::event::EventType,
) {
    let trigger = make_trigger(event_type, "unknown-project", serde_json::json!({}));
    let result = block.execute(&trigger).await.unwrap();
    assert!(!result.success, "expected failure for missing project");
    assert!(result.events.is_empty(), "expected no events for missing project");
}

/// Register the standard iterate-chain blocks into `engine`.
///
/// Registers: `CheckCharter`, `ResolveGates`, `RunPreflightGates`,
/// `AssessProject`, `TriageAssessment`, `CreatePlan`, `ExecutePlan`,
/// `RunVerifyGates`, `RouteGateResult`, `RetryExecution`, `SummarizeResult`.
///
/// The `shell` gateway is shared by ALL shell-using blocks, including the
/// execution blocks (`ExecutePlan`, `RetryExecution`).  This ensures that
/// `detect_post_execution_changes` uses the fake shell rather than spawning
/// a real `git` process against the test temp directory.
///
/// Chain-specific blocks (e.g. `ExecuteMaintain`, `CommitAndPush`) must be
/// registered separately by the caller after this call.
pub fn register_iterate_chain(
    engine: &mut Engine,
    shell: Arc<dyn ShellGateway>,
    agent: Arc<dyn AgentGateway>,
    registry: Arc<RwLock<Registry>>,
) {
    engine.register(Box::new(super::CheckCharter::new(registry.clone())));
    engine.register(Box::new(super::ResolveGates::new(registry.clone())));
    engine.register(Box::new(super::RunPreflightGates::new(shell.clone(), registry.clone())));
    engine.register(Box::new(super::AssessProject::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::TriageAssessment::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::CreatePlan::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::ExecutePlan::with_gateways(
        agent.clone(),
        registry.clone(),
        shell.clone(),
    )));
    engine.register(Box::new(super::RunVerifyGates::new(shell.clone(), registry.clone())));
    engine.register(Box::new(super::RouteGateResult));
    engine.register(Box::new(super::RetryExecution::with_gateways(
        agent.clone(),
        registry.clone(),
        shell,
    )));
    engine.register(Box::new(super::SummarizeResult::new(agent, registry)));
}
