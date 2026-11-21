//! `campaign cancel --now` against real Git: a checkout, a bare origin and a
//! campaign cycle worktree holding uncommitted work.
//!
//! The blocks are routed the way the engine routes them, by `sinks_on` and
//! `accepts`, following every event they emit. Each scenario runs in a child
//! process so `FOUNDRY_WORKTREES_DIR`, `FOUNDRY_PRESERVED_DIR` and the global
//! Git configuration never leak into parallel tests.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_sdk::campaign::{Campaign, CampaignStore};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    CampaignCancelledPayload, CampaignTerminalPayload, LoopContext, TaskRunCompletedPayload,
    TaskVerdict, WorkReconcileCompletedPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::TaskBlock;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::time::UtcDateTime;
use foundry_sdk::work_item::{
    WorkItem, WorkItemKind, WorkItemOperatorAction, WorkItemSpec, WorkItemState, WorkItemStore,
    WorkLane,
};
use foundry_sdk::work_source::WorkSource;

use crate::blocks::test_helpers::{make_trigger, registry_with_project};
use crate::blocks::{
    DisposeCampaignWork, DisposeCancelledWork, ReconcileWork, SettleCarriedCampaignWork,
    SettleWorkItem,
};

const PROJECT: &str = "demo";
const CAMPAIGN: &str = "ship";
const WORKSPACE: &str = "ship-c2-abcdef";
const BRANCH: &str = "foundry-task/demo-ship-c2-abcdef";
const TRACE: &str = "cccccccccccccccccccccccccccccccc";
const ABORTED_ROOT: &str = "evt_aborted";
const CHILD_ENV: &str = "FOUNDRY_CANCEL_BOUNDARY_ROOT";

async fn git(path: &Path, args: &[&str]) -> String {
    let result = crate::shell::run(path, "git", args, None, None).await.unwrap();
    assert!(result.success, "git {args:?}: {}", result.stderr);
    result.stdout.trim_end().to_owned()
}

async fn local(checkout: &Path, branch: &str) -> bool {
    crate::shell::run(
        checkout,
        "git",
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
        None,
        None,
    )
    .await
    .unwrap()
    .success
}

async fn on_origin(checkout: &Path, branch: &str) -> bool {
    !git(
        checkout,
        &[
            "ls-remote",
            "--heads",
            "origin",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await
    .is_empty()
}

/// A campaign cycle aborted mid-agent, before anything was cancelled.
struct World {
    root: PathBuf,
    checkout: PathBuf,
    worktree: PathBuf,
    ledger: PathBuf,
    campaigns: PathBuf,
    registry: Arc<RwLock<Registry>>,
    item: String,
}

impl World {
    async fn new(root: &Path) -> Self {
        let remote = root.join("origin.git");
        let checkout = root.join("checkout");
        git(root, &["init", "--bare", remote.to_str().unwrap()]).await;
        git(root, &["init", "-b", "main", checkout.to_str().unwrap()]).await;
        git(&checkout, &["config", "user.name", "Test"]).await;
        git(&checkout, &["config", "user.email", "test@example.com"]).await;
        std::fs::write(checkout.join("app.txt"), "base\n").unwrap();
        git(&checkout, &["add", "app.txt"]).await;
        git(&checkout, &["commit", "-q", "-m", "base"]).await;
        git(&checkout, &["remote", "add", "origin", remote.to_str().unwrap()]).await;
        git(&checkout, &["push", "-q", "-u", "origin", "main"]).await;

        // The cycle's worktree, as `prepare_task_workspace` lays it out, with
        // the agent's uncommitted edit in it.
        let worktree = foundry_sdk::paths::worktrees_dir().join(PROJECT).join(WORKSPACE);
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        git(
            &checkout,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                BRANCH,
                worktree.to_str().unwrap(),
                "main",
            ],
        )
        .await;
        std::fs::write(worktree.join("app.txt"), "half-finished change\n").unwrap();

        let mut item = WorkItem::dispatched(
            WorkItemSpec {
                project: PROJECT.into(),
                objective: "Ship the billing module.".into(),
                kind: WorkItemKind::CampaignCycle,
                lane: WorkLane::Campaign,
                origin: "campaign ship cycle 2".into(),
                trace_id: Some(TRACE.into()),
            },
            UtcDateTime::now(),
        )
        .with_source(Some(WorkSource::campaign(CAMPAIGN, 2)));
        let mut started = Event::new(
            EventType::ExecutionRequested,
            PROJECT.into(),
            Throttle::Full,
            serde_json::json!({ "campaign": CAMPAIGN, "campaign_cycle": 2 }),
        );
        started.id = ABORTED_ROOT.into();
        item.started_root = Some(started);
        let ledger = root.join("work-items.json");
        WorkItemStore {
            version: 1,
            items: vec![item.clone()],
        }
        .save(&ledger)
        .unwrap();

        let mut campaign: Campaign = serde_json::from_value(serde_json::json!({
            "name": CAMPAIGN, "project": PROJECT, "mission": "Ship billing.",
            "authorized_by": "owner",
            "done_evidence": [{ "kind": "review", "statement": "billing ships" }],
        }))
        .unwrap();
        assert_eq!(
            campaign.cancel("wrong approach", UtcDateTime::now()).unwrap(),
            foundry_sdk::campaign::Transition::Applied
        );
        let campaigns = root.join("campaigns.json");
        CampaignStore {
            findings: vec![],
            initiatives: vec![],
            version: 1,
            campaigns: vec![campaign],
        }
        .save(&campaigns)
        .unwrap();

        Self {
            registry: registry_with_project(PROJECT, checkout.to_str().unwrap()),
            root: root.to_path_buf(),
            checkout,
            worktree,
            ledger,
            campaigns,
            item: item.id,
        }
    }

    fn blocks(&self) -> Vec<Box<dyn TaskBlock>> {
        let worktrees = foundry_sdk::paths::worktrees_dir();
        vec![
            Box::new(DisposeCampaignWork::new(Arc::clone(&self.registry), self.ledger.clone())),
            Box::new(SettleCarriedCampaignWork::new(self.ledger.clone(), self.campaigns.clone())),
            Box::new(DisposeCancelledWork::new(
                Arc::clone(&self.registry),
                self.ledger.clone(),
                worktrees.clone(),
            )),
            Box::new(
                SettleWorkItem::with_registry(self.ledger.clone(), Arc::clone(&self.registry))
                    .with_worktrees(worktrees),
            ),
        ]
    }

    /// Route `event` to every block that sinks on it and accepts it, then
    /// route what they emit, as the engine does. Returns every event routed.
    async fn route(&self, event: Event) -> Vec<Event> {
        let blocks = self.blocks();
        let mut pending = vec![event];
        let mut routed = Vec::new();
        while let Some(event) = pending.pop() {
            for block in &blocks {
                if block.sinks_on().contains(&event.event_type) && block.accepts(&event) {
                    let result = block.execute(&event).await.unwrap();
                    assert!(result.success, "{}: {}", block.name(), result.summary);
                    pending.extend(result.events);
                }
            }
            routed.push(event);
        }
        routed
    }

    async fn cancel_now(&self, discard_work: bool) -> Vec<Event> {
        let payload = CampaignCancelledPayload {
            terminal: CampaignTerminalPayload {
                campaign: CAMPAIGN.into(),
                project: PROJECT.into(),
                reason: "wrong approach".into(),
                cycles_completed: 1,
                cycles_landed: 0,
            },
            terminated_now: true,
            discard_work,
            aborted_event_id: Some(ABORTED_ROOT.into()),
            aborted_trace_id: Some(TRACE.into()),
            operator_origin: Some("max-headroom".into()),
        };
        self.route(Event::new(
            EventType::CampaignCancelled,
            PROJECT.into(),
            Throttle::Full,
            Event::serialize_payload(&payload).unwrap(),
        ))
        .await
    }

    fn item(&self) -> WorkItem {
        WorkItemStore::load(&self.ledger).unwrap().find(&self.item).unwrap().clone()
    }

    /// Run reconciliation with an orphan sweep that treats everything as
    /// aged, and return the identities it swept.
    async fn sweep(&self) -> Vec<String> {
        let block = ReconcileWork::new(
            Arc::clone(&self.registry),
            self.ledger.clone(),
            foundry_sdk::paths::worktrees_dir(),
            self.root.join("events"),
            self.root.join("digest"),
        )
        .with_sweep_after(Some(Duration::ZERO));
        let result = block
            .execute(&make_trigger(
                EventType::WorkReconcileStarted,
                "system",
                serde_json::json!({}),
            ))
            .await
            .unwrap();
        let report: WorkReconcileCompletedPayload =
            result.events.last().unwrap().parse_payload().unwrap();
        assert!(report.success, "{:?}", report.errors);
        report
            .findings
            .iter()
            .filter(|finding| finding.category.starts_with("swept_"))
            .map(|finding| finding.identity.clone())
            .collect()
    }
}

/// Plain `--now`: the work is pushed and the cycle stays `preserved` for its
/// owner, through the campaign's terminal sweep, settle-time disposal and the
/// orphan sweep. A genuine orphan in the same repository is swept, so the
/// sweep was live.
async fn plain_now_keeps_the_cycle_preserved(root: &Path) -> World {
    let world = World::new(root).await;
    git(&world.checkout, &["branch", "foundry-task/demo-orphan", "main"]).await;

    let events = world.cancel_now(false).await;

    let kept = world.item();
    assert_eq!(kept.state, WorkItemState::Preserved, "{kept:?}");
    assert!(kept.kept_by_campaign_cancel());
    assert_eq!(kept.disposition.as_ref().unwrap().preservation_ref.as_deref(), Some(BRANCH));
    assert!(
        events.iter().all(|event| event.event_type != EventType::WorkItemCancelled),
        "nothing about the kept cycle was cancelled: {events:?}"
    );
    assert!(events.iter().any(|event| event.event_type == EventType::WorkItemSettled
        && event.payload["item_id"] == world.item.as_str()
        && event.payload["state"] == "preserved"));
    assert!(!world.worktree.exists(), "the work is on the branch, not the worktree");
    assert!(local(&world.checkout, BRANCH).await);
    assert!(on_origin(&world.checkout, BRANCH).await);
    let preserved = git(&world.checkout, &["show", &format!("{BRANCH}:app.txt")]).await;
    assert_eq!(preserved, "half-finished change", "the uncommitted edit was committed");

    let swept = world.sweep().await;
    assert!(
        swept.iter().any(|identity| identity.contains("foundry-task/demo-orphan")),
        "{swept:?}"
    );
    assert!(swept.iter().all(|identity| !identity.contains(BRANCH)), "{swept:?}");
    assert!(local(&world.checkout, BRANCH).await);
    assert!(on_origin(&world.checkout, BRANCH).await);
    assert_eq!(world.item(), kept, "the sweep left the kept item alone");
    world
}

async fn plain_now_then_a_resumed_landing_disposes(root: &Path) {
    let world = plain_now_keeps_the_cycle_preserved(root).await;

    // `foundry queue resume` admits a task that resumes the kept cycle; that
    // task's work reaches trunk.
    git(&world.checkout, &["merge", "-q", "--no-ff", "-m", "land", BRANCH]).await;
    git(&world.checkout, &["push", "-q", "origin", "main"]).await;
    let landed = git(&world.checkout, &["rev-parse", "HEAD"]).await;
    let child_trace = "d".repeat(32);
    let mut child = WorkItem::dispatched(
        WorkItemSpec {
            project: PROJECT.into(),
            objective: "Ship the billing module.".into(),
            kind: WorkItemKind::Task,
            lane: WorkLane::Interactive,
            origin: "resume".into(),
            trace_id: Some(child_trace.clone()),
        },
        UtcDateTime::now(),
    );
    child.resumes = Some(world.item.clone());
    let mut store = WorkItemStore::load(&world.ledger).unwrap();
    store.upsert(child);
    store.save(&world.ledger).unwrap();
    let completion = TaskRunCompletedPayload {
        project: PROJECT.into(),
        success: true,
        landed: true,
        summary: "landed".into(),
        preservation_ref: Some(landed),
        land_blocked: None,
        proof_evidence: None,
        trunk_arrivals: Vec::new(),
        verdict: TaskVerdict::Complete,
        context: LoopContext::default(),
        failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
    };

    world
        .route(
            Event::new(
                EventType::TaskRunCompleted,
                PROJECT.into(),
                Throttle::Full,
                Event::serialize_payload(&completion).unwrap(),
            )
            .with_trace_id(Some(child_trace)),
        )
        .await;

    assert_eq!(world.item().state, WorkItemState::Landed);
    assert!(!local(&world.checkout, BRANCH).await);
    assert!(!on_origin(&world.checkout, BRANCH).await);
}

async fn plain_now_then_a_close_disposes(root: &Path) {
    let world = plain_now_keeps_the_cycle_preserved(root).await;

    // What `foundry queue close` records, then the cancellation it announces.
    let mut store = WorkItemStore::load(&world.ledger).unwrap();
    let item = store.find_mut(&world.item).unwrap();
    item.operator_action = Some(WorkItemOperatorAction {
        command: "close".into(),
        origin: "max-headroom".into(),
        previous_state: item.state,
        previous_reason: item.reason.clone(),
        previous_settled_at: item.settled_at,
    });
    item.settle_cancelled("not needed after all", item.disposition.clone(), UtcDateTime::now());
    let closed = item.clone();
    store.save(&world.ledger).unwrap();
    world
        .route(crate::blocks::work_ledger::work_item_event(
            EventType::WorkItemCancelled,
            &Event::new(
                EventType::WorkItemCancelled,
                PROJECT.into(),
                Throttle::Full,
                serde_json::json!({}),
            ),
            &closed,
        ))
        .await;

    assert_eq!(world.item().state, WorkItemState::Cancelled);
    assert!(!local(&world.checkout, BRANCH).await);
    assert!(!on_origin(&world.checkout, BRANCH).await);
}

async fn discard_work_disposes(root: &Path) {
    let world = World::new(root).await;

    let events = world.cancel_now(true).await;

    let cancelled = world.item();
    assert_eq!(cancelled.state, WorkItemState::Cancelled);
    assert!(!cancelled.kept_by_campaign_cancel());
    assert!(events.iter().any(|event| event.event_type == EventType::WorkItemCancelled
        && event.payload["item_id"] == world.item.as_str()));
    assert!(!world.worktree.exists());
    assert!(!local(&world.checkout, BRANCH).await);
    assert!(!on_origin(&world.checkout, BRANCH).await, "discarded work is never pushed");
}

async fn scenario(name: &str, root: &Path) {
    match name {
        "plain" => {
            plain_now_keeps_the_cycle_preserved(root).await;
        }
        "resumed" => plain_now_then_a_resumed_landing_disposes(root).await,
        "closed" => plain_now_then_a_close_disposes(root).await,
        "discard" => discard_work_disposes(root).await,
        other => panic!("unknown scenario {other}"),
    }
}

/// Run `scenario` in a child process with its own Foundry roots and an empty
/// global Git configuration.
async fn isolated(scenario_name: &str, test: &str) {
    if let Some(root) = std::env::var_os(CHILD_ENV) {
        scenario(scenario_name, Path::new(&root)).await;
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let global = root.path().join("gitconfig");
    std::fs::write(&global, "").unwrap();
    let output = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env(CHILD_ENV, root.path())
        .env("FOUNDRY_WORKTREES_DIR", root.path().join("worktrees"))
        .env("FOUNDRY_PRESERVED_DIR", root.path().join("preserved"))
        .env("GIT_CONFIG_GLOBAL", global)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn plain_cancel_now_leaves_a_preserved_cycle_that_survives_disposal_and_the_sweep() {
    isolated(
        "plain",
        "blocks::dispose_campaign_work::boundary_tests::plain_cancel_now_leaves_a_preserved_cycle_that_survives_disposal_and_the_sweep",
    )
    .await;
}

#[tokio::test]
async fn cancel_now_with_discard_work_disposes_of_the_cycle() {
    isolated(
        "discard",
        "blocks::dispose_campaign_work::boundary_tests::cancel_now_with_discard_work_disposes_of_the_cycle",
    )
    .await;
}

#[tokio::test]
async fn a_kept_cycle_resumed_and_landed_is_disposed_of() {
    isolated(
        "resumed",
        "blocks::dispose_campaign_work::boundary_tests::a_kept_cycle_resumed_and_landed_is_disposed_of",
    )
    .await;
}

#[tokio::test]
async fn a_kept_cycle_closed_by_its_owner_is_disposed_of() {
    isolated(
        "closed",
        "blocks::dispose_campaign_work::boundary_tests::a_kept_cycle_closed_by_its_owner_is_disposed_of",
    )
    .await;
}
