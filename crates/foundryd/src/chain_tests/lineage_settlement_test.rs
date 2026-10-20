//! Landing discharges the whole lineage, and dependents re-evaluate.
//!
//! A landing settles what the landed task resumed or continued, then what
//! each of those resumed or continued, and so on. A dependent the scheduler
//! moved to `needs_decision` because its dependency settled preserved returns
//! to the queue once that dependency's resumed work lands. Both run the real
//! `SettleWorkItem` block in an engine and, for dependents, the real pacing
//! scheduler with a recording dispatcher and a pinned clock.
use super::*;

use chrono::{DateTime, TimeZone as _, Utc};
use foundry_sdk::payload::{LoopContext, TaskRunCompletedPayload, TaskVerdict};
use foundry_sdk::work_item::{WorkDisposition, WorkItemSpec};

use crate::pacing::PacingScheduler;

fn at(second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 20, 12, 0, second).single().expect("valid date")
}

fn spec(project: &str, id: &str) -> WorkItemSpec {
    WorkItemSpec {
        project: project.into(),
        objective: format!("objective of {id}"),
        kind: WorkItemKind::Task,
        lane: WorkLane::Interactive,
        origin: "lineage test".into(),
        trace_id: Some(mint_trace_id()),
    }
}

/// An item of `project` that settled `state`, with its work preserved on a ref.
fn settled(project: &str, id: &str, state: WorkItemState) -> WorkItem {
    let mut item = WorkItem::dispatched(spec(project, id), at(0));
    item.id = id.into();
    item.state = state;
    item.reason = format!("{} by its run", state.tag());
    item.settled_at = Some(at(1));
    item.execution_completed = true;
    item.disposition = Some(WorkDisposition {
        verdict: Some("remainder".into()),
        preservation_ref: Some(format!("foundry-task/{id}")),
        ..WorkDisposition::default()
    });
    item
}

/// A `queued` task of `project`, holding the root the scheduler starts it from.
fn queued(project: &str, id: &str, second: u32) -> WorkItem {
    let spec = spec(project, id);
    let root = Event::new(
        EventType::ExecutionRequested,
        project.into(),
        Throttle::Full,
        serde_json::json!({"project": project, "workflow": "task", "prompt": "x"}),
    )
    .with_trace_id(spec.trace_id.clone());
    let mut item = WorkItem::queued(spec, root, at(second));
    item.id = id.into();
    item
}

/// The task runner's terminal result for `item`'s run, on its trace.
fn terminal(item: &WorkItem, verdict: TaskVerdict, landed: bool, reference: &str) -> Event {
    Event::new(
        EventType::TaskRunCompleted,
        item.project.clone(),
        Throttle::Full,
        Event::serialize_payload(&TaskRunCompletedPayload {
            project: item.project.clone(),
            success: verdict.is_complete(),
            landed,
            summary: format!("{} finished", item.id),
            preservation_ref: Some(reference.into()),
            land_blocked: None,
            proof_evidence: None,
            uncommitted_artifacts: None,
            trunk_arrivals: Vec::new(),
            verdict,
            context: LoopContext::default(),
            failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
        })
        .unwrap(),
    )
    .with_trace_id(item.trace_id.clone())
}

/// An engine with the real settlement block, logging every event under `dir`.
fn settle_engine(dir: &Path, store_path: &Path) -> Arc<Engine> {
    let mut engine = Engine::new().with_event_writer(Arc::new(
        foundry_engine::event_writer::EventWriter::new(dir.join("events")),
    ));
    engine
        .register(Box::new(foundry_blocks::blocks::SettleWorkItem::new(store_path.to_path_buf())));
    Arc::new(engine)
}

fn save(store_path: &Path, items: Vec<WorkItem>) {
    WorkItemStore { version: 1, items }.save(store_path).unwrap();
}

fn settled_events_for<'a>(events: &'a [Event], id: &str) -> Vec<&'a Event> {
    events
        .iter()
        .filter(|event| {
            event.event_type == EventType::WorkItemSettled && event.payload["item_id"] == id
        })
        .collect()
}

#[tokio::test]
async fn a_landed_resume_of_a_follow_up_settles_what_the_follow_up_continued() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    // A was continued by B (`--continues A`); B was preserved and resumed as C.
    let a = settled("test-project", "wi_a", WorkItemState::Failed);
    let mut b = settled("test-project", "wi_b", WorkItemState::Preserved);
    b.continues = vec!["wi_a".into()];
    let mut c = WorkItem::dispatched(spec("test-project", "wi_c"), at(2));
    c.id = "wi_c".into();
    c.resumes = Some("wi_b".into());
    save(&store_path, vec![a.clone(), b.clone(), c.clone()]);
    let engine = settle_engine(dir.path(), &store_path);

    let result = engine.process(terminal(&c, TaskVerdict::Complete, true, "landedcommit")).await;

    let store = WorkItemStore::load(&store_path).unwrap();
    for (id, reason) in [
        ("wi_c", "wi_c finished"),
        ("wi_b", "resumed work landed"),
        ("wi_a", "superseded by landedcommit via wi_b"),
    ] {
        let item = store.find(id).unwrap();
        assert_eq!(item.state, WorkItemState::Landed, "{id}: {}", item.reason);
        assert_eq!(item.reason, reason, "{id}");
        assert_eq!(
            item.disposition.as_ref().unwrap().landed_commit.as_deref(),
            Some("landedcommit"),
            "{id}"
        );
        let announced = settled_events_for(&result.events, id);
        assert_eq!(announced.len(), 1, "one work_item_settled for {id}");
        assert_eq!(announced[0].payload["state"], "landed");
        assert_eq!(announced[0].payload["reason"], reason);
        assert_eq!(announced[0].trace_id, item.trace_id, "on {id}'s own trace");
    }
    assert_eq!(
        store.find("wi_a").unwrap().disposition.as_ref().unwrap().preservation_ref,
        a.disposition.as_ref().unwrap().preservation_ref,
        "preservation evidence is kept"
    );
    let logged = super::campaign_carry_test::logged_events(&dir.path().join("events"));
    for id in ["wi_a", "wi_b"] {
        assert_eq!(settled_events_for(&logged, id).len(), 1, "{id} settlement is logged");
    }
}

#[tokio::test]
async fn a_dependent_held_on_a_preserved_dependency_starts_once_its_resumed_work_lands() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let mut d = queued("beta", "wi_d", 2);
    d.depends_on = vec!["wi_x".into()];
    save(&store_path, vec![queued("alpha", "wi_x", 1), d]);
    let engine = settle_engine(dir.path(), &store_path);
    let registry = Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![],
    }));
    let dispatched: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&dispatched);
    let scheduler = PacingScheduler::new(
        runtime_context(Arc::clone(&engine), registry, dir.path()),
        store_path.clone(),
        pacing_paths_beside(&store_path),
        Arc::new(tokio::sync::Notify::new()),
    )
    .with_dispatch(Arc::new(move |root: Event| {
        recorder.lock().unwrap().push(
            root.payload[foundry_blocks::blocks::ADMITTED_WORK_ITEM_KEY]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }))
    .with_clock(Arc::new(|| at(30)));
    let item = |id: &str| WorkItemStore::load(&store_path).unwrap().find(id).unwrap().clone();
    // What the admission owner records once a started traversal, cleanup
    // included, has returned: the item stops holding a slot.
    let traversal_ended = |id: &str| {
        let mut store = WorkItemStore::load(&store_path).unwrap();
        store.find_mut(id).unwrap().execution_completed = true;
        store.save(&store_path).unwrap();
    };

    // X starts; D waits on it. X's run ends preserved; D needs a decision.
    scheduler.tick().await;
    assert_eq!(item("wi_d").reason, "waits on wi_x");
    let x = item("wi_x");
    engine
        .process(terminal(
            &x,
            TaskVerdict::Remainder {
                gaps: vec!["one gap".into()],
            },
            false,
            "foundry-task/wi_x",
        ))
        .await;
    assert_eq!(item("wi_x").state, WorkItemState::Preserved);
    traversal_ended("wi_x");
    scheduler.tick().await;
    assert_eq!(item("wi_d").state, WorkItemState::NeedsDecision);
    assert_eq!(item("wi_d").reason, "waits on wi_x, which settled preserved");

    // The owner resumes X as Y; Y starts while X is still preserved.
    let mut y = queued("alpha", "wi_y", 3);
    y.resumes = Some("wi_x".into());
    let mut store = WorkItemStore::load(&store_path).unwrap();
    store.upsert(y);
    store.save(&store_path).unwrap();
    let outcome = scheduler.tick().await;
    assert!(outcome.released.is_empty(), "X is still preserved");
    assert_eq!(*dispatched.lock().unwrap(), vec!["wi_x", "wi_y"]);

    // Y lands, which settles X landed.
    engine
        .process(terminal(&item("wi_y"), TaskVerdict::Complete, true, "landedcommit"))
        .await;
    traversal_ended("wi_y");
    assert_eq!(item("wi_x").state, WorkItemState::Landed);
    assert_eq!(item("wi_x").reason, "resumed work landed");
    assert_eq!(item("wi_d").state, WorkItemState::NeedsDecision, "until the next tick");

    let outcome = scheduler.tick().await;

    assert_eq!(outcome.released.len(), 1);
    assert_eq!(outcome.released[0].id, "wi_d");
    assert_eq!(outcome.released[0].state, WorkItemState::Queued);
    assert_eq!(outcome.released[0].reason, foundry_sdk::pacing::READY_REASON);
    assert_eq!(*dispatched.lock().unwrap(), vec!["wi_x", "wi_y", "wi_d"]);
    let d = item("wi_d");
    assert_eq!(d.state, WorkItemState::Running);
    assert_eq!(d.operator_action, None, "the scheduler's own decision");
    let logged = super::campaign_carry_test::logged_events(&dir.path().join("events"));
    let lifecycle: Vec<_> = logged
        .iter()
        .filter(|event| event.payload["item_id"] == "wi_d")
        .map(|event| (event.event_type.clone(), event.payload["state"].clone()))
        .collect();
    assert_eq!(
        lifecycle[lifecycle.len() - 2..],
        [
            (EventType::WorkItemReleased, serde_json::json!("queued")),
            (EventType::WorkItemStarted, serde_json::json!("running")),
        ]
    );
}
