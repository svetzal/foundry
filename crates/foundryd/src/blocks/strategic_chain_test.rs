//! Integration tests for the strategic (nested) iteration loop.
//!
//! Verifies:
//! - Strategic assessment → inner iterate → commit → re-assess → complete
//! - Early termination when AI says no more work
//! - Max iteration cap stops the loop
//! - Non-strategic iteration still works (backward compatibility)

use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::throttle::Throttle;

use crate::blocks::test_helpers;
use crate::engine::Engine;
use crate::gateway::fakes::{FakeAgentGateway, FakeShellGateway};
use crate::gateway::{AgentGateway, AgentResponse, ShellGateway};
use crate::shell::CommandResult;

fn test_registry(project_path: &str) -> Arc<Registry> {
    test_helpers::registry_with_project("test-project", project_path)
}

/// Build the full strategic loop engine with inner iterate chain.
fn strategic_engine(
    shell: Arc<dyn ShellGateway>,
    agent: Arc<dyn AgentGateway>,
    registry: Arc<Registry>,
) -> Engine {
    let mut engine = Engine::new();

    // Strategic loop blocks
    engine.register(Box::new(super::StrategicAssessor::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::StrategicLoopController::new(agent.clone(), registry.clone())));
    // Inner iterate chain blocks
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
    // Terminal blocks
    engine.register(Box::new(super::SummarizeResult::new(agent.clone(), registry.clone())));
    engine.register(Box::new(super::CommitAndPush::new(registry)));

    engine
}

fn strategic_iteration_requested() -> Event {
    Event::new(
        EventType::IterationRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({
            "project": "test-project",
            "strategic": true,
            "max_iterations": 2,
        }),
    )
}

fn non_strategic_iteration_requested() -> Event {
    Event::new(
        EventType::IterationRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({
            "project": "test-project",
            "actions": { "iterate": true, "maintain": false },
        }),
    )
}

/// Agent that returns responses in sequence. Used to simulate the strategic
/// assessment, then inner iterate blocks, then the continue check.
fn sequenced_agent(responses: Vec<&str>) -> Arc<dyn AgentGateway> {
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

/// Shell that always passes gates.
fn passing_shell() -> Arc<dyn ShellGateway> {
    FakeShellGateway::always(CommandResult {
        stdout: String::new(),
        stderr: String::new(),
        exit_code: 0,
        success: true,
    })
}

#[tokio::test]
async fn strategic_loop_runs_one_iteration_then_stops() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("CHARTER.md"), "a".repeat(100)).unwrap();
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"true","required":true}]}"#,
    )
    .unwrap();

    let registry = test_registry(dir.path().to_str().unwrap());

    // Agent responses in order:
    // 1. StrategicAssessor: returns areas
    // 2. AssessProject: assessment
    // 3. AssessProject: audit name
    // 4. TriageAssessment: accept
    // 5. CreatePlan: plan
    // 6. ExecutePlan: execute
    // 7. SummarizeResult (for InnerIterationCompleted — skipped because terminal guard)
    // 8. StrategicLoopController: continue check → false
    // 9. SummarizeResult: final summary
    let agent = sequenced_agent(vec![
        // 1: Strategic assessment
        r#"{"areas": [{"area": "test coverage", "severity": 8, "category": "testing"}]}"#,
        // 2: Assess project
        r#"{"severity": 8, "principle": "test coverage", "category": "testing", "assessment": "need tests"}"#,
        // 3: Audit name
        "fix-test-coverage",
        // 4: Triage
        r#"{"accepted": true, "reason": "high severity"}"#,
        // 5: Create plan
        "1. Add tests to module X",
        // 6: Execute plan
        "Done, added tests",
        // 7: StrategicLoopController continue check → stop
        r#"{"continue": false, "reason": "quality plateau"}"#,
        // 8: SummarizeResult (final)
        "HEADLINE: Improve test coverage\nSUMMARY: Added tests.",
    ]);

    let engine = strategic_engine(passing_shell(), agent, registry);
    let result = engine.process(strategic_iteration_requested()).await;

    // Collect event types
    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Should see the strategic assessment, inner iterate chain, then completion
    assert!(
        event_types.contains(&"strategic_assessment_completed"),
        "missing strategic_assessment_completed in {event_types:?}"
    );
    assert!(
        event_types.contains(&"inner_iteration_completed"),
        "missing inner_iteration_completed in {event_types:?}"
    );
    assert!(
        event_types.contains(&"project_iteration_completed"),
        "missing project_iteration_completed (terminal) in {event_types:?}"
    );
    assert!(
        event_types.contains(&"summarize_completed"),
        "missing summarize_completed in {event_types:?}"
    );

    // The terminal ProjectIterationCompleted should NOT have loop_context
    let terminal = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectIterationCompleted)
        .unwrap();
    assert!(
        terminal.payload.get("loop_context").is_none(),
        "terminal completion should not have loop_context"
    );
}

#[tokio::test]
async fn strategic_loop_stops_at_max_iterations() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("CHARTER.md"), "a".repeat(100)).unwrap();
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"true","required":true}]}"#,
    )
    .unwrap();

    let registry = test_registry(dir.path().to_str().unwrap());

    // With max_iterations=1, should run one inner loop then complete regardless
    // of continue check.
    let trigger = Event::new(
        EventType::IterationRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({
            "project": "test-project",
            "strategic": true,
            "max_iterations": 1,
        }),
    );

    let agent = sequenced_agent(vec![
        // Strategic assessment
        r#"{"areas": [{"area": "naming", "severity": 5, "category": "naming"}]}"#,
        // Assess
        r#"{"severity": 5, "principle": "naming", "category": "naming", "assessment": "inconsistent"}"#,
        // Audit name
        "fix-naming",
        // Triage
        r#"{"accepted": true, "reason": "ok"}"#,
        // Plan
        "1. Rename",
        // Execute
        "Done",
        // SummarizeResult (final — no continue check since max=1 reached)
        "HEADLINE: Fix naming\nSUMMARY: Renamed.",
    ]);

    let engine = strategic_engine(passing_shell(), agent, registry);
    let result = engine.process(trigger).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    assert!(event_types.contains(&"inner_iteration_completed"));
    assert!(event_types.contains(&"project_iteration_completed"));

    // Should NOT have a second iteration_requested (loop stopped at max)
    let iteration_requests: Vec<_> = result
        .events
        .iter()
        .filter(|e| e.event_type == EventType::IterationRequested)
        .collect();
    // First is the root event, second is from StrategicLoopController
    assert_eq!(
        iteration_requests.len(),
        2,
        "should have root + 1 inner iteration request, got {}: {:?}",
        iteration_requests.len(),
        iteration_requests.iter().map(|e| &e.payload).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn non_strategic_iteration_still_works() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("CHARTER.md"), "a".repeat(100)).unwrap();
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"true","required":true}]}"#,
    )
    .unwrap();

    let registry = test_registry(dir.path().to_str().unwrap());

    let agent = sequenced_agent(vec![
        // Assess
        r#"{"severity": 7, "principle": "clarity", "category": "clarity", "assessment": "unclear"}"#,
        // Audit name
        "fix-clarity",
        // Triage
        r#"{"accepted": true, "reason": "high severity"}"#,
        // Plan
        "1. Clarify",
        // Execute
        "Done",
        // Summarize
        "HEADLINE: Improve clarity\nSUMMARY: Clarified.",
    ]);

    let engine = strategic_engine(passing_shell(), agent, registry);
    let result = engine.process(non_strategic_iteration_requested()).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Should NOT see strategic events
    assert!(
        !event_types.contains(&"strategic_assessment_completed"),
        "strategic events should not appear in non-strategic flow"
    );
    assert!(
        !event_types.contains(&"inner_iteration_completed"),
        "inner_iteration_completed should not appear in non-strategic flow"
    );
    // Should see normal iteration completion
    assert!(
        event_types.contains(&"project_iteration_completed"),
        "should see normal project_iteration_completed"
    );
    assert!(event_types.contains(&"summarize_completed"), "should see summarize_completed");
}
