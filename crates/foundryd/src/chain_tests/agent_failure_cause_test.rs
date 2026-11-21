//! A failed maintain session through the real chain, admission and scheduler
//! carries the agent's own words into the durable record.
//!
//! On 2026-11-16 Claude Code's OAuth login lapsed on mojility-ops-01. For five
//! nights every project's maintenance failed in seconds, each retried three
//! times, and every work item settled `workflow failed; inspect its trace`.
//! The CLI's "Failed to authenticate" message was in no event or ledger
//! record. The agent here is an in-process double returning what the gateways
//! return for such a session.
use super::*;

use foundry_sdk::gateway::fakes::FakeAgentGateway;
use foundry_sdk::gateway::{AgentFailureKind, AgentFailureMetadata, AgentProvider};
use foundry_sdk::payload::ExecutionCompletedPayload;

const EXPIRED: &str = "Failed to authenticate: OAuth session expired and could not be refreshed";

/// The maintain chain with durable admission and settlement.
fn maintain_engine(
    agent: Arc<dyn AgentGateway>,
    registry: Arc<RwLock<Registry>>,
    store_path: &Path,
) -> Engine {
    let mut engine = Engine::new();
    test_helpers::register_gate_scaffold(
        &mut engine,
        test_helpers::passing_shell(),
        registry.clone(),
    );
    register_ledger_blocks(&mut engine, store_path, &registry);
    engine.register(Box::new(foundry_blocks::blocks::ClassifyDependencyUpdates::with_source(
        Arc::new(foundry_blocks::dependency_updates::fakes::FakeVersionSource::with(&[])),
        registry.clone(),
        store_path.parent().unwrap().join("no-events"),
    )));
    engine.register(Box::new(foundry_blocks::blocks::ExecuteMaintain::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::RetryExecution::new(agent, registry)));
    engine
}

fn maintenance_request() -> Event {
    Event::new(
        EventType::ProjectMaintenanceRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({ "project": "test-project", "workflow": "maintain" }),
    )
    .with_trace_id(Some(mint_trace_id()))
}

async fn run_maintenance(agent: Arc<FakeAgentGateway>) -> (Vec<Event>, WorkItem) {
    let dir = tempfile::tempdir().unwrap();
    let checkout = task_project(dir.path());
    let store_path = dir.path().join("work-items.json");
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let engine = Arc::new(maintain_engine(agent, registry.clone(), &store_path));

    let events = admit_and_run(&engine, &registry, &store_path, maintenance_request()).await;

    let store = WorkItemStore::load(&store_path).unwrap();
    let item = store
        .items
        .iter()
        .find(|item| item.kind == WorkItemKind::Maintenance)
        .expect("the maintenance root was admitted")
        .clone();
    (events, item)
}

fn executions(events: &[Event]) -> Vec<ExecutionCompletedPayload> {
    events
        .iter()
        .filter(|event| event.event_type == EventType::ExecutionCompleted)
        .map(|event| event.parse_payload().unwrap())
        .collect()
}

/// What the Claude gateway returns for the expired login: its result record
/// classifies as a terminal authentication failure, and the CLI printed the
/// message as its final text, with nothing on stderr.
fn expired_login() -> AgentResponse {
    AgentResponse {
        stdout: EXPIRED.to_string(),
        stderr: String::new(),
        exit_code: 1,
        success: false,
        failure: Some(
            AgentFailureMetadata::new(AgentProvider::Claude)
                .terminal(AgentFailureKind::Authentication)
                .with_message(EXPIRED),
        ),
    }
}

#[tokio::test]
async fn an_expired_login_stops_maintenance_once_and_names_the_cause_everywhere() {
    let agent = FakeAgentGateway::always(expired_login());

    let (events, item) = run_maintenance(agent.clone()).await;

    assert_eq!(agent.invocations().len(), 1, "a retry cannot fix a login");
    assert!(
        !events.iter().any(|event| event.event_type == EventType::RetryRequested),
        "no retry was requested"
    );
    let cause = format!("agent authentication failed: {EXPIRED}");
    let execution = &executions(&events)[0];
    let session = execution.session_failure.as_ref().expect("the session failed");
    assert_eq!(session.message, cause);
    assert_eq!(session.detail.as_deref(), Some(EXPIRED));
    assert_eq!(execution.failure.failure_kind, Some(AgentFailureKind::Authentication));
    let completed = events
        .iter()
        .find(|event| event.event_type == EventType::ProjectMaintenanceCompleted)
        .expect("maintenance completed");
    assert_eq!(completed.payload["success"], false);
    assert_eq!(completed.payload["summary"], "agent authentication failed; workflow stopped");

    assert_eq!(item.state, WorkItemState::Failed);
    assert_eq!(item.reason, format!("workflow failed: {cause}"));
}

/// A provider error no rule classifies (the Claude gateway reports its result
/// text as the failure message) still reaches the summary and the work item,
/// and is retried as before.
#[tokio::test]
async fn an_unclassified_provider_error_reaches_the_work_item() {
    let agent = FakeAgentGateway::always(AgentResponse {
        stdout: "Error: workspace is locked by another session".to_string(),
        stderr: String::new(),
        exit_code: 1,
        success: false,
        failure: Some(
            AgentFailureMetadata::new(AgentProvider::Claude)
                .with_message("Error: workspace is locked by another session"),
        ),
    });

    let (events, item) = run_maintenance(agent.clone()).await;

    assert_eq!(agent.invocations().len(), 4, "the first attempt and three retries");
    let last = executions(&events).pop().unwrap();
    let session = last.session_failure.expect("the session failed");
    assert_eq!(session.message, "retry 3 failed: Error: workspace is locked by another session");
    assert_eq!(session.detail.as_deref(), Some("Error: workspace is locked by another session"));
    assert_eq!(item.state, WorkItemState::Failed);
    assert_eq!(
        item.reason,
        "workflow failed: retry 3 failed: Error: workspace is locked by another session"
    );
}
