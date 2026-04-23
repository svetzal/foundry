//! Integration tests for the prompt-driven workflow formation.
//!
//! Verifies:
//! - Happy path: `PromptExecutionRequested` → charter check → gates → preflight
//!   → direct prompt → execute → verify → completion → summarise → commit
//! - Assessment/triage/plan blocks do NOT fire
//! - Standard iterate still works when engine has both formations

use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::throttle::Throttle;

use crate::blocks::test_helpers;
use crate::engine::Engine;
use crate::gateway::fakes::FakeAgentGateway;
use crate::gateway::{AgentGateway, ShellGateway};

fn prompt_engine(
    shell: Arc<dyn ShellGateway>,
    agent: &Arc<dyn AgentGateway>,
    registry: Arc<Registry>,
) -> Engine {
    let mut engine = Engine::new();

    // Charter check (sinks on IterationRequested + PromptExecutionRequested)
    engine.register(Box::new(super::CheckCharter::new(registry.clone())));
    // Gate resolution (sinks on CharterCheckCompleted)
    engine.register(Box::new(super::ResolveGates::new(registry.clone())));
    // Preflight gates (sinks on GateResolutionCompleted)
    engine.register(Box::new(super::RunPreflightGates::new(shell.clone(), registry.clone())));
    // Direct prompt (sinks on PreflightCompleted, workflow=prompt only)
    engine.register(Box::new(super::DirectPrompt));
    // Assessment blocks — should NOT fire for prompt workflow
    engine.register(Box::new(super::AssessProject::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::TriageAssessment::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::CreatePlan::new(agent.clone(), registry.clone())));
    // Execution (sinks on PlanCompleted)
    engine.register(Box::new(super::ExecutePlan::new(agent.clone(), registry.clone())));
    // Verify gates (sinks on ExecutionCompleted)
    engine.register(Box::new(super::RunVerifyGates::new(shell, registry.clone())));
    // Routing (sinks on GateVerificationCompleted)
    engine.register(Box::new(super::RouteGateResult));
    // Retry (sinks on RetryRequested)
    engine.register(Box::new(super::RetryExecution::new(agent.clone(), registry.clone())));
    // Terminal blocks
    engine.register(Box::new(super::SummarizeResult::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::CommitAndPush::new(registry)));

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

    let engine = prompt_engine(test_helpers::passing_shell(), &agent, registry);

    let trigger = Event::new(
        EventType::PromptExecutionRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({
            "project": "test-project",
            "prompt": "Pick the highest priority interaction from et and implement it.",
        }),
    );

    let result = engine.process(trigger).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Should see the full chain minus assess/triage/plan
    assert!(
        event_types.contains(&"charter_check_completed"),
        "missing charter_check_completed in {event_types:?}"
    );
    assert!(
        event_types.contains(&"gate_resolution_completed"),
        "missing gate_resolution_completed in {event_types:?}"
    );
    assert!(
        event_types.contains(&"preflight_completed"),
        "missing preflight_completed in {event_types:?}"
    );
    assert!(
        event_types.contains(&"plan_completed"),
        "missing plan_completed (from DirectPrompt) in {event_types:?}"
    );
    assert!(
        event_types.contains(&"execution_completed"),
        "missing execution_completed in {event_types:?}"
    );
    assert!(
        event_types.contains(&"gate_verification_completed"),
        "missing gate_verification_completed in {event_types:?}"
    );
    assert!(
        event_types.contains(&"project_iteration_completed"),
        "missing project_iteration_completed in {event_types:?}"
    );
    assert!(
        event_types.contains(&"summarize_completed"),
        "missing summarize_completed in {event_types:?}"
    );

    // Should NOT see assessment/triage events
    assert!(
        !event_types.contains(&"assessment_completed"),
        "assessment_completed should not appear in prompt workflow"
    );
    assert!(
        !event_types.contains(&"triage_completed"),
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
    let agent: Arc<dyn AgentGateway> = FakeAgentGateway::success();

    let engine = prompt_engine(test_helpers::passing_shell(), &agent, registry);

    let trigger = Event::new(
        EventType::PromptExecutionRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({
            "project": "test-project",
            "prompt": "Do something.",
        }),
    );

    let result = engine.process(trigger).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Charter check should complete (with success=false)
    assert!(event_types.contains(&"charter_check_completed"));
    // Gate resolution should see success=false and skip
    // No execution should happen
    assert!(!event_types.contains(&"execution_completed"));
    assert!(!event_types.contains(&"project_iteration_completed"));
}
