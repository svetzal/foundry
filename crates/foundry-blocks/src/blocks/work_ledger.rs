//! The two blocks that keep the work-item ledger in step with a task dispatch.
//!
//! [`RecordWorkItem`] opens the record when a task workflow clears preflight —
//! before the coding agent is ever invoked — and [`SettleWorkItem`] closes it
//! from the task runner's typed terminal result.
//!
//! Neither block changes a dispatch. They observe the chain that already
//! exists and write a record beside it, and a ledger fault is absorbed rather
//! than propagated: an unwritable ledger must never be the reason a task does
//! not run.

use std::path::PathBuf;
use std::sync::Mutex;

use chrono::Utc;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    PreflightCompletedPayload, TaskRunCompletedPayload, WorkItemEventPayload,
};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::{WorkItem, WorkItemKind, WorkItemSpec, WorkItemStore, WorkLane};
use foundry_sdk::workflow::WorkflowType;

use super::SimulatedSuccess;

/// How a task dispatch reached the engine, as the ledger records it.
///
/// Every task-kind dispatch funnels through the same `ExecutionRequested` →
/// preflight → `PlanCompleted` chain, so the lane is read off the context the
/// dispatch already carries rather than from a new marker field.
fn classify(
    objective: &str,
    campaign: Option<&str>,
    cycle: Option<u64>,
) -> (WorkItemKind, WorkLane, String) {
    if let Some(campaign) = campaign {
        let origin = match cycle {
            Some(cycle) => format!("campaign {campaign} cycle {cycle}"),
            None => format!("campaign {campaign}"),
        };
        return (WorkItemKind::CampaignCycle, WorkLane::Campaign, origin);
    }
    // A major-upgrade objective is already self-identifying: the nightly
    // majors lane writes it, and `PlanMajorUpgrades` reads it back out of the
    // event log to dedupe its own dispatches. Reusing that parse keeps the
    // majors dispatch payload untouched.
    if crate::dependency_updates::majors::parse_objective(objective).is_some() {
        return (
            WorkItemKind::MajorUpgrade,
            WorkLane::Maintenance,
            "nightly majors lane".to_string(),
        );
    }
    (WorkItemKind::Task, WorkLane::Interactive, "foundry task".to_string())
}

/// The item a preflight-cleared task dispatch opens.
fn item_from_preflight(trigger: &Event, payload: &PreflightCompletedPayload) -> WorkItem {
    let objective = payload
        .chain
        .prompt
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let (kind, lane, origin) =
        classify(&objective, payload.chain.campaign.as_deref(), payload.chain.campaign_cycle);
    WorkItem::dispatched(
        WorkItemSpec {
            project: trigger.project.clone(),
            objective,
            kind,
            lane,
            origin,
            trace_id: trigger.trace_id.clone(),
        },
        Utc::now(),
    )
}

/// Whether a preflight completion is the start of a real task dispatch.
///
/// Mirrors `DirectPrompt`: the same events that forward a prompt to execution
/// are the ones the ledger records, so the ledger cannot hold an item for work
/// that was never dispatched.
fn accepts_dispatch(trigger: &Event) -> bool {
    if !trigger.throttle.permits_mutation() {
        return false;
    }
    if WorkflowType::from_payload(&trigger.payload) != WorkflowType::Task {
        return false;
    }
    trigger.parse_payload::<PreflightCompletedPayload>().ok().is_some_and(|p| {
        p.all_passed
            && p.chain
                .prompt
                .as_ref()
                .and_then(serde_json::Value::as_str)
                .is_some_and(|prompt| !prompt.is_empty())
    })
}

/// Records a dispatched unit of work in the ledger, `running`, before the
/// coding agent is invoked.
///
/// Mutator — sinks on `PreflightCompleted`, and only on the task-workflow
/// events `DirectPrompt` forwards to execution.
///
/// **Registration order matters**: this block must be registered before
/// `DirectPrompt`, because the engine runs the blocks matching one event in
/// registration order. That is what puts the item in the store in state
/// `running` before `DirectPrompt` emits `PlanCompleted` and `ExecutePlan`
/// invokes the agent.
pub struct RecordWorkItem {
    store_path: PathBuf,
    /// Serialises this block's read-modify-write of the ledger file. The file
    /// stays authoritative — the lock holds no state, it only keeps two
    /// concurrent workflows from clobbering each other's save.
    gate: Mutex<()>,
}

impl RecordWorkItem {
    /// Record dispatches in the ledger at `store_path`.
    #[must_use]
    pub fn new(store_path: PathBuf) -> Self {
        Self {
            store_path,
            gate: Mutex::new(()),
        }
    }
}

impl SimulatedSuccess for RecordWorkItem {
    type Outcome = WorkItem;

    fn simulate(&self, trigger: &Event) -> WorkItem {
        // accepts() has already filtered everything but a real task dispatch,
        // so a parse failure here is not reachable; an empty objective is the
        // honest synthetic stand-in if it ever were.
        let payload = trigger.parse_payload::<PreflightCompletedPayload>().ok();
        payload.map_or_else(
            || {
                WorkItem::dispatched(
                    WorkItemSpec {
                        project: trigger.project.clone(),
                        objective: String::new(),
                        kind: WorkItemKind::Task,
                        lane: WorkLane::Interactive,
                        origin: "foundry task".to_string(),
                        trace_id: trigger.trace_id.clone(),
                    },
                    Utc::now(),
                )
            },
            |payload| item_from_preflight(trigger, &payload),
        )
    }

    fn success_events(&self, trigger: &Event, outcome: &WorkItem) -> Vec<Event> {
        let mut submitted = outcome.clone();
        submitted.state = foundry_sdk::work_item::WorkItemState::Submitted;
        submitted.reason = "submitted".to_string();
        vec![
            work_item_event(EventType::WorkItemSubmitted, trigger, &submitted),
            work_item_event(EventType::WorkItemStarted, trigger, outcome),
        ]
    }
}

impl TaskBlock for RecordWorkItem {
    task_block_meta! {
        name: "Record Work Item",
        kind: Mutator,
        sinks_on: [PreflightCompleted],
    }

    dry_run_via_simulation!();

    fn accepts(&self, trigger: &Event) -> bool {
        accepts_dispatch(trigger)
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let payload = parse_payload!(trigger, PreflightCompletedPayload);
        let item = item_from_preflight(trigger, &payload);
        let stored = self.write(&item);
        let events = if stored {
            self.success_events(trigger, &item)
        } else {
            vec![]
        };
        let summary = if stored {
            format!("{}: recorded work item {}", trigger.project, item.id)
        } else {
            format!("{}: work item not recorded (ledger unavailable)", trigger.project)
        };
        Box::pin(async move { Ok(TaskBlockResult::success(summary, events)) })
    }
}

impl RecordWorkItem {
    /// Add `item` to the ledger. Returns whether it reached the file.
    fn write(&self, item: &WorkItem) -> bool {
        let Some(_guard) = lock(&self.gate, "work-item ledger") else {
            return false;
        };
        let Some(mut store) = load(&self.store_path) else {
            return false;
        };
        store.upsert(item.clone());
        save(&store, &self.store_path)
    }
}

/// Settles a ledger item from the task runner's typed terminal result.
///
/// Mutator — sinks on `TaskRunCompleted`. The mapping from verdict to settled
/// state lives in [`WorkItem::settle_from_task_run`], so this block only
/// decides *which* item settles and what the worktree looked like afterwards.
pub struct SettleWorkItem {
    store_path: PathBuf,
    gate: Mutex<()>,
}

impl SettleWorkItem {
    /// Settle items in the ledger at `store_path`.
    #[must_use]
    pub fn new(store_path: PathBuf) -> Self {
        Self {
            store_path,
            gate: Mutex::new(()),
        }
    }

    /// Settle the running item this result belongs to, and return it.
    ///
    /// `None` when the ledger holds no running item for the run — a task that
    /// started before the ledger existed, for one — or when the ledger cannot
    /// be read or written.
    fn settle(&self, trigger: &Event, result: &TaskRunCompletedPayload) -> Option<WorkItem> {
        let _guard = lock(&self.gate, "work-item ledger")?;
        let mut store = load(&self.store_path)?;
        let removed = worktree_removed(result);
        let item = store.running_for_settlement(trigger.trace_id.as_deref(), &trigger.project)?;
        item.settle_from_task_run(result, removed, Utc::now());
        let settled = item.clone();
        if save(&store, &self.store_path) {
            Some(settled)
        } else {
            None
        }
    }
}

/// Whether the run's isolated worktree is gone.
///
/// The finalize step removes it best-effort and reports nothing, so the only
/// honest reading is to look: the path is either still there or it is not.
/// `None` when the run recorded no worktree at all.
fn worktree_removed(result: &TaskRunCompletedPayload) -> Option<bool> {
    result
        .context
        .task_worktree
        .as_deref()
        .map(|path| !std::path::Path::new(path).exists())
}

impl SimulatedSuccess for SettleWorkItem {
    type Outcome = Option<WorkItem>;

    fn simulate(&self, trigger: &Event) -> Option<WorkItem> {
        let result = trigger.parse_payload::<TaskRunCompletedPayload>().ok()?;
        let objective = result
            .context
            .prompt
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        let (kind, lane, origin) =
            classify(&objective, result.context.campaign.as_deref(), result.context.campaign_cycle);
        let mut item = WorkItem::dispatched(
            WorkItemSpec {
                project: trigger.project.clone(),
                objective,
                kind,
                lane,
                origin,
                trace_id: trigger.trace_id.clone(),
            },
            Utc::now(),
        );
        item.settle_from_task_run(&result, worktree_removed(&result), Utc::now());
        Some(item)
    }

    fn success_events(&self, trigger: &Event, outcome: &Option<WorkItem>) -> Vec<Event> {
        outcome
            .as_ref()
            .map(|item| vec![work_item_event(EventType::WorkItemSettled, trigger, item)])
            .unwrap_or_default()
    }
}

impl TaskBlock for SettleWorkItem {
    task_block_meta! {
        name: "Settle Work Item",
        kind: Mutator,
        sinks_on: [TaskRunCompleted],
    }

    dry_run_via_simulation!();

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let result = parse_payload!(trigger, TaskRunCompletedPayload);
        let settled = self.settle(trigger, &result);
        let events = self.success_events(trigger, &settled);
        let summary = match &settled {
            Some(item) => {
                format!("{}: work item {} settled {:?}", trigger.project, item.id, item.state)
            }
            None => format!("{}: no ledger item to settle", trigger.project),
        };
        Box::pin(async move { Ok(TaskBlockResult::success(summary, events)) })
    }
}

/// Build one work-item lifecycle event.
fn work_item_event(event_type: EventType, trigger: &Event, item: &WorkItem) -> Event {
    super::event_from_infallible_payload(
        event_type,
        &trigger.project,
        trigger.throttle,
        &WorkItemEventPayload::from_item(item),
    )
}

/// Acquire `gate`, absorbing a poisoned lock rather than panicking.
///
/// `foundryd` is long-lived state: a poisoned ledger lock must degrade to "the
/// dispatch went unrecorded", never to a dead daemon.
fn lock<'a>(gate: &'a Mutex<()>, what: &str) -> Option<std::sync::MutexGuard<'a, ()>> {
    if let Ok(guard) = gate.lock() {
        return Some(guard);
    }
    // Best-effort: the ledger is advisory to a dispatch that is already under
    // way, and there is no caller who can act on this — but a poisoned lock
    // means every later dispatch goes unrecorded too, so it must be visible in
    // the log.
    tracing::warn!("{what} lock poisoned; dispatch left unrecorded");
    None
}

/// Load the ledger, absorbing a read fault.
fn load(path: &std::path::Path) -> Option<WorkItemStore> {
    match WorkItemStore::load(path) {
        Ok(store) => Some(store),
        Err(error) => {
            // Best-effort: a dispatch must not fail because its bookkeeping
            // file is unreadable. The fault is logged and the dispatch runs
            // unrecorded.
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "could not read the work-item ledger; dispatch left unrecorded"
            );
            None
        }
    }
}

/// Save the ledger, absorbing a write fault. Returns whether it was written.
fn save(store: &WorkItemStore, path: &std::path::Path) -> bool {
    match store.save(path) {
        Ok(()) => true,
        Err(error) => {
            // Best-effort: as with the read — the dispatch is already under way
            // and nothing downstream can act on a bookkeeping failure.
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "could not write the work-item ledger; dispatch left unrecorded"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::payload::{LoopContext, TaskRunCompletedPayload, TaskVerdict};
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::work_item::{WorkItemKind, WorkItemState, WorkItemStore, WorkLane};

    use super::{RecordWorkItem, SettleWorkItem};

    assert_block_meta!(
        RecordWorkItem::new(std::path::PathBuf::from("/tmp/work-items.json")),
        kind: Mutator,
        sinks_on: [PreflightCompleted],
    );

    /// `assert_block_meta!` may only be invoked once per module, so the
    /// second block's metadata is asserted by hand.
    #[test]
    fn settle_work_item_is_a_mutator_sinking_on_the_terminal_task_result() {
        let block = SettleWorkItem::new(std::path::PathBuf::from("/tmp/work-items.json"));
        assert_eq!(block.name(), "Settle Work Item");
        assert_eq!(block.kind(), foundry_sdk::task_block::BlockKind::Mutator);
        assert_eq!(block.sinks_on(), &[EventType::TaskRunCompleted]);
    }

    fn preflight(extra: &serde_json::Value) -> Event {
        let mut payload = serde_json::json!({
            "project": "alpha",
            "workflow": "task",
            "all_passed": true,
            "required_passed": true,
            "results": [],
            "prompt": "Add a --quiet flag.",
        });
        if let (Some(target), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }
        Event::new(EventType::PreflightCompleted, "alpha".to_string(), Throttle::Full, payload)
            .with_trace_id(Some("a".repeat(32)))
    }

    fn completion(verdict: TaskVerdict, landed: bool, worktree: Option<&str>) -> Event {
        let payload = TaskRunCompletedPayload {
            project: "alpha".to_string(),
            success: verdict.is_complete(),
            landed,
            summary: "task summary".to_string(),
            preservation_ref: Some("ref-or-commit".to_string()),
            verdict,
            context: LoopContext {
                task_worktree: worktree.map(str::to_string),
                ..LoopContext::default()
            },
        };
        Event::new(
            EventType::TaskRunCompleted,
            "alpha".to_string(),
            Throttle::Full,
            Event::serialize_payload(&payload).unwrap(),
        )
        .with_trace_id(Some("a".repeat(32)))
    }

    // --- routing -----------------------------------------------------------

    #[test]
    fn accepts_returns_true_for_a_task_dispatch_that_cleared_preflight() {
        assert!(
            RecordWorkItem::new("/tmp/x.json".into()).accepts(&preflight(&serde_json::json!({})))
        );
    }

    #[test]
    fn accepts_returns_false_for_a_non_task_workflow() {
        let trigger = preflight(&serde_json::json!({"workflow": "iterate"}));
        assert!(!RecordWorkItem::new("/tmp/x.json".into()).accepts(&trigger));
    }

    #[test]
    fn accepts_returns_false_when_preflight_failed() {
        let trigger = preflight(&serde_json::json!({"all_passed": false}));
        assert!(!RecordWorkItem::new("/tmp/x.json".into()).accepts(&trigger));
    }

    #[test]
    fn accepts_returns_false_when_there_is_no_prompt_to_dispatch() {
        let mut trigger = preflight(&serde_json::json!({}));
        trigger.payload.as_object_mut().unwrap().remove("prompt");
        assert!(!RecordWorkItem::new("/tmp/x.json".into()).accepts(&trigger));
    }

    #[test]
    fn accepts_returns_false_for_a_dry_run() {
        let mut trigger = preflight(&serde_json::json!({}));
        trigger.throttle = Throttle::DryRun;
        assert!(
            !RecordWorkItem::new("/tmp/x.json".into()).accepts(&trigger),
            "a dry run dispatches nothing, so the ledger must hold nothing"
        );
    }

    #[test]
    fn accepts_returns_false_for_a_malformed_payload() {
        let trigger = Event::new(
            EventType::PreflightCompleted,
            "alpha".to_string(),
            Throttle::Full,
            serde_json::json!({"workflow": "task", "nonsense": true}),
        );
        assert!(!RecordWorkItem::new("/tmp/x.json".into()).accepts(&trigger));
    }

    // --- recording ---------------------------------------------------------

    #[tokio::test]
    async fn a_task_dispatch_is_recorded_running_and_announced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let block = RecordWorkItem::new(path.clone());

        let result = block.execute(&preflight(&serde_json::json!({}))).await.unwrap();

        let store = WorkItemStore::load(&path).unwrap();
        assert_eq!(store.items.len(), 1);
        let item = &store.items[0];
        assert_eq!(item.state, WorkItemState::Running);
        assert_eq!(item.kind, WorkItemKind::Task);
        assert_eq!(item.lane, WorkLane::Interactive);
        assert_eq!(item.origin, "foundry task");
        assert_eq!(item.objective, "Add a --quiet flag.");
        assert_eq!(item.trace_id.as_deref(), Some("a".repeat(32).as_str()));

        let types: Vec<&EventType> = result.events.iter().map(|e| &e.event_type).collect();
        assert_eq!(types, vec![&EventType::WorkItemSubmitted, &EventType::WorkItemStarted]);
        assert_eq!(result.events[0].payload["state"], "submitted");
        assert_eq!(result.events[1].payload["state"], "running");
        assert_eq!(result.events[1].payload["item_id"], item.id.as_str());
        assert_eq!(result.events[1].payload["lane"], "interactive");
        assert_eq!(result.events[1].payload["kind"], "task");
        assert_eq!(result.events[1].payload["origin"], "foundry task");
    }

    #[tokio::test]
    async fn a_campaign_cycle_records_its_campaign_and_cycle_as_the_origin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let block = RecordWorkItem::new(path.clone());

        block
            .execute(&preflight(&serde_json::json!({"campaign": "tidy-cli", "campaign_cycle": 3})))
            .await
            .unwrap();

        let item = WorkItemStore::load(&path).unwrap().items.remove(0);
        assert_eq!(item.kind, WorkItemKind::CampaignCycle);
        assert_eq!(item.lane, WorkLane::Campaign);
        assert_eq!(item.origin, "campaign tidy-cli cycle 3");
    }

    #[tokio::test]
    async fn a_major_upgrade_objective_records_the_maintenance_lane() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let objective = crate::dependency_updates::majors::objective(
            "alpha",
            &foundry_sdk::payload::PlannedUpdate {
                ecosystem: foundry_sdk::payload::Ecosystem::Cargo,
                manifest: ".".to_string(),
                package: "serde".to_string(),
                from: "1.0.0".to_string(),
                to: "2.0.0".to_string(),
                class: foundry_sdk::payload::UpdateClass::Major,
                change: foundry_sdk::payload::ChangeKind::Manifest,
                security: None,
                beyond_policy: false,
                beyond_hold: false,
            },
        );
        let block = RecordWorkItem::new(path.clone());

        block
            .execute(&preflight(&serde_json::json!({"prompt": objective})))
            .await
            .unwrap();

        let item = WorkItemStore::load(&path).unwrap().items.remove(0);
        assert_eq!(item.kind, WorkItemKind::MajorUpgrade);
        assert_eq!(item.lane, WorkLane::Maintenance);
        assert_eq!(item.origin, "nightly majors lane");
    }

    #[tokio::test]
    async fn an_unwritable_ledger_neither_fails_the_dispatch_nor_announces_an_item() {
        let block = RecordWorkItem::new(std::path::PathBuf::from(
            "/proc/foundry-does-not-exist/work-items.json",
        ));

        let result = block.execute(&preflight(&serde_json::json!({}))).await.unwrap();

        assert!(result.success, "a ledger fault must not fail the dispatch");
        assert!(result.events.is_empty(), "nothing was recorded, so nothing is announced");
    }

    #[tokio::test]
    async fn a_malformed_ledger_neither_fails_the_dispatch_nor_overwrites_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        std::fs::write(&path, "{ not json").unwrap();
        let block = RecordWorkItem::new(path.clone());

        let result = block.execute(&preflight(&serde_json::json!({}))).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn dry_run_announces_the_item_the_dispatch_would_open() {
        let block = RecordWorkItem::new("/tmp/never-written.json".into());
        let events = block.dry_run_events(&preflight(&serde_json::json!({})));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, EventType::WorkItemSubmitted);
        assert_eq!(events[1].payload["state"], "running");
    }

    // --- settling ----------------------------------------------------------

    async fn record_then_settle(trigger: Event) -> (WorkItemStore, Vec<Event>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        RecordWorkItem::new(path.clone())
            .execute(&preflight(&serde_json::json!({})))
            .await
            .unwrap();
        let result = SettleWorkItem::new(path.clone()).execute(&trigger).await.unwrap();
        (WorkItemStore::load(&path).unwrap(), result.events)
    }

    #[tokio::test]
    async fn a_complete_run_settles_its_item_landed_and_announces_it() {
        let (store, events) =
            record_then_settle(completion(TaskVerdict::Complete, true, Some("/nope/gone"))).await;

        let item = &store.items[0];
        assert_eq!(item.state, WorkItemState::Landed);
        assert!(item.settled_at.is_some());
        let disposition = item.disposition.clone().unwrap();
        assert_eq!(disposition.landed_commit.as_deref(), Some("ref-or-commit"));
        assert_eq!(disposition.worktree.as_deref(), Some("/nope/gone"));
        assert_eq!(disposition.worktree_removed, Some(true));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::WorkItemSettled);
        assert_eq!(events[0].payload["state"], "landed");
        assert_eq!(events[0].payload["item_id"], item.id.as_str());
        assert_eq!(events[0].payload["disposition"]["verdict"], "complete");
    }

    #[tokio::test]
    async fn a_pre_agent_runner_error_settles_the_item_failed_with_its_reason() {
        let verdict = TaskVerdict::RunnerError {
            detail: "task worktree already exists".to_string(),
        };
        let (store, events) = record_then_settle(completion(verdict, false, None)).await;

        assert_eq!(store.items[0].state, WorkItemState::Failed);
        assert_eq!(store.items[0].reason, "task worktree already exists");
        assert_eq!(events[0].payload["reason"], "task worktree already exists");
    }

    #[tokio::test]
    async fn a_surviving_worktree_is_recorded_as_not_removed() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().display().to_string();
        let (store, _) =
            record_then_settle(completion(TaskVerdict::Complete, true, Some(&worktree))).await;
        assert_eq!(store.items[0].disposition.clone().unwrap().worktree_removed, Some(false));
    }

    #[tokio::test]
    async fn a_completion_with_no_ledger_item_announces_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let result = SettleWorkItem::new(path)
            .execute(&completion(TaskVerdict::Complete, true, None))
            .await
            .unwrap();
        assert!(result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn settling_twice_leaves_the_first_settlement_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        RecordWorkItem::new(path.clone())
            .execute(&preflight(&serde_json::json!({})))
            .await
            .unwrap();
        let settle = SettleWorkItem::new(path.clone());
        let trigger = completion(TaskVerdict::Complete, true, None);
        settle.execute(&trigger).await.unwrap();
        let again = settle.execute(&trigger).await.unwrap();

        let store = WorkItemStore::load(&path).unwrap();
        assert_eq!(store.items.len(), 1);
        assert_eq!(store.items[0].state, WorkItemState::Landed);
        assert!(again.events.is_empty(), "a settled item is not settled twice");
    }

    #[test]
    fn dry_run_announces_the_settlement_a_completion_would_make() {
        let block = SettleWorkItem::new("/tmp/never-written.json".into());
        let events = block.dry_run_events(&completion(TaskVerdict::Complete, true, None));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::WorkItemSettled);
        assert_eq!(events[0].payload["state"], "landed");
    }
}
