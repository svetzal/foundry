use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use foundry_sdk::campaign::{
    Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence, Transition,
};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    CampaignCancelledPayload, CampaignTerminalPayload, WorkItemEventPayload,
};
use foundry_sdk::task_block::TaskBlock;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{
    WorkDisposition, WorkItem, WorkItemKind, WorkItemOperatorAction, WorkItemSpec, WorkItemState,
    WorkItemStore, WorkLane,
};
use foundry_sdk::work_source::WorkSource;

use super::{SettleCarriedCampaignWork, sweep_for};

const PROJECT: &str = "alpha";
const CAMPAIGN: &str = "transient-recovery";

fn start() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-10T00:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

/// A work item submitted `minutes` after `start()`, in `state`, preserving
/// `ref-<id>` when open.
fn item(id: &str, source: Option<WorkSource>, state: WorkItemState, minutes: i64) -> WorkItem {
    let mut item = WorkItem::dispatched(
        WorkItemSpec {
            project: PROJECT.into(),
            objective: format!("objective of {id}"),
            kind: if source.as_ref().is_some_and(|s| s.cycle.is_some()) {
                WorkItemKind::CampaignCycle
            } else {
                WorkItemKind::Task
            },
            lane: WorkLane::Campaign,
            origin: "campaign".into(),
            trace_id: Some(format!("{minutes:0>32}")),
        },
        start() + Duration::minutes(minutes),
    )
    .with_source(source);
    item.id = id.into();
    item.state = state;
    if state.is_open() {
        item.reason = format!("{} by its run", state.tag());
        item.settled_at = Some(start() + Duration::minutes(minutes + 1));
        item.disposition = Some(WorkDisposition {
            task_branch: Some(format!("foundry-task/{id}")),
            branch_cleanup: Vec::new(),
            verdict: Some("remainder".into()),
            landed_commit: None,
            preservation_ref: Some(format!("foundry-task/{id}")),
            worktree: Some(format!("/worktrees/{id}")),
            worktree_removed: Some(true),
        });
    }
    item
}

fn cycle(id: &str, number: u64, state: WorkItemState, minutes: i64) -> WorkItem {
    item(id, Some(WorkSource::campaign(CAMPAIGN, number)), state, minutes)
}

fn campaign(status: CampaignStatus) -> Campaign {
    Campaign {
        name: CAMPAIGN.into(),
        project: PROJECT.into(),
        mission: "recover from transient faults".into(),
        intent_refs: vec![],
        context_paths: vec![],
        done_evidence: vec![DoneEvidence::Review {
            statement: "recovered".into(),
        }],
        budget: CampaignBudget::default(),
        escalation: vec![],
        status,
        cycles_completed: 0,
        cycles_landed: 0,
        authorized_by: Some("owner".into()),
        agent_provider: None,
        last_run_event_id: None,
        owner_decisions: vec![],
        pending_run_result: None,
        objective_history: vec![],
        writable_repositories: vec![],
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    ledger: PathBuf,
    campaigns: PathBuf,
}

impl Fixture {
    fn new(items: &[WorkItem], campaign: Campaign) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("work-items.json");
        let campaigns = dir.path().join("campaigns.json");
        WorkItemStore {
            version: 1,
            items: items.to_vec(),
        }
        .save(&ledger)
        .unwrap();
        CampaignStore {
            version: 1,
            campaigns: vec![campaign],
        }
        .save(&campaigns)
        .unwrap();
        Self {
            _dir: dir,
            ledger,
            campaigns,
        }
    }

    fn block(&self) -> SettleCarriedCampaignWork {
        SettleCarriedCampaignWork::new(self.ledger.clone(), self.campaigns.clone())
    }

    fn ledger(&self) -> WorkItemStore {
        WorkItemStore::load(&self.ledger).unwrap()
    }

    fn write_item(&self, item: &WorkItem) {
        let mut store = self.ledger();
        store.upsert(item.clone());
        store.save(&self.ledger).unwrap();
    }

    fn set_campaign(&self, campaign: Campaign) {
        CampaignStore {
            version: 1,
            campaigns: vec![campaign],
        }
        .save(&self.campaigns)
        .unwrap();
    }

    async fn run(&self, trigger: &Event) -> Vec<Event> {
        let block = self.block();
        assert!(block.accepts(trigger), "{:?} should be accepted", trigger.event_type);
        block.execute(trigger).await.unwrap().events
    }
}

fn lifecycle(event_type: EventType, item: &WorkItem) -> Event {
    Event::new(
        event_type,
        item.project.clone(),
        Throttle::Full,
        Event::serialize_payload(&WorkItemEventPayload::from_item(item)).unwrap(),
    )
    .with_trace_id(item.trace_id.clone())
}

fn terminal(reason: &str) -> CampaignTerminalPayload {
    CampaignTerminalPayload {
        campaign: CAMPAIGN.into(),
        project: PROJECT.into(),
        reason: reason.into(),
        cycles_completed: 12,
        cycles_landed: 3,
    }
}

fn completed(reason: &str) -> Event {
    Event::new(
        EventType::CampaignCompleted,
        PROJECT.into(),
        Throttle::Full,
        Event::serialize_payload(&terminal(reason)).unwrap(),
    )
}

fn cancelled(reason: &str, terminated_now: bool) -> Event {
    Event::new(
        EventType::CampaignCancelled,
        PROJECT.into(),
        Throttle::Full,
        Event::serialize_payload(&CampaignCancelledPayload {
            terminal: terminal(reason),
            terminated_now,
            discard_work: false,
            aborted_event_id: None,
            aborted_trace_id: None,
        })
        .unwrap(),
    )
}

/// `before`, settled `cancelled` with `reason` and nothing else changed.
fn assert_settled(after: &WorkItem, before: &WorkItem, reason: &str) {
    assert_eq!(after.state, WorkItemState::Cancelled, "{}", before.id);
    assert_eq!(after.reason, reason, "{}", before.id);
    assert!(after.settled_at.is_some());
    assert_eq!(after.disposition, before.disposition, "preservation evidence kept");
    assert_eq!(after.operator_action, None, "no person did this");
    let mut expected = before.clone();
    expected.state = after.state;
    expected.reason = after.reason.clone();
    expected.settled_at = after.settled_at;
    assert_eq!(after, &expected, "{}", before.id);
}

fn assert_announced(events: &[Event], settled: &[&WorkItem], ledger: &WorkItemStore) {
    assert_eq!(events.len(), settled.len());
    for (event, before) in events.iter().zip(settled) {
        let after = ledger.find(&before.id).unwrap();
        assert_eq!(event.event_type, EventType::WorkItemCancelled);
        assert_eq!(event.trace_id, before.trace_id, "on the cycle's own trace");
        assert_eq!(event.project, before.project);
        assert_eq!(
            event.parse_payload::<WorkItemEventPayload>().unwrap(),
            WorkItemEventPayload::from_item(after)
        );
    }
}

#[tokio::test]
async fn a_submitted_cycle_carries_every_earlier_open_cycle_of_its_campaign() {
    let carried = [
        cycle("wi_7", 7, WorkItemState::Preserved, 70),
        cycle("wi_9", 9, WorkItemState::NeedsDecision, 90),
        cycle("wi_10", 10, WorkItemState::Failed, 100),
        cycle("wi_11", 11, WorkItemState::Preserved, 110),
    ];
    let mut owner_cancelled = cycle("wi_8", 8, WorkItemState::Preserved, 80);
    owner_cancelled.operator_action = Some(WorkItemOperatorAction {
        command: "close".into(),
        origin: "owner host".into(),
        previous_state: WorkItemState::Preserved,
        previous_reason: owner_cancelled.reason.clone(),
        previous_settled_at: owner_cancelled.settled_at,
    });
    let evidence = owner_cancelled.disposition.clone();
    owner_cancelled.settle_cancelled("owner closed", evidence, start());
    let landed = cycle("wi_6", 6, WorkItemState::Landed, 60);
    // Durable admission records the campaign's formation workflow with the
    // campaign's source; it is not a cycle, so it neither carries nor is carried.
    let mut formation = cycle("wi_formation", 11, WorkItemState::Failed, 115);
    formation.kind = WorkItemKind::Workflow;
    let untouched = [
        owner_cancelled,
        landed,
        formation,
        item(
            "wi_other_campaign",
            Some(WorkSource::campaign("other", 2)),
            WorkItemState::Preserved,
            50,
        ),
        item(
            "wi_operator",
            Some(WorkSource::operator("laptop")),
            WorkItemState::Preserved,
            51,
        ),
        item("wi_sourceless", None, WorkItemState::NeedsDecision, 52),
    ];
    let submitted = cycle("wi_12", 12, WorkItemState::Queued, 120);
    let mut items = carried.to_vec();
    items.extend(untouched.iter().cloned());
    items.push(submitted.clone());
    let fixture = Fixture::new(&items, campaign(CampaignStatus::Active));

    let events = fixture.run(&lifecycle(EventType::WorkItemSubmitted, &submitted)).await;

    let ledger = fixture.ledger();
    for before in &carried {
        assert_settled(
            ledger.find(&before.id).unwrap(),
            before,
            "carried forward by campaign transient-recovery cycle 12 (wi_12)",
        );
    }
    for before in untouched.iter().chain([&submitted]) {
        assert_eq!(ledger.find(&before.id), Some(before), "{}", before.id);
    }
    assert_announced(&events, &carried.iter().collect::<Vec<_>>(), &ledger);
    // A repeated trigger settles nothing more.
    let bytes = std::fs::read(&fixture.ledger).unwrap();
    assert!(fixture.run(&lifecycle(EventType::WorkItemStarted, &submitted)).await.is_empty());
    assert_eq!(std::fs::read(&fixture.ledger).unwrap(), bytes);
}

#[tokio::test]
async fn an_escalated_cycle_stays_open_until_the_campaign_advances_again() {
    let earlier = cycle("wi_3", 3, WorkItemState::Preserved, 30);
    let escalated = cycle("wi_4", 4, WorkItemState::NeedsDecision, 40);
    let fixture =
        Fixture::new(&[earlier.clone(), escalated.clone()], campaign(CampaignStatus::Escalated));

    // The escalated cycle settling carries its predecessor, not itself.
    let events = fixture.run(&lifecycle(EventType::WorkItemSettled, &escalated)).await;
    assert_eq!(events.len(), 1);
    assert_eq!(fixture.ledger().find(&escalated.id), Some(&escalated));

    // The owner decides; the campaign is live again and the question stands.
    fixture.set_campaign(campaign(CampaignStatus::Active));
    assert!(fixture.run(&lifecycle(EventType::WorkItemSettled, &escalated)).await.is_empty());
    assert_eq!(fixture.ledger().find(&escalated.id), Some(&escalated));

    // The next advance submits cycle 5, which carries the escalated cycle.
    let next = cycle("wi_5", 5, WorkItemState::Queued, 50);
    fixture.write_item(&next);
    let events = fixture.run(&lifecycle(EventType::WorkItemSubmitted, &next)).await;
    let ledger = fixture.ledger();
    assert_settled(
        ledger.find(&escalated.id).unwrap(),
        &escalated,
        "carried forward by campaign transient-recovery cycle 5 (wi_5)",
    );
    assert_announced(&events, &[&escalated], &ledger);
}

#[tokio::test]
async fn completion_settles_every_remaining_open_cycle_with_its_reason() {
    let open = [
        cycle("wi_1", 1, WorkItemState::Failed, 10),
        cycle("wi_2", 2, WorkItemState::Preserved, 20),
    ];
    let mut done = campaign(CampaignStatus::Active);
    assert_eq!(done.complete("done evidence met", start()).unwrap(), Transition::Applied);
    let fixture = Fixture::new(&open, done);

    let events = fixture.run(&completed("done evidence met")).await;

    let ledger = fixture.ledger();
    assert_settled(
        ledger.find("wi_1").unwrap(),
        &open[0],
        "carried forward by campaign transient-recovery cycle 2 (wi_2)",
    );
    assert_settled(
        ledger.find("wi_2").unwrap(),
        &open[1],
        "campaign transient-recovery completed: done evidence met",
    );
    assert_announced(&events, &[&open[0], &open[1]], &ledger);
}

#[tokio::test]
async fn a_graceful_cancel_settles_the_in_flight_cycle_when_it_finishes() {
    let open = cycle("wi_1", 1, WorkItemState::NeedsDecision, 10);
    let in_flight = cycle("wi_2", 2, WorkItemState::Running, 20);
    let fixture =
        Fixture::new(&[open.clone(), in_flight.clone()], campaign(CampaignStatus::Active));
    let mut stopped = campaign(CampaignStatus::Active);
    assert_eq!(stopped.cancel("owner stopped", start()).unwrap(), Transition::Applied);
    fixture.set_campaign(stopped);

    // The cancellation carries the open cycle; the running one is left to finish.
    let events = fixture.run(&cancelled("owner stopped", false)).await;
    assert_eq!(events.len(), 1);
    assert_eq!(fixture.ledger().find(&in_flight.id), Some(&in_flight));

    // It finishes after CampaignCancelled, preserved, and is not stranded.
    let mut finished = in_flight.clone();
    finished.state = WorkItemState::Preserved;
    finished.reason = "remainder".into();
    finished.disposition = cycle("wi_2", 2, WorkItemState::Preserved, 20).disposition;
    fixture.write_item(&finished);
    let events = fixture.run(&lifecycle(EventType::WorkItemSettled, &finished)).await;

    let ledger = fixture.ledger();
    assert_settled(
        ledger.find(&finished.id).unwrap(),
        &finished,
        "campaign transient-recovery cancelled: owner stopped",
    );
    assert_announced(&events, &[&finished], &ledger);
}

#[tokio::test]
async fn an_owner_cancelled_carrier_carries_nothing() {
    let open = cycle("wi_1", 1, WorkItemState::Preserved, 10);
    let mut stopped_cycle = cycle("wi_2", 2, WorkItemState::Queued, 20);
    stopped_cycle.settle_cancelled("cancelled by operator", None, start());
    let fixture =
        Fixture::new(&[open.clone(), stopped_cycle.clone()], campaign(CampaignStatus::Active));

    assert!(fixture.run(&lifecycle(EventType::WorkItemSettled, &open)).await.is_empty());

    assert_eq!(fixture.ledger().find(&open.id), Some(&open));
}

#[test]
fn a_concurrent_owner_action_wins_because_the_decision_is_taken_under_the_gate() {
    // The decision reads the ledger it writes, under the write gate. An owner
    // close that lands before the gate is taken is what the sweep sees.
    let mut closed = cycle("wi_1", 1, WorkItemState::Preserved, 10);
    let next = cycle("wi_2", 2, WorkItemState::Queued, 20);
    let mut store = WorkItemStore {
        version: 1,
        items: vec![closed.clone(), next],
    };
    let evidence = closed.disposition.clone();
    closed.settle_cancelled("owner closed", evidence, start());
    store.upsert(closed.clone());

    let settled = super::settle_campaign_cycles(&mut store, CAMPAIGN, PROJECT, Some("x"), start());

    assert!(settled.is_empty());
    assert_eq!(store.find("wi_1"), Some(&closed));
}

#[test]
fn only_campaign_cycle_lifecycle_and_campaign_terminal_events_are_accepted() {
    let fixture = Fixture::new(&[], campaign(CampaignStatus::Active));
    let block = fixture.block();
    let operator = item("wi_op", Some(WorkSource::operator("laptop")), WorkItemState::Queued, 1);
    let sourceless = item("wi_none", None, WorkItemState::Queued, 1);
    let cycle_item = cycle("wi_c", 1, WorkItemState::Queued, 1);
    assert!(!block.accepts(&lifecycle(EventType::WorkItemSubmitted, &operator)));
    assert!(!block.accepts(&lifecycle(EventType::WorkItemSubmitted, &sourceless)));
    assert!(!block.accepts(&lifecycle(EventType::WorkItemCancelled, &cycle_item)));
    assert!(block.accepts(&lifecycle(EventType::WorkItemSubmitted, &cycle_item)));
    let mut formation = cycle_item.clone();
    formation.kind = WorkItemKind::Workflow;
    assert!(!block.accepts(&lifecycle(EventType::WorkItemStarted, &formation)));
    assert!(block.accepts(&completed("")));
    assert!(block.accepts(&cancelled("why", true)));
    assert_eq!(
        sweep_for(&completed("")).unwrap().terminal.as_deref(),
        Some("campaign transient-recovery completed")
    );
}

#[test]
fn a_dry_run_reads_and_writes_nothing() {
    let fixture = Fixture::new(&[], campaign(CampaignStatus::Active));
    let path: &Path = &fixture.ledger;
    let before = std::fs::read(path).unwrap();
    let cycle_item = cycle("wi_c", 1, WorkItemState::Queued, 1);
    assert!(
        fixture
            .block()
            .dry_run_events(&lifecycle(EventType::WorkItemSubmitted, &cycle_item))
            .is_empty()
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
}
