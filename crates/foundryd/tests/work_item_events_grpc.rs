//! Service-boundary tests for the `ListWorkItemEvents` RPC.
//!
//! Each test builds a real `FoundryService` over a temporary ledger and a
//! temporary events directory whose log lines are written by the real
//! `EventWriter`, then asks the service for one item's `work_item_*` events.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, TimeZone as _, Utc};
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_engine::event_writer::EventWriter;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::WorkItemEventPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{
    WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
};
use foundryd::proto::foundry_server::Foundry as _;
use foundryd::proto::{GetWorkItemRequest, ListWorkItemEventsRequest, WorkItemEvent};
use foundryd::service::{FoundryService, RuntimeContext, StoreConfig};
use foundryd::trace_store::TraceStore;
use foundryd::workflow_tracker::WorkflowTracker;
use tempfile::TempDir;
use tokio::sync::{Notify, broadcast};
use tonic::Request;

const SHARED_TRACE: &str = "0123456789abcdef0123456789abcdef";

fn item(id: &str, kind: WorkItemKind, lane: WorkLane) -> WorkItem {
    let mut item = WorkItem::submitted(
        WorkItemSpec {
            project: "alpha".to_string(),
            objective: format!("objective for {id}"),
            kind,
            lane,
            origin: "maintenance cycle".to_string(),
            trace_id: Some(SHARED_TRACE.to_string()),
        },
        Utc::now(),
    );
    item.id = id.to_string();
    item
}

/// One `work_item_*` event for `subject` as it stands in `state`, on the
/// shared trace and project.
fn lifecycle_event(
    subject: &WorkItem,
    event_type: EventType,
    state: WorkItemState,
    reason: &str,
    occurred_at: DateTime<Utc>,
) -> Event {
    let mut snapshot = subject.clone();
    snapshot.state = state;
    snapshot.reason = reason.to_string();
    let mut event = Event::new(
        event_type,
        subject.project.clone(),
        Throttle::Full,
        Event::serialize_payload(&WorkItemEventPayload::from_item(&snapshot))
            .expect("payload serializes"),
    );
    event.occurred_at = occurred_at;
    event.recorded_at = occurred_at;
    event.trace_id.clone_from(&subject.trace_id);
    event
}

fn at(year: i32, month: u32, day: u32, second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, 12, 0, second)
        .single()
        .expect("valid date")
}

struct Fixture {
    service: FoundryService,
    _root: TempDir,
    events_dir: PathBuf,
}

fn fixture(items: Vec<WorkItem>) -> Fixture {
    let root = tempfile::tempdir().expect("tempdir");
    let ledger = root.path().join("work-items.json");
    WorkItemStore { version: 1, items }.save(&ledger).expect("seed ledger");
    let events_dir = root.path().join("events");

    let (event_tx, _rx) = broadcast::channel(64);
    let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));
    let traces = root.path().join("traces");
    let trace_writer =
        Arc::new(TraceWriter::new(traces.to_str().expect("trace dir must be UTF-8")));
    let trace_store = Arc::new(TraceStore::with_trace_writer(
        Duration::from_secs(60),
        Arc::clone(&trace_writer),
    ));
    let ctx = RuntimeContext {
        engine,
        trace_store,
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer,
        event_tx,
        registry: Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        })),
    };
    let stores = StoreConfig {
        work_items_path: ledger,
        events_dir: events_dir.clone(),
        campaigns_path: PathBuf::new(),
        registry_path: PathBuf::new(),
        sentinels: Arc::new(RwLock::new(SentinelStore {
            version: 1,
            sentinels: vec![],
        })),
        sentinels_path: PathBuf::new(),
        scheduler_reload: Arc::new(Notify::new()),
    };
    Fixture {
        service: FoundryService::new(ctx, stores),
        _root: root,
        events_dir,
    }
}

fn write_all(events_dir: &Path, events: &[&Event]) {
    let writer = EventWriter::new(events_dir);
    for event in events {
        writer.write(event).expect("EventWriter writes the event");
    }
}

async fn events_of(service: &FoundryService, id: &str) -> Vec<WorkItemEvent> {
    service
        .list_work_item_events(Request::new(ListWorkItemEventsRequest { id: id.to_string() }))
        .await
        .expect("list_work_item_events should succeed")
        .into_inner()
        .events
}

fn ids_types_states(events: &[WorkItemEvent]) -> Vec<(String, String, String)> {
    events
        .iter()
        .map(|event| (event.id.clone(), event.event_type.clone(), event.state.clone()))
        .collect()
}

fn expected(events: &[(&Event, &str)]) -> Vec<(String, String, String)> {
    events
        .iter()
        .map(|(event, state)| (event.id.clone(), event.event_type.as_str(), (*state).to_string()))
        .collect()
}

// (a) ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_items_submitted_started_and_settled_events_come_back_in_order() {
    let task = item("wi_task", WorkItemKind::Task, WorkLane::Interactive);
    let fx = fixture(vec![task.clone()]);

    let submitted = lifecycle_event(
        &task,
        EventType::WorkItemSubmitted,
        WorkItemState::Submitted,
        "submitted",
        at(2026, 9, 29, 1),
    );
    let started = lifecycle_event(
        &task,
        EventType::WorkItemStarted,
        WorkItemState::Running,
        "agent started",
        at(2026, 9, 29, 2),
    );
    let settled = lifecycle_event(
        &task,
        EventType::WorkItemSettled,
        WorkItemState::Landed,
        "landed on main",
        at(2026, 9, 29, 3),
    );
    // Written out of chronological order: the RPC orders by occurred_at.
    write_all(&fx.events_dir, &[&started, &settled, &submitted]);

    let events = events_of(&fx.service, "wi_task").await;
    assert_eq!(
        ids_types_states(&events),
        expected(&[
            (&submitted, "submitted"),
            (&started, "running"),
            (&settled, "landed"),
        ])
    );
    assert_eq!(
        ids_types_states(&events).iter().map(|(_, t, _)| t.as_str()).collect::<Vec<_>>(),
        vec![
            "work_item_submitted",
            "work_item_started",
            "work_item_settled"
        ]
    );
    let last = events.last().expect("three events");
    assert_eq!(last.reason, "landed on main");
    assert_eq!(last.trace_id.as_deref(), Some(SHARED_TRACE));
    assert_eq!(last.occurred_at, at(2026, 9, 29, 3).to_rfc3339());
}

// (b) ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn two_items_sharing_one_trace_and_project_each_get_only_their_own_events() {
    let maintenance = item("wi_maint", WorkItemKind::Maintenance, WorkLane::Maintenance);
    let remediation = item("wi_remed", WorkItemKind::Remediation, WorkLane::Maintenance);
    let fx = fixture(vec![maintenance.clone(), remediation.clone()]);

    let m_submitted = lifecycle_event(
        &maintenance,
        EventType::WorkItemSubmitted,
        WorkItemState::Submitted,
        "submitted",
        at(2026, 9, 29, 1),
    );
    let r_submitted = lifecycle_event(
        &remediation,
        EventType::WorkItemSubmitted,
        WorkItemState::Submitted,
        "submitted",
        at(2026, 9, 29, 2),
    );
    let m_started = lifecycle_event(
        &maintenance,
        EventType::WorkItemStarted,
        WorkItemState::Running,
        "started",
        at(2026, 9, 29, 3),
    );
    let r_settled = lifecycle_event(
        &remediation,
        EventType::WorkItemSettled,
        WorkItemState::Failed,
        "remediation failed",
        at(2026, 9, 29, 4),
    );
    let m_settled = lifecycle_event(
        &maintenance,
        EventType::WorkItemSettled,
        WorkItemState::Landed,
        "maintained",
        at(2026, 9, 29, 5),
    );
    write_all(
        &fx.events_dir,
        &[
            &m_submitted,
            &r_submitted,
            &m_started,
            &r_settled,
            &m_settled,
        ],
    );

    assert_eq!(
        ids_types_states(&events_of(&fx.service, "wi_maint").await),
        expected(&[
            (&m_submitted, "submitted"),
            (&m_started, "running"),
            (&m_settled, "landed"),
        ])
    );
    assert_eq!(
        ids_types_states(&events_of(&fx.service, "wi_remed").await),
        expected(&[(&r_submitted, "submitted"), (&r_settled, "failed")])
    );
}

// (c) ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn an_items_events_are_found_in_a_monthly_file_far_older_than_the_prior_month() {
    let task = item("wi_old", WorkItemKind::Task, WorkLane::Interactive);
    let fx = fixture(vec![task.clone()]);

    let submitted = lifecycle_event(
        &task,
        EventType::WorkItemSubmitted,
        WorkItemState::Submitted,
        "submitted",
        at(2024, 1, 15, 1),
    );
    let settled = lifecycle_event(
        &task,
        EventType::WorkItemSettled,
        WorkItemState::Preserved,
        "work preserved",
        at(2024, 1, 15, 2),
    );
    write_all(&fx.events_dir, &[&submitted, &settled]);
    assert!(fx.events_dir.join("2024-01.jsonl").is_file(), "the writer routes by month");

    assert_eq!(
        ids_types_states(&events_of(&fx.service, "wi_old").await),
        expected(&[(&submitted, "submitted"), (&settled, "preserved")])
    );
}

// (d) ─────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn malformed_and_glued_lines_do_not_hide_well_formed_events_on_other_lines() {
    let task = item("wi_task", WorkItemKind::Task, WorkLane::Interactive);
    let fx = fixture(vec![task.clone()]);

    let submitted = lifecycle_event(
        &task,
        EventType::WorkItemSubmitted,
        WorkItemState::Submitted,
        "submitted",
        at(2026, 9, 29, 1),
    );
    let glued_a = lifecycle_event(
        &task,
        EventType::WorkItemStarted,
        WorkItemState::Running,
        "glued",
        at(2026, 9, 29, 2),
    );
    let glued_b = lifecycle_event(
        &task,
        EventType::WorkItemStarted,
        WorkItemState::Running,
        "glued too",
        at(2026, 9, 29, 3),
    );
    let settled = lifecycle_event(
        &task,
        EventType::WorkItemSettled,
        WorkItemState::Landed,
        "landed",
        at(2026, 9, 29, 4),
    );
    write_all(&fx.events_dir, &[&submitted]);
    let log = fx.events_dir.join("2026-09.jsonl");
    {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&log)
            .expect("open the writer's log");
        writeln!(file, "this is not json but names wi_task").expect("append junk");
        writeln!(
            file,
            "{}{}",
            serde_json::to_string(&glued_a).expect("serialize"),
            serde_json::to_string(&glued_b).expect("serialize")
        )
        .expect("append glued line");
    }
    write_all(&fx.events_dir, &[&settled]);

    assert_eq!(
        ids_types_states(&events_of(&fx.service, "wi_task").await),
        expected(&[(&submitted, "submitted"), (&settled, "landed")])
    );
}

#[tokio::test]
async fn an_unknown_item_id_is_not_found() {
    let fx = fixture(vec![item("wi_task", WorkItemKind::Task, WorkLane::Interactive)]);
    let err = fx
        .service
        .list_work_item_events(Request::new(ListWorkItemEventsRequest {
            id: "wi_missing".to_string(),
        }))
        .await
        .expect_err("an id absent from the ledger must fail");
    assert_eq!(err.code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn a_missing_events_directory_yields_the_record_and_an_empty_event_list() {
    let fx = fixture(vec![item("wi_task", WorkItemKind::Task, WorkLane::Interactive)]);
    assert!(!fx.events_dir.exists(), "precondition: no events directory");

    let record = fx
        .service
        .get_work_item(Request::new(GetWorkItemRequest {
            id: "wi_task".to_string(),
        }))
        .await
        .expect("get_work_item should succeed")
        .into_inner()
        .item
        .expect("record present");
    assert_eq!(record.id, "wi_task");
    assert!(events_of(&fx.service, "wi_task").await.is_empty());
    assert!(!fx.events_dir.exists(), "a read must not create the events directory");
}

#[tokio::test]
async fn an_unreadable_events_directory_is_internal_not_an_empty_list() {
    let fx = fixture(vec![item("wi_task", WorkItemKind::Task, WorkLane::Interactive)]);
    // A file where the directory should be cannot be listed.
    std::fs::write(&fx.events_dir, b"not a directory").expect("write");

    let err = fx
        .service
        .list_work_item_events(Request::new(ListWorkItemEventsRequest {
            id: "wi_task".to_string(),
        }))
        .await
        .expect_err("an unreadable log must not read as no events");
    assert_eq!(err.code(), tonic::Code::Internal);
}
