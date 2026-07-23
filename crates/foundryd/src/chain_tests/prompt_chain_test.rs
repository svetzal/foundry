//! Integration tests for the prompt-driven workflow formation.
//!
//! Verifies:
//! - Happy path: `ExecutionRequested` → charter check → gates → preflight
//!   → direct prompt → execute → verify → completion → summarise → commit
//! - Assessment/triage/plan blocks do NOT fire
//! - Standard iterate still works when engine has both formations

use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, RwLock};

use foundry_sdk::campaign::{
    Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::registry::Registry;
use foundry_sdk::throttle::Throttle;

use super::test_helpers;
use foundry_blocks::gateway::{AgentGateway, ShellGateway};
use foundry_engine::engine::Engine;
use foundry_sdk::gateway::fakes::FakeAgentGateway;

fn git_ok(cwd: Option<&std::path::Path>, args: &[&str]) -> bool {
    let mut command = Command::new("git");
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .args(args);
    for index in 0..8 {
        command.env_remove(format!("GIT_CONFIG_KEY_{index}"));
        command.env_remove(format!("GIT_CONFIG_VALUE_{index}"));
    }
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.status().unwrap().success()
}

#[allow(clippy::needless_pass_by_value)]
fn prompt_engine(
    shell: Arc<dyn ShellGateway>,
    agent: Arc<dyn AgentGateway>,
    registry: Arc<RwLock<Registry>>,
) -> Engine {
    let mut engine = Engine::new();

    // Charter check (sinks on ProjectIterationRequested + ExecutionRequested)
    engine.register(Box::new(foundry_blocks::blocks::CheckCharter::new(registry.clone())));
    // Gate scaffold: ResolveGates, RunPreflightGates, RunVerifyGates, RouteGateResult
    test_helpers::register_gate_scaffold(&mut engine, shell.clone(), registry.clone());
    // Direct prompt (sinks on PreflightCompleted, workflow=prompt only)
    engine.register(Box::new(foundry_blocks::blocks::DirectPrompt));
    engine.register(Box::new(foundry_blocks::blocks::ReviewTask::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::FinalizeTask::new(registry.clone())));
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
    // Execution blocks use ProcessShellGateway (::new) rather than the fake shell because
    // task_workflow_happy_path relies on real worktree change-detection in an isolated git repo.
    engine.register(Box::new(foundry_blocks::blocks::ExecutePlan::new(
        agent.clone(),
        registry.clone(),
    )));
    // Retry (sinks on RetryRequested)
    engine.register(Box::new(foundry_blocks::blocks::RetryExecution::new(
        agent.clone(),
        registry.clone(),
    )));
    // Terminal blocks
    engine.register(Box::new(foundry_blocks::blocks::SummarizeResult::new(
        Arc::clone(&agent),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::CommitAndPush::new(registry)));

    engine
}

fn campaign_engine(
    shell: Arc<dyn ShellGateway>,
    agent: Arc<dyn AgentGateway>,
    registry: Arc<RwLock<Registry>>,
    store_path: PathBuf,
) -> Engine {
    let mut engine = prompt_engine(shell.clone(), agent.clone(), registry.clone());
    engine.register(Box::new(foundry_blocks::blocks::RequestCampaignAdvance));
    engine.register(Box::new(foundry_blocks::blocks::AdvanceCampaign::new(
        agent, shell, registry, store_path,
    )));
    engine
}

fn save_campaign(path: &std::path::Path, repo: &std::path::Path, context_paths: Vec<String>) {
    let mut store = CampaignStore::default();
    store
        .add(Campaign {
            name: "campaign-test".to_string(),
            project: "test-project".to_string(),
            mission: "Prove one campaign cycle closes safely.".to_string(),
            intent_refs: vec!["test.intent".to_string()],
            context_paths,
            done_evidence: vec![DoneEvidence::Review {
                statement: "The requested behavior exists.".to_string(),
            }],
            budget: CampaignBudget { max_cycles: 2 },
            escalation: vec![],
            status: CampaignStatus::Active,
            cycles_completed: 0,
            cycles_landed: 0,
            authorized_by: Some("test-owner".to_string()),
            agent_provider: None,
            last_run_event_id: None,
            owner_decisions: vec![],
            pending_run_result: None,
            objective_history: vec![],
        })
        .unwrap();
    assert!(repo.exists());
    store.save(path).unwrap();
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
#[allow(clippy::too_many_lines)]
// The `task` workflow reuses the direct prompt formation: `ExecutionRequested` with
// `workflow: "task"` routes through `DirectPrompt` exactly like `workflow: "prompt"`,
// skipping assessment/triage/plan blocks.
async fn task_workflow_happy_path() {
    let dir = tempfile::tempdir().unwrap();
    let remote = dir.path().join("remote.git");
    let remote_url = format!("file://{}", remote.display());
    let checkout = dir.path().join("checkout");
    assert!(git_ok(None, &["init", "--bare", remote.to_str().unwrap()]));
    assert!(git_ok(None, &["init", "-b", "main", checkout.to_str().unwrap()]));
    assert!(git_ok(Some(&checkout), &["config", "user.email", "foundry-test@example.com"]));
    assert!(git_ok(Some(&checkout), &["config", "user.name", "Foundry Test"]));
    std::fs::write(checkout.join("CHARTER.md"), "a".repeat(100)).unwrap();
    std::fs::write(
        checkout.join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"true","required":true}]}"#,
    )
    .unwrap();
    assert!(git_ok(Some(&checkout), &["add", "CHARTER.md", ".hone-gates.json"]));
    assert!(git_ok(Some(&checkout), &["commit", "-m", "initial"]));
    assert!(git_ok(Some(&checkout), &["remote", "add", "origin", &remote_url]));
    let _ = Command::new("git")
        .current_dir(&checkout)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .args(["config", "--unset-all", "remote.origin.pushurl"])
        .status();
    assert!(git_ok(Some(&checkout), &["remote", "set-url", "--push", "origin", &remote_url],));
    assert!(git_ok(Some(&checkout), &["push", "-u", "origin", "main"]));

    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());

    let agent = FakeAgentGateway::sequence(vec![
        foundry_blocks::gateway::AgentResponse::success("Done, implemented the task"),
        foundry_blocks::gateway::AgentResponse::success("```json\n{\"verdict\":\"complete\"}\n```"),
    ]);

    let engine = prompt_engine(test_helpers::passing_shell(), agent.clone(), registry);

    let trigger = Event::new(
        EventType::ExecutionRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({
            "project": "test-project",
            "workflow": "task",
            "prompt": "Add a --quiet flag to the CLI.",
        }),
    );

    let result = engine.process(trigger).await;

    let event_types: Vec<String> = result.events.iter().map(|e| e.event_type.as_str()).collect();

    assert!(event_types.iter().any(|t| t == "plan_completed"));
    assert!(event_types.iter().any(|t| t == "task_run_started"));
    assert!(event_types.iter().any(|t| t == "execution_completed"));
    assert!(event_types.iter().any(|t| t == "gate_verification_completed"));
    assert!(event_types.iter().any(|t| t == "task_reviewed"));
    assert!(event_types.iter().any(|t| t == "task_run_completed"));
    assert!(!event_types.iter().any(|t| t == "retry_requested"));
    assert!(!event_types.iter().any(|t| t == "project_iteration_completed"));
    assert!(!event_types.iter().any(|t| t == "assessment_completed"));
    assert!(!event_types.iter().any(|t| t == "triage_completed"));

    let plan_event =
        result.events.iter().find(|e| e.event_type == EventType::PlanCompleted).unwrap();
    assert_eq!(plan_event.payload["plan"], "Add a --quiet flag to the CLI.");
    assert_eq!(plan_event.payload["workflow"], "task");

    let terminal_event = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::TaskRunCompleted)
        .unwrap();
    assert_eq!(terminal_event.payload["success"], true);
    assert_eq!(terminal_event.payload["verdict"], "complete");

    let invocations = agent.invocations();
    assert_eq!(invocations.len(), 2);
    assert_ne!(invocations[0].working_dir, checkout);
    assert_eq!(invocations[0].working_dir, invocations[1].working_dir);
    assert!(
        invocations[0].working_dir.contains(".foundry/worktrees"),
        "executor escaped isolated worktree: {}",
        invocations[0].working_dir
    );
}

#[tokio::test]
async fn campaign_dry_run_completes_one_simulated_task_without_looping() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("CHARTER.md"), "a".repeat(100)).unwrap();
    std::fs::write(
        repo.join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"true","required":true}]}"#,
    )
    .unwrap();
    let store_path = dir.path().join("campaigns.json");
    save_campaign(&store_path, &repo, vec![]);
    let registry = test_helpers::registry_with_project("test-project", repo.to_str().unwrap());
    let agent = FakeAgentGateway::sequence(vec![foundry_blocks::gateway::AgentResponse::success(
        "```json\n{\"verdict\":\"complete\"}\n```",
    )]);
    let engine =
        campaign_engine(test_helpers::passing_shell(), agent.clone(), registry, store_path.clone());
    let trigger = Event::new(
        EventType::CampaignAdvanceRequested,
        "test-project".to_string(),
        Throttle::DryRun,
        serde_json::json!({"campaign": "campaign-test"}),
    );

    let result = engine.process(trigger).await;
    let event_types =
        result.events.iter().map(|event| event.event_type.clone()).collect::<Vec<_>>();

    assert_eq!(
        event_types
            .iter()
            .filter(|event_type| **event_type == EventType::CampaignAdvanceRequested)
            .count(),
        1,
        "dry-run task result must not recursively auto-advance"
    );
    assert!(event_types.contains(&EventType::CampaignAdvanceCompleted));
    assert!(event_types.contains(&EventType::TaskRunCompleted));
    assert!(!event_types.contains(&EventType::CampaignEscalated));
    let invocations = agent.invocations();
    assert_eq!(invocations.len(), 1, "only the skeptical reviewer should run");
    assert_eq!(invocations[0].working_dir, repo);
    let stored = CampaignStore::load(&store_path).unwrap();
    let campaign = stored.find("campaign-test").unwrap();
    assert_eq!(campaign.status, CampaignStatus::Active);
    assert_eq!(campaign.cycles_completed, 0);
}

#[tokio::test]
async fn campaign_formation_error_emits_terminal_escalation() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let store_path = dir.path().join("campaigns.json");
    save_campaign(&store_path, &repo, vec!["missing-context.md".to_string()]);
    let registry = test_helpers::registry_with_project("test-project", repo.to_str().unwrap());
    let agent = FakeAgentGateway::success();
    let engine =
        campaign_engine(test_helpers::passing_shell(), agent.clone(), registry, store_path.clone());
    let trigger = Event::new(
        EventType::CampaignAdvanceRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({"campaign": "campaign-test"}),
    );

    let result = engine.process(trigger).await;

    assert!(
        result
            .events
            .iter()
            .any(|event| event.event_type == EventType::CampaignAdvanceCompleted)
    );
    assert!(
        result
            .events
            .iter()
            .any(|event| event.event_type == EventType::CampaignEscalated)
    );
    assert!(agent.invocations().is_empty());
    assert_eq!(
        CampaignStore::load(&store_path).unwrap().find("campaign-test").unwrap().status,
        CampaignStatus::Escalated
    );
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
