//! The blocks that keep the work-item ledger in step with a task dispatch.
//!
//! [`RecordWorkItem`] opens the record from the task-workflow root event — the
//! `ExecutionRequested` that `foundry task`, a campaign cycle and the nightly
//! majors lane all emit — so a dispatch is in the ledger before anything in the
//! chain can fail. [`SettleFailedDispatch`] closes it when the chain stops
//! before the coding agent starts, and [`SettleWorkItem`] closes it from the
//! task runner's typed terminal result.
//!
//! None of them changes a dispatch. They observe the chain that already exists
//! and write a record beside it, and a ledger fault is absorbed rather than
//! propagated: an unwritable ledger must never be the reason a task does not
//! run.
//!
//! Every mutation here is a `load` → apply → `save` against the file, which
//! stays the single source of truth, and every one of them takes the same
//! process-wide gate ([`foundry_sdk::work_item::ledger_write_gate`]) so a
//! record in one spawned workflow cannot interleave with a settle in another.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    CharterCheckCompletedPayload, ExecutionRequestedPayload, PreflightCompletedPayload,
    TaskRunCompletedPayload, WorkItemEventPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::{
    WorkItem, WorkItemKind, WorkItemSpec, WorkItemStore, WorkLane, ledger_write_gate,
};
use foundry_sdk::workflow::WorkflowType;

use super::SimulatedSuccess;
use super::work_supersession::{prove_supersession, verified_commit};

/// How a task dispatch reached the engine, as the ledger records it.
///
/// Every task-kind dispatch funnels through the same `ExecutionRequested` root
/// event, so the lane is read off the context that dispatch already carries
/// rather than from a new marker field.
fn classify(
    objective: &str,
    campaign: Option<&str>,
    cycle: Option<u64>,
    operator_origin: Option<&str>,
) -> (WorkItemKind, WorkLane, String) {
    let (kind, lane, dispatch_origin) = classify_dispatch(objective, campaign, cycle);
    (kind, lane, with_operator_origin(dispatch_origin, operator_origin))
}

/// Append the opaque operator context to the origin the dispatch itself implies.
///
/// The ledger stores one `origin` string, so operator context is appended to the
/// dispatch origin rather than replacing it: a campaign cycle still says which
/// campaign and cycle it is. Nothing reads the result back apart — it is
/// display text, and neither the daemon nor the read RPCs parse it.
fn with_operator_origin(dispatch_origin: String, operator_origin: Option<&str>) -> String {
    match operator_origin.filter(|text| !text.is_empty()) {
        Some(operator) => format!("{dispatch_origin} ({operator})"),
        None => dispatch_origin,
    }
}

/// The origin, kind and lane the dispatch itself implies, before any operator
/// context is appended.
fn classify_dispatch(
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

/// The item a task dispatch opens, read off its root `ExecutionRequested`.
///
/// The typed source is whatever the root carries: `foundry task` names the
/// operator's host, a campaign advance names the campaign and cycle, the
/// majors lane names the sentinel that fired the nightly. A root that names
/// none (a raw emit, an older client) records none.
fn item_from_dispatch(trigger: &Event, payload: &ExecutionRequestedPayload) -> WorkItem {
    let (kind, lane, origin) = classify(
        &payload.prompt,
        payload.chain.campaign.as_deref(),
        payload.chain.campaign_cycle,
        payload.operator_origin.as_deref(),
    );
    WorkItem::dispatched(
        WorkItemSpec {
            project: trigger.project.clone(),
            objective: payload.prompt.clone(),
            kind,
            lane,
            origin,
            trace_id: trigger.trace_id.clone(),
        },
        Utc::now(),
    )
    .with_source(trigger.source.clone())
}

/// Whether `trigger` is the root event of a real task dispatch.
///
/// The task workflow is the one that runs a coding agent against an isolated
/// worktree and reports a typed verdict, so it is the one the ledger records. A
/// dry run dispatches nothing, so it records nothing.
fn accepts_dispatch(trigger: &Event) -> bool {
    if !trigger.throttle.permits_mutation() {
        return false;
    }
    if WorkflowType::from_payload(&trigger.payload) != WorkflowType::Task {
        return false;
    }
    trigger
        .parse_payload::<ExecutionRequestedPayload>()
        .is_ok_and(|p| !p.prompt.is_empty())
}

/// Records a dispatched unit of work in the ledger, `running`, from the root
/// event of its task workflow.
///
/// Mutator — sinks on `ExecutionRequested`, and only on the task-workflow
/// dispatches that run a coding agent.
///
/// Recording at the root rather than at `PreflightCompleted` is what makes the
/// ledger complete: a dispatch that fails its charter check, or fails preflight,
/// never reaches preflight's success event, and an unrecorded dispatch is
/// invisible to anyone reading the ledger for what still needs a person.
pub struct RecordWorkItem {
    store_path: PathBuf,
    /// Read only to answer "is this project one Foundry runs?" — a dispatch the
    /// engine refuses outright is not work, and must not leave an item the
    /// ledger can never settle.
    registry: Arc<RwLock<Registry>>,
}

impl RecordWorkItem {
    /// Record dispatches in the ledger at `store_path`.
    #[must_use]
    pub fn new(store_path: PathBuf, registry: Arc<RwLock<Registry>>) -> Self {
        Self {
            store_path,
            registry,
        }
    }

    /// Whether the dispatch names a project in the registry.
    ///
    /// The task chain's first block fails a dispatch for an unknown project
    /// without emitting any domain event, so nothing downstream could ever
    /// settle an item recorded for one: it would sit `running` until a restart
    /// closed it with the wrong reason. Recording is therefore declined here
    /// rather than settled later.
    fn project_is_registered(&self, project: &str) -> bool {
        match super::read_registry(&self.registry) {
            Ok(guard) => guard.find_project(project).is_some(),
            Err(error) => {
                // Best-effort: a poisoned registry lock is not the ledger's
                // fault, and declining every record would hide real work. Record
                // and let the restart sweep be the backstop.
                tracing::warn!(
                    error = %error,
                    "could not read the registry to check a dispatch; recording it anyway"
                );
                true
            }
        }
    }
}

impl SimulatedSuccess for RecordWorkItem {
    type Outcome = Option<WorkItem>;

    fn simulate(&self, trigger: &Event) -> Option<WorkItem> {
        if trigger.payload.get("admitted_work_item_id").is_some() {
            return None;
        }
        // accepts() has already filtered everything but a real task dispatch,
        // so a parse failure here is not reachable; an empty objective is the
        // honest synthetic stand-in if it ever were.
        let payload = trigger.parse_payload::<ExecutionRequestedPayload>().ok();
        Some(payload.map_or_else(
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
                .with_source(trigger.source.clone())
            },
            |payload| item_from_dispatch(trigger, &payload),
        ))
    }

    fn success_events(&self, trigger: &Event, outcome: &Option<WorkItem>) -> Vec<Event> {
        let Some(outcome) = outcome else {
            return Vec::new();
        };
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
        sinks_on: [ExecutionRequested],
    }

    dry_run_via_simulation!();

    fn accepts(&self, trigger: &Event) -> bool {
        // ResumeWorkItem admits its child atomically before dispatch; recording
        // that root again would mint a second identity on the same trace.
        accepts_dispatch(trigger)
            && trigger.payload.get("admitted_work_item_id").is_none()
            && self.project_is_registered(&trigger.project)
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let payload = parse_payload!(trigger, ExecutionRequestedPayload);
        let item = item_from_dispatch(trigger, &payload);
        let stored = self.write(&item);
        let events = if stored {
            self.success_events(trigger, &Some(item.clone()))
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
        let Some(_guard) = ledger_lock() else {
            return false;
        };
        let Some(mut store) = load_ledger(&self.store_path) else {
            return false;
        };
        store.upsert(item.clone());
        save_ledger(&store, &self.store_path)
    }
}

/// Why a task dispatch stopped before its coding agent started, read off the
/// event that stopped it.
///
/// `None` when the event is not a task-workflow failure — the same condition
/// [`SettleFailedDispatch::accepts`] filters on.
fn pre_agent_failure(trigger: &Event) -> Option<String> {
    if WorkflowType::from_payload(&trigger.payload) != WorkflowType::Task {
        return None;
    }
    match trigger.event_type {
        EventType::CharterCheckCompleted => {
            let p = trigger.parse_payload::<CharterCheckCompletedPayload>().ok()?;
            (!p.success).then(|| format!("charter check failed: {}", p.guidance))
        }
        EventType::PreflightCompleted => {
            let p = trigger.parse_payload::<PreflightCompletedPayload>().ok()?;
            if p.all_passed {
                return None;
            }
            let failed: Vec<&str> = p
                .results
                .iter()
                .filter(|result| !result.passed)
                .map(|result| result.name.as_str())
                .collect();
            Some(if failed.is_empty() {
                "preflight gates failed".to_string()
            } else {
                format!("preflight gates failed: {}", failed.join(", "))
            })
        }
        _ => None,
    }
}

/// Settles a ledger item for a dispatch that stopped before the coding agent
/// started.
///
/// Mutator — sinks on the two task-workflow events that stop a chain ahead of
/// execution: a failed charter check and a failed preflight. Without this, such
/// a dispatch would stay `running` in the ledger until the next daemon restart
/// settled it with the wrong reason.
pub struct SettleFailedDispatch {
    store_path: PathBuf,
}

impl SettleFailedDispatch {
    /// Settle abandoned dispatches in the ledger at `store_path`.
    #[must_use]
    pub fn new(store_path: PathBuf) -> Self {
        Self { store_path }
    }
}

impl SimulatedSuccess for SettleFailedDispatch {
    type Outcome = Option<WorkItem>;

    fn simulate(&self, trigger: &Event) -> Option<WorkItem> {
        let reason = pre_agent_failure(trigger)?;
        let mut item = WorkItem::dispatched(
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
        .with_source(trigger.source.clone());
        item.settle_failed(&reason, Utc::now());
        Some(item)
    }

    fn success_events(&self, trigger: &Event, outcome: &Option<WorkItem>) -> Vec<Event> {
        outcome
            .as_ref()
            .map(|item| vec![work_item_event(EventType::WorkItemSettled, trigger, item)])
            .unwrap_or_default()
    }
}

impl TaskBlock for SettleFailedDispatch {
    task_block_meta! {
        name: "Settle Failed Dispatch",
        kind: Mutator,
        sinks_on: [CharterCheckCompleted, PreflightCompleted],
    }

    dry_run_via_simulation!();

    fn accepts(&self, trigger: &Event) -> bool {
        trigger.throttle.permits_mutation() && pre_agent_failure(trigger).is_some()
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        // Defensive: accepts() filters every event that is not a task-workflow
        // failure before dispatch.
        let Some(reason) = pre_agent_failure(trigger) else {
            let summary = format!("{}: no failed dispatch to settle", trigger.project);
            return skip!(summary);
        };
        let settled = settle_failed_in_ledger(&self.store_path, trigger, &reason);
        let events = self.success_events(trigger, &settled);
        let summary = match &settled {
            Some(item) => format!("{}: work item {} settled failed", trigger.project, item.id),
            None => format!("{}: no ledger item to settle", trigger.project),
        };
        Box::pin(async move { Ok(TaskBlockResult::success(summary, events)) })
    }
}

/// Settle this run's `running` item `failed` with `reason`, and return it.
fn settle_failed_in_ledger(path: &Path, trigger: &Event, reason: &str) -> Option<WorkItem> {
    let _guard = ledger_lock()?;
    let mut store = load_ledger(path)?;
    let item = store.running_for_settlement(trigger.trace_id.as_deref(), &trigger.project)?;
    item.settle_failed(reason, Utc::now());
    let settled = item.clone();
    save_ledger(&store, path).then_some(settled)
}

/// Settles a ledger item from the task runner's typed terminal result.
///
/// Mutator — sinks on `TaskRunCompleted`. The mapping from verdict to settled
/// state lives in [`WorkItem::settle_from_task_run`], so this block only
/// decides *which* item settles and what the worktree looked like afterwards.
pub struct SettleWorkItem {
    store_path: PathBuf,
    registry: Option<Arc<RwLock<Registry>>>,
}

impl SettleWorkItem {
    /// Settle items in the ledger at `store_path`.
    #[must_use]
    pub fn new(store_path: PathBuf) -> Self {
        Self {
            store_path,
            registry: None,
        }
    }

    /// Enable landing-triggered supersession against each project's registered trunk.
    #[must_use]
    pub fn with_registry(store_path: PathBuf, registry: Arc<RwLock<Registry>>) -> Self {
        Self {
            store_path,
            registry: Some(registry),
        }
    }

    async fn verify_supersession(
        &self,
        trigger: &Event,
        result: &TaskRunCompletedPayload,
    ) -> (Vec<(WorkItem, String)>, Vec<String>) {
        if !result.landed {
            return (Vec::new(), Vec::new());
        }
        let Some(registry) = &self.registry else {
            return (Vec::new(), Vec::new());
        };
        let project = match registry.read() {
            Ok(registry) => registry.find_project(&trigger.project).cloned(),
            Err(error) => {
                return (
                    Vec::new(),
                    vec![format!("supersession unresolved: registry lock: {error}")],
                );
            }
        };
        let Some(project) = project else {
            return (Vec::new(), vec!["supersession unresolved: project is not registered".into()]);
        };
        let candidates = {
            let Some(_guard) = ledger_lock() else {
                return (Vec::new(), Vec::new());
            };
            let Some(mut store) = load_ledger(&self.store_path) else {
                return (Vec::new(), Vec::new());
            };
            let Some(running) =
                store.running_for_settlement(trigger.trace_id.as_deref(), &trigger.project)
            else {
                return (Vec::new(), Vec::new());
            };
            // The exact resume parent already settles through the existing linked
            // continuation proof. Supersession checks the remaining obligations.
            let resumes = running.resumes.clone();
            store
                .items
                .into_iter()
                .filter(|item| {
                    item.project == trigger.project
                        && resumes.as_deref() != Some(item.id.as_str())
                        && item.state == foundry_sdk::work_item::WorkItemState::Preserved
                })
                .collect::<Vec<_>>()
        };
        let path = Path::new(&project.path);
        let trunk = verified_commit(path, &format!("refs/heads/{}", project.branch)).await;
        let mut verified = Vec::new();
        let mut diagnostics = Vec::new();
        for item in candidates {
            let proof = match &trunk {
                Ok(trunk) => prove_supersession(path, trunk, &item).await,
                Err(error) => Err(anyhow::anyhow!("registered trunk unavailable: {error}")),
            };
            match proof {
                Ok(commit) => verified.push((item, commit)),
                Err(error) => {
                    // Best-effort: unresolved evidence leaves the obligation open;
                    // bookkeeping must never interrupt a task that already landed.
                    tracing::warn!(item_id = %item.id, project = %item.project,
                        trace_id = ?item.trace_id, error = %error, "supersession unresolved");
                    diagnostics.push(format!("supersession unresolved for {}: {error}", item.id));
                }
            }
        }
        (verified, diagnostics)
    }

    /// Settle the running item this result belongs to, and return it.
    ///
    /// `None` when the ledger holds no running item for the run — a task that
    /// started before the ledger existed, for one — or when the ledger cannot
    /// be read or written.
    fn settle(
        &self,
        trigger: &Event,
        result: &TaskRunCompletedPayload,
        verified: &[(WorkItem, String)],
    ) -> Option<(WorkItem, Vec<WorkItem>)> {
        let _guard = ledger_lock()?;
        let mut store = load_ledger(&self.store_path)?;
        let removed = worktree_removed(result);
        let item = store.running_for_settlement(trigger.trace_id.as_deref(), &trigger.project)?;
        item.settle_from_task_run(result, removed, Utc::now());
        let settled = item.clone();
        let parent =
            if result.landed && settled.state == foundry_sdk::work_item::WorkItemState::Landed {
                settled
                    .resumes
                    .as_deref()
                    .and_then(|id| {
                        store.items.iter_mut().find(|item| {
                            item.id == id
                                && item.project == settled.project
                                && item.state == foundry_sdk::work_item::WorkItemState::Preserved
                        })
                    })
                    .and_then(|parent| {
                        let commit = result
                            .preservation_ref
                            .as_ref()
                            .filter(|commit| !commit.trim().is_empty())?;
                        let disposition = parent.disposition.as_mut()?;
                        disposition.landed_commit = Some(commit.clone());
                        parent.settle_landed("resumed work landed", Utc::now());
                        Some(parent.clone())
                    })
            } else {
                None
            };
        let mut additional: Vec<_> = parent.into_iter().collect();
        for (candidate, commit) in verified.iter().filter(|_| {
            result.landed
                && settled.project == trigger.project
                && settled.state == foundry_sdk::work_item::WorkItemState::Landed
        }) {
            // Recheck the entire snapshot under the owner-controls write gate.
            // An owner cancellation or another writer must win over stale Git evidence.
            if let Some(item) = store.items.iter_mut().find(|item| **item == *candidate)
                && let Some(disposition) = item.disposition.as_mut()
            {
                disposition.landed_commit = Some(commit.clone());
                item.settle_landed(&format!("superseded by {commit}"), Utc::now());
                additional.push(item.clone());
            }
        }
        save_ledger(&store, &self.store_path).then_some((settled, additional))
    }
}

/// Whether the run's isolated worktree is gone.
///
/// The finalize step removes it best-effort and reports nothing, so the only
/// honest reading is to look: the path is either still there or it is not.
/// `None` when the run recorded no worktree at all.
fn worktree_removed(result: &TaskRunCompletedPayload) -> Option<bool> {
    result.context.task_worktree.as_deref().map(|path| !Path::new(path).exists())
}

impl SimulatedSuccess for SettleWorkItem {
    type Outcome = Option<(WorkItem, Option<WorkItem>)>;

    fn simulate(&self, trigger: &Event) -> Self::Outcome {
        let result = trigger.parse_payload::<TaskRunCompletedPayload>().ok()?;
        let objective = result
            .context
            .prompt
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string();
        // A settle simulation reconstructs a synthetic item from a result
        // event, which carries no operator context — the recorded item it
        // stands in for already holds whatever origin the dispatch recorded.
        let (kind, lane, origin) = classify(
            &objective,
            result.context.campaign.as_deref(),
            result.context.campaign_cycle,
            None,
        );
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
        )
        .with_source(trigger.source.clone());
        item.settle_from_task_run(&result, worktree_removed(&result), Utc::now());
        Some((item, None))
    }

    fn success_events(&self, trigger: &Event, outcome: &Self::Outcome) -> Vec<Event> {
        outcome.as_ref().map_or_else(Vec::new, |(child, parent)| {
            std::iter::once(child)
                .chain(parent.iter())
                .map(|item| {
                    work_item_event(EventType::WorkItemSettled, trigger, item)
                        .with_trace_id(item.trace_id.clone())
                })
                .collect()
        })
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
        let trigger = trigger.clone();
        Box::pin(async move {
            let (verified, diagnostics) = self.verify_supersession(&trigger, &result).await;
            let mut settled = self.settle(&trigger, &result, &verified);
            if let Some((child, additional)) = &mut settled {
                for item in std::iter::once(child).chain(additional.iter_mut()) {
                    super::work_branch_cleanup::cleanup(
                        &self.store_path,
                        self.registry.as_ref(),
                        item,
                    )
                    .await;
                }
            }
            let events = settled.as_ref().map_or_else(Vec::new, |(child, additional)| {
                std::iter::once(child)
                    .chain(additional.iter())
                    .map(|item| {
                        work_item_event(EventType::WorkItemSettled, &trigger, item)
                            .with_trace_id(item.trace_id.clone())
                    })
                    .collect()
            });
            let summary = match &settled {
                Some((item, _)) => {
                    format!("{}: work item {} settled {:?}", trigger.project, item.id, item.state)
                }
                None => format!("{}: no ledger item to settle", trigger.project),
            };
            let summary = if diagnostics.is_empty() {
                summary
            } else {
                format!("{summary}; {}", diagnostics.join("; "))
            };
            Ok(TaskBlockResult::success(summary, events))
        })
    }
}

/// Build one work-item lifecycle event.
///
/// Shared with [`super::run_ledger`] so every kind of item reports itself in
/// the same payload shape.
pub(super) fn work_item_event(event_type: EventType, trigger: &Event, item: &WorkItem) -> Event {
    super::event_from_infallible_payload(
        event_type,
        &trigger.project,
        trigger.throttle,
        &WorkItemEventPayload::from_item(item),
    )
}

/// Acquire the process-wide ledger write gate, absorbing a poisoned lock rather
/// than panicking.
///
/// `foundryd` is long-lived state: a poisoned ledger lock must degrade to "the
/// dispatch went unrecorded", never to a dead daemon.
pub(super) fn ledger_lock() -> Option<std::sync::MutexGuard<'static, ()>> {
    match ledger_write_gate().lock() {
        Ok(guard) => Some(guard),
        Err(_poisoned) => {
            // Best-effort: the ledger is advisory to a dispatch that is already
            // under way, and there is no caller who can act on this — but a
            // poisoned lock means every later mutation is skipped too, so it
            // must be visible in the log.
            tracing::warn!("work-item ledger lock poisoned; dispatch left unrecorded");
            None
        }
    }
}

/// Load the ledger, absorbing a read fault.
pub(super) fn load_ledger(path: &Path) -> Option<WorkItemStore> {
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
pub(super) fn save_ledger(store: &WorkItemStore, path: &Path) -> bool {
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
    use chrono::Utc;
    use std::sync::Arc;

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::payload::{LoopContext, TaskRunCompletedPayload, TaskVerdict};
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::work_item::{
        WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
    };
    use foundry_sdk::work_source::WorkSource;

    use super::super::test_helpers;
    use super::{RecordWorkItem, SettleFailedDispatch, SettleWorkItem, SimulatedSuccess};

    /// A recorder over `path`, with "alpha" the one project in the registry.
    fn recorder(path: &str) -> RecordWorkItem {
        RecordWorkItem::new(
            std::path::PathBuf::from(path),
            test_helpers::registry_with_project("alpha", "/tmp/alpha"),
        )
    }

    assert_block_meta!(
        recorder("/tmp/work-items.json"),
        kind: Mutator,
        sinks_on: [ExecutionRequested],
    );

    /// `assert_block_meta!` may only be invoked once per module, so the other
    /// blocks' metadata is asserted by hand.
    #[test]
    fn settle_work_item_is_a_mutator_sinking_on_the_terminal_task_result() {
        let block = SettleWorkItem::new(std::path::PathBuf::from("/tmp/work-items.json"));
        assert_eq!(block.name(), "Settle Work Item");
        assert_eq!(block.kind(), foundry_sdk::task_block::BlockKind::Mutator);
        assert_eq!(block.sinks_on(), &[EventType::TaskRunCompleted]);
    }

    #[test]
    fn settle_failed_dispatch_is_a_mutator_sinking_on_the_two_pre_agent_stops() {
        let block = SettleFailedDispatch::new(std::path::PathBuf::from("/tmp/work-items.json"));
        assert_eq!(block.name(), "Settle Failed Dispatch");
        assert_eq!(block.kind(), foundry_sdk::task_block::BlockKind::Mutator);
        assert_eq!(
            block.sinks_on(),
            &[
                EventType::CharterCheckCompleted,
                EventType::PreflightCompleted
            ]
        );
    }

    const TRACE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// A task-workflow root dispatch, the event `RecordWorkItem` records from.
    fn dispatch(extra: &serde_json::Value) -> Event {
        let mut payload = serde_json::json!({
            "project": "alpha",
            "workflow": "task",
            "prompt": "Add a --quiet flag.",
        });
        if let (Some(target), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }
        Event::new(EventType::ExecutionRequested, "alpha".to_string(), Throttle::Full, payload)
            .with_trace_id(Some(TRACE.to_string()))
    }

    /// A failed charter check in the task workflow.
    fn charter_failed(success: bool) -> Event {
        Event::new(
            EventType::CharterCheckCompleted,
            "alpha".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "alpha",
                "success": success,
                "sources": [],
                "guidance": "Add a CHARTER.md describing the project's intent.",
                "workflow": "task",
                "prompt": "Add a --quiet flag.",
            }),
        )
        .with_trace_id(Some(TRACE.to_string()))
    }

    /// A failed preflight in the task workflow, naming one failing gate.
    fn preflight_failed(workflow: &str) -> Event {
        Event::new(
            EventType::PreflightCompleted,
            "alpha".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "alpha",
                "workflow": workflow,
                "all_passed": false,
                "required_passed": false,
                "results": [
                    {"name": "clippy", "command": "cargo clippy", "passed": false,
                     "required": true, "output": "", "exit_code": 101},
                    {"name": "fmt", "command": "cargo fmt", "passed": true,
                     "required": true, "output": "", "exit_code": 0}
                ],
                "prompt": "Add a --quiet flag.",
            }),
        )
        .with_trace_id(Some(TRACE.to_string()))
    }

    fn completion(verdict: TaskVerdict, landed: bool, worktree: Option<&str>) -> Event {
        let payload = TaskRunCompletedPayload {
            project: "alpha".to_string(),
            success: verdict.is_complete(),
            landed,
            summary: "task summary".to_string(),
            preservation_ref: Some("ref-or-commit".to_string()),
            land_blocked: None,
            proof_evidence: None,
            trunk_arrivals: Vec::new(),
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
        .with_trace_id(Some(TRACE.to_string()))
    }

    // --- routing -----------------------------------------------------------

    #[test]
    fn accepts_returns_true_for_a_task_workflow_dispatch() {
        assert!(recorder("/tmp/x.json").accepts(&dispatch(&serde_json::json!({}))));
    }

    #[test]
    fn accepts_returns_false_for_a_non_task_workflow() {
        let trigger = dispatch(&serde_json::json!({"workflow": "prompt"}));
        assert!(
            !recorder("/tmp/x.json").accepts(&trigger),
            "only the task workflow runs a coding agent and reports a verdict"
        );
    }

    #[test]
    fn accepts_returns_false_when_there_is_no_prompt_to_dispatch() {
        let mut trigger = dispatch(&serde_json::json!({}));
        trigger.payload.as_object_mut().unwrap().remove("prompt");
        assert!(!recorder("/tmp/x.json").accepts(&trigger));
    }

    #[test]
    fn accepts_returns_false_for_a_project_the_registry_does_not_know() {
        let mut trigger = dispatch(&serde_json::json!({}));
        trigger.project = "not-registered".to_string();
        trigger.payload["project"] = serde_json::json!("not-registered");
        assert!(
            !recorder("/tmp/x.json").accepts(&trigger),
            "the chain refuses an unknown project without emitting anything that could settle an item"
        );
    }

    #[test]
    fn accepts_returns_false_for_a_dry_run() {
        let mut trigger = dispatch(&serde_json::json!({}));
        trigger.throttle = Throttle::DryRun;
        assert!(
            !recorder("/tmp/x.json").accepts(&trigger),
            "a dry run dispatches nothing, so the ledger must hold nothing"
        );
    }

    #[test]
    fn accepts_returns_false_for_a_malformed_payload() {
        let trigger = Event::new(
            EventType::ExecutionRequested,
            "alpha".to_string(),
            Throttle::Full,
            serde_json::json!({"workflow": "task", "nonsense": true}),
        );
        assert!(!recorder("/tmp/x.json").accepts(&trigger));
    }

    #[test]
    fn accepts_returns_true_for_a_task_charter_failure_and_false_for_a_pass() {
        let block = SettleFailedDispatch::new("/tmp/x.json".into());
        assert!(block.accepts(&charter_failed(false)));
        assert!(!block.accepts(&charter_failed(true)), "a passing charter stops nothing");
    }

    #[test]
    fn accepts_returns_false_for_a_charter_failure_outside_the_task_workflow() {
        let mut trigger = charter_failed(false);
        trigger.payload["workflow"] = serde_json::json!("iterate");
        assert!(
            !SettleFailedDispatch::new("/tmp/x.json".into()).accepts(&trigger),
            "the ledger holds no item for an iterate run, so there is nothing to settle"
        );
    }

    #[test]
    fn accepts_returns_true_for_a_failed_task_preflight_and_false_for_a_passing_one() {
        let block = SettleFailedDispatch::new("/tmp/x.json".into());
        assert!(block.accepts(&preflight_failed("task")));
        assert!(!block.accepts(&preflight_failed("iterate")));

        let mut passed = preflight_failed("task");
        passed.payload["all_passed"] = serde_json::json!(true);
        passed.payload["required_passed"] = serde_json::json!(true);
        assert!(!block.accepts(&passed), "a cleared preflight is not a failed dispatch");
    }

    #[test]
    fn dry_run_and_accepts_agree_on_skip_for_a_cleared_preflight() {
        let block = SettleFailedDispatch::new("/tmp/x.json".into());
        let mut passed = preflight_failed("task");
        passed.payload["all_passed"] = serde_json::json!(true);
        assert!(!block.accepts(&passed));
        assert!(
            block.dry_run_events(&passed).is_empty(),
            "simulate() must skip on the same condition accepts() rejects"
        );
        assert_eq!(block.dry_run_events(&preflight_failed("task")).len(), 1);
    }

    // --- recording ---------------------------------------------------------

    #[tokio::test]
    async fn a_task_dispatch_is_recorded_running_and_announced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let block = recorder(path.to_str().unwrap());

        let result = block.execute(&dispatch(&serde_json::json!({}))).await.unwrap();

        let store = WorkItemStore::load(&path).unwrap();
        assert_eq!(store.items.len(), 1);
        let item = &store.items[0];
        assert_eq!(item.state, WorkItemState::Running);
        assert_eq!(item.kind, WorkItemKind::Task);
        assert_eq!(item.lane, WorkLane::Interactive);
        assert_eq!(item.origin, "foundry task");
        assert_eq!(item.objective, "Add a --quiet flag.");
        assert_eq!(item.trace_id.as_deref(), Some(TRACE));

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
        let block = recorder(path.to_str().unwrap());

        block
            .execute(&dispatch(&serde_json::json!({"campaign": "tidy-cli", "campaign_cycle": 3})))
            .await
            .unwrap();

        let item = WorkItemStore::load(&path).unwrap().items.remove(0);
        assert_eq!(item.kind, WorkItemKind::CampaignCycle);
        assert_eq!(item.lane, WorkLane::Campaign);
        assert_eq!(item.origin, "campaign tidy-cli cycle 3");
    }

    // --- the typed source, one test per kind, each read back from the file ---

    #[tokio::test]
    async fn a_foundry_task_dispatch_records_the_operator_host_as_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let trigger = dispatch(&serde_json::json!({"operator_origin": "host workbench"}))
            .with_source(Some(WorkSource::operator("workbench")));

        let result = recorder(path.to_str().unwrap()).execute(&trigger).await.unwrap();

        let item = WorkItemStore::load(&path).unwrap().items.remove(0);
        assert_eq!(item.source, Some(WorkSource::operator("workbench")));
        assert_eq!(item.origin, "foundry task (host workbench)", "origin is untouched");
        for event in &result.events {
            assert_eq!(event.payload["source"]["kind"], "operator");
            assert_eq!(event.payload["source"]["ref"], "workbench");
            assert!(event.payload["source"].get("cycle").is_none());
        }
    }

    #[tokio::test]
    async fn a_campaign_cycle_records_the_campaign_and_its_cycle_number_as_its_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let trigger = dispatch(&serde_json::json!({"campaign": "tidy-cli", "campaign_cycle": 3}))
            .with_source(Some(WorkSource::campaign("tidy-cli", 3)));

        let result = recorder(path.to_str().unwrap()).execute(&trigger).await.unwrap();

        let item = WorkItemStore::load(&path).unwrap().items.remove(0);
        assert_eq!(item.source, Some(WorkSource::campaign("tidy-cli", 3)));
        assert_eq!(item.kind, WorkItemKind::CampaignCycle);
        assert_eq!(item.origin, "campaign tidy-cli cycle 3");
        assert_eq!(result.events[0].payload["source"]["cycle"], 3);
    }

    #[tokio::test]
    async fn a_majors_lane_upgrade_records_the_sentinel_that_fired_the_nightly() {
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
        let trigger = dispatch(&serde_json::json!({"prompt": objective}))
            .with_source(Some(WorkSource::sentinel("nightly-maintenance")));

        recorder(path.to_str().unwrap()).execute(&trigger).await.unwrap();

        let item = WorkItemStore::load(&path).unwrap().items.remove(0);
        assert_eq!(item.kind, WorkItemKind::MajorUpgrade);
        assert_eq!(item.source, Some(WorkSource::sentinel("nightly-maintenance")));
    }

    #[tokio::test]
    async fn a_dispatch_whose_root_names_no_source_records_none_and_announces_no_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");

        let result = recorder(path.to_str().unwrap())
            .execute(&dispatch(&serde_json::json!({})))
            .await
            .unwrap();

        let item = WorkItemStore::load(&path).unwrap().items.remove(0);
        assert_eq!(item.source, None);
        for event in &result.events {
            assert!(event.payload.get("source").is_none(), "no source, no key: {}", event.payload);
        }
    }

    #[test]
    fn dry_run_and_the_settlement_simulations_carry_the_triggers_source() {
        let block = recorder("/tmp/never-written.json");
        let trigger =
            dispatch(&serde_json::json!({})).with_source(Some(WorkSource::operator("workbench")));
        assert_eq!(
            block.simulate(&trigger).unwrap().source,
            Some(WorkSource::operator("workbench"))
        );

        let settle = SettleWorkItem::new("/tmp/never-written.json".into());
        let done = completion(TaskVerdict::Complete, true, None)
            .with_source(Some(WorkSource::campaign("tidy-cli", 2)));
        assert_eq!(
            settle.simulate(&done).unwrap().0.source,
            Some(WorkSource::campaign("tidy-cli", 2))
        );

        let failed = SettleFailedDispatch::new("/tmp/never-written.json".into());
        let stopped =
            charter_failed(false).with_source(Some(WorkSource::sentinel("nightly-maintenance")));
        assert_eq!(
            failed.simulate(&stopped).unwrap().source,
            Some(WorkSource::sentinel("nightly-maintenance"))
        );
    }

    #[tokio::test]
    async fn operator_origin_is_recorded_verbatim_beside_the_dispatch_origin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let block = recorder(path.to_str().unwrap());

        let result = block
            .execute(&dispatch(
                &serde_json::json!({"operator_origin": "host workbench: asked by Stacey"}),
            ))
            .await
            .unwrap();

        let item = WorkItemStore::load(&path).unwrap().items.remove(0);
        assert_eq!(item.origin, "foundry task (host workbench: asked by Stacey)");
        assert_eq!(
            result.events[0].payload["origin"],
            "foundry task (host workbench: asked by Stacey)"
        );
        assert_eq!(
            result.events[1].payload["origin"],
            "foundry task (host workbench: asked by Stacey)"
        );
    }

    #[tokio::test]
    async fn a_campaign_cycle_keeps_its_campaign_and_cycle_beside_the_operator_origin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let block = recorder(path.to_str().unwrap());

        block
            .execute(&dispatch(&serde_json::json!({
                "campaign": "tidy-cli",
                "campaign_cycle": 3,
                "operator_origin": "host workbench: by hand",
            })))
            .await
            .unwrap();

        let item = WorkItemStore::load(&path).unwrap().items.remove(0);
        assert_eq!(item.origin, "campaign tidy-cli cycle 3 (host workbench: by hand)");
    }

    #[test]
    fn a_dispatch_without_operator_origin_records_exactly_the_origins_it_always_did() {
        assert_eq!(super::classify("Add a flag.", None, None, None).2, "foundry task");
        assert_eq!(
            super::classify("Add a flag.", Some("tidy-cli"), Some(3), None).2,
            "campaign tidy-cli cycle 3"
        );
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
        assert_eq!(super::classify(&objective, None, None, None).2, "nightly majors lane");
        // An empty operator origin says nothing, so it adds nothing.
        assert_eq!(super::classify("Add a flag.", None, None, Some("")).2, "foundry task");
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
        let block = recorder(path.to_str().unwrap());

        block
            .execute(&dispatch(&serde_json::json!({"prompt": objective})))
            .await
            .unwrap();

        let item = WorkItemStore::load(&path).unwrap().items.remove(0);
        assert_eq!(item.kind, WorkItemKind::MajorUpgrade);
        assert_eq!(item.lane, WorkLane::Maintenance);
        assert_eq!(item.origin, "nightly majors lane");
    }

    #[tokio::test]
    async fn an_unwritable_ledger_neither_fails_the_dispatch_nor_announces_an_item() {
        let block = recorder("/proc/foundry-does-not-exist/work-items.json");

        let result = block.execute(&dispatch(&serde_json::json!({}))).await.unwrap();

        assert!(result.success, "a ledger fault must not fail the dispatch");
        assert!(result.events.is_empty(), "nothing was recorded, so nothing is announced");
    }

    #[tokio::test]
    async fn a_malformed_ledger_neither_fails_the_dispatch_nor_overwrites_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        std::fs::write(&path, "{ not json").unwrap();
        let block = recorder(path.to_str().unwrap());

        let result = block.execute(&dispatch(&serde_json::json!({}))).await.unwrap();

        assert!(result.success);
        assert!(result.events.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
    }

    #[test]
    fn dry_run_announces_the_item_the_dispatch_would_open() {
        let block = recorder("/tmp/never-written.json");
        let events = block.dry_run_events(&dispatch(&serde_json::json!({})));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, EventType::WorkItemSubmitted);
        assert_eq!(events[1].payload["state"], "running");
    }

    // --- settling a dispatch that stopped before the agent -----------------

    /// Record a dispatch, then hand `trigger` to `SettleFailedDispatch`.
    async fn record_then_abandon(trigger: Event) -> (WorkItemStore, Vec<Event>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        recorder(path.to_str().unwrap())
            .execute(&dispatch(&serde_json::json!({})))
            .await
            .unwrap();
        let result = SettleFailedDispatch::new(path.clone()).execute(&trigger).await.unwrap();
        (WorkItemStore::load(&path).unwrap(), result.events)
    }

    #[tokio::test]
    async fn a_failed_charter_check_settles_the_item_failed_with_its_guidance() {
        let (store, events) = record_then_abandon(charter_failed(false)).await;

        assert_eq!(store.items.len(), 1, "no second item is created");
        assert_eq!(store.items[0].state, WorkItemState::Failed);
        assert_eq!(
            store.items[0].reason,
            "charter check failed: Add a CHARTER.md describing the project's intent."
        );
        assert!(store.items[0].settled_at.is_some());

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::WorkItemSettled);
        assert_eq!(events[0].payload["state"], "failed");
        assert_eq!(events[0].payload["item_id"], store.items[0].id.as_str());
    }

    #[tokio::test]
    async fn a_failed_preflight_settles_the_item_failed_naming_the_failing_gate() {
        let (store, events) = record_then_abandon(preflight_failed("task")).await;

        assert_eq!(store.items[0].state, WorkItemState::Failed);
        assert_eq!(store.items[0].reason, "preflight gates failed: clippy");
        assert_eq!(events[0].payload["reason"], "preflight gates failed: clippy");
    }

    #[tokio::test]
    async fn a_failed_dispatch_on_another_trace_settles_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        recorder(path.to_str().unwrap())
            .execute(&dispatch(&serde_json::json!({})))
            .await
            .unwrap();
        let mut elsewhere = charter_failed(false);
        elsewhere.trace_id = Some("b".repeat(32));

        let result = SettleFailedDispatch::new(path.clone()).execute(&elsewhere).await.unwrap();

        assert!(result.events.is_empty());
        assert_eq!(
            WorkItemStore::load(&path).unwrap().items[0].state,
            WorkItemState::Running,
            "another workflow's failure must not settle this run's item"
        );
    }

    // --- settling from the terminal task result ----------------------------

    async fn record_then_settle(trigger: Event) -> (WorkItemStore, Vec<Event>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        recorder(path.to_str().unwrap())
            .execute(&dispatch(&serde_json::json!({})))
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
        recorder(path.to_str().unwrap())
            .execute(&dispatch(&serde_json::json!({})))
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

    // --- one write serialiser for every mutation ---------------------------

    /// A record in one workflow and a settle in another, run concurrently
    /// against the same file, repeatedly.
    ///
    /// Each iteration seeds a `running` item on its own trace, then drives a
    /// record of a *new* dispatch and a settle of the seeded one at the same
    /// time. Both mutations are a `load` → apply → `save`, so with a private
    /// lock per block they interleave and whichever saves last drops the
    /// other's change. Every iteration must afterwards show both: the new item
    /// present *and* the seeded one settled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_concurrent_record_and_settle_both_reach_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let record = Arc::new(recorder(path.to_str().unwrap()));
        let settle = Arc::new(SettleWorkItem::new(path.clone()));

        for iteration in 0..64u32 {
            let seeded_trace = format!("{iteration:032x}");
            let new_trace = format!("{:032x}", iteration + 1_000_000);

            let mut store = WorkItemStore::load(&path).unwrap();
            let seeded = WorkItem::dispatched(
                WorkItemSpec {
                    project: "alpha".to_string(),
                    objective: "seeded run".to_string(),
                    kind: WorkItemKind::Task,
                    lane: WorkLane::Interactive,
                    origin: "foundry task".to_string(),
                    trace_id: Some(seeded_trace.clone()),
                },
                chrono::Utc::now(),
            );
            let seeded_id = seeded.id.clone();
            store.upsert(seeded);
            store.save(&path).unwrap();

            let mut dispatched = dispatch(&serde_json::json!({}));
            dispatched.trace_id = Some(new_trace.clone());
            let mut done = completion(TaskVerdict::Complete, true, None);
            done.trace_id = Some(seeded_trace);

            let recorder = Arc::clone(&record);
            let settler = Arc::clone(&settle);
            let recording = tokio::spawn(async move { recorder.execute(&dispatched).await });
            let settling = tokio::spawn(async move { settler.execute(&done).await });
            recording.await.unwrap().unwrap();
            settling.await.unwrap().unwrap();

            let store = WorkItemStore::load(&path).unwrap();
            let fresh_item = store
                .items
                .iter()
                .find(|item| item.trace_id.as_deref() == Some(new_trace.as_str()))
                .unwrap_or_else(|| panic!("iteration {iteration}: the record was lost"));
            assert_eq!(fresh_item.state, WorkItemState::Running);
            let settled = store
                .find(&seeded_id)
                .unwrap_or_else(|| panic!("iteration {iteration}: the seeded item was lost"));
            assert_eq!(
                settled.state,
                WorkItemState::Landed,
                "iteration {iteration}: the settle was lost"
            );
        }
    }

    #[test]
    fn dry_run_and_accepts_agree_on_skip_for_an_admitted_resume() {
        let block = recorder("/tmp/unused-resume-ledger.json");
        let event = dispatch(&serde_json::json!({"admitted_work_item_id": "wi_child"}));
        assert!(!block.accepts(&event));
        assert!(block.simulate(&event).is_none());
        assert!(block.dry_run_events(&event).is_empty());
    }
    #[tokio::test]
    async fn supersession_reloads_after_owner_cancellation_and_keeps_unrelated_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        recorder(path.to_str().unwrap())
            .execute(&dispatch(&serde_json::json!({})))
            .await
            .unwrap();
        let mut store = WorkItemStore::load(&path).unwrap();
        let mut candidate = store.items[0].clone();
        candidate.id = "wi_preserved_candidate".into();
        candidate.settle_from_task_run(
            &completion(TaskVerdict::Remainder { gaps: vec![] }, false, None)
                .parse_payload::<TaskRunCompletedPayload>()
                .unwrap(),
            None,
            Utc::now(),
        );
        let verified = vec![(candidate.clone(), "verified-trunk-commit".to_string())];
        // The Git proof snapshot predates an owner mutation under the same gate.
        candidate.settle_cancelled("owner stopped", None, Utc::now());
        let unrelated = WorkItem::dispatched(
            foundry_sdk::work_item::WorkItemSpec {
                project: "other-project".into(),
                objective: "new unrelated update".into(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "owner".into(),
                trace_id: Some("b".repeat(32)),
            },
            Utc::now(),
        );
        {
            let _guard = foundry_sdk::work_item::ledger_write_gate().lock().unwrap();
            store.upsert(candidate.clone());
            store.upsert(unrelated.clone());
            store.save(&path).unwrap();
        }
        let trigger = completion(TaskVerdict::Complete, true, None);
        let result = trigger.parse_payload::<TaskRunCompletedPayload>().unwrap();
        let outcome =
            SettleWorkItem::new(path.clone()).settle(&trigger, &result, &verified).unwrap();
        assert!(outcome.1.is_empty());
        let after = WorkItemStore::load(&path).unwrap();
        assert_eq!(after.find(&candidate.id), Some(&candidate));
        assert_eq!(after.find(&unrelated.id), Some(&unrelated));
        assert_eq!(after.items[0].state, WorkItemState::Landed);
    }
}
