//! Closes maintenance cycles that foundryd was stopped in the middle of.
//!
//! A maintenance cycle runs inside one `Engine::process` call. If foundryd
//! dies during it, nothing records the end: no `MaintenanceCycleCompleted`,
//! no summary. On 2026-09-26 foundryd was killed at 08:44 UTC with 14 of 27
//! projects still to run, came back at 12:03, and the cycle was never
//! reported.
//!
//! On start, foundryd rejoins durable queued or held maintenance children.
//! Their gather remains open until scheduled work completes or is cancelled.
//! It then records `MaintenanceCycleCompleted` and runs the summary phase.
//! Missing or failed children mark the recovered cycle interrupted; a cycle
//! whose queued children all complete normally remains an ordinary success.

use std::collections::{BTreeSet, HashSet};
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{GatherCompletedPayload, GatheredChild, InterruptedCycle, MissingChild};
use foundry_sdk::trace::ProcessResult;

use super::RuntimeContext;

/// How far back to look for interrupted cycles. A week covers a daemon that
/// stayed down over a weekend without replaying old history.
pub(crate) const LOOKBACK_DAYS: i64 = 7;

/// One cycle that started and never completed.
#[derive(Debug, Clone)]
pub(crate) struct InterruptedRun {
    /// The cycle's `MaintenanceCycleStarted`.
    pub(crate) root: Event,
    /// Every event recorded on the cycle's trace, in log order.
    pub(crate) events: Vec<Event>,
    pub(crate) record: InterruptedCycle,
    gather_id: Option<String>,
    expected: usize,
    children: Vec<GatheredChild>,
}

impl InterruptedRun {
    /// The `MaintenanceCycleCompleted` the engine would have synthesized, with
    /// the unfinished projects as `missing`.
    pub(crate) fn completion_event(&self) -> Event {
        let reason = self.record.unfinished_reason();
        let payload = GatherCompletedPayload {
            gather_id: self.gather_id.clone().unwrap_or_default(),
            expected: self.expected,
            arrived: self.children.len(),
            context: serde_json::json!({ "interrupted": !self.record.unfinished.is_empty() || self.children.iter().any(|child| child.success == Some(false)) }),
            children: self.children.clone(),
            missing: self
                .record
                .unfinished
                .iter()
                .map(|project| MissingChild {
                    project: project.clone(),
                    reason: reason.clone(),
                })
                .collect(),
        };
        #[allow(
            clippy::expect_used,
            reason = "GatherCompletedPayload is infallibly serializable (Payload Conventions, AGENTS.md)"
        )]
        let payload = Event::serialize_payload(&payload)
            .expect("GatherCompletedPayload is infallibly serializable");
        Event::new(
            EventType::MaintenanceCycleCompleted,
            "system".to_string(),
            self.root.throttle,
            payload,
        )
        .with_trace_id(self.root.trace_id.clone())
        .with_source(self.root.source.clone())
        .with_span_ids(self.root.span_id.clone(), self.root.parent_span_id.clone())
        .with_causation_id(Some(self.root.id.clone()))
    }

    /// The cycle as the summary phase reads it: what it logged, plus the
    /// completion recorded now. No block executions survive a killed daemon.
    pub(crate) fn process_result(&self, completion: Event) -> ProcessResult {
        let mut events = self.events.clone();
        events.push(completion);
        let elapsed = self.record.last_event_at - self.record.started_at;
        ProcessResult {
            events,
            block_executions: Vec::new(),
            total_duration_ms: u64::try_from(elapsed.num_milliseconds()).unwrap_or(0),
        }
    }
}

/// System maintenance cycles started at or after `since` that have no
/// `MaintenanceCycleCompleted` on their trace.
pub(crate) fn find_interrupted_cycles(
    events: &[Event],
    since: DateTime<Utc>,
) -> Vec<InterruptedRun> {
    let completed: HashSet<&str> = events
        .iter()
        .filter(|e| e.event_type == EventType::MaintenanceCycleCompleted)
        .filter_map(|e| e.trace_id.as_deref())
        .collect();
    events
        .iter()
        .filter(|e| {
            e.event_type == EventType::MaintenanceCycleStarted
                && e.project == "system"
                && e.occurred_at >= since
        })
        .filter_map(|root| {
            let trace = root.trace_id.as_deref()?;
            if completed.contains(trace) {
                return None;
            }
            let on_trace: Vec<Event> = events
                .iter()
                .filter(|e| e.trace_id.as_deref() == Some(trace))
                .cloned()
                .collect();
            Some(interrupted_run(root, on_trace))
        })
        .collect()
}

fn interrupted_run(root: &Event, events: Vec<Event>) -> InterruptedRun {
    let starts: Vec<&Event> = events
        .iter()
        .filter(|e| {
            e.event_type == EventType::ProjectRunStarted
                && e.project != "system"
                && e.payload.get(foundry_blocks::blocks::ADMITTED_WORK_ITEM_KEY).is_none()
        })
        .collect();
    let completions: Vec<&Event> = events
        .iter()
        .filter(|e| e.event_type == EventType::ProjectRunCompleted)
        .collect();
    let finished: HashSet<&str> = completions.iter().map(|e| e.project.as_str()).collect();
    let unfinished: BTreeSet<String> = starts
        .iter()
        .map(|e| e.project.clone())
        .filter(|p| !finished.contains(p.as_str()))
        .collect();
    let last_event_at = events.iter().map(|e| e.occurred_at).max().unwrap_or(root.occurred_at);
    let children = completions
        .iter()
        .map(|e| GatheredChild {
            event_id: e.id.clone(),
            event_type: e.event_type.clone(),
            project: e.project.clone(),
            success: e.payload.get("success").and_then(serde_json::Value::as_bool),
            payload: e.payload.clone(),
        })
        .collect();
    InterruptedRun {
        root: root.clone(),
        gather_id: starts.iter().find_map(|e| e.gather_id.clone()),
        expected: starts.len(),
        children,
        record: InterruptedCycle {
            started_at: root.occurred_at,
            last_event_at,
            unfinished: unfinished.into_iter().collect(),
        },
        events,
    }
}

/// Waiting children belong to the durable queue, even after their coordinator
/// was lost. Rejoin them before closing the cycle; held work stays held.
async fn rejoin_waiting_maintenance(ctx: &RuntimeContext, run: &mut InterruptedRun, path: &Path) {
    let store = match foundry_sdk::work_item::WorkItemStore::load(path) {
        Ok(store) => store,
        Err(error) => {
            tracing::error!(%error, "could not rejoin maintenance ledger");
            return;
        }
    };
    let waiting: Vec<_> = store
        .items
        .iter()
        .filter(|item| {
            item.kind == foundry_sdk::work_item::WorkItemKind::Maintenance
                && item.trace_id == run.root.trace_id
                && !run.events.iter().any(|event| {
                    event.event_type == EventType::ProjectRunCompleted
                        && event.project == item.project
                })
                && item.pending_root.as_ref().or(item.started_root.as_ref()).is_some_and(|root| {
                    root.event_type == EventType::ProjectRunStarted && root.gather_id.is_some()
                })
        })
        .map(|item| item.id.clone())
        .collect();
    if waiting.is_empty() {
        return;
    }
    for id in waiting {
        match ctx.engine.join_work(&id).await {
            Ok(result) => run.events.extend(result.events),
            Err(error) => {
                tracing::error!(%error, item_id = %id, "could not rejoin queued maintenance child");
            }
        }
    }
    *run = interrupted_run(&run.root, run.events.clone());
}

/// Held children can outlive the ordinary event lookback. Include the prior
/// day so their system cycle root, emitted before the scatter, remains visible.
fn recovery_since(now: DateTime<Utc>, items: &[foundry_sdk::work_item::WorkItem]) -> DateTime<Utc> {
    items
        .iter()
        .filter(|item| {
            item.kind == foundry_sdk::work_item::WorkItemKind::Maintenance
                && (item.pending_root.is_some()
                    || item.reason == super::work_ledger::RESTART_REASON)
        })
        .filter_map(|item| {
            item.pending_root
                .as_ref()
                .or(item.started_root.as_ref())
                .filter(|root| {
                    root.event_type == EventType::ProjectRunStarted && root.gather_id.is_some()
                })
                .map(|root| item.submitted_at.min(root.occurred_at) - Duration::days(1))
        })
        .fold(now - Duration::days(LOOKBACK_DAYS), DateTime::min)
}

/// Find and close every interrupted cycle in the last [`LOOKBACK_DAYS`].
pub(crate) async fn recover_interrupted_cycles(ctx: &RuntimeContext, events_dir: &Path) {
    let path = foundry_sdk::paths::work_items_path();
    let items = match foundry_sdk::work_item::WorkItemStore::load(&path) {
        Ok(store) => store.items,
        Err(error) => {
            tracing::error!(%error, "could not load durable maintenance recovery bound");
            return;
        }
    };
    let since = recovery_since(Utc::now(), &items);
    let events = foundry_blocks::blocks::triage_core::read_events_jsonl(events_dir, since);
    for mut run in find_interrupted_cycles(&events, since) {
        rejoin_waiting_maintenance(ctx, &mut run, &foundry_sdk::paths::work_items_path()).await;
        let recorded = foundry_blocks::blocks::triage_core::read_events_jsonl(events_dir, since);
        let seen: std::collections::HashSet<_> =
            run.events.iter().map(|event| event.id.clone()).collect();
        run.events.extend(
            recorded
                .into_iter()
                .filter(|event| event.trace_id == run.root.trace_id && !seen.contains(&event.id)),
        );
        run = interrupted_run(&run.root, run.events.clone());
        tracing::warn!(
            root_event_id = %run.root.id,
            started_at = %run.record.started_at,
            last_event_at = %run.record.last_event_at,
            unfinished = run.record.unfinished.len(),
            "closing a maintenance cycle foundryd was stopped during"
        );
        let completion = run.completion_event();
        // The engine persists the completion like any root event; no block
        // sinks on it, so nothing runs.
        let _recorded = ctx.engine.process(completion.clone()).await;
        let result = run.process_result(completion);
        super::eventing_ops::finalise_system_maintenance(
            &result,
            ctx,
            run.root.throttle,
            &run.root.id,
            (!run.record.unfinished.is_empty()
                || run.children.iter().any(|child| child.success == Some(false)))
            .then(|| run.record.clone()),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;
    use foundry_sdk::throttle::Throttle;

    use super::*;

    fn at(h: u32, m: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 26, h, m, 0).unwrap()
    }

    fn event(ty: EventType, project: &str, trace: &str, when: DateTime<Utc>) -> Event {
        let mut e = Event::new(ty, project.to_string(), Throttle::Full, serde_json::json!({}))
            .with_trace_id(Some(trace.to_string()));
        e.occurred_at = when;
        e
    }

    fn completed(project: &str, trace: &str, when: DateTime<Utc>) -> Event {
        let mut e = event(EventType::ProjectRunCompleted, project, trace, when);
        e.payload = serde_json::json!({ "success": true });
        e
    }

    /// The 2026-09-26 shape: three projects scattered, one finished, then the
    /// daemon went quiet mid-run.
    fn killed_cycle() -> Vec<Event> {
        let gid = Some("gth_1".to_string());
        vec![
            event(EventType::MaintenanceCycleStarted, "system", "t1", at(6, 0)),
            event(EventType::ProjectRunStarted, "ops-visualizer", "t1", at(6, 0))
                .with_gather_id(gid.clone()),
            event(EventType::ProjectRunStarted, "context-mixer2", "t1", at(6, 0))
                .with_gather_id(gid.clone()),
            event(EventType::ProjectRunStarted, "zk-chat", "t1", at(6, 0)).with_gather_id(gid),
            completed("ops-visualizer", "t1", at(8, 41)),
            event(EventType::AgentSessionStarted, "context-mixer2", "t1", at(8, 42)),
        ]
    }

    #[test]
    fn a_cycle_with_no_completion_is_interrupted_and_names_the_unfinished_projects() {
        let found = find_interrupted_cycles(&killed_cycle(), at(0, 0));
        assert_eq!(found.len(), 1);
        let run = &found[0];
        assert_eq!(run.record.started_at, at(6, 0));
        assert_eq!(run.record.last_event_at, at(8, 42));
        assert_eq!(run.record.unfinished, ["context-mixer2", "zk-chat"]);
    }

    #[test]
    fn a_completed_cycle_is_left_alone() {
        let mut events = killed_cycle();
        events.push(event(EventType::MaintenanceCycleCompleted, "system", "t1", at(9, 0)));
        assert!(find_interrupted_cycles(&events, at(0, 0)).is_empty());
    }

    #[test]
    fn cycles_before_the_lookback_and_project_cycles_are_ignored() {
        assert!(find_interrupted_cycles(&killed_cycle(), at(7, 0)).is_empty(), "too old");
        let project_cycle = vec![event(
            EventType::MaintenanceCycleStarted,
            "roost",
            "t2",
            at(6, 0),
        )];
        assert!(find_interrupted_cycles(&project_cycle, at(0, 0)).is_empty());
    }

    #[test]
    fn the_completion_event_closes_the_trace_and_lists_the_missing_projects() {
        let run = find_interrupted_cycles(&killed_cycle(), at(0, 0)).remove(0);
        let completion = run.completion_event();
        assert_eq!(completion.event_type, EventType::MaintenanceCycleCompleted);
        assert_eq!(completion.trace_id.as_deref(), Some("t1"));
        assert_eq!(completion.causation_id.as_deref(), Some(run.root.id.as_str()));
        let payload: GatherCompletedPayload = completion.parse_payload().unwrap();
        assert_eq!(payload.gather_id, "gth_1");
        assert_eq!((payload.expected, payload.arrived), (3, 1));
        let missing: Vec<&str> = payload.missing.iter().map(|m| m.project.as_str()).collect();
        assert_eq!(missing, ["context-mixer2", "zk-chat"]);
        assert!(payload.missing[0].reason.starts_with("interrupted: foundryd stopped"));

        // Once recorded, the same cycle is no longer found.
        let mut events = killed_cycle();
        events.push(completion);
        assert!(find_interrupted_cycles(&events, at(0, 0)).is_empty());
    }

    #[test]
    fn the_process_result_spans_the_logged_run() {
        let run = find_interrupted_cycles(&killed_cycle(), at(0, 0)).remove(0);
        let result = run.process_result(run.completion_event());
        assert_eq!(result.total_duration_ms, 162 * 60 * 1000);
        assert_eq!(result.events.len(), killed_cycle().len() + 1);
    }
    #[test]
    fn a_maintenance_child_held_over_a_week_retains_its_original_cycle() {
        use foundry_sdk::work_item::{WorkItem, WorkItemKind, WorkItemSpec, WorkLane};
        let now = Utc::now();
        let mut cycle = Event::new(
            EventType::MaintenanceCycleStarted,
            "system".into(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some("old-cycle".into()));
        cycle.occurred_at = now - Duration::days(20);
        let mut child = Event::new(
            EventType::ProjectRunStarted,
            "alpha".into(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(cycle.trace_id.clone())
        .with_gather_id(Some("old-gather".into()));
        child.occurred_at = cycle.occurred_at + Duration::seconds(1);
        let mut item = WorkItem::queued(
            WorkItemSpec {
                project: "alpha".into(),
                objective: "maintenance".into(),
                kind: WorkItemKind::Maintenance,
                lane: WorkLane::Maintenance,
                origin: "nightly".into(),
                trace_id: cycle.trace_id.clone(),
            },
            child.clone(),
            child.occurred_at,
        );
        item.hold();
        let since = recovery_since(now, std::slice::from_ref(&item));
        assert!(since < cycle.occurred_at);
        // A held child can start now, long after the original cycle. Its fresh
        // scheduler root must not shorten the historical recovery window.
        item.start(now);
        let mut started = child.clone();
        started.occurred_at = now;
        item.started_root = Some(started);
        item.settle_failed(super::super::work_ledger::RESTART_REASON, now);
        assert!(recovery_since(now, &[item]) < cycle.occurred_at);
        let recovered = find_interrupted_cycles(&[cycle, child], since);
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].record.unfinished, ["alpha"]);
    }
}
