//! Integration tests for the full native iterate workflow chain.
//!
//! Wires up the complete event chain with fake gateways and verifies:
//! - Happy path: `ProjectIterationRequested` -> `CharterCheckCompleted` -> `GateResolutionCompleted`
//!   -> `PreflightCompleted` -> `AssessmentCompleted` -> `TriageCompleted` -> `PlanCompleted`
//!   -> `ExecutionCompleted` -> `GateVerificationCompleted` -> `ProjectIterationCompleted`
//!   -> `SummarizeCompleted`
//! - Charter failure stops chain
//! - Preflight failure stops chain
//! - Triage rejection stops chain
//! - Retry loop on gate failure
//! - Iterate with maintain chaining

use std::sync::{Arc, RwLock};

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
        EventType::ProjectIterationRequested,
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
    registry: Arc<RwLock<Registry>>,
) -> Engine {
    let mut engine = Engine::new();
    test_helpers::register_iterate_chain(&mut engine, shell, agent, registry);
    engine
}

#[tokio::test]
async fn happy_path_iterate_chain() {
    let dir = test_helpers::test_project_dir();
    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());
    // Shell sequence:
    // 1. RunPreflightGates — gate command passes (empty stdout)
    // 2. ExecutePlan — `git rev-parse HEAD` before agent (pre-execution sha capture)
    // 3. ExecutePlan — `git diff --name-only <sha>` after agent (returns changed files)
    // 4. RunVerifyGates — gate command passes (empty stdout)
    let shell = FakeShellGateway::sequence(vec![
        // Preflight gate — pass
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // git rev-parse HEAD before ExecutePlan agent — returns sha
        CommandResult {
            stdout: "abc123\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // git diff --name-only after ExecutePlan agent — non-empty to indicate real changes
        CommandResult {
            stdout: "src/lib.rs\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // Verify gate — pass
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
    ]);
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
        event_types.contains(&"project_iteration_requested"),
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
    let dir = test_helpers::test_project_dir_no_charter();
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

    // ResolveGates emits the terminal failure event so is_success() is accurate.
    assert!(
        event_types.contains(&"project_iteration_completed"),
        "should emit terminal failure when charter check fails"
    );
    let completion = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectIterationCompleted)
        .unwrap();
    assert_eq!(completion.payload["success"], false, "terminal event must be success=false");
    assert!(!result.is_success(), "overall chain must be failure when charter fails");

    // Chain should stop at the terminal event — no downstream iterate blocks
    assert!(
        !event_types.contains(&"gate_resolution_completed"),
        "should NOT resolve gates after charter failure"
    );
    assert!(
        !event_types.contains(&"assessment_completed"),
        "should NOT assess after charter failure"
    );
}

#[tokio::test]
async fn preflight_failure_stops_chain() {
    let dir = test_helpers::test_project_dir();
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

    // RunPreflightGates emits a terminal failure event so is_success() is accurate.
    assert!(
        event_types.contains(&"project_iteration_completed"),
        "should emit terminal failure when preflight fails"
    );
    let completion = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectIterationCompleted)
        .unwrap();
    assert_eq!(completion.payload["success"], false, "terminal event must be success=false");
    assert!(!result.is_success(), "overall chain must be failure when preflight fails");

    // Chain should stop at the terminal event — no downstream iterate blocks
    assert!(
        !event_types.contains(&"assessment_completed"),
        "should NOT assess after preflight failure"
    );
}

#[tokio::test]
async fn triage_busywork_rejection_stops_chain_as_failure() {
    // Severity above threshold but rejected as busy-work → chain stops, success=false.
    let dir = test_helpers::test_project_dir();
    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());
    let shell = FakeShellGateway::success();
    // Agent responses: assess, name, triage (rejected — high severity, busy-work)
    let agent = FakeAgentGateway::sequence(vec![
        // AssessProject — assessment
        AgentResponse {
            stdout: r#"{"severity": 6, "principle": "formatting", "category": "conventions", "assessment": "Cosmetic whitespace issues."}"#.to_string(),
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
        // TriageAssessment — rejected as busy-work despite severity 6
        AgentResponse {
            stdout: r#"{"accepted": false, "reason": "purely cosmetic whitespace, busy-work"}"#.to_string(),
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

    // CreatePlan emits a terminal failure event so is_success() is accurate.
    assert!(
        event_types.contains(&"project_iteration_completed"),
        "should emit terminal failure when high-severity triage is rejected as busy-work"
    );
    let completion = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectIterationCompleted)
        .unwrap();
    assert_eq!(completion.payload["success"], false, "terminal event must be success=false");
    assert!(
        !result.is_success(),
        "overall chain must be failure when busy-work triage is rejected"
    );

    // Chain should stop at the terminal event — no downstream iterate blocks
    assert!(
        !event_types.contains(&"plan_completed"),
        "should NOT create plan after triage rejection"
    );
    assert!(
        !event_types.contains(&"execution_completed"),
        "should NOT execute after triage rejection"
    );
}

#[tokio::test]
async fn triage_below_threshold_rejection_stops_chain_as_success() {
    // Severity below threshold → triage correctly filters; chain stops, success=true (successful no-op).
    let dir = test_helpers::test_project_dir();
    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());
    let shell = FakeShellGateway::success();
    // Agent responses: assess (severity 2), name, triage (rejected — below threshold)
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
        // TriageAssessment — rejected because severity 2 < threshold
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

    // CreatePlan emits a terminal success event — below-threshold is a successful no-op.
    assert!(
        event_types.contains(&"project_iteration_completed"),
        "should emit terminal event when below-threshold triage is rejected"
    );
    let completion = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectIterationCompleted)
        .unwrap();
    assert_eq!(
        completion.payload["success"], true,
        "terminal event must be success=true for below-threshold rejection"
    );
    assert!(
        result.is_success(),
        "overall chain must succeed when below-threshold triage filters correctly"
    );

    // Chain still stops — no plan or execution
    assert!(
        !event_types.contains(&"plan_completed"),
        "should NOT create plan after below-threshold triage rejection"
    );
    assert!(
        !event_types.contains(&"execution_completed"),
        "should NOT execute after below-threshold triage rejection"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
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

    // Shell sequence:
    // 1. Preflight gate — pass
    // 2. ExecutePlan: git rev-parse HEAD (pre-sha)
    // 3. ExecutePlan: git diff --name-only <sha> — changes present (prevents silent no-op override)
    // 4. Verify gate after ExecutePlan — FAIL (triggers retry)
    // 5. RetryExecution: git rev-parse HEAD (pre-sha)
    // 6. RetryExecution: git diff --name-only <sha> — changes present
    // 7. Verify gate after RetryExecution — PASS (retry succeeds)
    let shell = FakeShellGateway::sequence(vec![
        // Preflight — pass
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // ExecutePlan: rev-parse HEAD
        CommandResult {
            stdout: "abc123\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // ExecutePlan: git diff --name-only — has real changes
        CommandResult {
            stdout: "src/lib.rs\n".to_string(),
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
        // RetryExecution: rev-parse HEAD
        CommandResult {
            stdout: "def456\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // RetryExecution: git diff --name-only — has real changes
        CommandResult {
            stdout: "src/lib.rs\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
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
#[allow(clippy::too_many_lines)]
async fn iterate_with_maintain_chaining() {
    let dir = test_helpers::test_project_dir();
    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());
    // Shell sequence:
    // 1. Preflight gate — pass
    // 2. ExecutePlan: rev-parse HEAD
    // 3. ExecutePlan: git diff --name-only <sha> — changes present (prevents silent no-op override)
    // 4. Verify gate (iterate) — pass
    // 5. ExecuteMaintain: rev-parse HEAD
    // 6. ExecuteMaintain: git diff --name-only <sha> — empty (maintain does not override)
    // 7. Verify gate (maintain) — pass
    let shell = FakeShellGateway::sequence(vec![
        // Preflight gate
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // ExecutePlan: rev-parse HEAD
        CommandResult {
            stdout: "abc123\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // ExecutePlan: git diff --name-only — changes
        CommandResult {
            stdout: "src/lib.rs\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // Verify gate (iterate)
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // ExecuteMaintain: rev-parse HEAD
        CommandResult {
            stdout: "def456\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // ExecuteMaintain: git diff --name-only — clean (maintain does not override)
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // Verify gate (maintain)
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
    ]);
    // Agent: assess, name, triage, plan, execute, summarize (iterate), then maintain chain agents...
    // We only verify ProjectMaintenanceRequested is emitted; the maintain chain needs its own engine blocks.
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
        // ExecuteMaintain (from chained ProjectMaintenanceRequested -> GateResolutionCompleted -> ExecuteMaintain)
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
    // Also register maintain blocks so the chained ProjectMaintenanceRequested is handled
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

    // Verify ProjectMaintenanceRequested was emitted
    assert!(
        event_types.contains(&"project_maintenance_requested"),
        "should emit maintenance_requested when actions.maintain=true"
    );

    // Verify the maintain chain also ran
    assert!(
        event_types.contains(&"project_maintenance_completed"),
        "should complete the chained maintain workflow"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn silent_no_op_iterate_triggers_retry_and_eventually_fails() {
    let dir = test_helpers::test_project_dir();
    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());

    // All shell calls return success with empty stdout.
    // - Call 1: RunPreflightGates (cargo fmt --check passes)
    // - For each of the 4 agent runs (execute + 3 retries): two shell calls —
    //   `git rev-parse HEAD` (returns empty → pre_sha = None) then
    //   `git status --porcelain` (returns empty → no changes → silent no-op override).
    // RunVerifyGates short-circuits on upstream failure and makes no shell calls.
    let shell = FakeShellGateway::success();

    // Agent: assess, name, triage, plan, execute, retry1, retry2, retry3
    // (no SummarizeResult because the chain ends in failure)
    let agent = FakeAgentGateway::sequence(vec![
        // AssessProject — assessment
        AgentResponse {
            stdout: r#"{"severity": 7, "principle": "DRY", "category": "duplication", "assessment": "Duplicate validation."}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // AssessProject — name
        AgentResponse {
            stdout: "fix-duplication".to_string(),
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
        // CreatePlan — includes correctionNeeded: true so clean tree triggers retry
        AgentResponse {
            stdout: "1. Extract shared helper\n2. Update callers\n\n\
                     ```json\n{ \"correctionNeeded\": true, \"reason\": \"Duplicate found.\" }\n```".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // ExecutePlan — exits 0 but makes no file changes
        AgentResponse {
            stdout: "I reviewed the code but no changes were needed.".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // RetryExecution attempt 1 — also no changes
        AgentResponse {
            stdout: "Still no changes.".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // RetryExecution attempt 2 — also no changes
        AgentResponse {
            stdout: "Nothing to do.".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // RetryExecution attempt 3 — also no changes
        AgentResponse {
            stdout: "Still nothing.".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
    ]);

    let engine = iterate_engine(shell, agent, registry);
    let result = engine.process(iteration_requested_event(false)).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // Should have gone through at least 3 RetryRequested events
    let retry_count = result
        .events
        .iter()
        .filter(|e| e.event_type == EventType::RetryRequested)
        .count();
    assert!(retry_count >= 3, "expected >= 3 RetryRequested events, got {retry_count}");

    // Terminal event must be ProjectIterationCompleted with success=false
    let completion = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectIterationCompleted)
        .unwrap();
    assert_eq!(
        completion.payload["success"], false,
        "iterate chain should fail when agent never modifies files"
    );

    // No changes were committed
    assert!(
        !event_types.contains(&"project_changes_committed"),
        "should NOT commit changes when agent never modifies files"
    );

    // No summarize — only emitted on success
    assert!(
        !event_types.contains(&"summarize_completed"),
        "should NOT summarize when iteration fails"
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn legitimate_no_op_iterate_succeeds_without_retry() {
    // When the plan agent concludes no correction is needed (correctionNeeded: false),
    // a clean working tree after ExecutePlan is a legitimate no-op — the run should
    // succeed and no retry should be triggered.
    let dir = test_helpers::test_project_dir();
    let registry =
        test_helpers::registry_with_project("test-project", dir.path().to_str().unwrap());

    // Shell sequence:
    // 1. RunPreflightGates — gate command passes
    // 2. ExecutePlan — git rev-parse HEAD (pre-execution sha capture) → sha
    // 3. ExecutePlan — git diff --name-only <sha> → empty (no file changes)
    // 4. RunVerifyGates — but since ExecutePlan succeeds (legitimate no-op),
    //    run_verify_gates runs but no agent_execution synthetic gate fires
    let shell = FakeShellGateway::sequence(vec![
        // Preflight gate — pass
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // git rev-parse HEAD before ExecutePlan agent
        CommandResult {
            stdout: "abc123\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // git diff --name-only after ExecutePlan agent — empty (no file changes)
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // Verify gate — pass
        CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
    ]);

    // Agent sequence: assess, name, triage, plan (correctionNeeded: false), execute, summarize
    let agent = FakeAgentGateway::sequence(vec![
        // AssessProject — assessment
        AgentResponse {
            stdout: r#"{"severity": 3, "principle": "DRY", "category": "duplication", "assessment": "Minor duplication."}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // AssessProject — name
        AgentResponse {
            stdout: "fix-duplication".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // TriageAssessment — accepted
        AgentResponse {
            stdout: r#"{"accepted": true, "reason": "severity warrants investigation"}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // CreatePlan — correctionNeeded: false (agent examined codebase and found no real violation)
        AgentResponse {
            stdout: "I examined the codebase and the assessment is inaccurate — the codebase \
                     already satisfies this principle.\n\n\
                     ```json\n{ \"correctionNeeded\": false, \"reason\": \"Codebase already satisfies DRY; assessment was overstated.\" }\n```".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // ExecutePlan — exits 0 with empty output (makes no file changes, which is expected)
        AgentResponse {
            stdout: "Reviewed; no changes needed.".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
        // SummarizeResult — emitted because the run succeeded
        AgentResponse {
            stdout: "HEADLINE: No correction needed\nSUMMARY: Assessment was overstated; codebase already satisfies the principle.".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
    ]);

    let engine = iterate_engine(shell, agent, registry);
    let result = engine.process(iteration_requested_event(false)).await;

    let event_types: Vec<&str> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    // ExecutionCompleted must be present with success=true
    let execution_completed = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ExecutionCompleted)
        .expect("execution_completed must be present");
    assert_eq!(
        execution_completed.payload["success"], true,
        "legitimate no-op: execution_completed must be success=true"
    );

    // GateVerificationCompleted — no synthetic agent_execution gate (since execution succeeded)
    let gate_verification = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::GateVerificationCompleted)
        .expect("gate_verification_completed must be present");
    let empty = vec![];
    let gate_results = gate_verification.payload["results"].as_array().unwrap_or(&empty);
    let has_agent_execution_gate = gate_results
        .iter()
        .any(|r| r.get("name").and_then(|v| v.as_str()) == Some("agent_execution"));
    assert!(
        !has_agent_execution_gate,
        "should NOT have a synthetic agent_execution gate when execution succeeded"
    );

    // ProjectIterationCompleted must be success=true
    let completion = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectIterationCompleted)
        .expect("project_iteration_completed must be present");
    assert_eq!(
        completion.payload["success"], true,
        "iterate chain must succeed for legitimate no-op"
    );

    // No retry — legitimate no-op does not need retrying
    assert!(
        !event_types.contains(&"retry_requested"),
        "should NOT emit retry_requested for legitimate no-op"
    );

    // SummarizeCompleted — emitted on success
    assert!(
        event_types.contains(&"summarize_completed"),
        "should emit summarize_completed for legitimate no-op success"
    );
}

/// Count terminal `ProjectIterationCompleted` events in a result.
///
/// Used by regression tests to assert the invariant: every trace must end with
/// exactly one terminal event whose `success` field accurately reflects the run.
fn count_terminal_events(result: &foundry_core::trace::ProcessResult) -> usize {
    result
        .events
        .iter()
        .filter(|e| e.event_type == EventType::ProjectIterationCompleted)
        .count()
}

/// Regression test for the gilt-cli bug.
///
/// gilt-cli traces ended at `preflight_completed` with no terminal event.
/// The registry consumer showed "running" forever because `is_success()` fell
/// back to block-level aggregation, and `ProjectRunCompleted.success` was wrong.
///
/// After the fix, `RunPreflightGates` emits `ProjectIterationCompleted { success: false }`
/// when preflight fails, giving the trace exactly one accurate terminal event.
#[tokio::test]
async fn gilt_cli_preflight_failure_regression() {
    let dir = test_helpers::test_project_dir();
    let registry = test_helpers::registry_with_project("gilt-cli", dir.path().to_str().unwrap());

    // Preflight gate fails (simulates gilt-cli's cargo fmt --check returning non-zero)
    let shell = FakeShellGateway::failure("formatting error: src/lib.rs");
    let agent = FakeAgentGateway::success();

    let engine = iterate_engine(shell, agent, registry);
    let result = engine
        .process(Event::new(
            EventType::ProjectIterationRequested,
            "gilt-cli".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "gilt-cli",
                "workflow": "iterate",
                "actions": { "iterate": true, "maintain": false },
            }),
        ))
        .await;

    // Invariant: exactly one terminal event
    let terminal_count = count_terminal_events(&result);
    assert_eq!(
        terminal_count, 1,
        "every trace must end with exactly one terminal ProjectIterationCompleted event"
    );

    // That terminal event must accurately report failure
    let terminal = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::ProjectIterationCompleted)
        .unwrap();
    assert_eq!(
        terminal.payload["success"], false,
        "terminal event must be success=false when preflight fails"
    );

    // is_success() must use the terminal event, not block aggregation
    assert!(
        !result.is_success(),
        "result.is_success() must return false when terminal event has success=false"
    );
}
