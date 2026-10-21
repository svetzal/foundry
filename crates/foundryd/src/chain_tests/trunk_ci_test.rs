//! Trunk CI after a landing, through the real blocks and the real watcher.
//!
//! In production 0.56.0 (2026-10-18..20) two campaigns completed while their
//! branch's GitHub CI was red: Foundry ran its gates on its own host and never
//! looked at CI afterwards. Here the real `SettleWorkItem` records a pending
//! watch on each landing, the real `TrunkCiWatcher` concludes it against an
//! in-process GitHub double, the real `AdvanceCampaign` waits for it and will
//! not call a red trunk done, and the real `CompleteCampaign` RPC refuses an
//! owner completion over it. The watcher's clock is pinned relative to the
//! watch it reads: nothing here sleeps, reads GitHub or waits on real time.
use super::*;

use chrono::{DateTime, Utc};
use foundry_blocks::gateway::trunk_ci::FakeTrunkCiGateway;
use foundry_sdk::campaign::{Campaign, CampaignStatus, CampaignStore};
use foundry_sdk::gateway::fakes::FakeAgentGateway;
use foundry_sdk::payload::{
    CampaignAdvanceRequestedPayload, LoopContext, TaskRunCompletedPayload, TaskVerdict,
};
use foundry_sdk::task_block::{TaskBlock, TaskBlockResult};
use foundry_sdk::trunk_ci::{TrunkCiState, TrunkCiWatch};
use foundry_sdk::work_item::{FollowUpObligation, WorkItemSpec};

use crate::ci_watch::TrunkCiWatcher;
use crate::proto::foundry_server::Foundry as _;

const PROJECT: &str = "test-project";
const REPO: &str = "owner/test-project";
const CAMPAIGN: &str = "tidy";

/// One landing's ledger, engine, watcher and GitHub double.
struct Fixture {
    dir: tempfile::TempDir,
    store_path: PathBuf,
    campaigns_path: PathBuf,
    registry: Arc<RwLock<Registry>>,
    engine: Arc<Engine>,
    github: Arc<FakeTrunkCiGateway>,
    advanced: Arc<Mutex<Vec<String>>>,
    /// What the watcher reads as "now".
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("work-items.json");
        let campaigns_path = dir.path().join("campaigns.json");
        let registry = test_helpers::registry_with_project(PROJECT, dir.path().to_str().unwrap());
        registry.write().unwrap().projects[0].repo = REPO.into();
        let mut engine = Engine::new().with_event_writer(Arc::new(
            foundry_engine::event_writer::EventWriter::new(dir.path().join("events")),
        ));
        engine.register(Box::new(foundry_blocks::blocks::SettleWorkItem::with_registry(
            store_path.clone(),
            registry.clone(),
        )));
        Self {
            dir,
            store_path,
            campaigns_path,
            registry,
            engine: Arc::new(engine),
            github: FakeTrunkCiGateway::new(),
            advanced: Arc::new(Mutex::new(Vec::new())),
            now: Arc::new(Mutex::new(Utc::now())),
        }
    }

    fn ctx(&self) -> crate::service::RuntimeContext {
        runtime_context(Arc::clone(&self.engine), self.registry.clone(), self.dir.path())
    }

    fn watcher(&self) -> TrunkCiWatcher {
        let recorder = Arc::clone(&self.advanced);
        let now = Arc::clone(&self.now);
        TrunkCiWatcher::new(
            self.ctx(),
            self.store_path.clone(),
            self.github.clone(),
            Arc::new(move |campaign| recorder.lock().unwrap().push(campaign)),
        )
        .with_clock(Arc::new(move || *now.lock().unwrap()))
    }

    /// Pin the watcher's clock `minutes` after `item`'s watch began.
    fn watch_age(&self, item: &str, minutes: i64) {
        let begun = self.item(item).ci.expect("a watch").requested_at;
        *self.now.lock().unwrap() = begun + chrono::Duration::minutes(minutes);
    }

    fn ledger(&self) -> WorkItemStore {
        WorkItemStore::load(&self.store_path).unwrap()
    }

    fn item(&self, id: &str) -> WorkItem {
        self.ledger().find(id).cloned().unwrap_or_else(|| panic!("{id} in the ledger"))
    }

    fn add(&self, item: WorkItem) {
        let mut store = self.ledger();
        store.upsert(item);
        store.save(&self.store_path).unwrap();
    }

    /// Land `item`, already running, as `commit` through the real
    /// `SettleWorkItem`, and return the terminal result event.
    async fn land(&self, item: &WorkItem, commit: &str, campaign: Option<(&str, u64)>) -> Event {
        let terminal = terminal(item, commit, campaign);
        self.engine.process(terminal.clone()).await;
        terminal
    }

    fn service(&self) -> crate::service::FoundryService {
        crate::service::FoundryService::new(
            self.ctx(),
            crate::service::StoreConfig {
                campaigns_path: self.campaigns_path.clone(),
                work_items_path: self.store_path.clone(),
                events_dir: self.dir.path().join("events"),
                registry_path: self.dir.path().join("registry.json"),
                sentinels: Arc::new(RwLock::new(
                    foundry_sdk::sentinel::SentinelStore::default_seed(),
                )),
                sentinels_path: self.dir.path().join("sentinels.json"),
                scheduler_reload: Arc::new(tokio::sync::Notify::new()),
            },
        )
    }

    /// The open group of `ListWorkItems`, the read `foundry queue open` renders.
    async fn open_items(&self) -> Vec<crate::proto::WorkItem> {
        self.service()
            .list_work_items(tonic::Request::new(crate::proto::ListWorkItemsRequest::default()))
            .await
            .unwrap()
            .into_inner()
            .items
            .into_iter()
            .filter(|item| matches!(item.state.as_str(), "preserved" | "needs_decision" | "failed"))
            .collect()
    }

    fn save_campaign(&self, cycles_completed: u64) {
        let campaign: Campaign = serde_json::from_value(serde_json::json!({
            "name": CAMPAIGN, "project": PROJECT, "mission": "Add a --quiet flag everywhere.",
            "done_evidence": [{"kind": "review", "statement": "every command takes --quiet"}],
            "status": "active", "authorized_by": "owner",
            "cycles_completed": cycles_completed, "budget": {"max_cycles": 5}
        }))
        .unwrap();
        CampaignStore {
            findings: vec![],
            initiatives: vec![],
            version: 1,
            campaigns: vec![campaign],
        }
        .save(&self.campaigns_path)
        .unwrap();
    }

    fn campaign(&self) -> Campaign {
        CampaignStore::load(&self.campaigns_path)
            .unwrap()
            .find(CAMPAIGN)
            .unwrap()
            .clone()
    }

    /// Run the real `AdvanceCampaign` on `requested` with a formation agent
    /// that would answer `decision`. Returns the result and every prompt the
    /// agent was shown.
    async fn advance(&self, requested: &Event, decision: &str) -> (TaskBlockResult, Vec<String>) {
        let agent = FakeAgentGateway::always(foundry_sdk::gateway::AgentResponse::success(
            format!("```json\n{decision}\n```"),
        ));
        let block = foundry_blocks::blocks::AdvanceCampaign::new(
            agent.clone(),
            test_helpers::passing_shell(),
            self.registry.clone(),
            self.campaigns_path.clone(),
            self.store_path.clone(),
        );
        let result = block.execute(requested).await.unwrap();
        let prompts = agent.invocations().into_iter().map(|call| call.prompt).collect();
        (result, prompts)
    }
}

/// An item of the project running now: a standalone task, or a cycle of
/// [`CAMPAIGN`].
fn running(id: &str, cycle: Option<u64>) -> WorkItem {
    let (kind, lane) = match cycle {
        Some(_) => (WorkItemKind::CampaignCycle, WorkLane::Campaign),
        None => (WorkItemKind::Task, WorkLane::Interactive),
    };
    let mut item = WorkItem::dispatched(
        WorkItemSpec {
            project: PROJECT.into(),
            objective: format!("Objective of {id}."),
            kind,
            lane,
            origin: "trunk CI test".into(),
            trace_id: Some(mint_trace_id()),
        },
        Utc::now(),
    );
    item.id = id.into();
    item
}

/// The task runner's landed result for `item`'s run, on its trace.
fn terminal(item: &WorkItem, commit: &str, campaign: Option<(&str, u64)>) -> Event {
    Event::new(
        EventType::TaskRunCompleted,
        PROJECT.into(),
        Throttle::Full,
        Event::serialize_payload(&TaskRunCompletedPayload {
            project: PROJECT.into(),
            success: true,
            landed: true,
            summary: format!("{} landed the quiet flag", item.id),
            preservation_ref: Some(commit.into()),
            land_blocked: None,
            proof_evidence: None,
            uncommitted_artifacts: None,
            trunk_arrivals: Vec::new(),
            verdict: TaskVerdict::Complete,
            context: LoopContext {
                campaign: campaign.map(|(name, _)| name.to_string()),
                campaign_cycle: campaign.map(|(_, cycle)| cycle),
                ..LoopContext::default()
            },
            failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
        })
        .unwrap(),
    )
    .with_trace_id(item.trace_id.clone())
}

/// A run of `commit`'s CI on main.
fn run(id: u64, status: &str, conclusion: &str) -> foundry_sdk::trunk_ci::CiRun {
    FakeTrunkCiGateway::run(id, "main", status, conclusion)
}

/// Every trunk CI follow-up in the ledger.
fn red_follow_ups(store: &WorkItemStore) -> Vec<&WorkItem> {
    store
        .items
        .iter()
        .filter(|item| {
            item.follow_up.as_ref().is_some_and(|record| {
                matches!(record.obligation, FollowUpObligation::TrunkCi { .. })
            })
        })
        .collect()
}

/// The advance the RPC dispatches for `foundry campaign advance`, and the
/// watcher's hook in production: no run result of its own.
fn manual_advance() -> Event {
    Event::new(
        EventType::CampaignAdvanceRequested,
        PROJECT.into(),
        Throttle::Full,
        Event::serialize_payload(&CampaignAdvanceRequestedPayload {
            campaign: CAMPAIGN.into(),
            run_event_id: None,
            run_result: None,
            operator_origin: Some(crate::ci_watch::ADVANCE_ORIGIN.into()),
            providers: foundry_sdk::gateway::StageProviders::default(),
        })
        .unwrap(),
    )
}

/// The advance `RequestCampaignAdvance` asks for after `terminal`.
async fn automatic_advance(terminal: &Event) -> Event {
    foundry_blocks::blocks::RequestCampaignAdvance
        .execute(terminal)
        .await
        .unwrap()
        .events
        .remove(0)
}

fn only(events: &[Event], event_type: &EventType) -> Event {
    let found: Vec<_> = events.iter().filter(|event| event.event_type == *event_type).collect();
    assert_eq!(found.len(), 1, "exactly one {event_type}: {found:?}");
    found[0].clone()
}

#[tokio::test]
async fn red_trunk_ci_opens_a_failed_follow_up_that_a_later_green_landing_settles() {
    let f = Fixture::new();
    let task = running("wi_task", None);
    f.add(task.clone());

    f.land(&task, "c1c1c1c1c1", None).await;

    let watch = f.item("wi_task").ci.expect("the landing is watched");
    assert_eq!(watch, TrunkCiWatch::pending(REPO, "main", "c1c1c1c1c1", watch.requested_at));

    f.github.set_runs("c1c1c1c1c1", vec![run(41, "completed", "failure")]);
    f.github.set_failed_jobs(41, &["test (macos)", "lint"]);
    f.github
        .set_log_tail(41, "error[E0425]: cannot find value `quiet`\n --> src/main.rs:9:5");
    f.watch_age("wi_task", 5);
    let red = f.watcher().tick().await;

    assert_eq!(red.opened.len(), 1, "one follow-up");
    let follow_up = red.opened[0].clone();
    assert_eq!(follow_up.kind, WorkItemKind::FollowUp);
    assert_eq!(follow_up.state, WorkItemState::Failed);
    assert_eq!(follow_up.reason, "trunk CI red at c1c1c1c after wi_task: test (macos), lint");
    assert!(
        follow_up
            .objective
            .contains("Make the CI of main on owner/test-project green again.")
    );
    assert!(follow_up.objective.contains("Objective of wi_task."));
    let watch = f.item("wi_task").ci.unwrap();
    assert_eq!(watch.state, TrunkCiState::Failed);
    assert_eq!(watch.follow_up.as_deref(), Some(follow_up.id.as_str()));

    let open = f.open_items().await;
    assert_eq!(open.len(), 1, "queue open lists the red follow-up: {open:?}");
    let wire = open[0].follow_up.clone().expect("its obligation");
    assert_eq!(open[0].id, follow_up.id);
    assert_eq!(open[0].state, "failed");
    assert_eq!(wire.obligation, "trunk_ci");
    assert_eq!(wire.failing_jobs, vec!["test (macos)", "lint"]);
    assert!(wire.log_tail.unwrap().contains("cannot find value `quiet`"));
    assert_eq!(wire.run_url.as_deref(), Some("https://github.com/o/r/actions/runs/41"));
    let logged = super::campaign_carry_test::logged_events(&f.dir.path().join("events"));
    let checked = only(&logged, &EventType::TrunkCiChecked);
    assert_eq!(checked.payload["state"], "failed");
    assert_eq!(checked.payload["item_id"], "wi_task");
    assert_eq!(checked.payload["follow_up"], follow_up.id.as_str());
    assert_eq!(checked.payload["failing_jobs"], serde_json::json!(["test (macos)", "lint"]));
    assert_eq!(checked.trace_id, task.trace_id, "on the landed task's trace");

    let fix = running("wi_fix", None);
    f.add(fix.clone());
    f.land(&fix, "c2c2c2c2c2", None).await;
    f.github.set_runs("c2c2c2c2c2", vec![run(42, "completed", "success")]);
    let green = f.watcher().tick().await;

    assert_eq!(
        green.settled.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
        vec![follow_up.id.as_str()]
    );
    let settled = f.item(&follow_up.id);
    assert_eq!(settled.state, WorkItemState::Landed);
    assert_eq!(settled.reason, "trunk CI green at c2c2c2c2c2");
    assert!(f.open_items().await.is_empty(), "nothing open any more");
}

#[tokio::test]
async fn no_run_waits_out_the_grace_and_a_run_still_going_after_two_hours_is_unavailable() {
    let f = Fixture::new();
    let quiet = running("wi_quiet", None);
    let slow = running("wi_slow", None);
    f.add(quiet.clone());
    f.add(slow.clone());
    f.land(&quiet, "aaaaaaaaaa", None).await;
    f.land(&slow, "bbbbbbbbbb", None).await;
    f.github.set_runs("bbbbbbbbbb", vec![run(7, "in_progress", "")]);
    let watcher = f.watcher();

    f.watch_age("wi_quiet", 14);
    watcher.tick().await;
    assert_eq!(f.item("wi_quiet").ci.unwrap().state, TrunkCiState::Pending, "inside the grace");

    f.watch_age("wi_quiet", 15);
    watcher.tick().await;
    let quiet = f.item("wi_quiet").ci.unwrap();
    assert_eq!(quiet.state, TrunkCiState::NoRun);
    assert_eq!(
        quiet.detail.as_deref(),
        Some("no CI run on main for this commit within 15 minutes")
    );

    f.watch_age("wi_slow", 119);
    watcher.tick().await;
    assert_eq!(f.item("wi_slow").ci.unwrap().state, TrunkCiState::Pending, "still running");

    f.watch_age("wi_slow", 120);
    watcher.tick().await;
    let slow = f.item("wi_slow").ci.unwrap();
    assert_eq!(slow.state, TrunkCiState::Unavailable);
    assert_eq!(
        slow.detail.as_deref(),
        Some("trunk CI still running after 2 hours: CI in_progress")
    );

    let store = f.ledger();
    assert!(red_follow_ups(&store).is_empty(), "neither is red");
    assert!(f.open_items().await.is_empty());
}

#[tokio::test]
async fn a_campaign_waits_for_pending_trunk_ci_and_forms_from_its_recorded_result_once_green() {
    let f = Fixture::new();
    f.save_campaign(1);
    let cycle = running("wi_cycle", Some(1));
    f.add(cycle.clone());
    let landed = f.land(&cycle, "c1c1c1c1c1", Some((CAMPAIGN, 1))).await;
    assert!(f.item("wi_cycle").ci.unwrap().is_pending());

    let (waited, prompts) = f.advance(&automatic_advance(&landed).await, "unused").await;

    assert!(waited.success, "{}", waited.summary);
    assert!(prompts.is_empty(), "no formation agent while trunk CI is pending");
    assert!(
        !waited
            .events
            .iter()
            .any(|event| event.event_type == EventType::ExecutionRequested),
        "nothing dispatched"
    );
    let stood_down = only(&waited.events, &EventType::CampaignAdvanceCompleted);
    assert_eq!(stood_down.payload["decision"], "stood_down");
    assert_eq!(stood_down.payload["reason"], "awaiting trunk CI on c1c1c1c (wi_cycle)");
    let campaign = f.campaign();
    assert_eq!(campaign.status, CampaignStatus::Active);
    assert_eq!(campaign.cycles_completed, 1, "no cycle counted");
    assert_eq!(campaign.cycles_landed, 1, "the landing itself is recorded");
    assert_eq!(
        campaign.pending_run_result.as_ref().map(|result| result.summary.as_str()),
        Some("wi_cycle landed the quiet flag"),
        "the arriving result waits on the campaign"
    );

    // Durable admission settles the advance's item with its stand-down
    // reason (`work_admission::stood_down_reason`); that is how the watcher
    // finds the campaign again.
    let mut advance = running("wi_advance", None);
    advance.kind = WorkItemKind::Workflow;
    advance.started_root = Some(automatic_advance(&landed).await);
    advance.settle_landed(stood_down.payload["reason"].as_str().unwrap(), Utc::now());
    f.add(advance);

    f.github.set_runs("c1c1c1c1c1", vec![run(5, "completed", "success")]);
    f.watch_age("wi_cycle", 9);
    let green = f.watcher().tick().await;

    assert_eq!(green.advanced, vec![CAMPAIGN]);
    assert_eq!(*f.advanced.lock().unwrap(), vec![CAMPAIGN]);

    let (formed, prompts) = f
        .advance(
            &manual_advance(),
            r#"{"decision":"advance","objective":"Add --quiet to the export command.","reason":"export lacks it"}"#,
        )
        .await;

    assert!(formed.success, "{}", formed.summary);
    assert_eq!(prompts.len(), 1, "formed once");
    assert!(
        prompts[0].contains("wi_cycle landed the quiet flag"),
        "formation sees the recorded run result:\n{}",
        prompts[0]
    );
    assert!(prompts[0].contains("TRUNK CI"), "{}", prompts[0]);
    let dispatched = only(&formed.events, &EventType::ExecutionRequested);
    assert_eq!(dispatched.payload["prompt"], "Add --quiet to the export command.");
    assert_eq!(dispatched.payload["campaign_cycle"], 2);
    let campaign = f.campaign();
    assert_eq!(campaign.cycles_completed, 2);
    assert_eq!(campaign.pending_run_result, None, "the recorded result was consumed");
}

#[tokio::test]
async fn red_trunk_ci_turns_a_done_into_a_fix_and_blocks_owner_completion_until_green() {
    let f = Fixture::new();
    f.save_campaign(1);
    let cycle = running("wi_cycle", Some(1));
    f.add(cycle.clone());
    let landed = f.land(&cycle, "c1c1c1c1c1", Some((CAMPAIGN, 1))).await;
    f.github.set_runs("c1c1c1c1c1", vec![run(41, "completed", "failure")]);
    f.github.set_failed_jobs(41, &["test (macos)"]);
    f.github.set_log_tail(41, "thread 'quiet_flag' panicked at tests/cli.rs:12");
    f.watch_age("wi_cycle", 4);
    let red = f.watcher().tick().await.opened.remove(0);

    let (formed, prompts) = f
        .advance(
            &automatic_advance(&landed).await,
            r#"{"decision":"done","reason":"every command takes --quiet"}"#,
        )
        .await;

    assert!(formed.success, "{}", formed.summary);
    assert!(
        prompts[0].contains(&format!("follow-up {}: CI red at c1c1c1c", red.id)),
        "{}",
        prompts[0]
    );
    let completed = only(&formed.events, &EventType::CampaignAdvanceCompleted);
    assert_eq!(completed.payload["decision"], "advance", "red CI is never done");
    assert_eq!(completed.payload["reason"], "review proposed done while trunk CI was red");
    let objective = completed.payload["objective"].as_str().unwrap();
    assert!(objective.contains("failing jobs: test (macos)"), "{objective}");
    assert!(objective.contains("panicked at tests/cli.rs:12"), "{objective}");
    assert!(
        objective.contains("weakening, skipping or deleting CI workflows"),
        "{objective}"
    );
    assert!(
        !formed
            .events
            .iter()
            .any(|event| event.event_type == EventType::CampaignCompleted)
    );
    assert_eq!(
        only(&formed.events, &EventType::ExecutionRequested).payload["prompt"],
        objective
    );
    assert_eq!(f.campaign().status, CampaignStatus::Active);

    let refusal = f
        .service()
        .complete_campaign(tonic::Request::new(crate::proto::CompleteCampaignRequest {
            name: CAMPAIGN.into(),
            reason: "shipped".into(),
        }))
        .await
        .unwrap_err();
    assert_eq!(refusal.code(), tonic::Code::FailedPrecondition);
    assert!(refusal.message().contains(&red.id), "{}", refusal.message());
    assert!(refusal.message().contains("foundry queue resume"), "{}", refusal.message());
    assert_eq!(f.campaign().status, CampaignStatus::Active, "a refusal changes nothing");

    let fix = running("wi_fix", Some(2));
    f.add(fix.clone());
    f.land(&fix, "c2c2c2c2c2", Some((CAMPAIGN, 2))).await;
    f.github.set_runs("c2c2c2c2c2", vec![run(42, "completed", "success")]);
    f.watcher().tick().await;
    assert_eq!(f.item(&red.id).state, WorkItemState::Landed);

    let completed = f
        .service()
        .complete_campaign(tonic::Request::new(crate::proto::CompleteCampaignRequest {
            name: CAMPAIGN.into(),
            reason: "shipped".into(),
        }))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(completed.campaign.unwrap().status, "completed");
}

#[tokio::test]
async fn resuming_a_red_ci_follow_up_admits_a_task_from_trunk_that_continues_it() {
    let f = Fixture::new();
    let task = running("wi_task", None);
    f.add(task.clone());
    f.land(&task, "c1c1c1c1c1", None).await;
    f.github.set_runs("c1c1c1c1c1", vec![run(41, "completed", "failure")]);
    f.github.set_failed_jobs(41, &["lint"]);
    f.watch_age("wi_task", 3);
    let red = f.watcher().tick().await.opened.remove(0);
    let ctx = f.ctx();

    let admitted = crate::service::work_item_ops::resume_item(
        &f.store_path,
        &f.campaigns_path,
        &ctx,
        &pacing_paths_beside(&f.store_path),
        red.id.clone(),
        "desk (trunk CI test)".into(),
        crate::service::work_item_ops::ScheduleHints::default(),
    )
    .await
    .expect("a failed trunk CI follow-up can be resumed");

    let child = f.item(&admitted.id);
    assert_eq!(child.kind, WorkItemKind::Task);
    assert_eq!(child.state, WorkItemState::Queued);
    assert_eq!(child.objective, red.objective);
    assert_eq!(child.continues, vec![red.id.clone()]);
    assert_eq!(child.resumes, None);
    assert_eq!(f.item(&red.id).state, WorkItemState::Failed, "open until the fix lands");

    let dispatched: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&dispatched);
    let pinned = *f.now.lock().unwrap();
    crate::pacing::PacingScheduler::new(
        ctx,
        f.store_path.clone(),
        pacing_paths_beside(&f.store_path),
        Arc::new(tokio::sync::Notify::new()),
    )
    .with_dispatch(Arc::new(move |root: Event| recorder.lock().unwrap().push(root)))
    .with_clock(Arc::new(move || pinned))
    .tick()
    .await;

    let roots = dispatched.lock().unwrap().clone();
    assert_eq!(roots.len(), 1);
    assert!(roots[0].payload.get("base_ref").is_none(), "started from current trunk");
    assert_eq!(roots[0].payload["continues"], serde_json::json!([red.id]));
    assert_eq!(roots[0].payload["prompt"], red.objective);
}
