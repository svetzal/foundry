//! Shared test fixtures for task block unit tests.
//!
//! Gated with `#[cfg(test)]` — this module is only compiled during testing.
#![allow(dead_code)]

use std::sync::Arc;

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
pub fn registry_with_project(name: &str, path: &str) -> Arc<Registry> {
    Arc::new(Registry {
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
    })
}

/// Build a registry containing a single project entry with custom fields via a pre-built entry.
pub fn registry_with_entry(entry: ProjectEntry) -> Arc<Registry> {
    Arc::new(Registry {
        version: 2,
        projects: vec![entry],
    })
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
) -> foundry_core::registry::ProjectEntry {
    foundry_core::registry::ProjectEntry {
        name: name.to_string(),
        path: path.to_string(),
        stack: foundry_core::registry::Stack::Rust,
        agent: "claude".to_string(),
        repo: String::new(),
        branch: "main".to_string(),
        skip: None,
        notes: None,
        actions: foundry_core::registry::ActionFlags::default(),
        install,
        installs_skill: None,
        timeout_secs: None,
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

/// Register the standard iterate-chain blocks into `engine`.
///
/// Registers: `CheckCharter`, `ResolveGates`, `RunPreflightGates`,
/// `AssessProject`, `TriageAssessment`, `CreatePlan`, `ExecutePlan`,
/// `RunVerifyGates`, `RouteGateResult`, `RetryExecution`, `SummarizeResult`.
///
/// Chain-specific blocks (e.g. `ExecuteMaintain`, `CommitAndPush`) must be
/// registered separately by the caller after this call.
pub fn register_iterate_chain(
    engine: &mut Engine,
    shell: Arc<dyn ShellGateway>,
    agent: Arc<dyn AgentGateway>,
    registry: Arc<Registry>,
) {
    engine.register(Box::new(super::CheckCharter::new(registry.clone())));
    engine.register(Box::new(super::ResolveGates::new(registry.clone())));
    engine.register(Box::new(super::RunPreflightGates::new(shell.clone(), registry.clone())));
    engine.register(Box::new(super::AssessProject::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::TriageAssessment::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::CreatePlan::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::ExecutePlan::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::RunVerifyGates::new(shell, registry.clone())));
    engine.register(Box::new(super::RouteGateResult));
    engine.register(Box::new(super::RetryExecution::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::SummarizeResult::new(agent, registry)));
}
