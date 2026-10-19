//! A provider outage through the real task chain, admission, scheduler and
//! campaign advance (2026-10-19: Codex sessions failed at startup with
//! "Selected model is at capacity", exit 1, zero tokens).
//!
//! The agent is an in-process double and the execution block's sleeper
//! records each pause and returns at once, so no test waits for real time.
use super::*;

use foundry_sdk::campaign::{Campaign, CampaignStatus, CampaignStore};
use foundry_sdk::gateway::fakes::{FakeAgentGateway, RecordingSleeper};
use foundry_sdk::gateway::{AgentProvider, AgentResponse, Sleeper};
use foundry_sdk::task_block::TaskBlock;

const CAPACITY: &str = "Selected model is at capacity. Please try a different model.";
const OBJECTIVE: &str = "Parse the config file through the public API.";

fn at_capacity() -> AgentResponse {
    AgentResponse::provider_unavailable(AgentProvider::Codex, CAPACITY)
}

/// The task chain `task_engine` builds, with `sleeper` in the execution block.
fn outage_task_engine(
    agent: Arc<dyn AgentGateway>,
    sleeper: Arc<dyn Sleeper>,
    registry: Arc<RwLock<Registry>>,
    store_path: &Path,
) -> Engine {
    let shell = test_helpers::passing_shell();
    let mut engine = Engine::new();
    engine.register(Box::new(foundry_blocks::blocks::CheckCharter::new(registry.clone())));
    test_helpers::register_gate_scaffold(&mut engine, shell, registry.clone());
    engine.register(Box::new(foundry_blocks::blocks::DirectPrompt));
    register_ledger_blocks(&mut engine, store_path, &registry);
    engine.register(Box::new(
        foundry_blocks::blocks::ExecutePlan::new(agent.clone(), registry.clone())
            .with_sleeper(sleeper),
    ));
    engine.register(Box::new(foundry_blocks::blocks::ReviewTask::new(agent, registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::FinalizeTask::new(registry)));
    engine
}

fn only(events: &[Event], event_type: &EventType) -> Event {
    let found: Vec<_> = events.iter().filter(|event| event.event_type == *event_type).collect();
    assert_eq!(found.len(), 1, "exactly one {event_type}: {found:?}");
    found[0].clone()
}

/// Scenario 1: the provider is at capacity for the first attempt only. The
/// execution block pauses and tries again, and the run is reviewed and
/// settles `landed` as if nothing had happened.
#[tokio::test]
async fn a_capacity_refusal_that_clears_within_the_execution_backoff_lands_the_task() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = task_project(dir.path());
    let store_path = dir.path().join("work-items.json");
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let agent = FakeAgentGateway::sequence(vec![
        at_capacity(),
        AgentResponse::success("Done, implemented the task"),
        AgentResponse::success("```json\n{\"verdict\":\"complete\"}\n```"),
    ]);
    let sleeper = RecordingSleeper::new();
    let engine = Arc::new(outage_task_engine(
        agent.clone(),
        sleeper.clone(),
        registry.clone(),
        &store_path,
    ));

    let events = admit_and_run(
        &engine,
        &registry,
        &store_path,
        task_dispatch("test-project", OBJECTIVE, &serde_json::json!({})),
    )
    .await;

    assert_eq!(sleeper.pauses(), vec![std::time::Duration::from_secs(30)]);
    assert_eq!(agent.invocations().len(), 3, "two execution attempts and one review");
    let completed = only(&events, &EventType::TaskRunCompleted);
    assert_eq!(completed.payload["verdict"], "complete");
    assert!(completed.payload.get("failure_kind").is_none(), "{}", completed.payload);
    let store = WorkItemStore::load(&store_path).unwrap();
    assert_eq!(store.items[0].state, WorkItemState::Landed);
}

/// Scenarios 2 and 3: the provider stays at capacity for every attempt of a
/// campaign cycle.
///
/// Execution tries four times, then the run settles as a `runner_error`
/// that says "provider unavailable" and carries the failure kind. No gate
/// command runs, no review session runs and nothing reports missing proof.
/// The campaign then repeats the same objective under the same cycle number
/// after a pause: no formation agent, no cycle budget, no recovery for a
/// person, and no `capability_unavailable` anywhere.
#[tokio::test]
#[allow(clippy::too_many_lines)] // End-to-end chain assertion; length is inherent.
async fn a_campaign_cycle_the_provider_keeps_refusing_is_repeated_without_anyone_acting() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = task_project(dir.path());
    let store_path = dir.path().join("work-items.json");
    let campaigns = dir.path().join("campaigns.json");
    let mut campaign: Campaign = serde_json::from_value(serde_json::json!({
        "name": "parse-config", "project": "test-project", "mission": "parse the config",
        "done_evidence": [{"kind": "review", "statement": "the config parses"}],
        "status": "active", "authorized_by": "owner", "cycles_completed": 2,
        "budget": {"max_cycles": 2}
    }))
    .unwrap();
    campaign.record_dispatched_objective(OBJECTIVE.into());
    CampaignStore {
        findings: vec![],
        initiatives: vec![],
        version: 1,
        campaigns: vec![campaign],
    }
    .save(&campaigns)
    .unwrap();
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let agent = FakeAgentGateway::always(at_capacity());
    let sleeper = RecordingSleeper::new();
    let engine = Arc::new(outage_task_engine(
        agent.clone(),
        sleeper.clone(),
        registry.clone(),
        &store_path,
    ));
    let root = task_dispatch(
        "test-project",
        OBJECTIVE,
        &serde_json::json!({
            "campaign": "parse-config", "campaign_cycle": 2, "campaign_limits": {},
            "agent_provider": "codex"
        }),
    )
    .with_source(Some(foundry_sdk::work_source::WorkSource::campaign("parse-config", 2)));

    let events = admit_and_run(&engine, &registry, &store_path, root).await;

    // Execution: four attempts, paused by the recording sleeper.
    assert_eq!(
        sleeper.pauses(),
        vec![
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(120),
            std::time::Duration::from_secs(300),
        ]
    );
    assert_eq!(agent.invocations().len(), 4, "four execution attempts and no review session");
    // Gates: only the synthetic stand-in for the execution that did not run.
    let verification = only(&events, &EventType::GateVerificationCompleted);
    let gates = verification.payload["results"].as_array().unwrap();
    assert_eq!(gates.len(), 1);
    assert_eq!(gates[0]["name"], "agent_execution");
    // Review and finalize: a runner error naming the outage.
    let completed = only(&events, &EventType::TaskRunCompleted);
    assert_eq!(completed.payload["verdict"], "runner_error");
    assert_eq!(
        completed.payload["detail"],
        format!("provider unavailable: {CAPACITY} (4 attempts)")
    );
    assert_eq!(completed.payload["failure_kind"], "provider_unavailable");
    let everything = serde_json::to_string(&events).unwrap();
    assert!(
        !everything.contains("proof.json"),
        "no missing-proof defect for a run that did not start"
    );
    assert!(!everything.contains("\"defect\""));

    // The campaign advance that `task_run_completed` asks for.
    let requested = foundry_blocks::blocks::RequestCampaignAdvance
        .execute(&completed)
        .await
        .unwrap()
        .events
        .remove(0);
    let formation_agent = FakeAgentGateway::success();
    let advance = foundry_blocks::blocks::AdvanceCampaign::new(
        formation_agent.clone(),
        test_helpers::passing_shell(),
        registry.clone(),
        campaigns.clone(),
        store_path.clone(),
    );
    let before = chrono::Utc::now();
    let advanced = advance.execute(&requested).await.unwrap();

    assert!(advanced.success, "{}", advanced.summary);
    assert!(formation_agent.invocations().is_empty(), "the repeat is not formed");
    let dispatch = only(&advanced.events, &EventType::ExecutionRequested);
    assert_eq!(dispatch.payload["prompt"], OBJECTIVE);
    assert_eq!(dispatch.payload["campaign_cycle"], 2);
    assert_eq!(dispatch.payload["base_ref"], completed.payload["preservation_ref"]);
    let not_before: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(dispatch.payload["not_before"].clone()).unwrap();
    assert!(not_before >= before + chrono::Duration::minutes(5), "{not_before}");
    assert!(
        advanced
            .events
            .iter()
            .all(|event| event.event_type != EventType::CampaignRecoveryRecorded),
        "no recovery: {:?}",
        advanced.events
    );
    assert!(
        !serde_json::to_string(&advanced.events)
            .unwrap()
            .contains("capability_unavailable")
    );
    let stored = CampaignStore::load(&campaigns).unwrap();
    let campaign = stored.find("parse-config").unwrap();
    assert_eq!(campaign.cycles_completed, 2, "no cycle budget spent");
    assert_eq!(campaign.status, CampaignStatus::Active);
    assert!(campaign.recovery.is_none());
}
