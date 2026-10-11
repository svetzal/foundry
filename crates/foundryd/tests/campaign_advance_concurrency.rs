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
//!   since formation read it. A campaign paused, cancelled or edited while its
//!   formation ran keeps the operator's change, and the outcome is discarded;
//! - an advance for a campaign whose advance or cycle is already submitted,
//!   queued, held or running dispatches nothing and names that work instead.
//!   On 2026-10-11 an operator retried a hung advance and got two advance
//!   workflows for one campaign, which had to be cancelled by hand.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::{
    blocks::AdvanceCampaign,
    gateway::{AgentGateway, AgentRequest, AgentResponse},
    trace_writer::TraceWriter,
};
use foundry_engine::engine::Engine;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gateway::fakes::FakeShellGateway;
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
    proto::{
        AdvanceCampaignRequest, CancelCampaignRequest, ListCampaignsRequest, PauseCampaignRequest,
        foundry_server::Foundry,
    },
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
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
}

impl GatedAgent {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: AtomicUsize::new(0),
            release: Semaphore::new(0),
        })
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
        _request: &'a AgentRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.release.acquire().await.expect("semaphore open").forget();
            Ok(AgentResponse::success(
                r#"{"decision":"advance","objective":"Add the next tested slice of the mission.","reason":"the mission is not yet met"}"#,
            ))
        })
    }
}

fn active_campaign(name: &str) -> Campaign {
    Campaign {
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
    tracker: Arc<WorkflowTracker>,
    events: broadcast::Receiver<Event>,
}

impl Harness {
    fn new(campaigns: &[&str], agent: Arc<GatedAgent>) -> Self {
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
        let (event_tx, events) = broadcast::channel(256);
        let mut engine = Engine::new().with_event_broadcaster(event_tx.clone());
        engine.register(Box::new(AdvanceCampaign::new(
            agent,
            FakeShellGateway::success(),
            registry.clone(),
            path.clone(),
        )));
        let trace_writer = Arc::new(TraceWriter::new(dir.path().to_str().unwrap()));
        let tracker = Arc::new(WorkflowTracker::new());
        let service = Arc::new(FoundryService::new(
            RuntimeContext {
                engine: Arc::new(engine),
                trace_store: Arc::new(TraceStore::with_trace_writer(
                    Duration::from_secs(60),
                    trace_writer.clone(),
                )),
                workflow_tracker: tracker.clone(),
                trace_writer,
                event_tx,
                registry,
            },
            StoreConfig {
                campaigns_path: path.clone(),
                work_items_path: dir.path().join("work.json"),
                events_dir: dir.path().join("events"),
                registry_path: dir.path().join("registry.json"),
                sentinels: Arc::new(RwLock::new(SentinelStore::default_seed())),
                sentinels_path: dir.path().join("sentinels.json"),
                scheduler_reload: Arc::new(Notify::new()),
            },
        ));
        Self {
            dir,
            path,
            service,
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

    /// Every campaign-scoped event broadcast so far, as `(type, campaign)`.
    fn drain_events(&mut self) -> Vec<(EventType, String)> {
        let mut seen = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            let campaign = event
                .payload
                .get("campaign")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string();
            seen.push((event.event_type, campaign));
        }
        seen
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
    assert!(!first.event_id.is_empty());
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_campaign_paused_while_it_forms_stays_paused_and_dispatches_nothing() {
    let agent = GatedAgent::new();
    let mut harness = Harness::new(&["alpha"], agent.clone());

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

    let stored = harness.stored("alpha");
    assert_eq!(stored.status, CampaignStatus::Paused, "the operator's pause stands");
    assert_eq!(stored.cycles_completed, 0, "the formed objective was not dispatched");
    assert!(stored.objective_history.is_empty());
    let events = harness.drain_events();
    assert_eq!(count(&events, &EventType::ExecutionRequested, "alpha"), 0);
    assert_eq!(count(&events, &EventType::CampaignAdvanceCompleted, "alpha"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_campaign_cancelled_while_it_forms_stays_cancelled_and_dispatches_nothing() {
    let agent = GatedAgent::new();
    let mut harness = Harness::new(&["alpha"], agent.clone());

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

    let stored = harness.stored("alpha");
    assert_eq!(stored.status, CampaignStatus::Cancelled, "the cancellation stands");
    assert_eq!(stored.cycles_completed, 0);
    let events = harness.drain_events();
    assert_eq!(count(&events, &EventType::ExecutionRequested, "alpha"), 0);
    assert_eq!(count(&events, &EventType::CampaignAdvanceCompleted, "alpha"), 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_campaign_edited_while_it_forms_keeps_the_edit_and_dispatches_nothing() {
    let agent = GatedAgent::new();
    let mut harness = Harness::new(&["alpha"], agent.clone());

    harness.advance("alpha").await;
    wait_until("alpha's formation to reach its agent", || agent.entered() >= 1).await;
    // An owner edit through the store's own lock, as `--offline` makes it.
    let path = harness.path.clone();
    tokio::time::timeout(
        PROMPT,
        tokio::task::spawn_blocking(move || {
            let mut guard = CampaignStore::lock_exclusive(&path).unwrap();
            guard.store.find_mut("alpha").unwrap().budget.max_cycles = 25;
            guard.save().unwrap();
        }),
    )
    .await
    .expect("an owner edit waited on the campaign's formation")
    .unwrap();

    agent.release(1);
    harness.settled().await;

    let stored = harness.stored("alpha");
    assert_eq!(stored.budget.max_cycles, 25, "the owner's edit was not overwritten");
    assert_eq!(stored.status, CampaignStatus::Active);
    assert_eq!(stored.cycles_completed, 0, "the stale outcome was not applied");
    let events = harness.drain_events();
    assert_eq!(count(&events, &EventType::ExecutionRequested, "alpha"), 0);
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

    assert!(response.event_id.is_empty());
    let in_flight = response.in_flight.expect("the queued advance is named");
    assert_eq!(in_flight.kind, "advance");
    assert_eq!(in_flight.work_item_id, queued.id);
    assert_eq!(in_flight.state, "queued");
    assert_eq!(in_flight.event_id, queued.pending_root.unwrap().id);
    harness.settled().await;
    assert_eq!(agent.entered(), 0);
    assert!(harness.drain_events().is_empty());
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
