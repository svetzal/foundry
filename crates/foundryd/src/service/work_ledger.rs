//! Closes work items the previous `foundryd` process was stopped in the
//! middle of.
//!
//! A `running` item means "an agent is working on this". Nothing in the
//! process that was holding that agent survives a restart, so on start every
//! item still `running` is a lie. Each one is settled `failed` with the reason
//! `daemon restarted`, and a `work_item_settled` event is recorded, before the
//! daemon dispatches anything new.

use std::path::Path;

use chrono::Utc;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::WorkItemEventPayload;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{WorkItem, WorkItemStore, ledger_write_gate};

use super::RuntimeContext;

/// The reason a restart settles an item with. Stable text: an operator reading
/// the ledger or the ops digest sees the same phrase every time.
pub(crate) const RESTART_REASON: &str = "daemon restarted";

/// Settle every item left `running` and return the settled items.
///
/// Pure but for the store: the caller records the events, so this is testable
/// without an engine.
pub(crate) fn settle_running(store: &mut WorkItemStore) -> Vec<WorkItem> {
    let now = Utc::now();
    let mut settled = Vec::new();
    for item in &mut store.items {
        if item.is_running() {
            item.settle_failed(RESTART_REASON, now);
            settled.push(item.clone());
        }
    }
    settled
}

/// The `work_item_settled` event that records one restart settlement.
pub(crate) fn settled_event(item: &WorkItem) -> Event {
    #[allow(
        clippy::expect_used,
        reason = "WorkItemEventPayload is infallibly serializable (Payload Conventions, AGENTS.md)"
    )]
    let payload = Event::serialize_payload(&WorkItemEventPayload::from_item(item))
        .expect("WorkItemEventPayload is infallibly serializable");
    Event::new(EventType::WorkItemSettled, item.project.clone(), Throttle::Full, payload)
        .with_trace_id(item.trace_id.clone())
}

/// The `load` → settle → `save` half of the restart sweep, under the shared
/// ledger write gate.
///
/// Kept synchronous and separate from the event recording so the gate is
/// released before the first `.await`: the same gate orders every other ledger
/// mutation in the process, and holding it across an engine round trip would
/// stall them.
fn settle_running_in_file(path: &Path) -> Vec<WorkItem> {
    let Ok(_guard) = ledger_write_gate().lock() else {
        // Best-effort: a poisoned gate means some other mutation panicked
        // mid-sequence. The daemon must still start; the items stay running
        // until an operator or a later restart closes them.
        tracing::warn!("work-item ledger lock poisoned on start; items left as they are");
        return Vec::new();
    };
    let mut store = match WorkItemStore::load(path) {
        Ok(store) => store,
        Err(error) => {
            // Best-effort: the ledger is a record of work, not a precondition
            // for serving. Refusing to start would cost more than the lost
            // bookkeeping.
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "could not read the work-item ledger on start; items left as they are"
            );
            return Vec::new();
        }
    };

    let settled = settle_running(&mut store);
    if settled.is_empty() {
        return settled;
    }
    if let Err(error) = store.save(path) {
        // Best-effort: see above. Nothing was written, so no event is recorded
        // either — the ledger and the event log stay consistent with each other.
        tracing::warn!(
            path = %path.display(),
            error = %error,
            count = settled.len(),
            "could not write the work-item ledger on start; items left running"
        );
        return Vec::new();
    }
    settled
}

/// Settle every `running` item in the ledger at `path` and record an event for
/// each.
///
/// Called on daemon start, before any new dispatch. A ledger fault is absorbed:
/// an unreadable or unwritable ledger must not keep the daemon from starting.
pub(crate) async fn settle_running_items_on_start(ctx: &RuntimeContext, path: &Path) {
    let settled = settle_running_in_file(path);

    for item in &settled {
        tracing::warn!(
            item_id = %item.id,
            project = %item.project,
            "settling a work item foundryd was stopped during"
        );
        // The engine persists and broadcasts a root event like any other; no
        // block sinks on `work_item_settled`, so nothing else runs.
        let _recorded = ctx.engine.process(settled_event(item)).await;
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use foundry_sdk::work_item::{
        WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
    };

    use super::{RESTART_REASON, settle_running, settled_event};

    fn running(project: &str) -> WorkItem {
        WorkItem::dispatched(
            WorkItemSpec {
                project: project.to_string(),
                objective: "Add a --quiet flag.".to_string(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "foundry task".to_string(),
                trace_id: Some("a".repeat(32)),
            },
            Utc::now(),
        )
    }

    #[test]
    fn a_running_item_settles_failed_with_the_restart_reason() {
        let mut store = WorkItemStore::default();
        store.upsert(running("alpha"));

        let settled = settle_running(&mut store);

        assert_eq!(settled.len(), 1);
        assert_eq!(store.items[0].state, WorkItemState::Failed);
        assert_eq!(store.items[0].reason, RESTART_REASON);
        assert!(store.items[0].settled_at.is_some());
        assert_eq!(store.running().count(), 0);
    }

    #[test]
    fn an_already_settled_item_is_left_alone() {
        let mut store = WorkItemStore::default();
        let mut landed = running("alpha");
        landed.state = WorkItemState::Landed;
        landed.reason = "task completed, reviewed, and landed".to_string();
        store.upsert(landed);

        assert!(settle_running(&mut store).is_empty());
        assert_eq!(store.items[0].state, WorkItemState::Landed);
        assert_eq!(store.items[0].reason, "task completed, reviewed, and landed");
    }

    #[test]
    fn the_settled_event_names_the_item_and_stays_on_its_trace() {
        let mut store = WorkItemStore::default();
        store.upsert(running("alpha"));
        let settled = settle_running(&mut store).remove(0);

        let event = settled_event(&settled);

        assert_eq!(event.event_type, foundry_sdk::event::EventType::WorkItemSettled);
        assert_eq!(event.project, "alpha");
        assert_eq!(event.trace_id.as_deref(), Some("a".repeat(32).as_str()));
        assert_eq!(event.payload["item_id"], settled.id.as_str());
        assert_eq!(event.payload["state"], "failed");
        assert_eq!(event.payload["reason"], RESTART_REASON);
        assert_eq!(event.payload["kind"], "task");
        assert_eq!(event.payload["lane"], "interactive");
        assert_eq!(event.payload["origin"], "foundry task");
    }
}
