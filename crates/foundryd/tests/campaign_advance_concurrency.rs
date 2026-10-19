//! Concurrent campaign advances: formation runs its agent without holding the
//! campaign store, and a campaign never gets a second advance while one is in
//! flight.
//!
//! Reproduces the 2026-10-11 production hang: five concurrent
//! `foundry campaign advance` calls, one per campaign. Three were admitted; the
//! other two waited on the campaign store's flock, which the running formation
//! held across its whole agent call, and were admitted only as formations
//! finished. Formations for different campaigns were also serialised behind one
//! process-wide mutex.
//!
//! These tests pin the replacement contract:
//!
//! - an advance RPC (or any other campaign control) returns promptly while
//!   another campaign forms;
//! - different campaigns form at the same time;
//! - a formed outcome is applied only when the campaign record is unchanged
//!   since formation read it. A campaign paused or cancelled while its
//!   formation ran keeps the operator's change, the outcome is discarded, and
//!   the advance stands down: it emits a `stood_down` completion and its work
//!   item settles `landed`, because it did what the operator asked. A
//!   campaign edited in any other way keeps the edit and forms again from the
//!   edited record, up to a small bound; past the bound the advance fails;
//! - an advance for a campaign whose advance or cycle is already submitted,
//!   queued, held or running dispatches nothing and names that work instead.
//!   On 2026-10-11 an operator retried a hung advance and got two advance
//!   workflows for one campaign, which had to be cancelled by hand.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use foundry_blocks::{
    blocks::AdvanceCampaign,
    gateway::{AgentGateway, AgentRequest, AgentResponse},
    trace_writer::TraceWriter,
};
use foundry_engine::engine::Engine;
use foundry_engine::event_writer::EventWriter;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gateway::fakes::FakeShellGateway;
use foundry_sdk::pacing::PacingPaths;
use foundry_sdk::payload::{
    CampaignAdvanceRequestedPayload, LoopContext, TaskRunCompletedPayload, TaskVerdict,
};
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{
    WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
};
use foundry_sdk::work_source::WorkSource;
use foundry_sdk::{
    campaign::{Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence},
    registry::Registry,
    sentinel::SentinelStore,
};
use foundryd::{
    pacing::PacingScheduler,
    proto::{
        AdvanceCampaignRequest, CancelCampaignRequest, ListCampaignsRequest, PauseCampaignRequest,
        ResumeCampaignRequest, foundry_server::Foundry,
    },
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    work_admission::DurableWorkAdmission,
    workflow_tracker::WorkflowTracker,
};
use tokio::sync::{Notify, Semaphore, broadcast};
use tonic::Request;

/// How long a control that must not wait on a formation may take.
const PROMPT: Duration = Duration::from_secs(2);

/// A decision agent that parks every invocation until the test releases it.
struct GatedAgent {
    entered: AtomicUsize,
    release: Semaphore,
    prompts: Mutex<Vec<String>>,
}

impl GatedAgent {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: AtomicUsize::new(0),
            release: Semaphore::new(0),
            prompts: Mutex::new(Vec::new()),
        })
    }

    /// Every prompt the agent was shown, in the order it was asked.
    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }

    fn entered(&self) -> usize {
        self.entered.load(Ordering::SeqCst)
    }

    /// Let `count` parked (or future) invocations answer.
    fn release(&self, count: usize) {
        self.release.add_permits(count);
    }
}

impl AgentGateway for GatedAgent {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            self.prompts.lock().unwrap().push(request.prompt.clone());
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.release.acquire().await.expect("semaphore open").forget();
            Ok(AgentResponse::success(ADVANCE_DECISION))
        })
    }
}

const ADVANCE_DECISION: &str = r#"{"decision":"advance","objective":"Add the next tested slice of the mission.","reason":"the mission is not yet met"}"#;

/// A decision agent that edits its own campaign's record every time it is
/// asked, as an owner who keeps adjusting a campaign would.
struct EditingAgent {
    store: std::path::PathBuf,
    asked: Arc<AtomicUsize>,
}

impl AgentGateway for EditingAgent {
    fn invoke<'a>(
        &'a self,
        _request: &'a AgentRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            let asked = self.asked.fetch_add(1, Ordering::SeqCst) + 1;
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || {
                let mut guard = CampaignStore::lock_exclusive(&store).unwrap();
                guard.store.find_mut("alpha").unwrap().budget.max_cycles = 10 + asked as u64;
                guard.save().unwrap();
            })
            .await
            .unwrap();
            Ok(AgentResponse::success(ADVANCE_DECISION))
        })
    }
}

fn active_campaign(name: &str) -> Campaign {
    Campaign {
        recovery: None,
        initiative: None,
        providers: foundry_sdk::gateway::StageProviders::default(),
        name: name.to_string(),
        project: "test-project".to_string(),
        mission: format!("Mission for {name}"),
        intent_refs: vec![],
        context_paths: vec![],
        done_evidence: vec![DoneEvidence::Review {
            statement: "done".to_string(),
        }],
        budget: CampaignBudget {
            max_cycles: 10,
            ..Default::default()
        },
        escalation: vec![],
        status: CampaignStatus::Active,
        cycles_completed: 0,
        cycles_landed: 0,
        authorized_by: Some("owner".to_string()),
        agent_provider: None,
        last_run_event_id: None,
        owner_decisions: vec![],
        pending_run_result: None,
        objective_history: vec![],
        open_escalation: None,
        writable_repositories: vec![],
    }
}

struct Harness {
    dir: tempfile::TempDir,
    path: std::path::PathBuf,
    service: Arc<FoundryService>,
    engine: Arc<Engine>,
    tracker: Arc<WorkflowTracker>,
    events: broadcast::Receiver<Event>,
}

impl Harness {
    /// A daemon without the work-item ledger: workflows run as soon as they
    /// are spawned.
    fn new(campaigns: &[&str], agent: Arc<dyn AgentGateway>) -> Self {
        Self::build(campaigns, |_| agent, false)
    }

    /// A daemon wired as production wires it: every advance is admitted to
    /// the work-item ledger, started by the pacing scheduler, and settled from
    /// its workflow's outcome.
    fn with_ledger(campaigns: &[&str], agent: Arc<dyn AgentGateway>) -> Self {
        Self::build(campaigns, |_| agent, true)
    }

    /// `agent_for` receives the campaign store's path, for an agent that
    /// edits the campaign it is forming.
    fn build(
        campaigns: &[&str],
        agent_for: impl FnOnce(&std::path::Path) -> Arc<dyn AgentGateway>,
        ledger: bool,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        for name in campaigns {
            store.add(active_campaign(name)).unwrap();
        }
        store.save(&path).unwrap();

        let registry: Registry = serde_json::from_value(serde_json::json!({
            "version": 2, "projects": [{"name":"test-project", "path":dir.path(), "stack":"rust", "agent":"codex", "repo":"", "branch":"main"}]
        }))
        .unwrap();
        let registry = Arc::new(RwLock::new(registry));
        let (event_tx, events) = broadcast::channel(1024);
        let trace_writer = Arc::new(TraceWriter::new(dir.path().to_str().unwrap()));
        let work_items_path = dir.path().join("work.json");
        let pacing = PacingPaths {
            limits: dir.path().join("pacing.json"),
            state: dir.path().join("pacing-state.json"),
        };
        let mut engine = Engine::new().with_event_broadcaster(event_tx.clone());
        if ledger {
            let writer = Arc::new(EventWriter::new(dir.path().join("events")));
            engine = engine.with_event_writer(writer.clone()).with_work_admission(Arc::new(
                DurableWorkAdmission::new(
                    work_items_path.clone(),
                    registry.clone(),
                    pacing.clone(),
                    event_tx.clone(),
                    writer,
                    trace_writer.clone(),
                ),
            ));
        }
        engine.register(Box::new(AdvanceCampaign::new(
            agent_for(&path),
            FakeShellGateway::success(),
            registry.clone(),
            path.clone(),
            work_items_path.clone(),
        )));
        let wake = engine.admission_wake();
        let engine = Arc::new(engine);
        let tracker = Arc::new(WorkflowTracker::new());
        let ctx = RuntimeContext {
            engine: engine.clone(),
            trace_store: Arc::new(TraceStore::with_trace_writer(
                Duration::from_secs(60),
                trace_writer.clone(),
            )),
            workflow_tracker: tracker.clone(),
            trace_writer,
            event_tx,
            registry,
        };
        let mut service = FoundryService::new(
            ctx.clone(),
            StoreConfig {
                campaigns_path: path.clone(),
                work_items_path: work_items_path.clone(),
                events_dir: dir.path().join("events"),
                registry_path: dir.path().join("registry.json"),
                sentinels: Arc::new(RwLock::new(SentinelStore::default_seed())),
                sentinels_path: dir.path().join("sentinels.json"),
                scheduler_reload: Arc::new(Notify::new()),
            },
        );
        if let Some(wake) = wake {
            service = service.with_pacing(pacing.clone(), wake.clone());
            tokio::spawn(PacingScheduler::new(ctx, work_items_path, pacing, wake).run());
        }
        Self {
            dir,
            path,
            service: Arc::new(service),
            engine,
            tracker,
            events,
        }
    }

    /// Ask for an advance, bounded so a regression fails instead of hanging.
    async fn advance(&self, name: &str) -> foundryd::proto::AdvanceCampaignResponse {
        tokio::time::timeout(
            PROMPT,
            self.service.advance_campaign(Request::new(AdvanceCampaignRequest {
                name: name.into(),
                operator_origin: String::new(),
            })),
        )
        .await
        .unwrap_or_else(|_| panic!("advance for '{name}' waited on another formation"))
        .unwrap()
        .into_inner()
    }

    fn ledger_path(&self) -> std::path::PathBuf {
        self.dir.path().join("work.json")
    }

    fn stored(&self, name: &str) -> Campaign {
        CampaignStore::load(&self.path).unwrap().find(name).unwrap().clone()
    }

    /// Wait until every workflow this harness started has finished.
    async fn settled(&self) {
        wait_until("every workflow to finish", || self.tracker.list().is_empty()).await;
    }

    /// Every event broadcast so far.
    fn drain(&mut self) -> Vec<Event> {
        let mut seen = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            seen.push(event);
        }
        seen
    }

    /// Every campaign-scoped event broadcast so far, as `(type, campaign)`.
    fn drain_events(&mut self) -> Vec<(EventType, String)> {
        self.drain()
            .into_iter()
            .map(|event| {
                let campaign = event
                    .payload
                    .get("campaign")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                (event.event_type, campaign)
            })
            .collect()
    }

    /// The ledger's advance items, oldest first.
    fn advance_items(&self) -> Vec<WorkItem> {
        let store = WorkItemStore::load(&self.ledger_path()).unwrap();
        let mut items: Vec<WorkItem> = store
            .items
            .into_iter()
            .filter(|item| {
                item.started_root
                    .as_ref()
                    .or(item.pending_root.as_ref())
                    .is_some_and(|root| root.event_type == EventType::CampaignAdvanceRequested)
            })
            .collect();
        items.sort_by_key(|item| item.submitted_at);
        items
    }

    /// The single advance this harness ran, once its item has settled.
    fn only_advance_item(&self) -> WorkItem {
        let items = self.advance_items();
        assert_eq!(items.len(), 1, "exactly one advance item: {items:#?}");
        items.into_iter().next().unwrap()
    }

    /// Run the automatic advance a finished cycle asks for, through the
    /// engine and the ledger, as `RequestCampaignAdvance` would emit it.
    async fn advance_with_result(&self, name: &str, run_result: TaskRunCompletedPayload) {
        let event = Event::new(
            EventType::CampaignAdvanceRequested,
            "test-project".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                campaign: name.to_string(),
                run_event_id: Some(format!("evt_run_{name}")),
                run_result: Some(run_result),
                operator_origin: None,
                providers: foundry_sdk::gateway::StageProviders::default(),
            })
            .unwrap(),
        );
        tokio::time::timeout(Duration::from_secs(10), self.engine.process(event))
            .await
            .expect("the automatic advance settled");
    }

    /// Change the stored campaign under the store's own lock, as an
    /// `--offline` command would.
    async fn edit(&self, name: &str, change: impl FnOnce(&mut Campaign) + Send + 'static) {
        let path = self.path.clone();
        let name = name.to_string();
        tokio::time::timeout(
            PROMPT,
            tokio::task::spawn_blocking(move || {
                let mut guard = CampaignStore::lock_exclusive(&path).unwrap();
                change(guard.store.find_mut(&name).unwrap());
                guard.save().unwrap();
            }),
        )
        .await
        .expect("an edit waited on the campaign's formation")
        .unwrap();
    }
}

/// The campaign's `campaign_advance_completed` events, in broadcast order.
fn advance_completions<'a>(events: &'a [Event], campaign: &str) -> Vec<&'a Event> {
    events
        .iter()
        .filter(|event| {
            event.event_type == EventType::CampaignAdvanceCompleted
                && event.payload_str("campaign") == Some(campaign)
        })
        .collect()
}

/// Assert `event` is a stand-down for a campaign in `status`, and nothing
/// that could be read as a decision.
fn assert_stood_down(event: &Event, status: &str) {
    let payload = &event.payload;
    assert_eq!(payload["decision"], "stood_down", "{payload}");
    assert_eq!(payload["campaign_status"], status, "{payload}");
    assert_eq!(payload["success"], true, "{payload}");
    assert!(payload.get("objective").is_none(), "a stand-down dispatches nothing: {payload}");
    assert!(
        payload["reason"].as_str().is_some_and(|reason| reason.contains(status)),
        "the reason names the status: {payload}"
    );
}

/// The summary the `Advance Campaign` block reported, from its broadcast
/// `block_completed`.
fn advance_block_summary(events: &[Event]) -> String {
    events
        .iter()
        .rev()
        .find(|event| {
            event.event_type.as_str() == "block_completed"
                && event.payload_str("block") == Some("Advance Campaign")
        })
        .and_then(|event| event.payload_str("summary"))
        .expect("the advance block completed")
        .to_string()
}

fn landed_result(name: &str, summary: &str) -> TaskRunCompletedPayload {
    TaskRunCompletedPayload {
        project: "test-project".to_string(),
        success: true,
        landed: true,
        summary: summary.to_string(),
        preservation_ref: None,
        land_blocked: None,
        proof_evidence: None,
        uncommitted_artifacts: None,
        trunk_arrivals: Vec::new(),
        verdict: TaskVerdict::Complete,
        context: LoopContext {
            campaign: Some(name.to_string()),
            ..LoopContext::default()
        },
        failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
    }
}

async fn wait_until(what: &str, condition: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn count(events: &[(EventType, String)], kind: &EventType, campaign: &str) -> usize {
    events.iter().filter(|(ty, name)| ty == kind && name == campaign).count()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_advance_for_another_campaign_is_admitted_while_one_forms() {
    let agent = GatedAgent::new();
    let harness = Harness::new(&["alpha", "beta"], agent.clone());

    let first = harness.advance("alpha").await;
    assert!(!first.event_id.is_empty(), "expected `first.event_id` to be non-empty");
    wait_until("alpha's formation to reach its agent", || agent.entered() >= 1).await;

    // The production hang: this call waited on alpha's formation.
    let second = harness.advance("beta").await;
    assert!(!second.event_id.is_empty(), "beta's advance was dispatched");

    // Other controls and reads do not wait on a formation either.
    let paused = tokio::time::timeout(
        PROMPT,
        harness.service.pause_campaign(Request::new(PauseCampaignRequest {
            name: "beta".into(),
        })),
    )
    .await
    .expect("pause waited on another campaign's formation")
    .unwrap();
    assert_eq!(paused.into_inner().campaign.unwrap().status, "paused");
    let listed = tokio::time::timeout(
        PROMPT,
        harness.service.list_campaigns(Request::new(ListCampaignsRequest {
            project: String::new(),
        })),
    )
    .await
    .expect("list waited on a formation")
    .unwrap();
    assert_eq!(listed.into_inner().campaigns.len(), 2);

    agent.release(2);
    harness.settled().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn different_campaigns_form_at_the_same_time() {
    let agent = GatedAgent::new();
    let mut harness = Harness::new(&["alpha", "beta", "gamma"], agent.clone());

    for name in ["alpha", "beta", "gamma"] {
        harness.advance(name).await;
    }
    // Every formation reaches its agent while none has been answered.
    wait_until("all three formations to run at once", || agent.entered() == 3).await;

    agent.release(3);
    harness.settled().await;
    let events = harness.drain_events();
    for name in ["alpha", "beta", "gamma"] {
        assert_eq!(harness.stored(name).cycles_completed, 1, "{name} advanced once");
        assert_eq!(count(&events, &EventType::ExecutionRequested, name), 1, "{name} dispatched");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unchanged_campaign_receives_its_formed_outcome() {
    let agent = GatedAgent::new();
    let mut harness = Harness::new(&["alpha"], agent.clone());

    harness.advance("alpha").await;
    agent.release(1);
    harness.settled().await;

    let stored = harness.stored("alpha");
    assert_eq!(stored.cycles_completed, 1);
    assert_eq!(stored.objective_history.len(), 1);
    let events = harness.drain_events();
    assert_eq!(count(&events, &EventType::CampaignAdvanceCompleted, "alpha"), 1);
    assert_eq!(count(&events, &EventType::ExecutionRequested, "alpha"), 1);
}

// ── Settlement: a stop stands down, an edit re-forms, a fault fails ─────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_campaign_paused_while_it_forms_stays_paused_and_its_advance_lands() {
    let agent = GatedAgent::new();
    let mut harness = Harness::with_ledger(&["alpha"], agent.clone());

    harness.advance("alpha").await;
    wait_until("alpha's formation to reach its agent", || agent.entered() >= 1).await;
    tokio::time::timeout(
        PROMPT,
        harness.service.pause_campaign(Request::new(PauseCampaignRequest {
            name: "alpha".into(),
        })),
    )
    .await
    .expect("pause waited on the campaign's own formation")
    .unwrap();

    agent.release(1);
    harness.settled().await;

    let item = harness.only_advance_item();
    assert_eq!(
        item.state,
        WorkItemState::Landed,
        "standing down was the advance's job: {item:#?}"
    );
    let stored = harness.stored("alpha");
    assert_eq!(stored.status, CampaignStatus::Paused, "the operator's pause stands");
    assert_eq!(stored.cycles_completed, 0, "the formed objective was not dispatched");
    assert!(
        stored.objective_history.is_empty(),
        "expected `stored.objective_history` to be empty: {:?}",
        stored.objective_history
    );
    let events = harness.drain();
    assert_eq!(count_of(&events, &EventType::ExecutionRequested), 0);
    let completions = advance_completions(&events, "alpha");
    assert_eq!(completions.len(), 1, "the advance reports that it stood down");
    assert_stood_down(completions[0], "paused");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_campaign_cancelled_while_it_forms_stays_cancelled_and_its_advance_lands() {
    let agent = GatedAgent::new();
    let mut harness = Harness::with_ledger(&["alpha"], agent.clone());

    harness.advance("alpha").await;
    wait_until("alpha's formation to reach its agent", || agent.entered() >= 1).await;
    tokio::time::timeout(
        PROMPT,
        harness.service.cancel_campaign(Request::new(CancelCampaignRequest {
            name: "alpha".into(),
            reason: "no longer wanted".into(),
            terminate_now: false,
            discard_work: false,
        })),
    )
    .await
    .expect("graceful cancel waited on the campaign's own formation")
    .unwrap();

    agent.release(1);
    harness.settled().await;

    assert_eq!(harness.only_advance_item().state, WorkItemState::Landed);
    let stored = harness.stored("alpha");
    assert_eq!(stored.status, CampaignStatus::Cancelled, "the cancellation stands");
    assert_eq!(stored.cycles_completed, 0);
    let events = harness.drain();
    assert_eq!(count_of(&events, &EventType::ExecutionRequested), 0);
    assert_eq!(count_of(&events, &EventType::CampaignCompleted), 0, "not recorded as completed");
    let completions = advance_completions(&events, "alpha");
    assert_eq!(completions.len(), 1);
    assert_stood_down(completions[0], "cancelled");
}

/// The pre-formation halt: a cycle's result reaches a campaign that was
/// paused while the cycle ran. The result is recorded, nothing is formed, the
/// advance stands down and lands, and the advance after `resume` forms from
/// that recorded result without counting it twice.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_result_reaching_a_paused_campaign_lands_its_advance_and_replays_after_resume() {
    let agent = GatedAgent::new();
    let mut harness = Harness::with_ledger(&["alpha"], agent.clone());
    harness.edit("alpha", |campaign| campaign.status = CampaignStatus::Paused).await;

    harness
        .advance_with_result("alpha", landed_result("alpha", "RESULT-WHILE-PAUSED"))
        .await;
    harness.settled().await;

    assert_eq!(harness.only_advance_item().state, WorkItemState::Landed);
    assert_eq!(agent.entered(), 0, "nothing was formed while paused");
    let stored = harness.stored("alpha");
    assert_eq!(stored.status, CampaignStatus::Paused);
    assert_eq!(stored.cycles_landed, 1, "the result was recorded");
    assert_eq!(
        stored.pending_run_result.as_ref().map(|result| result.summary.as_str()),
        Some("RESULT-WHILE-PAUSED"),
        "the result waits for the advance after resume"
    );
    let events = harness.drain();
    let completions = advance_completions(&events, "alpha");
    assert_eq!(completions.len(), 1);
    assert_stood_down(completions[0], "paused");

    harness
        .service
        .resume_campaign(Request::new(ResumeCampaignRequest {
            name: "alpha".into(),
            add_cycles: 0,
        }))
        .await
        .unwrap();
    harness.advance("alpha").await;
    agent.release(1);
    harness.settled().await;

    assert!(
        agent.prompts()[0].contains("RESULT-WHILE-PAUSED"),
        "formation after resume was shown the result recorded while paused"
    );
    let stored = harness.stored("alpha");
    assert_eq!(stored.cycles_completed, 1, "the resumed advance dispatched");
    assert_eq!(stored.cycles_landed, 1, "the replayed result was not counted again");
    assert!(harness.advance_items().iter().all(|item| item.state == WorkItemState::Landed));
}

/// The graceful half of `cancel`: the in-flight cycle's result reaches the
/// cancelled campaign. The advance stands down and lands; nothing it emits
/// moves the campaign off `Cancelled`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_result_reaching_a_cancelled_campaign_lands_its_advance_and_stays_cancelled() {
    let agent = GatedAgent::new();
    let mut harness = Harness::with_ledger(&["alpha"], agent.clone());
    harness
        .edit("alpha", |campaign| campaign.status = CampaignStatus::Cancelled)
        .await;

    harness
        .advance_with_result("alpha", landed_result("alpha", "the last cycle"))
        .await;
    harness.settled().await;

    assert_eq!(harness.only_advance_item().state, WorkItemState::Landed);
    assert_eq!(agent.entered(), 0);
    let stored = harness.stored("alpha");
    assert_eq!(stored.status, CampaignStatus::Cancelled);
    assert_eq!(stored.cycles_landed, 1, "the last cycle's result was recorded");
    let events = harness.drain();
    for kind in [
        EventType::CampaignCompleted,
        EventType::CampaignEscalated,
        EventType::ExecutionRequested,
    ] {
        assert_eq!(count_of(&events, &kind), 0, "{kind} after a cancellation");
    }
    let completions = advance_completions(&events, "alpha");
    assert_eq!(completions.len(), 1);
    assert_stood_down(completions[0], "cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_campaign_edited_while_it_forms_keeps_the_edit_and_forms_again_from_it() {
    let agent = GatedAgent::new();
    let mut harness = Harness::with_ledger(&["alpha"], agent.clone());

    harness.advance("alpha").await;
    wait_until("alpha's first formation to reach its agent", || agent.entered() >= 1).await;
    harness
        .edit("alpha", |campaign| {
            campaign.budget.max_cycles = 25;
            campaign.mission = "EDITED MISSION".to_string();
        })
        .await;
    agent.release(1);
    wait_until("alpha to form again", || agent.entered() >= 2).await;
    agent.release(1);
    harness.settled().await;

    assert_eq!(harness.only_advance_item().state, WorkItemState::Landed);
    let prompts = agent.prompts();
    assert_eq!(prompts.len(), 2, "formed twice");
    assert!(!prompts[0].contains("EDITED MISSION"));
    assert!(prompts[1].contains("EDITED MISSION"), "the second formation read the edit");
    let stored = harness.stored("alpha");
    assert_eq!(stored.budget.max_cycles, 25, "the owner's edit was not overwritten");
    assert_eq!(stored.mission, "EDITED MISSION");
    assert_eq!(stored.status, CampaignStatus::Active);
    assert_eq!(stored.cycles_completed, 1, "the second formation's outcome was applied once");
    assert_eq!(stored.objective_history.len(), 1);
    let events = harness.drain();
    assert_eq!(count_of(&events, &EventType::ExecutionRequested), 1);
    let completions = advance_completions(&events, "alpha");
    assert_eq!(completions.len(), 1, "only the applied formation completes the advance");
    assert_eq!(completions[0].payload["decision"], "advance");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_campaign_edited_during_every_formation_fails_its_advance_visibly() {
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = asked.clone();
    let mut harness = Harness::build(
        &["alpha"],
        |store| {
            Arc::new(EditingAgent {
                store: store.to_path_buf(),
                asked: counter,
            })
        },
        true,
    );

    harness.advance("alpha").await;
    harness.settled().await;

    let item = harness.only_advance_item();
    assert_eq!(item.state, WorkItemState::Failed, "an advance that cannot settle is a fault");
    assert_eq!(asked.load(Ordering::SeqCst), 3, "formed exactly up to the bound");
    let stored = harness.stored("alpha");
    assert_eq!(stored.cycles_completed, 0, "no stale outcome was applied");
    assert_eq!(stored.status, CampaignStatus::Active);
    let events = harness.drain();
    assert_eq!(count_of(&events, &EventType::ExecutionRequested), 0);
    let summary = advance_block_summary(&events);
    assert!(
        summary.contains("changed while it formed") && summary.contains("3 formations"),
        "the failure says what happened: {summary}"
    );
}

/// A campaign deleted while it forms is a real fault, so its advance fails
/// rather than landing on an escalation nobody can read back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_campaign_removed_while_it_forms_fails_its_advance() {
    let agent = GatedAgent::new();
    let mut harness = Harness::with_ledger(&["alpha"], agent.clone());

    harness.advance("alpha").await;
    wait_until("alpha's formation to reach its agent", || agent.entered() >= 1).await;
    let path = harness.path.clone();
    tokio::task::spawn_blocking(move || {
        let mut guard = CampaignStore::lock_exclusive(&path).unwrap();
        guard.store.campaigns.retain(|campaign| campaign.name != "alpha");
        guard.save().unwrap();
    })
    .await
    .unwrap();
    agent.release(1);
    harness.settled().await;

    assert_eq!(harness.only_advance_item().state, WorkItemState::Failed);
    let events = harness.drain();
    let completions = advance_completions(&events, "alpha");
    assert_eq!(completions.len(), 1);
    assert_eq!(
        completions[0].payload["success"], false,
        "the fault is recorded on the terminal"
    );
}

/// Events of `kind` as their emitter emitted them. With the ledger wired, the
/// scheduler starts an admitted root by re-issuing it with the item's id, so
/// that copy is not counted again.
fn count_of(events: &[Event], kind: &EventType) -> usize {
    events
        .iter()
        .filter(|event| {
            &event.event_type == kind && event.payload.get("admitted_work_item_id").is_none()
        })
        .count()
}

// ── A second advance while one is in flight ─────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_advance_while_one_forms_names_it_and_dispatches_nothing() {
    let agent = GatedAgent::new();
    let mut harness = Harness::new(&["alpha"], agent.clone());

    let first = harness.advance("alpha").await;
    wait_until("alpha's formation to reach its agent", || agent.entered() >= 1).await;
    let second = harness.advance("alpha").await;

    assert!(second.event_id.is_empty(), "no second workflow was dispatched");
    let in_flight = second.in_flight.expect("the in-flight advance is named");
    assert_eq!(in_flight.kind, "advance");
    assert_eq!(in_flight.event_id, first.event_id);

    agent.release(2);
    harness.settled().await;
    assert_eq!(agent.entered(), 1, "alpha formed once");
    assert_eq!(harness.stored("alpha").cycles_completed, 1);
    let events = harness.drain_events();
    assert_eq!(count(&events, &EventType::CampaignAdvanceRequested, "alpha"), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_simultaneous_advances_for_one_campaign_dispatch_exactly_one() {
    for _ in 0..10 {
        let agent = GatedAgent::new();
        let mut harness = Harness::new(&["alpha"], agent.clone());

        let (left, right) = tokio::join!(harness.advance("alpha"), harness.advance("alpha"));
        let dispatched = [&left, &right].iter().filter(|r| !r.event_id.is_empty()).count();
        let refused = [&left, &right].iter().filter(|r| r.in_flight.is_some()).count();
        assert_eq!((dispatched, refused), (1, 1), "exactly one advance dispatched");

        agent.release(2);
        harness.settled().await;
        assert_eq!(agent.entered(), 1, "alpha formed once");
        let events = harness.drain_events();
        assert_eq!(count(&events, &EventType::CampaignAdvanceRequested, "alpha"), 1);
    }
}

fn cycle_root(campaign: &str) -> Event {
    Event::new(
        EventType::ExecutionRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({"workflow": "task", "prompt": "a cycle", "campaign": campaign}),
    )
    .with_source(Some(WorkSource::campaign(campaign, 1)))
}

fn advance_root(campaign: &str) -> Event {
    Event::new(
        EventType::CampaignAdvanceRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({"campaign": campaign}),
    )
}

/// A ledger item in `state`, rooted at `root`, as the daemon records it.
fn ledger_item(root: Event, kind: WorkItemKind, state: WorkItemState) -> WorkItem {
    let source = root.source.clone();
    let mut item = WorkItem::queued(
        WorkItemSpec {
            project: root.project.clone(),
            objective: "work".to_string(),
            kind,
            lane: WorkLane::Campaign,
            origin: root.event_type.to_string(),
            trace_id: None,
        },
        root.clone(),
        chrono::Utc::now(),
    )
    .with_source(source);
    match state {
        WorkItemState::Queued => {}
        WorkItemState::Held => item.hold(),
        WorkItemState::Running => {
            item.start(chrono::Utc::now());
            item.started_root = Some(root);
        }
        WorkItemState::Landed => {
            item.start(chrono::Utc::now());
            item.started_root = Some(root);
            item.settle_landed("landed", chrono::Utc::now());
            item.execution_completed = true;
        }
        other => panic!("fixture does not build {other:?}"),
    }
    item
}

fn write_ledger(harness: &Harness, items: Vec<WorkItem>) {
    let store = WorkItemStore {
        items,
        ..WorkItemStore::default()
    };
    store.save(&harness.ledger_path()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_advance_while_a_cycle_waits_or_runs_names_that_cycle() {
    for state in [
        WorkItemState::Queued,
        WorkItemState::Held,
        WorkItemState::Running,
    ] {
        let agent = GatedAgent::new();
        let mut harness = Harness::new(&["alpha"], agent.clone());
        let cycle = ledger_item(cycle_root("alpha"), WorkItemKind::CampaignCycle, state);
        write_ledger(&harness, vec![cycle.clone()]);

        let response = harness.advance("alpha").await;

        assert!(response.event_id.is_empty(), "{state:?}: nothing dispatched");
        let in_flight = response.in_flight.expect("the cycle is named");
        assert_eq!(in_flight.kind, "cycle");
        assert_eq!(in_flight.work_item_id, cycle.id);
        assert_eq!(in_flight.state, state.tag());
        harness.settled().await;
        assert_eq!(agent.entered(), 0, "{state:?}: no formation ran");
        assert!(harness.drain_events().is_empty(), "{state:?}: no event emitted");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_advance_while_an_advance_waits_in_the_ledger_names_it() {
    let agent = GatedAgent::new();
    let mut harness = Harness::new(&["alpha"], agent.clone());
    let queued = ledger_item(advance_root("alpha"), WorkItemKind::Workflow, WorkItemState::Queued);
    write_ledger(&harness, vec![queued.clone()]);

    let response = harness.advance("alpha").await;

    assert!(
        response.event_id.is_empty(),
        "expected `response.event_id` to be empty: {:?}",
        response.event_id
    );
    let in_flight = response.in_flight.expect("the queued advance is named");
    assert_eq!(in_flight.kind, "advance");
    assert_eq!(in_flight.work_item_id, queued.id);
    assert_eq!(in_flight.state, "queued");
    assert_eq!(in_flight.event_id, queued.pending_root.unwrap().id);
    harness.settled().await;
    assert_eq!(agent.entered(), 0);
    assert!(
        harness.drain_events().is_empty(),
        "expected `harness.drain_events()` to be empty: {:?}",
        harness.drain_events()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settled_work_and_other_campaigns_work_do_not_block_an_advance() {
    let agent = GatedAgent::new();
    let harness = Harness::new(&["alpha", "beta"], agent.clone());
    write_ledger(
        &harness,
        vec![
            ledger_item(cycle_root("alpha"), WorkItemKind::CampaignCycle, WorkItemState::Landed),
            ledger_item(cycle_root("beta"), WorkItemKind::CampaignCycle, WorkItemState::Running),
            ledger_item(advance_root("beta"), WorkItemKind::Workflow, WorkItemState::Queued),
        ],
    );

    let response = harness.advance("alpha").await;

    assert!(response.in_flight.is_none());
    assert!(!response.event_id.is_empty(), "alpha's advance was dispatched");
    agent.release(1);
    harness.settled().await;
    assert_eq!(harness.stored("alpha").cycles_completed, 1);
}
