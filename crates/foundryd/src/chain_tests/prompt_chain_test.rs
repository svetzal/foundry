//! Integration tests for the prompt-driven workflow formation.
//!
//! Verifies:
//! - Happy path: `ExecutionRequested` → charter check → gates → preflight
//!   → direct prompt → execute → verify → completion → summarise → commit
//! - Assessment/triage/plan blocks do NOT fire
//! - Standard iterate still works when engine has both formations

use std::sync::{Arc, RwLock};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::registry::Registry;
use foundry_sdk::throttle::Throttle;

use super::test_helpers;
use foundry_blocks::gateway::{AgentGateway, ShellGateway};
use foundry_engine::engine::Engine;
use foundry_sdk::gateway::fakes::FakeAgentGateway;

#[allow(clippy::needless_pass_by_value)]
fn prompt_engine(
    shell: Arc<dyn ShellGateway>,
    agent: Arc<dyn AgentGateway>,
    registry: Arc<RwLock<Registry>>,
) -> Engine {
    let mut engine = Engine::new();

    // Charter check (sinks on ProjectIterationRequested + ExecutionRequested)
    engine.register(Box::new(foundry_blocks::blocks::CheckCharter::new(registry.clone())));
    // Gate resolution (sinks on CharterCheckCompleted)
    engine.register(Box::new(foundry_blocks::blocks::ResolveGates::new(registry.clone())));
    // Preflight gates (sinks on GateResolutionCompleted)
    engine.register(Box::new(foundry_blocks::blocks::RunPreflightGates::new(
        shell.clone(),
        registry.clone(),
    )));
    // Direct prompt (sinks on PreflightCompleted, workflow=prompt only)
    engine.register(Box::new(foundry_blocks::blocks::DirectPrompt));
    // Assessment blocks — should NOT fire for prompt workflow
    engine.register(Box::new(foundry_blocks::blocks::AssessProject::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::TriageAssessment::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::CreatePlan::new(
        agent.clone(),
        registry.clone(),
    )));
    // Execution (sinks on PlanCompleted)
    engine.register(Box::new(foundry_blocks::blocks::ExecutePlan::new(
        agent.clone(),
        registry.clone(),
    )));
    // Verify gates (sinks on ExecutionCompleted)
    engine.register(Box::new(foundry_blocks::blocks::RunVerifyGates::new(shell, registry.clone())));
    // Routing (sinks on GateVerificationCompleted)
    engine.register(Box::new(foundry_blocks::blocks::RouteGateResult));
    // Retry (sinks on RetryRequested)
    engine.register(Box::new(foundry_blocks::blocks::RetryExecution::new(
        agent.clone(),
        registry.clone(),
    )));
    // Terminal blocks
    engine.register(Box::new(foundry_blocks::blocks::SummarizeResult::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::CommitAndPush::new(registry)));

    engine
}

#[tokio::test]
async fn prompt_workflow_happy_path() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("CHARTER.md"), "a".repeat(100)).unwrap();
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"true","required":true}]}"#,
    )
    .unwrap();

    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());

    // Agent responses:
    // 1. ExecutePlan: execute the user's prompt
    // 2. SummarizeResult: generate summary
    let agent = test_helpers::sequenced_agent(vec![
        "Done, implemented the feature",
        "HEADLINE: Implement feature\nSUMMARY: Implemented the requested feature.",
    ]);

    let engine = prompt_engine(test_helpers::passing_shell(), agent, registry);

    let trigger = Event::new(
        EventType::ExecutionRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({
            "project": "test-project",
            "prompt": "Pick the highest priority interaction from et and implement it.",
        }),
    );

    let result = engine.process(trigger).await;

    let event_types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Should see the full chain minus assess/triage/plan
    assert!(
        event_types.iter().any(|t| t == "charter_check_completed"),
        "missing charter_check_completed in {event_types:?}"
    );
    assert!(
        event_types.iter().any(|t| t == "gate_resolution_completed"),
        "missing gate_resolution_completed in {event_types:?}"
    );
    assert!(
        event_types.iter().any(|t| t == "preflight_completed"),
        "missing preflight_completed in {event_types:?}"
    );
    assert!(
        event_types.iter().any(|t| t == "plan_completed"),
        "missing plan_completed (from DirectPrompt) in {event_types:?}"
    );
    assert!(
        event_types.iter().any(|t| t == "execution_completed"),
        "missing execution_completed in {event_types:?}"
    );
    assert!(
        event_types.iter().any(|t| t == "gate_verification_completed"),
        "missing gate_verification_completed in {event_types:?}"
    );
    assert!(
        event_types.iter().any(|t| t == "project_iteration_completed"),
        "missing project_iteration_completed in {event_types:?}"
    );
    assert!(
        event_types.iter().any(|t| t == "summarize_completed"),
        "missing summarize_completed in {event_types:?}"
    );

    // Should NOT see assessment/triage events
    assert!(
        !event_types.iter().any(|t| t == "assessment_completed"),
        "assessment_completed should not appear in prompt workflow"
    );
    assert!(
        !event_types.iter().any(|t| t == "triage_completed"),
        "triage_completed should not appear in prompt workflow"
    );

    // The PlanCompleted event should carry the user's prompt as the plan
    let plan_event =
        result.events.iter().find(|e| e.event_type == EventType::PlanCompleted).unwrap();
    assert_eq!(
        plan_event.payload["plan"],
        "Pick the highest priority interaction from et and implement it."
    );
    assert_eq!(plan_event.payload["workflow"], "prompt");
}

#[tokio::test]
async fn prompt_workflow_charter_failure_stops_chain() {
    let dir = tempfile::tempdir().unwrap();
    // No CHARTER.md — charter check will fail
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"true","required":true}]}"#,
    )
    .unwrap();

    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());
    let agent = FakeAgentGateway::success();

    let engine = prompt_engine(test_helpers::passing_shell(), agent, registry);

    let trigger = Event::new(
        EventType::ExecutionRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({
            "project": "test-project",
            "prompt": "Do something.",
        }),
    );

    let result = engine.process(trigger).await;

    let event_types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Charter check should complete (with success=false)
    assert!(event_types.iter().any(|t| t == "charter_check_completed"));
    // Gate resolution should see success=false and skip
    // No execution should happen
    assert!(!event_types.iter().any(|t| t == "execution_completed"));
    assert!(!event_types.iter().any(|t| t == "project_iteration_completed"));
}
