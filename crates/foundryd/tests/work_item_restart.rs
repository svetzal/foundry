//! Integration test for the daemon-start settlement of the work-item ledger.
//!
//! A `running` item means "an agent is working on this". The process holding
//! that agent does not survive a restart, so on start every still-`running`
//! item is settled `failed` with the reason `daemon restarted`, and a
//! `work_item_settled` event is recorded for each.
//!
//! This drives the real entry point `main` calls, with a broadcasting engine
//! and a durable event writer attached, so the event is observed exactly where
//! a Watch client and the JSONL log would see it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::Utc;
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_engine::event_writer::EventWriter;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::registry::Registry;
use foundry_sdk::work_item::{
    WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
};
use foundryd::{
    service::{RuntimeContext, settle_running_work_items_on_start},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tokio::sync::broadcast;

fn runtime_context(
    events_dir: &std::path::Path,
    traces_dir: &std::path::Path,
) -> (RuntimeContext, broadcast::Receiver<Event>) {
    let (event_tx, rx) = broadcast::channel(64);
    let engine = Arc::new(
        Engine::new()
            .with_event_broadcaster(event_tx.clone())
            .with_event_writer(Arc::new(EventWriter::new(events_dir.to_path_buf()))),
    );
    let trace_writer = Arc::new(TraceWriter::new(traces_dir.to_str().unwrap()));
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
    (ctx, rx)
}

fn running_item(project: &str) -> WorkItem {
    WorkItem::dispatched(
        WorkItemSpec {
            project: project.to_string(),
            objective: "Add a --quiet flag.".to_string(),
            kind: WorkItemKind::Task,
            lane: WorkLane::Interactive,
            origin: "foundry task".to_string(),
            trace_id: Some("a".repeat(32)),
        },
        Utc::now(),
    )
}

#[tokio::test]
async fn a_restart_settles_a_running_item_failed_and_records_the_event() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let events_dir = dir.path().join("events");
    let traces_dir = dir.path().join("traces");

    let mut store = WorkItemStore::default();
    let item = running_item("alpha");
    let item_id = item.id.clone();
    store.upsert(item);
    store.save(&store_path).unwrap();

    let (ctx, mut rx) = runtime_context(&events_dir, &traces_dir);
    settle_running_work_items_on_start(&ctx, &store_path).await;

    let reloaded = WorkItemStore::load(&store_path).unwrap();
    assert_eq!(reloaded.items.len(), 1);
    assert_eq!(reloaded.items[0].state, WorkItemState::Failed);
    assert_eq!(reloaded.items[0].reason, "daemon restarted");
    assert!(reloaded.items[0].settled_at.is_some());
    assert_eq!(reloaded.running().count(), 0);

    let mut settled = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if event.event_type == EventType::WorkItemSettled {
            settled.push(event);
        }
    }
    assert_eq!(settled.len(), 1, "one settled item, one work_item_settled event");
    assert_eq!(settled[0].payload["item_id"], item_id.as_str());
    assert_eq!(settled[0].payload["state"], "failed");
    assert_eq!(settled[0].payload["reason"], "daemon restarted");
    assert_eq!(settled[0].project, "alpha");

    let logged: String = std::fs::read_dir(&events_dir)
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    assert!(logged.contains("work_item_settled"));
    assert!(logged.contains(&item_id));
}

#[tokio::test]
async fn a_restart_with_nothing_running_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let events_dir = dir.path().join("events");
    let traces_dir = dir.path().join("traces");

    let mut store = WorkItemStore::default();
    let mut landed = running_item("alpha");
    landed.state = WorkItemState::Landed;
    store.upsert(landed);
    store.save(&store_path).unwrap();
    let before = std::fs::read(&store_path).unwrap();

    let (ctx, mut rx) = runtime_context(&events_dir, &traces_dir);
    settle_running_work_items_on_start(&ctx, &store_path).await;

    assert_eq!(std::fs::read(&store_path).unwrap(), before, "the ledger was not rewritten");
    assert!(rx.try_recv().is_err(), "no event for a ledger with nothing running");
}

#[tokio::test]
async fn a_missing_ledger_is_not_a_start_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (ctx, mut rx) = runtime_context(&dir.path().join("events"), &dir.path().join("traces"));
    settle_running_work_items_on_start(&ctx, &dir.path().join("absent.json")).await;
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn a_malformed_ledger_is_not_a_start_failure_and_is_left_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    std::fs::write(&store_path, "{ not json").unwrap();

    let (ctx, mut rx) = runtime_context(&dir.path().join("events"), &dir.path().join("traces"));
    settle_running_work_items_on_start(&ctx, &store_path).await;

    assert_eq!(std::fs::read_to_string(&store_path).unwrap(), "{ not json");
    assert!(rx.try_recv().is_err());
}
