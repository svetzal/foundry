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

/// A campaign at its last authorized cycle (2 of 2), whose cycle 2 runs
/// through the real task chain over `agent`.
struct CampaignCycle {
    _dir: tempfile::TempDir,
    registry: Arc<RwLock<Registry>>,
    campaigns: std::path::PathBuf,
    store_path: std::path::PathBuf,
    sleeper: Arc<RecordingSleeper>,
    events: Vec<Event>,
}

impl CampaignCycle {
    async fn run(agent: Arc<FakeAgentGateway>) -> Self {
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
        let registry =
            test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
        let sleeper = RecordingSleeper::new();
        let engine =
            Arc::new(outage_task_engine(agent, sleeper.clone(), registry.clone(), &store_path));
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
        Self {
            _dir: dir,
            registry,
            campaigns,
            store_path,
            sleeper,
            events,
        }
    }

    fn completed(&self) -> Event {
        only(&self.events, &EventType::TaskRunCompleted)
    }

    /// The campaign advance that `task_run_completed` asks for, with a
    /// formation agent that would answer if asked. Returns the advance and
    /// how often formation was asked.
    async fn advance(&self) -> (foundry_sdk::task_block::TaskBlockResult, usize) {
        let requested = foundry_blocks::blocks::RequestCampaignAdvance
            .execute(&self.completed())
            .await
            .unwrap()
            .events
            .remove(0);
        let formation_agent = FakeAgentGateway::success();
        let advance = foundry_blocks::blocks::AdvanceCampaign::new(
            formation_agent.clone(),
            test_helpers::passing_shell(),
            self.registry.clone(),
            self.campaigns.clone(),
            self.store_path.clone(),
        );
        let advanced = advance.execute(&requested).await.unwrap();
        (advanced, formation_agent.invocations().len())
    }

    fn campaign(&self) -> Campaign {
        CampaignStore::load(&self.campaigns)
            .unwrap()
            .find("parse-config")
            .unwrap()
            .clone()
    }
}

/// The campaign repeated cycle 2's objective after a pause, without forming,
/// spending budget or asking a person: no recovery and no
/// `capability_unavailable` anywhere.
fn assert_redispatched_without_anyone_acting(
    cycle: &CampaignCycle,
    advanced: &foundry_sdk::task_block::TaskBlockResult,
    formed: usize,
    before: chrono::DateTime<chrono::Utc>,
) {
    assert!(advanced.success, "{}", advanced.summary);
    assert_eq!(formed, 0, "the repeat is not formed");
    let dispatch = only(&advanced.events, &EventType::ExecutionRequested);
    assert_eq!(dispatch.payload["prompt"], OBJECTIVE);
    assert_eq!(dispatch.payload["campaign_cycle"], 2);
    assert_eq!(dispatch.payload["base_ref"], cycle.completed().payload["preservation_ref"]);
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
    let campaign = cycle.campaign();
    assert_eq!(campaign.cycles_completed, 2, "no cycle budget spent");
    assert_eq!(campaign.status, CampaignStatus::Active);
    assert!(campaign.recovery.is_none());
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
async fn a_campaign_cycle_the_provider_keeps_refusing_is_repeated_without_anyone_acting() {
    let agent = FakeAgentGateway::always(at_capacity());
    let cycle = CampaignCycle::run(agent.clone()).await;

    // Execution: four attempts, paused by the recording sleeper.
    assert_eq!(
        cycle.sleeper.pauses(),
        vec![
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(120),
            std::time::Duration::from_secs(300),
        ]
    );
    assert_eq!(agent.invocations().len(), 4, "four execution attempts and no review session");
    // Gates: only the synthetic stand-in for the execution that did not run.
    let verification = only(&cycle.events, &EventType::GateVerificationCompleted);
    let gates = verification.payload["results"].as_array().unwrap();
    assert_eq!(gates.len(), 1);
    assert_eq!(gates[0]["name"], "agent_execution");
    // Review and finalize: a runner error naming the outage.
    let completed = cycle.completed();
    assert_eq!(completed.payload["verdict"], "runner_error");
    assert_eq!(
        completed.payload["detail"],
        format!("provider unavailable: {CAPACITY} (4 attempts)")
    );
    assert_eq!(completed.payload["failure_kind"], "provider_unavailable");
    let everything = serde_json::to_string(&cycle.events).unwrap();
    assert!(
        !everything.contains("proof.json"),
        "no missing-proof defect for a run that did not start"
    );
    assert!(!everything.contains("\"defect\""));

    let before = chrono::Utc::now();
    let (advanced, formed) = cycle.advance().await;

    assert_redispatched_without_anyone_acting(&cycle, &advanced, formed, before);
}

/// The same capacity refusal arriving without a failure kind: a gateway
/// that wrote it only to stderr with no classification, and an agent that
/// could not run at all with it as its error. Execution recognises the
/// wording, so the run ends `provider unavailable` and the campaign pauses
/// and repeats the cycle rather than escalating.
#[tokio::test]
async fn an_unclassified_capacity_refusal_is_repeated_like_a_classified_one() {
    for agent in [
        FakeAgentGateway::failure(CAPACITY),
        FakeAgentGateway::unavailable(CAPACITY),
    ] {
        let cycle = CampaignCycle::run(agent.clone()).await;

        assert_eq!(agent.invocations().len(), 4, "retried as provider unavailable");
        let completed = cycle.completed();
        assert_eq!(completed.payload["verdict"], "runner_error");
        assert_eq!(completed.payload["failure_kind"], "provider_unavailable");
        assert!(!serde_json::to_string(&cycle.events).unwrap().contains("proof.json"));

        let before = chrono::Utc::now();
        let (advanced, formed) = cycle.advance().await;

        assert_redispatched_without_anyone_acting(&cycle, &advanced, formed, before);
    }
}

/// Cycles 3 and 5 of mojentic-py-no-network-tests on 2026-10-19: a Codex
/// session whose log held only an error event no rule recognised. The
/// session did no work, so review settles a runner error with the
/// session's message rather than a missing-proof defect, and the campaign
/// records a `runner_unavailable` recovery. Formation is not asked, so it
/// cannot invent a capability blocker.
#[tokio::test]
async fn a_failed_session_that_did_no_work_records_runner_unavailable_not_a_defect() {
    let agent = FakeAgentGateway::failure("unexpected error event from the model stream");
    let cycle = CampaignCycle::run(agent.clone()).await;

    assert_eq!(agent.invocations().len(), 1, "one execution attempt and no review session");
    let completed = cycle.completed();
    assert_eq!(completed.payload["verdict"], "runner_error", "{}", completed.payload);
    let detail = completed.payload["detail"].as_str().unwrap().to_string();
    assert!(
        detail.ends_with("failed: unexpected error event from the model stream"),
        "{detail}"
    );
    let everything = serde_json::to_string(&cycle.events).unwrap();
    assert!(!everything.contains("proof.json"), "{everything}");
    assert!(!everything.contains("\"defect\""));

    let (advanced, formed) = cycle.advance().await;

    assert_eq!(formed, 0, "formation is not asked to judge a run that did no work");
    let recorded = only(&advanced.events, &EventType::CampaignRecoveryRecorded);
    assert_eq!(
        recorded.payload["recovery"]["kind"], "runner_unavailable",
        "{}",
        recorded.payload
    );
    assert!(
        !serde_json::to_string(&advanced.events)
            .unwrap()
            .contains("capability_unavailable")
    );
    assert_eq!(cycle.campaign().cycles_completed, 2);
}
