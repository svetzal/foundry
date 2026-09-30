//! Integration test for the daemon-start ending of interrupted agent sessions.
//!
//! An agent session is a child of the daemon; it dies when the daemon stops,
//! and nothing records its end. On start, every `agent_session_started` in the
//! lookback with no `agent_session_ended` for its `session_id` gets one, with
//! status `interrupted` and the error `daemon restarted`.
//!
//! This drives the real entry point `main` calls, with a broadcasting engine
//! and a durable event writer on the same events directory the sweep reads,
//! so a second start sees exactly what the first one wrote.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write as _;
use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::Utc;
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_engine::event_writer::EventWriter;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::registry::Registry;
use foundry_sdk::throttle::Throttle;
use foundryd::{
    service::{RuntimeContext, end_interrupted_agent_sessions_on_start},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tokio::sync::broadcast;

fn runtime_context(
    events_dir: &Path,
    traces_dir: &Path,
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

fn lifecycle(ty: EventType, session: &str, age: chrono::Duration) -> Event {
    let mut event = Event::new(
        ty,
        "bedrock".to_string(),
        Throttle::Full,
        serde_json::json!({ "session_id": session }),
    )
    .with_trace_id(Some("b".repeat(32)));
    event.occurred_at = Utc::now() - age;
    event.recorded_at = event.occurred_at;
    event
}

fn log(events_dir: &Path, events: &[Event]) {
    let writer = EventWriter::new(events_dir.to_path_buf());
    for event in events {
        writer.write(event).unwrap();
    }
}

fn ended_events(rx: &mut broadcast::Receiver<Event>) -> Vec<Event> {
    let mut ended = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if event.event_type == EventType::AgentSessionEnded {
            ended.push(event);
        }
    }
    ended
}

#[tokio::test]
async fn a_started_session_with_no_end_gets_exactly_one_interrupted_end() {
    let dir = tempfile::tempdir().unwrap();
    let events_dir = dir.path().join("events");
    let started = lifecycle(EventType::AgentSessionStarted, "s1", chrono::Duration::hours(1));
    log(&events_dir, std::slice::from_ref(&started));

    let (ctx, mut rx) = runtime_context(&events_dir, &dir.path().join("traces"));
    let before = Utc::now();
    end_interrupted_agent_sessions_on_start(&ctx, &events_dir).await;

    let ended = ended_events(&mut rx);
    assert_eq!(ended.len(), 1, "one interrupted session, one end");
    let end = &ended[0];
    assert_eq!(end.project, "bedrock");
    assert_eq!(end.trace_id, started.trace_id);
    assert_eq!(end.payload["session_id"], "s1");
    assert_eq!(end.payload["status"], "interrupted");
    assert_eq!(end.payload["error"], "daemon restarted");
    let ended_at: chrono::DateTime<Utc> =
        end.payload["ended_at"].as_str().unwrap().parse().unwrap();
    assert!(ended_at >= before - chrono::Duration::seconds(1), "ended at the daemon start");

    let logged: String = std::fs::read_dir(&events_dir)
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    assert!(logged.contains("\"interrupted\""), "the end is persisted to the event log");
}

#[tokio::test]
async fn a_properly_ended_session_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let events_dir = dir.path().join("events");
    log(
        &events_dir,
        &[
            lifecycle(EventType::AgentSessionStarted, "s1", chrono::Duration::hours(2)),
            lifecycle(EventType::AgentSessionEnded, "s1", chrono::Duration::hours(1)),
        ],
    );

    let (ctx, mut rx) = runtime_context(&events_dir, &dir.path().join("traces"));
    end_interrupted_agent_sessions_on_start(&ctx, &events_dir).await;

    assert!(ended_events(&mut rx).is_empty());
}

#[tokio::test]
async fn a_second_start_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let events_dir = dir.path().join("events");
    let traces_dir = dir.path().join("traces");
    log(
        &events_dir,
        &[lifecycle(
            EventType::AgentSessionStarted,
            "s1",
            chrono::Duration::hours(1),
        )],
    );

    let (first, mut first_rx) = runtime_context(&events_dir, &traces_dir);
    end_interrupted_agent_sessions_on_start(&first, &events_dir).await;
    assert_eq!(ended_events(&mut first_rx).len(), 1);

    let (second, mut second_rx) = runtime_context(&events_dir, &traces_dir);
    end_interrupted_agent_sessions_on_start(&second, &events_dir).await;
    assert!(ended_events(&mut second_rx).is_empty(), "the first start's end is in the log");
}

#[tokio::test]
async fn an_unparseable_log_line_is_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let events_dir = dir.path().join("events");
    let started = lifecycle(EventType::AgentSessionStarted, "s1", chrono::Duration::hours(1));
    log(&events_dir, std::slice::from_ref(&started));
    let month_file = events_dir.join(format!("{}.jsonl", started.occurred_at.format("%Y-%m")));
    let mut file = std::fs::OpenOptions::new().append(true).open(month_file).unwrap();
    writeln!(file, "{{\"id\": \"half a line").unwrap();
    drop(file);

    let (ctx, mut rx) = runtime_context(&events_dir, &dir.path().join("traces"));
    end_interrupted_agent_sessions_on_start(&ctx, &events_dir).await;

    let ended = ended_events(&mut rx);
    assert_eq!(ended.len(), 1, "the good line still closes its session");
    assert_eq!(ended[0].payload["session_id"], "s1");
}

#[tokio::test]
async fn a_session_older_than_the_lookback_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let events_dir = dir.path().join("events");
    log(
        &events_dir,
        &[lifecycle(
            EventType::AgentSessionStarted,
            "old",
            chrono::Duration::days(8),
        )],
    );

    let (ctx, mut rx) = runtime_context(&events_dir, &dir.path().join("traces"));
    end_interrupted_agent_sessions_on_start(&ctx, &events_dir).await;

    assert!(ended_events(&mut rx).is_empty());
}

#[tokio::test]
async fn a_missing_events_directory_is_not_a_start_failure() {
    let dir = tempfile::tempdir().unwrap();
    let events_dir = dir.path().join("events");

    let (ctx, mut rx) = runtime_context(&events_dir, &dir.path().join("traces"));
    end_interrupted_agent_sessions_on_start(&ctx, &events_dir).await;

    assert!(ended_events(&mut rx).is_empty());
}
