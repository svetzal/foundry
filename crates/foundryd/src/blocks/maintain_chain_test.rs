//! Integration tests for the full native maintain workflow chain.
//!
//! Wires up the complete event chain with fake gateways and verifies:
//! - Happy path: MaintenanceRequested -> GateResolutionCompleted -> ExecutionCompleted
//!   -> GateVerificationCompleted -> ProjectMaintenanceCompleted -> SummarizeCompleted
//! - Retry path: gate failure triggers RetryRequested -> RetryExecution -> loop

use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
use foundry_core::throttle::Throttle;

use crate::engine::Engine;
use crate::gateway::fakes::{FakeAgentGateway, FakeShellGateway};
use crate::gateway::{AgentGateway, ShellGateway};
use crate::shell::CommandResult;

fn test_registry(project_path: &str) -> Arc<Registry> {
    Arc::new(Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: "test-project".to_string(),
            path: project_path.to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: ActionFlags::default(),
            install: None,
            timeout_secs: None,
        }],
    })
}

fn maintenance_requested_event() -> Event {
    Event::new(
        EventType::MaintenanceRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({ "project": "test-project" }),
    )
}

/// Build the full maintain chain engine with fake gateways.
fn maintain_engine(
    shell: Arc<dyn ShellGateway>,
    agent: Arc<dyn AgentGateway>,
    registry: Arc<Registry>,
) -> Engine {
    let mut engine = Engine::new();

    // Gate resolution and verification
    engine.register(Box::new(super::ResolveGates::new(registry.clone())));
    engine.register(Box::new(super::RunPreflightGates::new(shell.clone(), registry.clone())));
    engine.register(Box::new(super::RunVerifyGates::new(shell, registry.clone())));
    engine.register(Box::new(super::RouteGateResult));

    // Native maintain workflow blocks
    engine.register(Box::new(super::ExecuteMaintain::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::RetryExecution::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::SummarizeResult::new(agent, registry)));

    engine
}

#[tokio::test]
async fn happy_path_maintain_chain() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
    )
    .unwrap();

    let registry = test_registry(dir.path().to_str().unwrap());
    // All gates pass
    let shell = FakeShellGateway::success();
    // Agent succeeds for both ExecuteMaintain and SummarizeResult
    let agent = FakeAgentGateway::success_with(
        "HEADLINE: Update dependencies\nSUMMARY: Updated all deps to latest.",
    );

    let engine = maintain_engine(shell, agent, registry);
    let result = engine.process(maintenance_requested_event()).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Expected chain:
    // MaintenanceRequested -> GateResolutionCompleted -> PreflightCompleted (skipped for maintain)
    //   -> ExecutionCompleted -> GateVerificationCompleted -> ProjectMaintenanceCompleted
    //   -> SummarizeCompleted
    assert!(
        event_types.contains(&"maintenance_requested"),
        "chain should start with maintenance_requested"
    );
    assert!(event_types.contains(&"gate_resolution_completed"), "should resolve gates");
    assert!(event_types.contains(&"execution_completed"), "should complete execution");
    assert!(event_types.contains(&"gate_verification_completed"), "should verify gates");
    assert!(event_types.contains(&"project_maintenance_completed"), "should emit completion");
    assert!(event_types.contains(&"summarize_completed"), "should summarize result");

    // Verify the completion event has success=true
    let completion = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectMaintenanceCompleted)
        .unwrap();
    assert_eq!(completion.payload["success"], true);

    // Verify summary was generated
    let summary = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::SummarizeCompleted)
        .unwrap();
    assert_eq!(summary.payload["headline"], "Update dependencies");

    // Should NOT have any retry events
    assert!(!event_types.contains(&"retry_requested"), "no retries should be needed");
}

#[tokio::test]
async fn retry_loop_on_gate_failure_then_success() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"test","command":"cargo test","required":true}]}"#,
    )
    .unwrap();

    let registry = test_registry(dir.path().to_str().unwrap());

    // First gate verification fails, second succeeds
    let shell = FakeShellGateway::sequence(vec![
        // First: RunPreflightGates for maintain (skipped, no shell call expected for maintain)
        // Actually RunPreflightGates skips maintain workflow without calling shell.
        // First shell call: RunVerifyGates after initial ExecuteMaintain -> FAIL
        CommandResult {
            stdout: String::new(),
            stderr: "test failed".to_string(),
            exit_code: 1,
            success: false,
        },
        // Second shell call: RunVerifyGates after RetryExecution -> PASS
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
    ]);

    // Agent succeeds for ExecuteMaintain, RetryExecution, and SummarizeResult
    let agent =
        FakeAgentGateway::success_with("HEADLINE: Fix tests\nSUMMARY: Fixed failing tests.");

    let engine = maintain_engine(shell, agent, registry);
    let result = engine.process(maintenance_requested_event()).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Should have retry flow
    assert!(
        event_types.contains(&"retry_requested"),
        "should request retry after first gate failure"
    );

    // Count ExecutionCompleted events - should have 2 (initial + retry)
    let execution_count = result
        .events
        .iter()
        .filter(|e| e.event_type == EventType::ExecutionCompleted)
        .count();
    assert_eq!(execution_count, 2, "should have initial execution + retry");

    // Count GateVerificationCompleted events - should have 2
    let verification_count = result
        .events
        .iter()
        .filter(|e| e.event_type == EventType::GateVerificationCompleted)
        .count();
    assert_eq!(verification_count, 2, "should have two gate verifications");

    // Final outcome should be success
    let completion = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectMaintenanceCompleted)
        .unwrap();
    assert_eq!(completion.payload["success"], true);

    // ProcessResult::is_success() must agree — the retry-recovered chain is successful
    assert!(
        result.is_success(),
        "is_success() should be true when retry recovers from gate failure"
    );

    // Should have summary
    assert!(event_types.contains(&"summarize_completed"), "should summarize after success");
}

#[tokio::test]
async fn retries_exhausted_emits_failure() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"test","command":"cargo test","required":true}]}"#,
    )
    .unwrap();

    let registry = test_registry(dir.path().to_str().unwrap());

    // All gate verifications fail
    let shell = FakeShellGateway::failure("tests keep failing");

    // Agent always succeeds (but gates keep failing)
    let agent = FakeAgentGateway::success();

    let engine = maintain_engine(shell, agent, registry);
    let result = engine.process(maintenance_requested_event()).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Should have 3 retry attempts (retry_count 1, 2, 3)
    let retry_count = result
        .events
        .iter()
        .filter(|e| e.event_type == EventType::RetryRequested)
        .count();
    assert_eq!(retry_count, 3, "should have 3 retry requests");

    // Final outcome should be failure
    let completion = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectMaintenanceCompleted)
        .unwrap();
    assert_eq!(completion.payload["success"], false);

    // ProcessResult::is_success() must agree — exhausted retries is a real failure
    assert!(!result.is_success(), "is_success() should be false when retries are exhausted");

    // Should NOT have SummarizeCompleted (only on success)
    assert!(!event_types.contains(&"summarize_completed"), "should not summarize on failure");
}

#[tokio::test]
async fn no_gates_file_still_completes() {
    let dir = tempfile::tempdir().unwrap();
    // No .hone-gates.json written

    let registry = test_registry(dir.path().to_str().unwrap());
    let shell = FakeShellGateway::success();
    let agent = FakeAgentGateway::success_with("HEADLINE: Maintain\nSUMMARY: Done.");

    let engine = maintain_engine(shell, agent, registry);
    let result = engine.process(maintenance_requested_event()).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Should still complete successfully (empty gates = all pass)
    assert!(event_types.contains(&"project_maintenance_completed"));
    assert!(event_types.contains(&"summarize_completed"));

    let completion = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectMaintenanceCompleted)
        .unwrap();
    assert_eq!(completion.payload["success"], true);
}
