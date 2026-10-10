//! Service-boundary tests for the pacing owner controls: `HoldWorkItem`,
//! `ReleaseWorkItem`, `GetPacing`, `PausePacing` and `ResumePacing`.
//!
//! Each test builds a real `FoundryService` over a temporary ledger and
//! temporary pacing files, with a broadcasting engine and a durable event
//! writer attached, so every event is observed exactly where a Watch client
//! and the JSONL log would see it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::Utc;
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_engine::event_writer::EventWriter;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::pacing::{PacingPaths, PauseState};
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{
    WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
};
use foundryd::proto::foundry_server::Foundry as _;
use foundryd::proto::{
    GetPacingRequest, HoldWorkItemRequest, PausePacingRequest, ReleaseWorkItemRequest,
    ResumePacingRequest,
};
use foundryd::service::{FoundryService, RuntimeContext, StoreConfig};
use foundryd::trace_store::TraceStore;
use foundryd::workflow_tracker::WorkflowTracker;
use tempfile::TempDir;
use tokio::sync::{Notify, broadcast};
use tonic::{Code, Request};

const ORIGIN: &str = "host workbench: by hand";

fn root(project: &str) -> Event {
    Event::new(
        EventType::ExecutionRequested,
        project.to_string(),
        Throttle::Full,
        serde_json::json!({"project": project, "workflow": "task", "prompt": "x"}),
    )
    .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
}

fn queued(id: &str, project: &str) -> WorkItem {
    let mut item = WorkItem::queued(
        WorkItemSpec {
            project: project.to_string(),
            objective: format!("objective for {id}"),
            kind: WorkItemKind::Task,
            lane: WorkLane::Interactive,
            origin: "foundry task".to_string(),
            trace_id: Some(id.repeat(2)),
        },
        root(project),
        Utc::now(),
    );
    item.id = id.to_string();
    item.reason = "ready".to_string();
    item
}

fn settled(id: &str, state: WorkItemState) -> WorkItem {
    let mut item = queued(id, "alpha");
    item.start(Utc::now());
    item.state = state;
    item.settled_at = Some(Utc::now());
    item
}

struct Fixture {
    service: FoundryService,
    events: broadcast::Receiver<Event>,
    ledger: PathBuf,
    pacing: PacingPaths,
    events_dir: PathBuf,
    _root: TempDir,
}

fn fixture(items: Vec<WorkItem>) -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let ledger = root.path().join("work-items.json");
    WorkItemStore { version: 1, items }.save(&ledger).expect("seed ledger");
    let events_dir = root.path().join("events");
    let pacing = PacingPaths {
        limits: root.path().join("pacing.json"),
        state: root.path().join("pacing-state.json"),
    };

    let (event_tx, events) = broadcast::channel(64);
    let engine = Arc::new(
        Engine::new()
            .with_event_broadcaster(event_tx.clone())
            .with_event_writer(Arc::new(EventWriter::new(events_dir.clone()))),
    );
    let trace_writer = Arc::new(TraceWriter::new(
        root.path().join("traces").to_str().expect("trace dir must be UTF-8"),
    ));
    let ctx = RuntimeContext {
        engine,
        trace_store: Arc::new(TraceStore::with_trace_writer(
            Duration::from_secs(60),
            Arc::clone(&trace_writer),
        )),
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer,
        event_tx,
        registry: Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        })),
    };
    let stores = StoreConfig {
        work_items_path: ledger.clone(),
        events_dir: events_dir.clone(),
        campaigns_path: PathBuf::new(),
        registry_path: PathBuf::new(),
        sentinels: Arc::new(RwLock::new(SentinelStore::default_seed())),
        sentinels_path: PathBuf::new(),
        scheduler_reload: Arc::new(Notify::new()),
    };
    Fixture {
        service: FoundryService::new(ctx, stores)
            .with_pacing(pacing.clone(), Arc::new(Notify::new())),
        events,
        ledger,
        pacing,
        events_dir,
        _root: root,
    }
}

impl Fixture {
    fn item(&self, id: &str) -> WorkItem {
        WorkItemStore::load(&self.ledger).unwrap().find(id).cloned().expect("the item")
    }

    fn broadcast(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            out.push(event);
        }
        out
    }

    fn logged(&self) -> String {
        std::fs::read_dir(&self.events_dir)
            .map(|entries| {
                entries
                    .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }
}

fn hold(id: &str) -> Request<HoldWorkItemRequest> {
    Request::new(HoldWorkItemRequest {
        id: id.to_string(),
        operator_origin: ORIGIN.to_string(),
    })
}

fn release(id: &str) -> Request<ReleaseWorkItemRequest> {
    Request::new(ReleaseWorkItemRequest {
        id: id.to_string(),
        operator_origin: ORIGIN.to_string(),
    })
}

fn pause(lanes: &[&str]) -> Request<PausePacingRequest> {
    Request::new(PausePacingRequest {
        lanes: lanes.iter().map(ToString::to_string).collect(),
        operator_origin: ORIGIN.to_string(),
    })
}

fn resume(lanes: &[&str]) -> Request<ResumePacingRequest> {
    Request::new(ResumePacingRequest {
        lanes: lanes.iter().map(ToString::to_string).collect(),
        operator_origin: ORIGIN.to_string(),
    })
}

// ── hold and release ──────────────────────────────────────────────────────────

#[tokio::test]
async fn hold_moves_a_queued_item_to_held_and_records_the_event_durably_and_on_watch() {
    let mut f = fixture(vec![queued("wi_q", "alpha")]);

    let held = f.service.hold_work_item(hold("wi_q")).await.unwrap().into_inner().item.unwrap();

    assert_eq!(held.state, "held");
    assert_eq!(held.reason, "held by operator");
    let stored = f.item("wi_q");
    assert_eq!(stored.state, WorkItemState::Held);
    assert!(stored.pending_root.is_some(), "the root survives a hold");
    let action = stored.operator_action.expect("the hold is recorded");
    assert_eq!((action.command.as_str(), action.origin.as_str()), ("hold", ORIGIN));
    assert_eq!(action.previous_state, WorkItemState::Queued);
    assert_eq!(action.previous_reason, "ready");

    let events = f.broadcast();
    assert_eq!(events.len(), 1, "one transition, one event: {events:?}");
    assert_eq!(events[0].event_type, EventType::WorkItemHeld);
    assert_eq!(events[0].payload["item_id"], "wi_q");
    assert_eq!(events[0].payload["state"], "held");
    assert_eq!(events[0].trace_id.as_deref(), Some("wi_qwi_q"));
    assert!(f.logged().contains("work_item_held"), "durable in the JSONL log");
}

#[tokio::test]
async fn release_returns_a_held_item_to_the_queue_with_the_schedulers_reason() {
    let mut f = fixture(vec![
        queued("wi_q", "alpha"),
        settled("wi_busy", WorkItemState::Running),
    ]);
    f.service.hold_work_item(hold("wi_q")).await.unwrap();
    f.broadcast();

    let released = f
        .service
        .release_work_item(release("wi_q"))
        .await
        .unwrap()
        .into_inner()
        .item
        .unwrap();

    assert_eq!(released.state, "queued");
    assert_eq!(
        released.reason, "repository busy: wi_busy",
        "the reason is the next tick's verdict"
    );
    let stored = f.item("wi_q");
    assert_eq!(stored.operator_action.unwrap().command, "release");
    let events = f.broadcast();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, EventType::WorkItemReleased);
    assert_eq!(events[0].payload["reason"], "repository busy: wi_busy");
    assert!(f.logged().contains("work_item_released"));
}

#[tokio::test]
async fn release_runs_a_dependency_blocked_item_regardless_of_the_settled_dependency() {
    let mut blocked = queued("wi_b", "beta");
    blocked.depends_on = vec!["wi_failed".to_string(), "wi_landed".to_string()];
    blocked.settle_needs_decision("waits on wi_failed, which settled failed", Utc::now());
    let f = fixture(vec![
        settled("wi_failed", WorkItemState::Failed),
        settled("wi_landed", WorkItemState::Landed),
        blocked,
    ]);

    let released = f
        .service
        .release_work_item(release("wi_b"))
        .await
        .unwrap()
        .into_inner()
        .item
        .unwrap();

    assert_eq!(released.state, "queued");
    assert_eq!(released.reason, "ready");
    assert_eq!(released.depends_on, vec!["wi_landed".to_string()], "the failed one is dropped");
    let stored = f.item("wi_b");
    assert_eq!(stored.state, WorkItemState::Queued);
    assert_eq!(stored.settled_at, None);
    assert_eq!(
        stored.operator_action.unwrap().previous_reason,
        "waits on wi_failed, which settled failed",
        "the dependency verdict is kept as evidence"
    );
}

#[tokio::test]
async fn hold_and_release_refuse_every_other_state_and_unknown_ids() {
    let mut reviewed = settled("wi_reviewed", WorkItemState::NeedsDecision);
    reviewed.started_at = Some(Utc::now());
    let f = fixture(vec![
        settled("wi_running", WorkItemState::Running),
        settled("wi_landed", WorkItemState::Landed),
        reviewed,
        queued("wi_q", "alpha"),
    ]);
    for id in ["wi_running", "wi_landed", "wi_reviewed"] {
        let status = f.service.hold_work_item(hold(id)).await.unwrap_err();
        assert_eq!(status.code(), Code::FailedPrecondition, "hold {id}");
        let status = f.service.release_work_item(release(id)).await.unwrap_err();
        assert_eq!(status.code(), Code::FailedPrecondition, "release {id}");
    }
    assert_eq!(
        f.service.release_work_item(release("wi_q")).await.unwrap_err().code(),
        Code::FailedPrecondition,
        "a queued item is not held"
    );
    assert_eq!(
        f.service.hold_work_item(hold("wi_absent")).await.unwrap_err().code(),
        Code::NotFound
    );
    assert_eq!(
        f.service
            .hold_work_item(Request::new(HoldWorkItemRequest {
                id: "wi_q".to_string(),
                operator_origin: " ".to_string(),
            }))
            .await
            .unwrap_err()
            .code(),
        Code::InvalidArgument
    );
    assert_eq!(f.item("wi_q").state, WorkItemState::Queued, "nothing changed");
}

// ── pause, resume and the read ────────────────────────────────────────────────

#[tokio::test]
async fn pause_and_resume_persist_the_lanes_and_record_the_events() {
    let mut f = fixture(vec![queued("wi_q", "alpha")]);

    let paused = f
        .service
        .pause_pacing(pause(&["campaign", "interactive"]))
        .await
        .unwrap()
        .into_inner()
        .pacing
        .unwrap();
    assert_eq!(paused.paused_lanes, vec!["interactive", "campaign"]);
    let on_disk = PauseState::load(&f.pacing.state).unwrap();
    assert_eq!(on_disk.paused, vec![WorkLane::Interactive, WorkLane::Campaign]);
    assert!(!f.pacing.state.with_extension("json.tmp").exists(), "saved through a rename");

    let events = f.broadcast();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, EventType::PacingPaused);
    assert_eq!(events[0].project, "system");
    assert_eq!(events[0].payload["lanes"], serde_json::json!(["interactive", "campaign"]));
    assert_eq!(events[0].payload["paused"], serde_json::json!(["interactive", "campaign"]));
    assert_eq!(events[0].payload["operator_origin"], ORIGIN);
    assert!(f.logged().contains("pacing_paused"));

    let resumed = f
        .service
        .resume_pacing(resume(&["interactive"]))
        .await
        .unwrap()
        .into_inner()
        .pacing
        .unwrap();
    assert_eq!(resumed.paused_lanes, vec!["campaign"]);
    assert_eq!(PauseState::load(&f.pacing.state).unwrap().paused, vec![WorkLane::Campaign]);
    let events = f.broadcast();
    assert_eq!(events[0].event_type, EventType::PacingResumed);
    assert_eq!(events[0].payload["paused"], serde_json::json!(["campaign"]));
    assert!(f.logged().contains("pacing_resumed"));

    let all = f.service.resume_pacing(resume(&[])).await.unwrap().into_inner().pacing.unwrap();
    assert!(all.paused_lanes.is_empty(), "no lanes means every lane");
}

#[tokio::test]
async fn an_unknown_lane_is_refused_before_anything_is_written() {
    let f = fixture(vec![]);
    let status = f.service.pause_pacing(pause(&["nightly"])).await.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(!f.pacing.state.exists(), "nothing was saved");
}

#[tokio::test]
async fn get_pacing_reads_the_limits_the_lanes_and_every_item_with_its_reason() {
    std::fs::create_dir_all(Path::new(&std::env::temp_dir())).unwrap();
    let mut waiting = queued("wi_wait", "alpha");
    waiting.reason = "repository busy: wi_run".to_string();
    let mut held = queued("wi_held", "beta");
    held.hold();
    let f = fixture(vec![settled("wi_run", WorkItemState::Running), waiting, held]);
    std::fs::write(&f.pacing.limits, r#"{"max_running": 3}"#).unwrap();
    f.service.pause_pacing(pause(&["maintenance"])).await.unwrap();

    let status = f
        .service
        .get_pacing(Request::new(GetPacingRequest {}))
        .await
        .unwrap()
        .into_inner()
        .pacing
        .unwrap();

    assert_eq!(status.max_running, 3);
    assert_eq!(status.running, 1);
    assert_eq!(status.paused_lanes, vec!["maintenance"]);
    assert_eq!(status.running_items.len(), 1);
    assert_eq!(status.running_items[0].id, "wi_run");
    assert_eq!(status.running_items[0].repository, "alpha", "unregistered: the project name");
    let waiting: Vec<(&str, &str, &str)> = status
        .waiting_items
        .iter()
        .map(|item| (item.id.as_str(), item.state.as_str(), item.reason.as_str()))
        .collect();
    assert_eq!(
        waiting,
        vec![
            ("wi_wait", "queued", "repository busy: wi_run"),
            ("wi_held", "held", "held by operator"),
        ]
    );
}
