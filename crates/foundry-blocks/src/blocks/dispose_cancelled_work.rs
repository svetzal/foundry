//! Disposes of what a cancelled work item no longer needs.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::WorkItemEventPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::{WorkItem, WorkItemState, WorkItemStore, ledger_write_gate};

use super::SimulatedSuccess;
use super::work_disposal::{Disposal, dispose, dispose_released};

/// Removes a cancelled item's worktree, its local task branch and its origin
/// task branch, then examines the refs the item was holding for others.
///
/// Mutator. It sinks on `work_item_cancelled`, whoever cancelled the item: an
/// owner's `queue cancel` or `queue close`, a campaign carrying or cancelling
/// its cycles, or an initiative. The decision is taken from the ledger as it
/// stands, so a cancellation that a later write reversed disposes of nothing.
///
/// It emits one `work_item_disposed` per item whose disposal recorded
/// something, on that item's own trace, so `foundry queue show` names what
/// was removed and what was left. A failure is a warning in the summary,
/// never a failure of the workflow: the cancellation is already durable.
pub struct DisposeCancelledWork {
    registry: Arc<RwLock<Registry>>,
    work_items_path: PathBuf,
    worktrees: PathBuf,
}

impl DisposeCancelledWork {
    /// Dispose of cancelled work recorded in the ledger at `work_items_path`,
    /// removing only worktrees under `worktrees`.
    #[must_use]
    pub fn new(
        registry: Arc<RwLock<Registry>>,
        work_items_path: PathBuf,
        worktrees: PathBuf,
    ) -> Self {
        Self {
            registry,
            work_items_path,
            worktrees,
        }
    }
}

/// Whether disposal left a record worth announcing on `item`.
fn recorded(item: &WorkItem) -> bool {
    item.disposition
        .as_ref()
        .and_then(foundry_sdk::work_item::WorkDisposition::disposal_summary)
        .is_some()
}

impl SimulatedSuccess for DisposeCancelledWork {
    type Outcome = Vec<WorkItem>;

    /// What disposal removes depends on the live ledger and repositories,
    /// which a dry run does not touch, so a dry run announces nothing.
    fn simulate(&self, _trigger: &Event) -> Vec<WorkItem> {
        Vec::new()
    }

    fn success_events(&self, trigger: &Event, outcome: &Vec<WorkItem>) -> Vec<Event> {
        outcome
            .iter()
            .filter(|item| recorded(item))
            .map(|item| {
                super::event_from_infallible_payload(
                    EventType::WorkItemDisposed,
                    &item.project,
                    trigger.throttle,
                    &WorkItemEventPayload::from_item(item),
                )
                .with_trace_id(item.trace_id.clone())
            })
            .collect()
    }
}

impl TaskBlock for DisposeCancelledWork {
    task_block_meta! {
        name: "Dispose Cancelled Work",
        kind: Mutator,
        sinks_on: [WorkItemCancelled],
    }

    dry_run_via_simulation!();

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let payload = parse_payload!(trigger, WorkItemEventPayload);
        let trigger = trigger.clone();
        Box::pin(async move {
            let path = self.work_items_path.clone();
            let id = payload.item_id.clone();
            let item = tokio::task::spawn_blocking(move || -> anyhow::Result<Option<WorkItem>> {
                let _guard =
                    ledger_write_gate().lock().map_err(|e| anyhow::anyhow!("ledger gate: {e}"))?;
                Ok(WorkItemStore::load(&path)?.find(&id).cloned())
            })
            .await?;
            let mut item = match item {
                Ok(Some(item)) if item.state == WorkItemState::Cancelled => item,
                Ok(_) => {
                    return Ok(TaskBlockResult::success(
                        format!(
                            "{}: work item is not cancelled; nothing disposed",
                            payload.item_id
                        ),
                        Vec::new(),
                    ));
                }
                Err(error) => {
                    // Best-effort: the reconciler's sweep removes what is left.
                    tracing::warn!(item_id = %payload.item_id, %error, "disposal could not read the ledger");
                    return Ok(TaskBlockResult::success(
                        format!("{}: disposal skipped: {error:#}", payload.item_id),
                        Vec::new(),
                    ));
                }
            };
            let registry = Some(&self.registry);
            let before = item.clone();
            dispose(
                &self.work_items_path,
                registry,
                &self.worktrees,
                &mut item,
                Disposal::Discarded,
            )
            .await;
            let released =
                dispose_released(&self.work_items_path, registry, &self.worktrees, &item).await;
            // Only an item this pass changed is announced, so a repeated
            // cancellation event adds no duplicate history.
            let examined: Vec<WorkItem> =
                std::iter::once(item).filter(|item| *item != before).chain(released).collect();
            if examined.is_empty() {
                return Ok(TaskBlockResult::success(
                    format!("{}: nothing to dispose of", before.id),
                    Vec::new(),
                ));
            }
            let lines: Vec<String> = examined
                .iter()
                .filter_map(|item| {
                    let summary = item.disposition.as_ref()?.disposal_summary()?;
                    Some(format!("{}: {summary}", item.id))
                })
                .collect();
            let summary = if lines.is_empty() {
                format!("{}: nothing to dispose of", before.id)
            } else {
                lines.join("; ")
            };
            Ok(TaskBlockResult::success(summary, self.success_events(&trigger, &examined)))
        })
    }
}
