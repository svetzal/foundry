//! Integration tests for the full native iterate workflow chain.
//!
//! Wires up the complete event chain with fake gateways and verifies:
//! - Happy path: IterationRequested -> CharterCheckCompleted -> GateResolutionCompleted
//!   -> PreflightCompleted -> AssessmentCompleted -> TriageCompleted -> PlanCompleted
//!   -> ExecutionCompleted -> GateVerificationCompleted -> ProjectIterationCompleted
//!   -> SummarizeCompleted
//! - Charter failure stops chain
//! - Preflight failure stops chain
//! - Triage rejection stops chain
//! - Retry loop on gate failure
//! - Iterate with maintain chaining

use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::throttle::Throttle;

use crate::blocks::test_helpers;
use crate::engine::Engine;
use crate::gateway::fakes::{FakeAgentGateway, FakeShellGateway};
use crate::gateway::{AgentGateway, AgentResponse, ShellGateway};
use crate::shell::CommandResult;

fn iteration_requested_event(maintain: bool) -> Event {
    Event::new(
        EventType::IterationRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({
            "project": "test-project",
            "workflow": "iterate",
            "actions": { "iterate": true, "maintain": maintain },
        }),
    )
}

/// Build the full iterate chain engine with fake gateways.
fn iterate_engine(
    shell: Arc<dyn ShellGateway>,
    agent: Arc<dyn AgentGateway>,
    registry: Arc<Registry>,
) -> Engine {
    let mut engine = Engine::new();
    test_helpers::register_iterate_chain(&mut engine, shell, agent, registry);
    engine
}

#[tokio::test]
async fn happy_path_iterate_chain() {
    let dir = tempfile::tempdir().unwrap();
    // Charter file so CheckCharter passes
    std::fs::write(dir.path().join("CHARTER.md"), "a".repeat(100)).unwrap();
    // Gates file
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
    )
    .unwrap();

    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());
    // All gates pass
    let shell = FakeShellGateway::success();
    // Agent responses: assess (JSON), name (kebab), triage (JSON), plan (text), execute (success), summarize
    let agent = FakeAgentGateway::sequence(vec![
        // AssessProject — assessment response
        AgentResponse {
            stdout: r#"{"severity": 7, "principle": "DRY", "category": "duplication", "assessment": "Duplicate validation logic."}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // AssessProject — name generation
        AgentResponse {
            stdout: "fix-duplicate-validation".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // TriageAssessment
        AgentResponse {
            stdout: r#"{"accepted": true, "reason": "severity warrants fix"}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // CreatePlan
        AgentResponse {
            stdout: "1. Extract shared validation\n2. Update callers\n3. Add tests".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // ExecutePlan
        AgentResponse {
            stdout: "Changes applied successfully".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // SummarizeResult
        AgentResponse {
            stdout: "HEADLINE: Fix duplicate validation logic\nSUMMARY: Extracted shared validation helper.".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
    ]);

    let engine = iterate_engine(shell, agent, registry);
    let result = engine.process(iteration_requested_event(false)).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Verify the full chain
    assert!(
        event_types.contains(&"iteration_requested"),
        "chain should start with iteration_requested"
    );
    assert!(event_types.contains(&"charter_check_completed"), "should check charter");
    assert!(event_types.contains(&"gate_resolution_completed"), "should resolve gates");
    assert!(event_types.contains(&"preflight_completed"), "should complete preflight");
    assert!(event_types.contains(&"assessment_completed"), "should complete assessment");
    assert!(event_types.contains(&"triage_completed"), "should complete triage");
    assert!(event_types.contains(&"plan_completed"), "should complete plan");
    assert!(event_types.contains(&"execution_completed"), "should complete execution");
    assert!(event_types.contains(&"gate_verification_completed"), "should verify gates");
    assert!(
        event_types.contains(&"project_iteration_completed"),
        "should emit iterate completion"
    );
    assert!(event_types.contains(&"summarize_completed"), "should summarize result");

    // Verify completion event has success=true
    let completion = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectIterationCompleted)
        .unwrap();
    assert_eq!(completion.payload["success"], true);

    // Verify summary
    let summary = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::SummarizeCompleted)
        .unwrap();
    assert_eq!(summary.payload["headline"], "Fix duplicate validation logic");

    // No retries needed
    assert!(!event_types.contains(&"retry_requested"), "no retries should be needed");
}

#[tokio::test]
async fn charter_failure_stops_chain() {
    let dir = tempfile::tempdir().unwrap();
    // No CHARTER.md — charter check will fail

    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());
    let shell = FakeShellGateway::success();
    let agent = FakeAgentGateway::success();

    let engine = iterate_engine(shell, agent, registry);
    let result = engine.process(iteration_requested_event(false)).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Charter check should be emitted with passed=false
    assert!(event_types.contains(&"charter_check_completed"), "should check charter");
    let charter_event = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::CharterCheckCompleted)
        .unwrap();
    assert_eq!(charter_event.payload["success"], false);

    // Chain should stop — no downstream events
    assert!(
        !event_types.contains(&"gate_resolution_completed"),
        "should NOT resolve gates after charter failure"
    );
    assert!(
        !event_types.contains(&"assessment_completed"),
        "should NOT assess after charter failure"
    );
    assert!(
        !event_types.contains(&"project_iteration_completed"),
        "should NOT complete iterate"
    );
}

#[tokio::test]
async fn preflight_failure_stops_chain() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("CHARTER.md"), "a".repeat(100)).unwrap();
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
    )
    .unwrap();

    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());
    // Preflight gate fails
    let shell = FakeShellGateway::failure("formatting error");
    let agent = FakeAgentGateway::success();

    let engine = iterate_engine(shell, agent, registry);
    let result = engine.process(iteration_requested_event(false)).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    assert!(event_types.contains(&"charter_check_completed"), "should check charter");
    assert!(event_types.contains(&"gate_resolution_completed"), "should resolve gates");
    assert!(event_types.contains(&"preflight_completed"), "should complete preflight");

    // Preflight should have all_passed=false
    let preflight = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::PreflightCompleted)
        .unwrap();
    assert_eq!(preflight.payload["all_passed"], false);

    // Chain should stop — AssessProject self-filters on failed preflight
    assert!(
        !event_types.contains(&"assessment_completed"),
        "should NOT assess after preflight failure"
    );
    assert!(
        !event_types.contains(&"project_iteration_completed"),
        "should NOT complete iterate"
    );
}

#[tokio::test]
async fn triage_rejection_stops_chain() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("CHARTER.md"), "a".repeat(100)).unwrap();
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
    )
    .unwrap();

    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());
    let shell = FakeShellGateway::success();
    // Agent responses: assess, name, triage (rejected)
    let agent = FakeAgentGateway::sequence(vec![
        // AssessProject — assessment
        AgentResponse {
            stdout: r#"{"severity": 2, "principle": "formatting", "category": "conventions", "assessment": "Minor formatting issues."}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // AssessProject — name
        AgentResponse {
            stdout: "fix-formatting".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // TriageAssessment — rejected
        AgentResponse {
            stdout: r#"{"accepted": false, "reason": "too trivial, severity only 2"}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
    ]);

    let engine = iterate_engine(shell, agent, registry);
    let result = engine.process(iteration_requested_event(false)).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    assert!(event_types.contains(&"triage_completed"), "should complete triage");
    let triage = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::TriageCompleted)
        .unwrap();
    assert_eq!(triage.payload["accepted"], false);

    // Chain should stop — CreatePlan self-filters on rejected triage
    assert!(
        !event_types.contains(&"plan_completed"),
        "should NOT create plan after triage rejection"
    );
    assert!(
        !event_types.contains(&"execution_completed"),
        "should NOT execute after triage rejection"
    );
    assert!(
        !event_types.contains(&"project_iteration_completed"),
        "should NOT complete iterate"
    );
}

#[tokio::test]
async fn gate_verification_retry_loop() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("CHARTER.md"), "a".repeat(100)).unwrap();
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"test","command":"cargo test","required":true}]}"#,
    )
    .unwrap();

    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());

    // Shell: preflight passes, first verify fails, second verify passes
    let shell = FakeShellGateway::sequence(vec![
        // Preflight — pass
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // First verify (after ExecutePlan) — fail
        CommandResult {
            stdout: String::new(),
            stderr: "test failed".to_string(),
            exit_code: 1,
            success: false,
        },
        // Second verify (after RetryExecution) — pass
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
    ]);

    // Agent: assess, name, triage, plan, execute, retry, summarize
    let agent = FakeAgentGateway::sequence(vec![
        // AssessProject — assessment
        AgentResponse {
            stdout: r#"{"severity": 6, "principle": "testing", "category": "testing", "assessment": "Missing test coverage."}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // AssessProject — name
        AgentResponse {
            stdout: "add-test-coverage".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // TriageAssessment — accepted
        AgentResponse {
            stdout: r#"{"accepted": true, "reason": "needs more tests"}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // CreatePlan
        AgentResponse {
            stdout: "1. Add tests for uncovered functions".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // ExecutePlan
        AgentResponse {
            stdout: "Tests added".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // RetryExecution (after first gate failure)
        AgentResponse {
            stdout: "Fixed test issues".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // SummarizeResult
        AgentResponse {
            stdout: "HEADLINE: Add test coverage\nSUMMARY: Added missing tests.".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
    ]);

    let engine = iterate_engine(shell, agent, registry);
    let result = engine.process(iteration_requested_event(false)).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Should have retry flow
    assert!(
        event_types.contains(&"retry_requested"),
        "should request retry after gate failure"
    );

    // Count ExecutionCompleted events — should have 2 (initial + retry)
    let execution_count = result
        .events
        .iter()
        .filter(|e| e.event_type == EventType::ExecutionCompleted)
        .count();
    assert_eq!(execution_count, 2, "should have initial execution + retry");

    // Count GateVerificationCompleted — should have 2
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
        .find(|e| e.event_type == EventType::ProjectIterationCompleted)
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
async fn iterate_with_maintain_chaining() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("CHARTER.md"), "a".repeat(100)).unwrap();
    std::fs::write(
        dir.path().join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"cargo fmt --check","required":true}]}"#,
    )
    .unwrap();

    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());
    let shell = FakeShellGateway::success();
    // Agent: assess, name, triage, plan, execute, summarize (iterate), then maintain chain agents...
    // We only verify MaintenanceRequested is emitted; the maintain chain needs its own engine blocks.
    let agent = FakeAgentGateway::sequence(vec![
        // AssessProject — assessment
        AgentResponse {
            stdout: r#"{"severity": 5, "principle": "clarity", "category": "clarity", "assessment": "Unclear naming."}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // AssessProject — name
        AgentResponse {
            stdout: "improve-naming".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // TriageAssessment — accepted
        AgentResponse {
            stdout: r#"{"accepted": true, "reason": "severity warrants fix"}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // CreatePlan
        AgentResponse {
            stdout: "1. Rename unclear variables".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // ExecutePlan
        AgentResponse {
            stdout: "Names improved".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // SummarizeResult (iterate)
        AgentResponse {
            stdout: "HEADLINE: Improve naming\nSUMMARY: Renamed unclear variables.".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // ExecuteMaintain (from chained MaintenanceRequested -> GateResolutionCompleted -> ExecuteMaintain)
        AgentResponse {
            stdout: "Dependencies updated".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // SummarizeResult (maintain)
        AgentResponse {
            stdout: "HEADLINE: Update deps\nSUMMARY: Updated.".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
    ]);

    // Build engine with BOTH iterate and maintain chain blocks
    let mut engine = Engine::new();
    test_helpers::register_iterate_chain(&mut engine, shell, agent.clone(), registry.clone());
    // Also register maintain blocks so the chained MaintenanceRequested is handled
    engine.register(Box::new(super::ExecuteMaintain::new(agent.clone(), registry.clone())));

    let result = engine.process(iteration_requested_event(true)).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Verify iterate completed successfully
    assert!(event_types.contains(&"project_iteration_completed"), "should complete iterate");
    let completion = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectIterationCompleted)
        .unwrap();
    assert_eq!(completion.payload["success"], true);

    // Verify MaintenanceRequested was emitted
    assert!(
        event_types.contains(&"maintenance_requested"),
        "should emit maintenance_requested when actions.maintain=true"
    );

    // Verify the maintain chain also ran
    assert!(
        event_types.contains(&"project_maintenance_completed"),
        "should complete the chained maintain workflow"
    );
}
