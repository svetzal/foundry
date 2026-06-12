//! Shared test fixtures for task block unit tests.
//!
//! Gated with `#[cfg(test)]` — this module is only compiled during testing.
#![allow(dead_code)]

use std::sync::{Arc, RwLock};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};
use foundry_sdk::task_block::TaskBlock;
use foundry_sdk::throttle::Throttle;

use crate::gateway::{AgentGateway, AgentResponse, ShellGateway};
use crate::shell::CommandResult;
use foundry_sdk::gateway::fakes::{FakeAgentGateway, FakeShellGateway};

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
    install: Option<foundry_sdk::registry::InstallConfig>,
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

/// Assert that a block with a failing agent emits `ExecutionCompleted` with `success: false`.
pub async fn assert_agent_failure_emits_failure(block: &dyn TaskBlock, trigger: &Event) {
    let result = block.execute(trigger).await.unwrap();
    assert!(!result.success);
    assert_eq!(result.events.len(), 1);
    assert_eq!(result.events[0].event_type, EventType::ExecutionCompleted);
    assert_eq!(result.events[0].payload["success"], false);
}

/// Assert that a block forwards `actions` from the trigger payload to `ExecutionCompleted`.
///
/// Expects `actions.maintain == true` in the first emitted event.
pub async fn assert_forwards_actions(block: &dyn TaskBlock, trigger: &Event) {
    let result = block.execute(trigger).await.unwrap();
    let actions = result.events[0].payload.get("actions").unwrap();
    assert_eq!(actions["maintain"], true);
}

/// Assert change detection when the working tree is dirty.
///
/// `expected_files` must all appear in `files_changed` of the first emitted event.
pub async fn assert_detects_changes_when_dirty(
    block: &dyn TaskBlock,
    trigger: &Event,
    expected_files: &[&str],
) {
    let result = block.execute(trigger).await.unwrap();
    assert!(result.success);
    assert_eq!(result.events[0].payload["changes_detected"], true);
    let files = result.events[0].payload["files_changed"].as_array().unwrap();
    for expected in expected_files {
        assert!(files.iter().any(|f| f == *expected), "expected {expected} in {files:?}");
    }
}

/// Assert that a clean working tree is reported correctly.
///
/// `expect_success`: `true` for blocks where a clean tree stays success (maintain),
/// `false` for blocks where the iterate override fires.
pub async fn assert_reports_no_changes_when_clean(
    block: &dyn TaskBlock,
    trigger: &Event,
    expect_success: bool,
) {
    let result = block.execute(trigger).await.unwrap();
    if expect_success {
        assert!(result.success, "expected success on clean tree");
    } else {
        assert!(!result.success, "expected iterate override to failure on clean tree");
    }
    assert_eq!(result.events[0].payload["changes_detected"], false);
    assert!(
        result.events[0]
            .payload
            .get("files_changed")
            .is_none_or(|v| v.as_array().is_none_or(std::vec::Vec::is_empty))
    );
}

/// Assert that a git status failure is tolerated, reporting `changes_detected: false`.
pub async fn assert_tolerates_git_failure(
    block: &dyn TaskBlock,
    trigger: &Event,
    expect_success: bool,
) {
    let result = block.execute(trigger).await.unwrap();
    if expect_success {
        assert!(result.success);
    } else {
        assert!(!result.success);
    }
    assert_eq!(result.events[0].payload["changes_detected"], false);
}

/// Build an empty registry with no projects.
pub fn empty_registry() -> Arc<RwLock<Registry>> {
    Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![],
    }))
}

/// Assert that a block returns a not-found failure when the trigger project is not in the registry.
///
/// Constructs a trigger with event type `event_type` and project `"unknown-project"`,
/// executes the block, and asserts `!result.success && result.events.is_empty()`.
pub async fn assert_missing_project_fails(
    block: &dyn foundry_sdk::task_block::TaskBlock,
    event_type: foundry_sdk::event::EventType,
) {
    let trigger = make_trigger(event_type, "unknown-project", serde_json::json!({}));
    let result = block.execute(&trigger).await.unwrap();
    assert!(!result.success, "expected failure for missing project");
    assert!(result.events.is_empty(), "expected no events for missing project");
}
