//! The blocks that keep the work-item ledger in step with a task dispatch.
//!
//! Durable admission in the daemon opens each queued record before its
//! traversal starts. The task chain declines a root that [`awaits_admission`]
//! and accepts the started root naming its admitted item. [`SettleFailedDispatch`]
//! records a stop before the coding agent runs, and [`SettleWorkItem`] records
//! the task runner's typed terminal result and preservation evidence.
//!
//! Every mutation here is a `load` → apply → `save` against the file, which
//! stays the single source of truth, and every one of them takes the same
//! process-wide gate ([`foundry_sdk::work_item::ledger_write_gate`]) so a
//! record in one spawned workflow cannot interleave with a settle in another.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::pacing::{self, Decision, PacingPaths};
use foundry_sdk::payload::{
    CharterCheckCompletedPayload, ExecutionRequestedPayload, PreflightCompletedPayload,
    TaskRunCompletedPayload, WorkItemEventPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::{
    WorkDisposition, WorkItem, WorkItemKind, WorkItemSpec, WorkItemStore, WorkLane,
    ledger_write_gate,
};
use foundry_sdk::workflow::WorkflowType;

/// The payload key a started root carries to name the ledger item it runs.
///
/// Admission records the item from a root that lacks it; the scheduler's
/// started root carries it, so the task chain runs and nothing records the
/// item a second time. A resume child's root carries it for the same reason.
pub const ADMITTED_WORK_ITEM_KEY: &str = "admitted_work_item_id";

use super::SimulatedSuccess;
use super::work_supersession::{is_supersession_candidate, prove_supersession, verified_commit};

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

/// The item a task dispatch opens, read off its root `ExecutionRequested`,
/// `queued` and holding that root to start from.
///
/// The typed source is whatever the root carries: `foundry task` names the
/// operator's host, a campaign advance names the campaign and cycle, the
/// majors lane names the sentinel that fired the nightly. A root that names
/// none (a raw emit, an older client) records none. The reason it waits with
/// is the caller's to set, from the ledger it is admitted into.
pub fn item_from_dispatch(trigger: &Event, payload: &ExecutionRequestedPayload) -> WorkItem {
    let (kind, lane, origin) = classify(
        &payload.prompt,
        payload.chain.campaign.as_deref(),
        payload.chain.campaign_cycle,
        payload.operator_origin.as_deref(),
    );
    let mut item = WorkItem::queued(
        WorkItemSpec {
            project: trigger.project.clone(),
            objective: payload.prompt.clone(),
            kind,
            lane,
            origin,
            trace_id: trigger.trace_id.clone(),
        },
        trigger.clone(),
        Utc::now(),
    )
    .with_source(trigger.source.clone());
    item.depends_on.clone_from(&payload.depends_on);
    item.not_before = payload.not_before;
    item.continues.clone_from(&payload.continues);
    item
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

/// Whether the dispatch names a project in the registry.
///
/// The task chain's first block fails a dispatch for an unknown project
/// without emitting any domain event, so nothing downstream could ever
/// settle an item recorded for one: it would sit in the ledger until a
/// restart closed it with the wrong reason. Admission is therefore declined
/// here rather than settled later, and the chain is left to refuse the
/// dispatch the way it always has.
fn project_is_registered(registry: &Arc<RwLock<Registry>>, project: &str) -> bool {
    match super::read_registry(registry) {
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

/// Whether `trigger` is a task dispatch that has to pass through admission
/// before its chain may run: a real task dispatch, for a registered project,
/// not yet naming the ledger item it runs (see [`ADMITTED_WORK_ITEM_KEY`]).
///
/// `CheckCharter` declines these roots until the scheduler starts their
/// durable ledger item.
#[must_use]
pub fn awaits_admission(trigger: &Event, registry: &Arc<RwLock<Registry>>) -> bool {
    accepts_dispatch(trigger)
        && trigger.payload.get(ADMITTED_WORK_ITEM_KEY).is_none()
        && project_is_registered(registry, &trigger.project)
}

/// The reason the queued item `id` waits with in `store`, as the scheduler
/// would decide it now: `ready` when nothing holds it.
///
/// Shared by admission and the owner controls that return an item to the
/// queue, so every path that leaves an item `queued` records the same reason
/// the next tick would.
#[must_use]
pub fn queued_reason(
    store: &WorkItemStore,
    id: &str,
    registry: &Arc<RwLock<Registry>>,
    pacing: &PacingPaths,
) -> String {
    let limits = pacing::limits_in_force(&pacing.limits);
    let pauses = pacing::pauses_in_force(&pacing.state);
    let repository_of = |project: &str| match super::read_registry(registry) {
        Ok(guard) => pacing::repository_key(&guard, project),
        Err(error) => {
            // Best-effort: a poisoned registry lock leaves the project name as
            // the repository key, which is the fallback for an unregistered
            // project too; the rule still serialises one project's work.
            tracing::warn!(error = %error, "could not read the registry to key a repository");
            project.to_string()
        }
    };
    let verdict = pacing::evaluate(&store.items, &limits, &pauses, Utc::now(), &repository_of)
        .into_iter()
        .find(|verdict| verdict.id == id);
    match verdict.map(|verdict| verdict.decision) {
        Some(Decision::Wait(reason) | Decision::NeedsDecision(reason)) => reason,
        Some(Decision::Start) | None => pacing::READY_REASON.to_string(),
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
            // The exact resume parent and the items this run continues already
            // settle through their recorded lineage when it lands. Supersession
            // checks the remaining obligations: every open item that preserved
            // work on a ref.
            let resumes = running.resumes.clone();
            let continues = running.continues.clone();
            store
                .items
                .into_iter()
                .filter(|item| {
                    item.project == trigger.project
                        && resumes.as_deref() != Some(item.id.as_str())
                        && !continues.contains(&item.id)
                        && is_supersession_candidate(item)
                })
                .collect::<Vec<_>>()
        };
        let path = Path::new(&project.path);
        let trunk = verified_commit(path, &format!("refs/heads/{}", project.branch)).await;
        let mut verified = Vec::new();
        let mut diagnostics = Vec::new();
        for item in candidates {
            let proof = match &trunk {
                Ok(trunk) => {
                    prove_supersession(path, trunk, &item).await.map(|proof| (trunk.clone(), proof))
                }
                Err(error) => Err(anyhow::anyhow!("registered trunk unavailable: {error}")),
            };
            match proof {
                Ok((commit, proof)) => {
                    tracing::info!(item_id = %item.id, project = %item.project,
                        state = item.state.tag(), %commit, %proof, "supersession proved");
                    verified.push((item, commit));
                }
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
        if result.landed && settled.state == foundry_sdk::work_item::WorkItemState::Landed {
            additional.extend(settle_continued(&mut store, &settled, Utc::now()));
        }
        for (candidate, commit) in verified.iter().filter(|_| {
            result.landed
                && settled.project == trigger.project
                && settled.state == foundry_sdk::work_item::WorkItemState::Landed
        }) {
            // Recheck the entire snapshot under the owner-controls write gate.
            // An owner cancellation or another writer must win over stale Git evidence.
            if let Some(item) = store.items.iter_mut().find(|item| **item == *candidate)
                && item.state.is_open()
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

/// Settle `landed` every item `continuation` names in `continues` that is
/// still open in `store`, as superseded by the continuation's landed commit,
/// and return them.
///
/// Called under the ledger write gate on the ledger as loaded under it, so an
/// owner's close or cancel that reached the ledger first wins: a continued
/// item that is no longer open, or that belongs to another project, is left
/// exactly as it is. Preservation evidence is kept.
fn settle_continued(
    store: &mut WorkItemStore,
    continuation: &WorkItem,
    at: chrono::DateTime<Utc>,
) -> Vec<WorkItem> {
    let Some(commit) = continuation
        .disposition
        .as_ref()
        .and_then(|d| d.landed_commit.clone())
        .filter(|commit| !commit.trim().is_empty())
    else {
        return Vec::new();
    };
    let reason = format!("superseded by {commit} via {}", continuation.id);
    let mut settled = Vec::new();
    for item in store.items.iter_mut().filter(|item| {
        continuation.continues.contains(&item.id)
            && item.project == continuation.project
            && item.state.is_open()
    }) {
        item.disposition
            .get_or_insert_with(|| WorkDisposition {
                task_branch: None,
                branch_cleanup: Vec::new(),
                verdict: None,
                landed_commit: None,
                preservation_ref: None,
                worktree: None,
                worktree_removed: None,
            })
            .landed_commit = Some(commit.clone());
        item.settle_landed(&reason, at);
        settled.push(item.clone());
    }
    settled
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
    use super::{SettleFailedDispatch, SettleWorkItem, SimulatedSuccess};

    fn admission_pending(trigger: &Event) -> bool {
        super::awaits_admission(
            trigger,
            &test_helpers::registry_with_project("alpha", "/tmp/alpha"),
        )
    }

    #[test]
    fn dispatch_metadata_reaches_the_queued_item_and_its_lifecycle_payload() {
        for (extra, source, kind, lane, origin) in [
            (
                serde_json::json!({"operator_origin": "host workbench: asked by Stacey"}),
                Some(WorkSource::operator("workbench")),
                WorkItemKind::Task,
                WorkLane::Interactive,
                "foundry task (host workbench: asked by Stacey)",
            ),
            (
                serde_json::json!({"campaign": "tidy-cli", "campaign_cycle": 3}),
                Some(WorkSource::campaign("tidy-cli", 3)),
                WorkItemKind::CampaignCycle,
                WorkLane::Campaign,
                "campaign tidy-cli cycle 3",
            ),
            (
                serde_json::json!({}),
                None,
                WorkItemKind::Task,
                WorkLane::Interactive,
                "foundry task",
            ),
        ] {
            let mut trigger = dispatch(&extra).with_source(source.clone());
            trigger.payload["depends_on"] = serde_json::json!(["wi_parent"]);
            trigger.payload["not_before"] = serde_json::json!("2099-01-01T00:00:00Z");
            let payload = trigger.parse_payload().unwrap();
            let item = super::item_from_dispatch(&trigger, &payload);
            assert_eq!(item.state, WorkItemState::Queued);
            assert_eq!(item.kind, kind);
            assert_eq!(item.lane, lane);
            assert_eq!(item.origin, origin);
            assert_eq!(item.source, source);
            assert_eq!(item.trace_id.as_deref(), Some(TRACE));
            assert_eq!(item.depends_on, vec!["wi_parent"]);
            assert_eq!(item.not_before.unwrap().to_rfc3339(), "2099-01-01T00:00:00+00:00");
            assert_eq!(item.pending_root.as_ref(), Some(&trigger));
            let fact = super::work_item_event(EventType::WorkItemSubmitted, &trigger, &item);
            assert_eq!(fact.payload["origin"], origin);
            assert_eq!(fact.payload["depends_on"], serde_json::json!(["wi_parent"]));
            if let Some(source) = source {
                assert_eq!(fact.payload["source"], serde_json::to_value(source).unwrap());
            } else {
                assert!(fact.payload.get("source").is_none());
            }
        }
    }

    /// Seed a queued dispatch to exercise settlement independently of admission.
    fn seed_dispatch(path: &std::path::Path, trigger: &Event) {
        let _guard = foundry_sdk::work_item::ledger_write_gate().lock().unwrap();
        let mut store = WorkItemStore::load(path).unwrap();
        let payload = trigger.parse_payload().unwrap();
        store.upsert(super::item_from_dispatch(trigger, &payload));
        store.save(path).unwrap();
    }

    /// Start every queued item in the ledger at `path`, as the daemon's
    /// scheduler would.
    fn start_queued(path: &std::path::Path) {
        let _guard = foundry_sdk::work_item::ledger_write_gate().lock().unwrap();
        let mut store = WorkItemStore::load(path).unwrap();
        for item in store.items.iter_mut().filter(|item| item.is_queued()) {
            item.start(Utc::now());
        }
        store.save(path).unwrap();
    }

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

    /// A task-workflow root dispatch.
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
            uncommitted_artifacts: None,
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
        assert!(admission_pending(&dispatch(&serde_json::json!({}))));
    }

    #[test]
    fn accepts_returns_false_for_a_non_task_workflow() {
        let trigger = dispatch(&serde_json::json!({"workflow": "prompt"}));
        assert!(
            !admission_pending(&trigger),
            "only the task workflow runs a coding agent and reports a verdict"
        );
    }

    #[test]
    fn accepts_returns_false_when_there_is_no_prompt_to_dispatch() {
        let mut trigger = dispatch(&serde_json::json!({}));
        trigger.payload.as_object_mut().unwrap().remove("prompt");
        assert!(!admission_pending(&trigger));
    }

    #[test]
    fn accepts_returns_false_for_a_project_the_registry_does_not_know() {
        let mut trigger = dispatch(&serde_json::json!({}));
        trigger.project = "not-registered".to_string();
        trigger.payload["project"] = serde_json::json!("not-registered");
        assert!(
            !admission_pending(&trigger),
            "the chain refuses an unknown project without emitting anything that could settle an item"
        );
    }

    #[test]
    fn accepts_returns_false_for_a_dry_run() {
        let mut trigger = dispatch(&serde_json::json!({}));
        trigger.throttle = Throttle::DryRun;
        assert!(
            !admission_pending(&trigger),
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
        assert!(!admission_pending(&trigger));
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

    // --- the typed source, one test per kind, each read back from the file ---

    #[test]
    fn the_settlement_simulations_carry_the_triggers_source() {
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

    #[test]
    fn a_major_upgrade_objective_records_the_maintenance_lane() {
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
        let payload = trigger.parse_payload().unwrap();
        let item = super::item_from_dispatch(&trigger, &payload);
        assert_eq!(item.source, Some(WorkSource::sentinel("nightly-maintenance")));
        assert_eq!(item.kind, WorkItemKind::MajorUpgrade);
        assert_eq!(item.lane, WorkLane::Maintenance);
        assert_eq!(item.origin, "nightly majors lane");
    }

    // --- settling a dispatch that stopped before the agent -----------------

    /// Admit and start a dispatch, then hand `trigger` to `SettleFailedDispatch`.
    async fn record_then_abandon(trigger: Event) -> (WorkItemStore, Vec<Event>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        seed_dispatch(&path, &dispatch(&serde_json::json!({})));
        start_queued(&path);
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
        seed_dispatch(&path, &dispatch(&serde_json::json!({})));
        start_queued(&path);
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
        seed_dispatch(&path, &dispatch(&serde_json::json!({})));
        start_queued(&path);
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
        seed_dispatch(&path, &dispatch(&serde_json::json!({})));
        start_queued(&path);
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
    /// Each iteration seeds a `running` item on its own trace, then drives an
    /// admission of a *new* dispatch and a settle of the seeded one at the same
    /// time. Both mutations are a `load` → apply → `save`, so with a private
    /// lock per block they interleave and whichever saves last drops the
    /// other's change. Every iteration must afterwards show both: the new item
    /// present *and* the seeded one settled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_concurrent_record_and_settle_both_reach_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let record_path = path.clone();
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

            let record_path = record_path.clone();
            let settler = Arc::clone(&settle);
            let recording = tokio::spawn(async move { seed_dispatch(&record_path, &dispatched) });
            let settling = tokio::spawn(async move { settler.execute(&done).await });
            recording.await.unwrap();
            settling.await.unwrap().unwrap();

            let store = WorkItemStore::load(&path).unwrap();
            let fresh_item = store
                .items
                .iter()
                .find(|item| item.trace_id.as_deref() == Some(new_trace.as_str()))
                .unwrap_or_else(|| panic!("iteration {iteration}: the record was lost"));
            assert_eq!(fresh_item.state, WorkItemState::Queued);
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

    /// An open item settled from `verdict`, with its own id and the
    /// completion's preservation ref.
    fn open_candidate(running: &WorkItem, id: &str, verdict: TaskVerdict) -> WorkItem {
        let mut candidate = running.clone();
        candidate.id = id.into();
        candidate.settle_from_task_run(
            &completion(verdict, false, None)
                .parse_payload::<TaskRunCompletedPayload>()
                .unwrap(),
            None,
            Utc::now(),
        );
        assert!(candidate.state.is_open(), "{id} must be open, not {:?}", candidate.state);
        candidate
    }

    #[tokio::test]
    async fn supersession_settles_any_open_item_the_owner_and_other_writers_left_alone() {
        for verdict in [
            TaskVerdict::Remainder { gaps: vec![] },
            TaskVerdict::BlockedOnDecision {
                finding: "which schema?".into(),
                options: vec![],
            },
            TaskVerdict::RunnerError {
                detail: "agent crashed".into(),
            },
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("work-items.json");
            seed_dispatch(&path, &dispatch(&serde_json::json!({})));
            start_queued(&path);
            let mut store = WorkItemStore::load(&path).unwrap();
            let running = store.items[0].clone();
            let unchanged = open_candidate(&running, "wi_unchanged", verdict.clone());
            let mut cancelled = open_candidate(&running, "wi_owner_cancelled", verdict.clone());
            let mut edited = open_candidate(&running, "wi_edited", verdict.clone());
            let verified: Vec<_> = [&unchanged, &cancelled, &edited]
                .into_iter()
                .map(|item| (item.clone(), "verified-trunk-commit".to_string()))
                .collect();
            // The Git proof snapshot predates an owner cancellation and an
            // unrelated write to a still-open item, both under the same gate.
            cancelled.settle_cancelled("owner stopped", None, Utc::now());
            edited.reason = "a later writer's reason".into();
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
                for item in [&unchanged, &cancelled, &edited, &unrelated] {
                    store.upsert(item.clone());
                }
                store.save(&path).unwrap();
            }
            let trigger = completion(TaskVerdict::Complete, true, None);
            let result = trigger.parse_payload::<TaskRunCompletedPayload>().unwrap();
            let (_, additional) =
                SettleWorkItem::new(path.clone()).settle(&trigger, &result, &verified).unwrap();
            let after = WorkItemStore::load(&path).unwrap();
            assert_eq!(after.items[0].state, WorkItemState::Landed);
            assert_eq!(
                additional.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
                vec!["wi_unchanged"],
                "{verdict:?}"
            );
            let landed = after.find(&unchanged.id).unwrap();
            assert_eq!(landed.state, WorkItemState::Landed);
            assert_eq!(landed.reason, "superseded by verified-trunk-commit");
            assert_eq!(
                landed.disposition.as_ref().unwrap().landed_commit.as_deref(),
                Some("verified-trunk-commit")
            );
            assert_eq!(after.find(&cancelled.id), Some(&cancelled));
            assert_eq!(after.find(&edited.id), Some(&edited));
            assert_eq!(after.find(&unrelated.id), Some(&unrelated));
        }
    }

    /// A ledger with a running follow-up that continues four items: two open
    /// ones of its project, one an owner already cancelled, and one open item
    /// of another project. Returns the ledger path and the four items.
    fn continuation_ledger(dir: &std::path::Path) -> (std::path::PathBuf, [WorkItem; 4]) {
        let path = dir.join("work-items.json");
        let continues = [
            "wi_preserved",
            "wi_needs_decision",
            "wi_cancelled",
            "wi_elsewhere",
        ];
        seed_dispatch(&path, &dispatch(&serde_json::json!({"continues": continues})));
        start_queued(&path);
        let mut store = WorkItemStore::load(&path).unwrap();
        let running = store.items[0].clone();
        assert_eq!(running.continues, continues, "the dispatch's lineage reaches the item");
        let mut preserved =
            open_candidate(&running, "wi_preserved", TaskVerdict::Remainder { gaps: vec![] });
        preserved.trace_id = Some("c".repeat(32));
        let mut needs_decision = open_candidate(
            &running,
            "wi_needs_decision",
            TaskVerdict::BlockedOnDecision {
                finding: "which schema?".into(),
                options: vec![],
            },
        );
        // A needs_decision item blocked on a dependency carries no disposition.
        needs_decision.disposition = None;
        needs_decision.trace_id = Some("d".repeat(32));
        let mut cancelled =
            open_candidate(&running, "wi_cancelled", TaskVerdict::Remainder { gaps: vec![] });
        let evidence = cancelled.disposition.clone();
        cancelled.settle_cancelled("owner closed", evidence, Utc::now());
        let mut elsewhere = open_candidate(
            &running,
            "wi_elsewhere",
            TaskVerdict::RunnerError {
                detail: "crashed".into(),
            },
        );
        elsewhere.project = "beta".into();
        let followed = [preserved, needs_decision, cancelled, elsewhere];
        for item in &followed {
            store.upsert(item.clone());
        }
        store.save(&path).unwrap();
        (path, followed)
    }

    #[tokio::test]
    async fn a_landed_continuation_supersedes_the_open_items_it_continues() {
        let dir = tempfile::tempdir().unwrap();
        let (path, [preserved, needs_decision, cancelled, elsewhere]) =
            continuation_ledger(dir.path());
        let trigger = completion(TaskVerdict::Complete, true, None);
        let result = trigger.parse_payload::<TaskRunCompletedPayload>().unwrap();

        let (follow_up, additional) =
            SettleWorkItem::new(path.clone()).settle(&trigger, &result, &[]).unwrap();

        assert_eq!(follow_up.state, WorkItemState::Landed);
        let reason = format!("superseded by ref-or-commit via {}", follow_up.id);
        assert_eq!(
            additional.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
            vec!["wi_preserved", "wi_needs_decision"]
        );
        let after = WorkItemStore::load(&path).unwrap();
        for before in [&preserved, &needs_decision] {
            let landed = after.find(&before.id).unwrap();
            assert_eq!(landed.state, WorkItemState::Landed);
            assert_eq!(landed.reason, reason);
            assert_eq!(
                landed.disposition.as_ref().unwrap().landed_commit.as_deref(),
                Some("ref-or-commit")
            );
            assert_eq!(landed.operator_action, None);
        }
        assert_eq!(
            after
                .find(&preserved.id)
                .unwrap()
                .disposition
                .as_ref()
                .unwrap()
                .preservation_ref,
            preserved.disposition.as_ref().unwrap().preservation_ref,
            "preservation evidence is kept"
        );
        assert_eq!(after.find(&cancelled.id), Some(&cancelled), "the owner's close wins");
        assert_eq!(after.find(&elsewhere.id), Some(&elsewhere), "another project is untouched");
    }

    #[tokio::test]
    async fn a_continuation_that_does_not_land_leaves_the_continued_items_open() {
        for (verdict, landed) in [
            (TaskVerdict::Remainder { gaps: vec![] }, false),
            // Complete without a deliverable settles landed but landed nothing.
            (TaskVerdict::Complete, false),
            (
                TaskVerdict::RunnerError {
                    detail: "crashed".into(),
                },
                false,
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (path, continued) = continuation_ledger(dir.path());
            let trigger = completion(verdict.clone(), landed, None);
            let result = trigger.parse_payload::<TaskRunCompletedPayload>().unwrap();

            let (_, additional) =
                SettleWorkItem::new(path.clone()).settle(&trigger, &result, &[]).unwrap();

            assert!(additional.is_empty(), "{verdict:?}");
            let after = WorkItemStore::load(&path).unwrap();
            for before in &continued {
                assert_eq!(after.find(&before.id), Some(before), "{verdict:?}");
            }
        }
    }
}
