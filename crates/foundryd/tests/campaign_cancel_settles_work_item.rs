//! Integration tests for the work-item half of `foundry campaign cancel --now`.
//!
//! A `--now` cancellation aborts the in-flight cycle, so the `TaskRunCompleted`
//! that would normally settle the cycle's ledger item never arrives. Without
//! the settlement these tests cover, that item sits `running` in
//! `foundry queue` until the next daemon restart closes it `failed` with the
//! reason `daemon restarted` — reporting an operator's deliberate stop as a
//! fault.
//!
//! These drive the real `CancelCampaign` RPC over gRPC, with the real
//! `DisposeCampaignWork` block registered and a broadcasting engine and durable
//! event writer attached, so `work_item_cancelled` is observed exactly where a
//! Watch client and the JSONL log see it.
//!
//! The companion file `campaign_cancel_terminates_now.rs` covers the abort
//! itself; nothing here changes when or whether that abort happens.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::Utc;
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_engine::event_writer::EventWriter;
use foundry_sdk::campaign::{
    Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gateway::fakes::FakeShellGateway;
use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::{
    WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
};
use foundryd::{
    proto::{
        CancelCampaignRequest, EmitRequest, GetWorkItemRequest, foundry_client::FoundryClient,
        foundry_server::FoundryServer,
    },
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::TempDir;
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

const CAMPAIGN: &str = "cancel-ledger-campaign";
const PROJECT: &str = "cancel-ledger-project";
const TRACE: &str = "cccccccccccccccccccccccccccccccc";
const REASON: &str = "superseded by the rewrite";

// ── A block that keeps the cycle in flight until it is aborted ───────────────

/// Sleeps, so the workflow is still active when `cancel --now` looks for it.
///
/// The cycle's ledger item is `running` for exactly this window, which is the
/// state the cancellation has to settle.
struct SleepUntilAborted;

impl TaskBlock for SleepUntilAborted {
    fn name(&self) -> &'static str {
        "Sleep Until Aborted"
    }
    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }
    fn sinks_on(&self) -> &[EventType] {
        &[EventType::GreetingRequested]
    }

    fn execute(
        &self,
        _trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(TaskBlockResult::success("slept", vec![]))
        })
    }
}

// ── Harness ─────────────────────────────────────────────────────────────────

struct Harness {
    service: FoundryService,
    events: broadcast::Receiver<Event>,
    events_dir: PathBuf,
    work_items_path: PathBuf,
    campaigns_path: PathBuf,
    /// Held so the temporary directory outlives the test.
    _tmp: TempDir,
}

fn registry_with_project(path: &Path) -> Arc<RwLock<Registry>> {
    Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: PROJECT.to_string(),
            path: path.display().to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: ActionFlags::default(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
            update_policy: None,
        }],
    }))
}

/// The real service, with the real `DisposeCampaignWork` block registered
/// against a temporary ledger.
///
/// The shell gateway is a fake: there is no worktree on disk for this
/// campaign, so disposal issues no commands at all, and a fake makes that
/// assertable rather than merely likely.
fn make_harness(work_items_path: Option<PathBuf>) -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    let events_dir = tmp.path().join("events");
    let campaigns_path = tmp.path().join("campaigns.json");
    let work_items_path = work_items_path.unwrap_or_else(|| tmp.path().join("work-items.json"));

    let (event_tx, events) = broadcast::channel(256);
    let registry = registry_with_project(tmp.path());
    let mut engine = Engine::new()
        .with_event_broadcaster(event_tx.clone())
        .with_event_writer(Arc::new(EventWriter::new(events_dir.clone())));
    engine.register(Box::new(SleepUntilAborted));
    engine.register(Box::new(foundry_blocks::blocks::DisposeCampaignWork::with_gateways(
        Arc::clone(&registry),
        FakeShellGateway::success(),
        work_items_path.clone(),
    )));
    let engine = Arc::new(engine);

    let trace_writer =
        Arc::new(TraceWriter::new(tmp.path().join("traces").to_str().expect("UTF-8")));
    let ctx = RuntimeContext {
        engine,
        trace_store: Arc::new(TraceStore::with_trace_writer(
            Duration::from_secs(60),
            Arc::clone(&trace_writer),
        )),
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer,
        event_tx,
        registry,
    };
    let stores = StoreConfig {
        work_items_path: work_items_path.clone(),
        events_dir: std::path::PathBuf::new(),
        campaigns_path: campaigns_path.clone(),
        registry_path: tmp.path().join("registry.json"),
        sentinels: Arc::new(RwLock::new(SentinelStore::default_seed())),
        sentinels_path: tmp.path().join("sentinels.json"),
        scheduler_reload: Arc::new(Notify::new()),
    };

    Harness {
        service: FoundryService::new(ctx, stores),
        events,
        events_dir,
        work_items_path,
        campaigns_path,
        _tmp: tmp,
    }
}

fn seed_campaign(path: &Path, status: CampaignStatus) {
    CampaignStore {
        version: 1,
        campaigns: vec![Campaign {
            name: CAMPAIGN.to_string(),
            project: PROJECT.to_string(),
            mission: "Run for a long time".to_string(),
            intent_refs: vec![],
            context_paths: vec![],
            done_evidence: vec![DoneEvidence::Review {
                statement: "done".to_string(),
            }],
            budget: CampaignBudget { max_cycles: 10 },
            escalation: vec![],
            status,
            cycles_completed: 1,
            cycles_landed: 0,
            authorized_by: Some("owner".to_string()),
            agent_provider: None,
            last_run_event_id: None,
            owner_decisions: vec![],
            pending_run_result: None,
            objective_history: vec![],
        }],
    }
    .save(path)
    .expect("seed campaign store");
}

/// A `running` campaign-cycle item on `trace`, as `RecordWorkItem` would have
/// left it when the cycle was dispatched.
fn seed_running_cycle(path: &Path, trace: &str) -> String {
    let item = WorkItem::dispatched(
        WorkItemSpec {
            project: PROJECT.to_string(),
            objective: "Tidy the billing module.".to_string(),
            kind: WorkItemKind::CampaignCycle,
            lane: WorkLane::Campaign,
            origin: "campaign cancel-ledger-campaign cycle 2".to_string(),
            trace_id: Some(trace.to_string()),
        },
        Utc::now(),
    );
    let id = item.id.clone();
    let mut store = WorkItemStore::default();
    store.upsert(item);
    store.save(path).expect("seed work-item ledger");
    id
}

fn campaign_root(trace: &str) -> EmitRequest {
    EmitRequest {
        event_type: EventType::GreetingRequested.to_string(),
        project: PROJECT.to_string(),
        throttle: 0,
        payload_json: serde_json::json!({ "campaign": CAMPAIGN }).to_string(),
        trace_id: trace.to_string(),
        span_id: String::new(),
        parent_span_id: String::new(),
    }
}

fn cancel(terminate_now: bool, discard_work: bool) -> CancelCampaignRequest {
    CancelCampaignRequest {
        name: CAMPAIGN.to_string(),
        reason: REASON.to_string(),
        terminate_now,
        discard_work,
    }
}

async fn start_server(service: FoundryService) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        Server::builder()
            .add_service(FoundryServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("gRPC server error");
    });
    tokio::task::yield_now().await;
    format!("http://127.0.0.1:{port}")
}

async fn wait_for<F: Fn() -> bool>(label: &str, condition: F) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    eprintln!("timed out waiting for {label}");
    false
}

/// Every `work_item_cancelled` event broadcast so far.
fn cancelled_events(rx: &mut broadcast::Receiver<Event>) -> Vec<Event> {
    let mut found = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if event.event_type == EventType::WorkItemCancelled {
            found.push(event);
        }
    }
    found
}

/// Ledger settlement precedes engine persistence and broadcast. Wait for the
/// event itself before checking its count; reading the ledger is not a barrier.
async fn wait_for_cancelled_events(rx: &mut broadcast::Receiver<Event>) -> Vec<Event> {
    let first = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.expect("cancellation stream must stay open without lag");
            if event.event_type == EventType::WorkItemCancelled {
                break event;
            }
        }
    })
    .await
    .expect("work_item_cancelled must reach Watch after ledger settlement");
    let mut events = vec![first];
    events.extend(cancelled_events(rx));
    events
}

/// Force the ledger-visible/event-not-yet-broadcast window that failed in CI.
#[tokio::test(start_paused = true)]
async fn cancellation_observation_waits_for_delayed_broadcast_after_settlement() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let ledger = tmp.path().join("work-items.json");
    let id = seed_running_cycle(&ledger, TRACE);
    let mut store = WorkItemStore::load(&ledger).expect("load ledger");
    store
        .find_mut(&id)
        .expect("the item")
        .settle_cancelled(REASON, None, Utc::now());
    store.save(&ledger).expect("save settlement");

    let (tx, mut rx) = broadcast::channel(16);
    let mut event = Event::new(
        EventType::WorkItemCancelled,
        PROJECT.to_string(),
        foundry_sdk::throttle::Throttle::default(),
        serde_json::json!({ "item_id": id, "reason": REASON }),
    );
    event.trace_id = Some(TRACE.to_string());
    let expected = event.clone();
    let sender = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        tx.send(event).expect("receiver stays attached");
    });

    assert_eq!(item_state(&ledger, &id), WorkItemState::Cancelled);
    assert!(cancelled_events(&mut rx).is_empty(), "ledger settlement precedes broadcast");
    let events = wait_for_cancelled_events(&mut rx).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, expected.id);
    assert_eq!(events[0].payload, expected.payload);
    assert_eq!(events[0].trace_id.as_deref(), Some(TRACE));
    sender.await.expect("sender completes");
}

fn item_state(path: &Path, id: &str) -> WorkItemState {
    WorkItemStore::load(path)
        .expect("load ledger")
        .find(id)
        .expect("the item")
        .state
}

// ── The settlement ──────────────────────────────────────────────────────────

/// The load-bearing proof: the aborted cycle's item settles `cancelled` with the
/// operator's own reason, and the cancellation is recorded as an event on that
/// cycle's trace — in the Watch stream and in the durable JSONL log.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_now_settles_the_aborted_cycles_item_cancelled_and_records_the_event() {
    let mut harness = make_harness(None);
    seed_campaign(&harness.campaigns_path, CampaignStatus::Active);
    let item_id = seed_running_cycle(&harness.work_items_path, TRACE);
    let ledger = harness.work_items_path.clone();
    let events_dir = harness.events_dir.clone();

    let addr = start_server(harness.service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client.emit(campaign_root(TRACE)).await.expect("emit the campaign root");

    client
        .cancel_campaign(cancel(true, false))
        .await
        .expect("cancel --now must succeed");

    assert!(
        wait_for("the cycle's item to settle", || item_state(&ledger, &item_id)
            == WorkItemState::Cancelled)
        .await,
        "the item stayed {:?}; only a restart would have closed it",
        item_state(&ledger, &item_id)
    );

    // The durable record, read back the way an operator reads it.
    let record = client
        .get_work_item(GetWorkItemRequest {
            id: item_id.clone(),
        })
        .await
        .expect("get_work_item")
        .into_inner()
        .item
        .expect("the item is in the ledger");
    assert_eq!(record.state, "cancelled");
    assert_eq!(record.reason, REASON, "the operator's reason, verbatim");
    assert!(
        record.settled_at.is_some_and(|at| !at.is_empty()),
        "a settled item records when it settled"
    );
    assert_eq!(record.kind, "campaign_cycle");
    assert_eq!(record.lane, "campaign");
    assert_eq!(record.project, PROJECT);

    let cancelled = wait_for_cancelled_events(&mut harness.events).await;
    assert_eq!(cancelled.len(), 1, "one settled item, one work_item_cancelled event");
    let event = &cancelled[0];
    assert_eq!(event.payload["item_id"], item_id.as_str());
    assert_eq!(event.payload["project"], PROJECT);
    assert_eq!(event.payload["kind"], "campaign_cycle");
    assert_eq!(event.payload["lane"], "campaign");
    assert_eq!(event.payload["state"], "cancelled");
    assert_eq!(event.payload["reason"], REASON);
    assert_eq!(event.payload["origin"], "campaign cancel-ledger-campaign cycle 2");
    assert_eq!(
        event.trace_id.as_deref(),
        Some(TRACE),
        "the event belongs to the aborted cycle's trace, not the cancellation's"
    );

    assert!(
        wait_for("the event to reach the JSONL log", || {
            logged(&events_dir).contains(&item_id)
        })
        .await,
        "work_item_cancelled never reached the durable log"
    );
    assert!(logged(&events_dir).contains("work_item_cancelled"));
}

fn logged(events_dir: &Path) -> String {
    std::fs::read_dir(events_dir)
        .map(|entries| {
            entries
                .filter_map(|entry| std::fs::read_to_string(entry.ok()?.path()).ok())
                .collect::<String>()
        })
        .unwrap_or_default()
}

/// `--discard-work` settles the item the same way: the operator's stop is the
/// fact being recorded, and what disposal did with the work is the disposition.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_now_with_discard_work_settles_the_item_too() {
    let mut harness = make_harness(None);
    seed_campaign(&harness.campaigns_path, CampaignStatus::Active);
    let item_id = seed_running_cycle(&harness.work_items_path, TRACE);
    let ledger = harness.work_items_path.clone();

    let addr = start_server(harness.service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client.emit(campaign_root(TRACE)).await.expect("emit the campaign root");
    client
        .cancel_campaign(cancel(true, true))
        .await
        .expect("cancel --now must succeed");

    assert!(
        wait_for("the cycle's item to settle", || item_state(&ledger, &item_id)
            == WorkItemState::Cancelled)
        .await
    );
    let item = WorkItemStore::load(&ledger).unwrap().find(&item_id).unwrap().clone();
    assert_eq!(item.reason, REASON);
    assert!(item.settled_at.is_some());
    assert_eq!(wait_for_cancelled_events(&mut harness.events).await.len(), 1);
}

// ── What must change nothing ────────────────────────────────────────────────

/// A graceful cancel leaves the cycle running to its own finish, so
/// `FinalizeTask` and `TaskRunCompleted` settle the item as before.
#[tokio::test(flavor = "multi_thread")]
async fn a_graceful_cancel_changes_no_item_and_records_no_cancellation() {
    let mut harness = make_harness(None);
    seed_campaign(&harness.campaigns_path, CampaignStatus::Active);
    let item_id = seed_running_cycle(&harness.work_items_path, TRACE);
    let ledger = harness.work_items_path.clone();
    let before = std::fs::read(&ledger).unwrap();

    let addr = start_server(harness.service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client.emit(campaign_root(TRACE)).await.expect("emit the campaign root");
    client.cancel_campaign(cancel(false, false)).await.expect("cancel must succeed");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(std::fs::read(&ledger).unwrap(), before, "the ledger file is byte-identical");
    assert_eq!(item_state(&ledger, &item_id), WorkItemState::Running);
    assert!(cancelled_events(&mut harness.events).is_empty());
}

/// A `--now` cancel whose aborted trace matches no running item settles
/// nothing. There is no fallback to the project's newest running item: settling
/// a concurrent run as cancelled would be worse than leaving this one.
#[tokio::test(flavor = "multi_thread")]
async fn a_cancellation_whose_trace_matches_no_running_item_changes_nothing() {
    let mut harness = make_harness(None);
    seed_campaign(&harness.campaigns_path, CampaignStatus::Active);
    let item_id = seed_running_cycle(&harness.work_items_path, &"d".repeat(32));
    let ledger = harness.work_items_path.clone();
    let before = std::fs::read(&ledger).unwrap();

    let addr = start_server(harness.service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client.emit(campaign_root(TRACE)).await.expect("emit the campaign root");
    client
        .cancel_campaign(cancel(true, false))
        .await
        .expect("cancel --now must succeed");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(std::fs::read(&ledger).unwrap(), before, "the ledger file is byte-identical");
    assert_eq!(item_state(&ledger, &item_id), WorkItemState::Running);
    assert!(cancelled_events(&mut harness.events).is_empty());
}

/// Cancelling an already-cancelled campaign is a no-op that emits no
/// `CampaignCancelled` at all, so nothing settles a second time.
#[tokio::test(flavor = "multi_thread")]
async fn an_already_cancelled_campaign_changes_no_item() {
    let mut harness = make_harness(None);
    seed_campaign(&harness.campaigns_path, CampaignStatus::Cancelled);
    let item_id = seed_running_cycle(&harness.work_items_path, TRACE);
    let ledger = harness.work_items_path.clone();
    let before = std::fs::read(&ledger).unwrap();

    let addr = start_server(harness.service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let response = client
        .cancel_campaign(cancel(true, false))
        .await
        .expect("cancelling twice is a no-op, not an error")
        .into_inner();
    assert_eq!(response.campaign.expect("detail").status, "cancelled");
    assert!(response.event_id.is_empty(), "a no-op emits no terminal event");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(std::fs::read(&ledger).unwrap(), before, "the ledger file is byte-identical");
    assert_eq!(item_state(&ledger, &item_id), WorkItemState::Running);
    assert!(cancelled_events(&mut harness.events).is_empty());
}

/// The ledger is bookkeeping beside the cancellation, never a precondition for
/// it: an unwritable ledger path still cancels the campaign and still emits
/// `CampaignCancelled`.
#[tokio::test(flavor = "multi_thread")]
async fn an_unwritable_ledger_never_fails_the_cancellation() {
    let unwritable = tempfile::tempdir().expect("tempdir");
    // A directory where the ledger file should be: neither readable as JSON nor
    // replaceable by a rename.
    let path = unwritable.path().join("work-items.json");
    std::fs::create_dir(&path).expect("create the blocking directory");

    let mut harness = make_harness(Some(path));
    seed_campaign(&harness.campaigns_path, CampaignStatus::Active);

    let addr = start_server(harness.service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client.emit(campaign_root(TRACE)).await.expect("emit the campaign root");

    let response = client
        .cancel_campaign(cancel(true, false))
        .await
        .expect("an unwritable ledger must not fail the cancellation")
        .into_inner();
    assert_eq!(response.campaign.expect("detail").status, "cancelled");
    assert!(!response.event_id.is_empty(), "CampaignCancelled is still emitted");

    let store = CampaignStore::load(&harness.campaigns_path).expect("load store");
    assert_eq!(store.find(CAMPAIGN).expect("campaign").status, CampaignStatus::Cancelled);
    assert!(
        cancelled_events(&mut harness.events).is_empty(),
        "nothing settled, nothing recorded"
    );
}
