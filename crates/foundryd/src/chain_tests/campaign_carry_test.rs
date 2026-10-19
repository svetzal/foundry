//! Campaign cycles settle when their campaign moves past them, through the
//! real engine, admission, pacing scheduler and task chain.
use super::*;

use foundry_sdk::payload::{
    CampaignCancelledPayload, CampaignTerminalPayload, TaskRunCompletedPayload, TaskVerdict,
};
use foundry_sdk::work_item::{WorkDisposition, WorkItemOperatorAction};
use foundry_sdk::work_source::WorkSource;

const CAMPAIGN: &str = "transient-recovery";

/// An item of `test-project` that settled `state` an hour ago, preserving
/// work on its own branch.
fn open_item(id: &str, source: Option<WorkSource>, state: WorkItemState) -> WorkItem {
    let earlier = chrono::Utc::now() - chrono::Duration::hours(1);
    let mut item = WorkItem::dispatched(
        foundry_sdk::work_item::WorkItemSpec {
            project: "test-project".into(),
            objective: format!("objective of {id}"),
            kind: WorkItemKind::CampaignCycle,
            lane: WorkLane::Campaign,
            origin: "campaign".into(),
            trace_id: Some(mint_trace_id()),
        },
        earlier,
    )
    .with_source(source);
    item.id = id.into();
    item.state = state;
    item.reason = format!("{} by its run", state.tag());
    item.settled_at = Some(earlier);
    item.disposition = Some(WorkDisposition {
        task_branch: Some(format!("foundry-task/{id}")),
        branch_cleanup: Vec::new(),
        verdict: Some("remainder".into()),
        landed_commit: None,
        preservation_ref: Some(format!("foundry-task/{id}")),
        worktree: Some(format!("/worktrees/{id}")),
        worktree_removed: Some(true),
    });
    item
}

fn cycle(number: u64, state: WorkItemState) -> WorkItem {
    open_item(
        &format!("wi_cycle_{number}"),
        Some(WorkSource::campaign(CAMPAIGN, number)),
        state,
    )
}

fn carry_block(store_path: &Path, campaigns: &Path) -> Box<dyn foundry_sdk::task_block::TaskBlock> {
    Box::new(foundry_blocks::blocks::SettleCarriedCampaignWork::new(
        store_path.to_path_buf(),
        campaigns.to_path_buf(),
    ))
}

fn assert_cancelled_with(after: &WorkItem, before: &WorkItem, reason: &str) {
    assert_eq!(after.state, WorkItemState::Cancelled, "{}", before.id);
    assert_eq!(after.reason, reason, "{}", before.id);
    assert_eq!(after.disposition, before.disposition, "preservation evidence kept");
    assert_eq!(after.operator_action, None, "no person settled it");
    assert_eq!(after.source, before.source);
}

fn assert_cancellation_announced(events: &[Event], item: &WorkItem) {
    let event = events
        .iter()
        .find(|event| {
            event.event_type == EventType::WorkItemCancelled && event.payload["item_id"] == item.id
        })
        .unwrap_or_else(|| panic!("no work_item_cancelled for {}", item.id));
    assert_eq!(event.trace_id, item.trace_id, "on the cycle's own trace");
    assert_eq!(event.payload["state"], "cancelled");
    assert_eq!(event.payload["reason"], item.reason.as_str());
}

/// Every event in the durable log under `events_dir`, oldest file first.
pub(super) fn logged_events(events_dir: &Path) -> Vec<Event> {
    let mut files: Vec<_> = std::fs::read_dir(events_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    files.sort();
    files
        .iter()
        .flat_map(|file| {
            std::fs::read_to_string(file)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str::<Event>(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[tokio::test]
async fn a_dispatched_campaign_cycle_carries_the_campaigns_earlier_open_cycles() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = task_project(dir.path());
    let store_path = dir.path().join("work-items.json");
    let campaigns = dir.path().join("campaigns.json");
    let carried = [
        cycle(7, WorkItemState::Preserved),
        cycle(9, WorkItemState::NeedsDecision),
        cycle(10, WorkItemState::Failed),
        cycle(11, WorkItemState::Preserved),
    ];
    let mut owner_cancelled = cycle(8, WorkItemState::Preserved);
    owner_cancelled.operator_action = Some(WorkItemOperatorAction {
        command: "close".into(),
        origin: "owner host".into(),
        previous_state: WorkItemState::Preserved,
        previous_reason: owner_cancelled.reason.clone(),
        previous_settled_at: owner_cancelled.settled_at,
    });
    let evidence = owner_cancelled.disposition.clone();
    owner_cancelled.settle_cancelled("owner closed", evidence, chrono::Utc::now());
    let untouched = [
        owner_cancelled,
        open_item(
            "wi_other_campaign",
            Some(WorkSource::campaign("another-campaign", 3)),
            WorkItemState::Preserved,
        ),
        open_item("wi_operator", Some(WorkSource::operator("laptop")), WorkItemState::Preserved),
        open_item("wi_sourceless", None, WorkItemState::NeedsDecision),
    ];
    let mut seeded = carried.to_vec();
    seeded.extend(untouched.iter().cloned());
    WorkItemStore {
        version: 1,
        items: seeded,
    }
    .save(&store_path)
    .unwrap();
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let agent = LedgerReadingAgent::new(
        store_path.clone(),
        vec![
            "Done, implemented the cycle",
            "```json\n{\"verdict\":\"complete\"}\n```",
        ],
    );
    let mut engine = task_engine(agent.clone(), registry.clone(), &store_path);
    engine.register(carry_block(&store_path, &campaigns));
    let engine = Arc::new(engine);
    let root = task_dispatch(
        "test-project",
        "Recover from transient faults, cycle 12.",
        &serde_json::json!({"campaign": CAMPAIGN, "campaign_cycle": 12}),
    )
    .with_source(Some(WorkSource::campaign(CAMPAIGN, 12)));
    let trace = root.trace_id.clone();

    admit_and_run(&engine, &registry, &store_path, root).await;

    let store = WorkItemStore::load(&store_path).unwrap();
    let cycle_12 = store.items.iter().find(|item| item.trace_id == trace).unwrap();
    assert_eq!(cycle_12.state, WorkItemState::Landed);
    let reason = format!("carried forward by campaign {CAMPAIGN} cycle 12 ({})", cycle_12.id);
    // Settled at submission: the agent already saw them settled.
    let at_first_call = agent.ledger_at_first_invocation();
    for before in &carried {
        let after = store.find(&before.id).unwrap();
        assert_cancelled_with(after, before, &reason);
        // Durable admission does not route `work_item_submitted` to blocks;
        // the scheduler's `work_item_started` for cycle 12 is what carries.
        let logged = logged_events(&dir.path().join("events"));
        assert_cancellation_announced(&logged, after);
        let cancelled = logged
            .iter()
            .filter(|event| {
                event.event_type == EventType::WorkItemCancelled
                    && event.payload["item_id"] == before.id
            })
            .collect::<Vec<_>>();
        assert_eq!(cancelled.len(), 1, "{} is announced exactly once", before.id);
        let started = logged
            .iter()
            .find(|event| {
                event.event_type == EventType::WorkItemStarted
                    && event.payload["item_id"] == cycle_12.id
            })
            .expect("cycle 12 was started");
        assert_eq!(cancelled[0].causation_id.as_ref(), Some(&started.id));
        let seen = at_first_call.iter().find(|item| item.id == before.id).unwrap();
        assert_eq!(seen.state, WorkItemState::Cancelled, "{}", before.id);
    }
    for before in &untouched {
        assert_eq!(store.find(&before.id), Some(before), "{}", before.id);
    }
}

fn terminal_payload(reason: &str) -> CampaignTerminalPayload {
    CampaignTerminalPayload {
        campaign: CAMPAIGN.into(),
        project: "test-project".into(),
        reason: reason.into(),
        cycles_completed: 3,
        cycles_landed: 1,
    }
}

#[tokio::test]
async fn a_gracefully_cancelled_campaigns_in_flight_cycle_settles_when_it_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let campaigns = dir.path().join("campaigns.json");
    let earlier = cycle(2, WorkItemState::Preserved);
    let mut in_flight = cycle(3, WorkItemState::Running);
    in_flight.reason = "running".into();
    in_flight.settled_at = None;
    in_flight.disposition = None;
    WorkItemStore {
        version: 1,
        items: vec![earlier.clone(), in_flight.clone()],
    }
    .save(&store_path)
    .unwrap();
    // The cancel RPC records the campaign cancelled before it emits the event.
    let mut stopped: foundry_sdk::campaign::Campaign = serde_json::from_value(serde_json::json!({
        "name": CAMPAIGN, "project": "test-project", "mission": "recover",
        "done_evidence": [{"kind": "review", "statement": "recovered"}],
        "status": "active", "authorized_by": "owner"
    }))
    .unwrap();
    assert_eq!(
        stopped.cancel("owner stopped", chrono::Utc::now()).unwrap(),
        foundry_sdk::campaign::Transition::Applied
    );
    foundry_sdk::campaign::CampaignStore {
        findings: vec![],
        initiatives: vec![],
        version: 1,
        campaigns: vec![stopped],
    }
    .save(&campaigns)
    .unwrap();
    let mut engine = Engine::new();
    engine.register(Box::new(foundry_blocks::blocks::SettleWorkItem::new(store_path.clone())));
    engine.register(carry_block(&store_path, &campaigns));

    let cancellation = engine
        .process(Event::new(
            EventType::CampaignCancelled,
            "test-project".into(),
            Throttle::Full,
            Event::serialize_payload(&CampaignCancelledPayload {
                terminal: terminal_payload("owner stopped"),
                terminated_now: false,
                discard_work: false,
                aborted_event_id: None,
                aborted_trace_id: None,
            })
            .unwrap(),
        ))
        .await;
    let store = WorkItemStore::load(&store_path).unwrap();
    assert_eq!(store.find(&in_flight.id), Some(&in_flight), "a graceful cancel lets it finish");
    let carried = store.find(&earlier.id).unwrap();
    assert_cancelled_with(
        carried,
        &earlier,
        &format!("carried forward by campaign {CAMPAIGN} cycle 3 ({})", in_flight.id),
    );
    assert_cancellation_announced(&cancellation.events, carried);

    // The in-flight cycle finishes after CampaignCancelled, with preserved work.
    let finished = engine
        .process(
            Event::new(
                EventType::TaskRunCompleted,
                "test-project".into(),
                Throttle::Full,
                Event::serialize_payload(&TaskRunCompletedPayload {
                    project: "test-project".into(),
                    success: false,
                    landed: false,
                    summary: "remainder left".into(),
                    preservation_ref: Some("foundry-task/wi_cycle_3".into()),
                    land_blocked: None,
                    proof_evidence: None,
                    uncommitted_artifacts: None,
                    trunk_arrivals: Vec::new(),
                    verdict: TaskVerdict::Remainder {
                        gaps: vec!["one gap".into()],
                    },
                    context: foundry_sdk::payload::LoopContext::default(),
                    failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
                })
                .unwrap(),
            )
            .with_trace_id(in_flight.trace_id.clone()),
        )
        .await;

    let store = WorkItemStore::load(&store_path).unwrap();
    let settled = store.find(&in_flight.id).unwrap();
    assert_eq!(settled.state, WorkItemState::Cancelled, "not stranded open");
    assert_eq!(settled.reason, format!("campaign {CAMPAIGN} cancelled: owner stopped"));
    assert_eq!(
        settled.disposition.as_ref().unwrap().preservation_ref.as_deref(),
        Some("foundry-task/wi_cycle_3"),
        "the preserved work stays recorded"
    );
    assert_eq!(settled.operator_action, None);
    let types: Vec<_> = finished.events.iter().map(|event| &event.event_type).collect();
    assert!(types.contains(&&EventType::WorkItemSettled), "{types:?}");
    assert_cancellation_announced(&finished.events, settled);
}
