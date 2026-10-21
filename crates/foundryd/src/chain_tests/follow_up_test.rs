//! A landing's remaining gaps become an open follow-up, and resuming it lands.
//!
//! A standalone task the reviewer lets land with a `remainder` verdict settles
//! `landed`; its gaps used to exist only in that run's event payloads. The
//! real `SettleWorkItem` block now opens a `follow_up` item holding them. The
//! owner resumes it through the service function, the real pacing scheduler
//! starts the admitted task from current trunk (recording dispatcher, pinned
//! clock), and that task's landing settles the follow-up through lineage.
use super::*;

use chrono::{DateTime, TimeZone as _, Utc};
use foundry_sdk::payload::{LoopContext, TaskRunCompletedPayload, TaskVerdict};
use foundry_sdk::work_item::{FollowUp, FollowUpObligation, WorkItemSpec};

use crate::pacing::PacingScheduler;

const PROJECT: &str = "test-project";

fn at(second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 20, 12, 0, second).single().expect("valid date")
}

/// A standalone task (or, with `campaign`, a campaign cycle) running now.
fn running(id: &str, campaign: Option<&str>) -> WorkItem {
    let (kind, lane) = match campaign {
        Some(_) => (WorkItemKind::CampaignCycle, WorkLane::Campaign),
        None => (WorkItemKind::Task, WorkLane::Interactive),
    };
    let mut item = WorkItem::dispatched(
        WorkItemSpec {
            project: PROJECT.into(),
            objective: "Add a --quiet flag.".into(),
            kind,
            lane,
            origin: "follow-up test".into(),
            trace_id: Some(mint_trace_id()),
        },
        at(0),
    );
    item.id = id.into();
    item
}

/// The task runner's terminal result for `item`'s run, on its trace.
fn terminal(item: &WorkItem, verdict: TaskVerdict, commit: &str, campaign: Option<&str>) -> Event {
    Event::new(
        EventType::TaskRunCompleted,
        item.project.clone(),
        Throttle::Full,
        Event::serialize_payload(&TaskRunCompletedPayload {
            project: item.project.clone(),
            success: verdict.is_complete(),
            landed: true,
            summary: format!("{} landed", item.id),
            preservation_ref: Some(commit.into()),
            land_blocked: None,
            proof_evidence: None,
            uncommitted_artifacts: None,
            trunk_arrivals: Vec::new(),
            verdict,
            context: LoopContext {
                campaign: campaign.map(str::to_string),
                ..LoopContext::default()
            },
            failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
        })
        .unwrap(),
    )
    .with_trace_id(item.trace_id.clone())
}

fn two_gaps() -> TaskVerdict {
    TaskVerdict::Remainder {
        gaps: vec!["Document the flag.".into(), "Cover it with a test.".into()],
    }
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

fn follow_ups(store: &WorkItemStore) -> Vec<&WorkItem> {
    store.items.iter().filter(|item| item.kind == WorkItemKind::FollowUp).collect()
}

/// `(event type, state)` of every lifecycle event naming `id`, in order.
fn lifecycle_of(events: &[Event], id: &str) -> Vec<(EventType, serde_json::Value)> {
    events
        .iter()
        .filter(|event| event.payload["item_id"] == id)
        .map(|event| (event.event_type.clone(), event.payload["state"].clone()))
        .collect()
}

#[tokio::test]
async fn a_standalone_landing_with_remaining_gaps_opens_a_follow_up_holding_them() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let task = running("wi_task", None);
    save(&store_path, vec![task.clone()]);
    let engine = settle_engine(dir.path(), &store_path);

    let result = engine.process(terminal(&task, two_gaps(), "landedcommit", None)).await;

    let store = WorkItemStore::load(&store_path).unwrap();
    let landed = store.find("wi_task").unwrap();
    assert_eq!(landed.state, WorkItemState::Landed);
    assert_eq!(
        landed.disposition.as_ref().unwrap().landed_commit.as_deref(),
        Some("landedcommit")
    );
    let opened = follow_ups(&store);
    assert_eq!(opened.len(), 1, "one follow-up");
    let follow_up = opened[0];
    assert_eq!(follow_up.state, WorkItemState::NeedsDecision);
    assert_eq!(follow_up.lane, WorkLane::Interactive);
    assert_eq!(follow_up.project, PROJECT);
    assert_eq!(follow_up.trace_id, task.trace_id, "on the landed task's trace");
    assert_eq!(follow_up.reason, "remainder of wi_task: 2 gap(s): Document the flag.");
    assert_eq!(
        follow_up.follow_up,
        Some(FollowUp {
            of: "wi_task".into(),
            commit: Some("landedcommit".into()),
            obligation: FollowUpObligation::Remainder {
                gaps: vec!["Document the flag.".into(), "Cover it with a test.".into()],
            },
        })
    );
    assert!(follow_up.objective.starts_with("Add a --quiet flag.\n\nRemaining gaps"));
    assert!(follow_up.objective.ends_with("- Document the flag.\n- Cover it with a test."));
    let announced = lifecycle_of(&result.events, &follow_up.id);
    assert_eq!(
        announced,
        vec![
            (EventType::WorkItemSubmitted, serde_json::json!("needs_decision")),
            (EventType::WorkItemSettled, serde_json::json!("needs_decision")),
        ]
    );
    for event in result.events.iter().filter(|event| event.payload["item_id"] == follow_up.id) {
        assert_eq!(event.trace_id, follow_up.trace_id, "on the follow-up's own trace");
        assert_eq!(event.payload["kind"], "follow_up");
        assert_eq!(event.payload["follow_up"]["gaps"][1], "Cover it with a test.");
    }
    let logged = super::campaign_carry_test::logged_events(&dir.path().join("events"));
    assert_eq!(lifecycle_of(&logged, &follow_up.id), announced, "both are logged");
}

#[tokio::test]
async fn a_campaign_cycle_landing_with_remaining_gaps_opens_no_follow_up() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let cycle = running("wi_cycle", Some("tidy-cli"));
    save(&store_path, vec![cycle.clone()]);
    let engine = settle_engine(dir.path(), &store_path);

    let result = engine
        .process(terminal(&cycle, two_gaps(), "landedcommit", Some("tidy-cli")))
        .await;

    let store = WorkItemStore::load(&store_path).unwrap();
    assert_eq!(store.find("wi_cycle").unwrap().state, WorkItemState::Landed);
    assert!(follow_ups(&store).is_empty(), "the campaign's formation carries the remainder");
    assert!(
        !result
            .events
            .iter()
            .any(|event| event.event_type == EventType::WorkItemSubmitted),
        "nothing new was submitted"
    );
}

#[tokio::test]
async fn resuming_a_follow_up_runs_a_task_from_trunk_whose_landing_settles_it() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let task = running("wi_task", None);
    save(&store_path, vec![task.clone()]);
    let engine = settle_engine(dir.path(), &store_path);
    engine.process(terminal(&task, two_gaps(), "landedcommit", None)).await;
    let follow_up = follow_ups(&WorkItemStore::load(&store_path).unwrap())[0].clone();
    let registry = test_helpers::registry_with_project(PROJECT, "/nonexistent/checkout");
    let ctx = runtime_context(Arc::clone(&engine), Arc::clone(&registry), dir.path());

    let admitted = crate::service::work_item_ops::resume_item(
        &store_path,
        &dir.path().join("campaigns.json"),
        &ctx,
        &pacing_paths_beside(&store_path),
        follow_up.id.clone(),
        "desk (follow-up test)".into(),
        crate::service::work_item_ops::ScheduleHints::default(),
    )
    .await
    .expect("an open follow-up can be resumed");

    let item = |id: &str| WorkItemStore::load(&store_path).unwrap().find(id).unwrap().clone();
    let child = item(&admitted.id);
    assert_eq!(child.kind, WorkItemKind::Task);
    assert_eq!(child.lane, WorkLane::Interactive);
    assert_eq!(child.state, WorkItemState::Queued);
    assert_eq!(child.objective, follow_up.objective);
    assert_eq!(child.continues, vec![follow_up.id.clone()]);
    assert_eq!(child.resumes, None);
    assert_eq!(
        child.source,
        Some(foundry_sdk::work_source::WorkSource::work_item(follow_up.id.clone()))
    );
    assert_eq!(child.operator_action.as_ref().unwrap().command, "resume");
    assert_eq!(item(&follow_up.id), follow_up, "the follow-up stays open until the task lands");

    let dispatched: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&dispatched);
    let scheduler = PacingScheduler::new(
        ctx,
        store_path.clone(),
        pacing_paths_beside(&store_path),
        Arc::new(tokio::sync::Notify::new()),
    )
    .with_dispatch(Arc::new(move |root: Event| recorder.lock().unwrap().push(root)))
    .with_clock(Arc::new(|| at(30)));
    scheduler.tick().await;

    let roots = dispatched.lock().unwrap().clone();
    assert_eq!(roots.len(), 1);
    let root = &roots[0];
    assert_eq!(root.payload[foundry_blocks::blocks::ADMITTED_WORK_ITEM_KEY], child.id);
    assert!(root.payload.get("base_ref").is_none(), "started from current trunk");
    assert_eq!(root.payload["continues"], serde_json::json!([follow_up.id]));
    assert_eq!(root.payload["prompt"], follow_up.objective);
    assert_eq!(item(&child.id).state, WorkItemState::Running);

    let result = engine
        .process(terminal(&item(&child.id), TaskVerdict::Complete, "secondcommit", None))
        .await;

    let store = WorkItemStore::load(&store_path).unwrap();
    assert_eq!(store.find(&child.id).unwrap().state, WorkItemState::Landed);
    let discharged = store.find(&follow_up.id).unwrap();
    assert_eq!(discharged.state, WorkItemState::Landed);
    assert_eq!(discharged.reason, format!("superseded by secondcommit via {}", child.id));
    assert_eq!(
        discharged.disposition.as_ref().unwrap().landed_commit.as_deref(),
        Some("secondcommit")
    );
    assert_eq!(discharged.follow_up, follow_up.follow_up, "the obligation stays on record");
    assert_eq!(
        lifecycle_of(&result.events, &follow_up.id),
        vec![(EventType::WorkItemSettled, serde_json::json!("landed"))]
    );
    assert_eq!(follow_ups(&store).len(), 1, "a complete landing owes nothing more");
}
