//! The pacing scheduler: the stage between a work item's admission and its
//! start.
//!
//! Admission (see `foundry_blocks::blocks::AdmitWorkItem`, and the resume
//! path in `service::work_item_ops`) records a task-shaped item `queued`,
//! holding the root event that will run it. This scheduler ticks over the
//! ledger and starts items as the rules in [`foundry_sdk::pacing`] allow.
//!
//! ## A tick
//!
//! 1. Under the shared ledger write gate, load the ledger, the limits and the
//!    pause state, and [`foundry_sdk::pacing::evaluate`] every queued item.
//! 2. Apply the verdicts to the loaded ledger: a start moves the item to
//!    `running` and takes its held root; a wait rewrites the reason when it
//!    changed; a dependency decision settles the item `needs_decision`.
//! 3. Save the ledger once. A save that fails dispatches nothing: the ledger
//!    is the record of what started, and a start it does not record would be
//!    a start the next restart could not see.
//! 4. Outside the gate, announce each transition (`work_item_started`,
//!    `work_item_settled`) through the engine, then hand each started root to
//!    the dispatcher.
//!
//! ## When it ticks
//!
//! On every `work_item_*` and `pacing_*` event on the broadcast (an
//! admission, a settlement, a hold, a pause), on the wake handle an owner
//! control pokes after a ledger write that follows its own announcement (a
//! resume admission), at the earliest `not_before` among the queued items,
//! and on a safety-net interval in between. A single task on an idle daemon
//! therefore starts within one tick of its admission.
//!
//! ## The started root
//!
//! The root held on the item is the request as admitted. The started root is
//! a fresh `ExecutionRequested`, built from it, that names the item (the
//! `admitted_work_item_id` key) so the task chain runs and admission records
//! nothing twice. It keeps the admitted root's trace, source and gather
//! membership, and opens its span under the admitted root's span, so a
//! campaign cycle's start still sits under the advance that derived it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::{Notify, broadcast};

use foundry_blocks::blocks::ADMITTED_WORK_ITEM_KEY;
use foundry_sdk::event::{Event, EventType, mint_span_id};
use foundry_sdk::pacing::{self, Decision, PacingPaths};
use foundry_sdk::payload::WorkItemEventPayload;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{WorkItem, WorkItemStore, ledger_write_gate};
use foundry_sdk::work_item_events::is_work_item_event;

use crate::service::RuntimeContext;

/// Hands a started root to whatever runs it. In production this is
/// `crate::service::spawn_workflow`; tests use a recording closure.
pub type DispatchFn = Arc<dyn Fn(Event) + Send + Sync + 'static>;

/// Source of "now". Indirected so tests pin a deterministic clock.
pub type ClockFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync + 'static>;

/// The safety-net interval between ticks when nothing wakes the scheduler.
/// Short enough that an admission whose announcement preceded its ledger
/// write (a nightly continuation) is not kept waiting long.
const IDLE_TICK: Duration = Duration::from_secs(15);

/// The reason a queued item settles `failed` with when it holds no root to
/// start from (a hand-edited ledger).
const NO_ROOT_REASON: &str = "queued item carries no dispatch to start";

/// What one tick changed in the ledger.
#[derive(Debug, Default)]
pub struct TickOutcome {
    /// Items moved to `running`, each with the root to dispatch.
    pub started: Vec<(WorkItem, Event)>,
    /// Items settled this tick (`needs_decision` for a dependency, or
    /// `failed` for a missing root).
    pub settled: Vec<WorkItem>,
    /// The earliest `not_before` still ahead among the queued items.
    pub next_not_before: Option<DateTime<Utc>>,
}

/// The pacing scheduler. `run` consumes it and loops forever.
pub struct PacingScheduler {
    ctx: RuntimeContext,
    work_items_path: PathBuf,
    pacing: PacingPaths,
    /// Poked by an owner control whose ledger write follows its own
    /// announcement, so the next tick does not wait for the interval.
    wake: Arc<Notify>,
    dispatch: DispatchFn,
    clock: ClockFn,
}

impl PacingScheduler {
    /// A scheduler over the ledger at `work_items_path` that starts items
    /// through `crate::service::spawn_workflow` and ticks early whenever
    /// `wake` is notified.
    #[must_use]
    pub fn new(
        ctx: RuntimeContext,
        work_items_path: PathBuf,
        pacing: PacingPaths,
        wake: Arc<Notify>,
    ) -> Self {
        let dispatch_ctx = ctx.clone();
        let dispatch: DispatchFn =
            Arc::new(move |event| crate::service::spawn_workflow(event, &dispatch_ctx));
        Self {
            ctx,
            work_items_path,
            pacing,
            wake,
            dispatch,
            clock: Arc::new(Utc::now),
        }
    }

    /// Start items through `dispatch` instead of spawning workflows. Test-only.
    #[cfg(test)]
    #[must_use]
    pub fn with_dispatch(mut self, dispatch: DispatchFn) -> Self {
        self.dispatch = dispatch;
        self
    }

    /// Read "now" from `clock` instead of the system clock. Test-only.
    #[cfg(test)]
    #[must_use]
    pub fn with_clock(mut self, clock: ClockFn) -> Self {
        self.clock = clock;
        self
    }

    /// Run the scheduler loop. Never returns under normal operation.
    pub async fn run(self) {
        let mut events = self.ctx.event_tx.subscribe();
        tracing::info!(path = %self.work_items_path.display(), "pacing scheduler started");
        loop {
            let outcome = self.tick().await;
            let now = (self.clock)();
            let wait = outcome
                .next_not_before
                .and_then(|at| (at - now).to_std().ok())
                .map_or(IDLE_TICK, |until| until.min(IDLE_TICK));
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = wake_on_ledger_event(&mut events) => {}
                () = self.wake.notified() => {}
            }
        }
    }

    /// One tick: decide, apply and save under the gate, then announce and
    /// dispatch what started.
    pub async fn tick(&self) -> TickOutcome {
        let now = (self.clock)();
        let path = self.work_items_path.clone();
        let pacing = self.pacing.clone();
        let registry = Arc::clone(&self.ctx.registry);
        let outcome = tokio::task::spawn_blocking(move || {
            let repository_of = |project: &str| match registry.read() {
                Ok(guard) => pacing::repository_key(&guard, project),
                Err(error) => {
                    // Best-effort: a poisoned registry lock leaves the project
                    // name as the key, which still serialises one project.
                    tracing::warn!(%error, "could not read the registry to key a repository");
                    project.to_string()
                }
            };
            apply_tick(&path, &pacing, now, &repository_of)
        })
        .await
        .unwrap_or_else(|error| {
            // Best-effort: a panicked tick dispatches nothing and the next
            // tick starts from the ledger as saved; the panic must not take
            // the scheduler down with it.
            tracing::error!(%error, "pacing tick did not finish; nothing started");
            TickOutcome::default()
        });

        for item in &outcome.settled {
            self.ctx.engine.process(lifecycle_event(EventType::WorkItemSettled, item)).await;
        }
        for (item, root) in &outcome.started {
            tracing::info!(
                item_id = %item.id,
                project = %item.project,
                kind = item.kind.tag(),
                root_event_id = %root.id,
                "starting a queued work item"
            );
            self.ctx.engine.process(lifecycle_event(EventType::WorkItemStarted, item)).await;
            (self.dispatch)(root.clone());
        }
        outcome
    }
}

/// Block until a ledger or pacing event arrives on the broadcast. A lagged
/// receiver counts as a wake: the tick reads the ledger, not the events.
async fn wake_on_ledger_event(events: &mut broadcast::Receiver<Event>) {
    loop {
        match events.recv().await {
            Ok(event) if wakes_scheduler(&event.event_type) => return,
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => return,
            Err(broadcast::error::RecvError::Closed) => {
                // Nothing will ever wake us again; fall back to the interval.
                tokio::time::sleep(IDLE_TICK).await;
                return;
            }
        }
    }
}

/// Whether an event on the broadcast means the ledger or the pacing state may
/// have changed since the last tick.
fn wakes_scheduler(event_type: &EventType) -> bool {
    is_work_item_event(event_type)
        || matches!(event_type, EventType::PacingPaused | EventType::PacingResumed)
}

/// The `load` → decide → apply → `save` half of a tick, under the gate.
///
/// Returns what to announce and dispatch. A ledger that cannot be read or
/// saved yields an empty outcome: nothing is announced or dispatched that the
/// ledger does not record.
fn apply_tick(
    path: &Path,
    pacing: &PacingPaths,
    now: DateTime<Utc>,
    repository_of: &dyn Fn(&str) -> String,
) -> TickOutcome {
    let Ok(_guard) = ledger_write_gate().lock() else {
        // Best-effort: a poisoned gate means some other mutation panicked
        // mid-sequence; this tick starts nothing and the next one retries.
        tracing::warn!("work-item ledger lock poisoned; pacing tick starts nothing");
        return TickOutcome::default();
    };
    let mut store = match WorkItemStore::load(path) {
        Ok(store) => store,
        Err(error) => {
            // Best-effort: an unreadable ledger is logged on every tick and
            // starts nothing until it is readable again.
            tracing::warn!(path = %path.display(), %error, "could not read the work-item ledger; pacing tick starts nothing");
            return TickOutcome::default();
        }
    };
    let limits = pacing::limits_in_force(&pacing.limits);
    let pauses = pacing::pauses_in_force(&pacing.state);
    let verdicts = pacing::evaluate(&store.items, &limits, &pauses, now, repository_of);

    let mut outcome = TickOutcome::default();
    let mut changed = false;
    for verdict in verdicts {
        let Some(item) = store.find_mut(&verdict.id) else {
            continue;
        };
        match verdict.decision {
            Decision::Start => {
                if let Some(root) = item.pending_root.clone() {
                    item.start(now);
                    let started = build_started_root(&root, &item.id);
                    outcome.started.push((item.clone(), started));
                } else {
                    item.settle_failed(NO_ROOT_REASON, now);
                    outcome.settled.push(item.clone());
                }
            }
            Decision::Wait(reason) => {
                if item.reason != reason {
                    item.reason = reason;
                    changed = true;
                }
            }
            Decision::NeedsDecision(reason) => {
                item.settle_needs_decision(&reason, now);
                item.pending_root = None;
                outcome.settled.push(item.clone());
            }
        }
    }
    outcome.next_not_before =
        store.queued().filter_map(|item| item.not_before).filter(|at| *at > now).min();

    if !changed && outcome.started.is_empty() && outcome.settled.is_empty() {
        return outcome;
    }
    if let Err(error) = store.save(path) {
        // Best-effort: nothing was written, so nothing is announced or
        // dispatched; the next tick decides again from the ledger as it was.
        tracing::warn!(path = %path.display(), %error, "could not write the work-item ledger; pacing tick starts nothing");
        return TickOutcome {
            next_not_before: outcome.next_not_before,
            ..TickOutcome::default()
        };
    }
    outcome
}

/// The root that runs `item`: the admitted root's request, as a fresh event
/// naming the item, on the same trace and under the admitted root's span.
#[must_use]
pub fn build_started_root(admitted: &Event, item_id: &str) -> Event {
    let mut payload = admitted.payload.clone();
    payload[ADMITTED_WORK_ITEM_KEY] = serde_json::Value::String(item_id.to_string());
    Event::new(
        admitted.event_type.clone(),
        admitted.project.clone(),
        admitted.throttle,
        payload,
    )
    .with_trace_id(admitted.trace_id.clone())
    .with_span_ids(Some(mint_span_id()), admitted.span_id.clone())
    .with_gather_id(admitted.gather_id.clone())
    .with_source(admitted.source.clone())
}

/// The `work_item_*` event recording one scheduler transition of `item`, on
/// the item's own trace.
fn lifecycle_event(event_type: EventType, item: &WorkItem) -> Event {
    #[allow(
        clippy::expect_used,
        reason = "WorkItemEventPayload is infallibly serializable (Payload Conventions, AGENTS.md)"
    )]
    let payload = Event::serialize_payload(&WorkItemEventPayload::from_item(item))
        .expect("WorkItemEventPayload is infallibly serializable");
    Event::new(event_type, item.project.clone(), Throttle::Full, payload)
        .with_trace_id(item.trace_id.clone())
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, RwLock};

    use chrono::TimeZone as _;
    use foundry_engine::engine::Engine;
    use foundry_sdk::pacing::{Limits, PauseState};
    use foundry_sdk::registry::Registry;
    use foundry_sdk::work_item::{WorkItemKind, WorkItemSpec, WorkItemState, WorkLane};
    use foundry_sdk::work_source::WorkSource;

    use super::*;
    use crate::service::settle_running_work_items_on_start;

    fn at(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, second).single().expect("valid date")
    }

    fn root(project: &str, extra: &serde_json::Value) -> Event {
        let mut payload =
            serde_json::json!({"project": project, "workflow": "task", "prompt": "x"});
        if let (Some(target), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                target.insert(key.clone(), value.clone());
            }
        }
        Event::new(EventType::ExecutionRequested, project.to_string(), Throttle::Full, payload)
            .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
            .with_span_ids(Some(mint_span_id()), None)
    }

    fn queued(
        id: &str,
        project: &str,
        kind: WorkItemKind,
        lane: WorkLane,
        second: u32,
    ) -> WorkItem {
        let trace = Some(id.repeat(2));
        let mut item = WorkItem::queued(
            WorkItemSpec {
                project: project.to_string(),
                objective: "o".to_string(),
                kind,
                lane,
                origin: "test".to_string(),
                trace_id: trace.clone(),
            },
            root(project, &serde_json::json!({})).with_trace_id(trace),
            at(second),
        );
        item.id = id.to_string();
        item.reason = "ready".to_string();
        item
    }

    fn task(id: &str, project: &str, second: u32) -> WorkItem {
        queued(id, project, WorkItemKind::Task, WorkLane::Interactive, second)
    }

    fn running(id: &str, project: &str, kind: WorkItemKind) -> WorkItem {
        let mut item = WorkItem::dispatched(
            WorkItemSpec {
                project: project.to_string(),
                objective: "o".to_string(),
                kind,
                lane: WorkLane::Maintenance,
                origin: "test".to_string(),
                trace_id: Some(id.repeat(2)),
            },
            at(0),
        );
        item.id = id.to_string();
        item
    }

    /// A scheduler over a temp ledger, with a recording dispatcher, a pinned
    /// clock and a broadcasting engine so announcements can be observed.
    struct Harness {
        dir: tempfile::TempDir,
        scheduler: PacingScheduler,
        dispatched: Arc<Mutex<Vec<Event>>>,
        events: broadcast::Receiver<Event>,
        clock: Arc<Mutex<DateTime<Utc>>>,
    }

    impl Harness {
        fn new(items: Vec<WorkItem>) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let ledger = dir.path().join("work-items.json");
            WorkItemStore { version: 1, items }.save(&ledger).expect("seed ledger");
            let (event_tx, events) = broadcast::channel(256);
            let engine =
                Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()).with_event_writer(
                    Arc::new(foundry_engine::event_writer::EventWriter::new(
                        dir.path().join("events"),
                    )),
                ));
            let trace_writer = Arc::new(foundry_blocks::trace_writer::TraceWriter::new(
                dir.path().join("traces").to_str().expect("utf-8"),
            ));
            let ctx = RuntimeContext {
                engine,
                trace_store: Arc::new(crate::trace_store::TraceStore::with_trace_writer(
                    Duration::from_secs(60),
                    Arc::clone(&trace_writer),
                )),
                workflow_tracker: Arc::new(crate::workflow_tracker::WorkflowTracker::new()),
                trace_writer,
                event_tx,
                registry: Arc::new(RwLock::new(Registry {
                    version: 2,
                    projects: vec![],
                })),
            };
            let dispatched: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
            let recorder = Arc::clone(&dispatched);
            let clock = Arc::new(Mutex::new(at(30)));
            let clock_state = Arc::clone(&clock);
            let scheduler = PacingScheduler::new(
                ctx,
                ledger,
                Self::pacing_paths(dir.path()),
                Arc::new(Notify::new()),
            )
            .with_dispatch(Arc::new(move |event| recorder.lock().expect("lock").push(event)))
            .with_clock(Arc::new(move || *clock_state.lock().expect("lock")));
            Self {
                dir,
                scheduler,
                dispatched,
                events,
                clock,
            }
        }

        fn pacing_paths(dir: &Path) -> PacingPaths {
            PacingPaths {
                limits: dir.join("pacing.json"),
                state: dir.join("pacing-state.json"),
            }
        }

        fn ledger_path(&self) -> PathBuf {
            self.dir.path().join("work-items.json")
        }

        fn ledger(&self) -> WorkItemStore {
            WorkItemStore::load(&self.ledger_path()).expect("load ledger")
        }

        fn item(&self, id: &str) -> WorkItem {
            self.ledger().find(id).cloned().unwrap_or_else(|| panic!("{id} in the ledger"))
        }

        fn settle(&self, id: &str, state: WorkItemState) {
            let mut store = self.ledger();
            let item = store.find_mut(id).expect("item");
            item.state = state;
            item.settled_at = Some(at(40));
            store.save(&self.ledger_path()).expect("save");
        }

        fn dispatched_ids(&self) -> Vec<String> {
            self.dispatched
                .lock()
                .expect("lock")
                .iter()
                .map(|event| {
                    event.payload[ADMITTED_WORK_ITEM_KEY].as_str().expect("id").to_string()
                })
                .collect()
        }

        fn drain_events(&mut self) -> Vec<Event> {
            let mut out = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                out.push(event);
            }
            out
        }

        fn set_clock(&self, now: DateTime<Utc>) {
            *self.clock.lock().expect("lock") = now;
        }
    }

    // --- the first review statement: repository, host, idle -----------------

    #[tokio::test]
    async fn two_tasks_on_one_project_run_one_after_the_other() {
        let h = Harness::new(vec![task("wi_first", "alpha", 1), task("wi_second", "alpha", 2)]);

        let outcome = h.scheduler.tick().await;
        assert_eq!(outcome.started.len(), 1);
        assert_eq!(h.dispatched_ids(), vec!["wi_first"]);
        assert_eq!(h.item("wi_first").state, WorkItemState::Running);
        assert!(h.item("wi_first").started_at.is_some());
        assert_eq!(h.item("wi_first").pending_root, None);
        assert_eq!(h.item("wi_second").state, WorkItemState::Queued);
        assert_eq!(h.item("wi_second").reason, "repository busy: wi_first");

        h.scheduler.tick().await;
        assert_eq!(h.dispatched_ids(), vec!["wi_first"], "still busy: nothing else starts");

        h.settle("wi_first", WorkItemState::Landed);
        h.scheduler.tick().await;
        assert_eq!(h.dispatched_ids(), vec!["wi_first", "wi_second"]);
        assert_eq!(h.item("wi_second").state, WorkItemState::Running);
    }

    #[tokio::test]
    async fn two_tasks_on_different_projects_run_at_once_and_a_third_waits_for_the_host() {
        let h = Harness::new(vec![
            task("wi_a", "alpha", 1),
            task("wi_b", "beta", 2),
            task("wi_c", "gamma", 3),
        ]);

        h.scheduler.tick().await;

        assert_eq!(h.dispatched_ids(), vec!["wi_a", "wi_b"]);
        assert_eq!(h.item("wi_c").state, WorkItemState::Queued);
        assert_eq!(h.item("wi_c").reason, "host at capacity 2/2");

        h.settle("wi_a", WorkItemState::Landed);
        h.scheduler.tick().await;
        assert_eq!(h.dispatched_ids(), vec!["wi_a", "wi_b", "wi_c"]);
    }

    #[tokio::test]
    async fn a_single_task_on_an_idle_daemon_starts_within_one_tick() {
        let mut h = Harness::new(vec![task("wi_only", "alpha", 1)]);

        let outcome = h.scheduler.tick().await;

        assert_eq!(outcome.started.len(), 1);
        assert_eq!(h.dispatched_ids(), vec!["wi_only"]);
        let started = h.dispatched.lock().expect("lock")[0].clone();
        assert_eq!(started.event_type, EventType::ExecutionRequested);
        assert_eq!(started.payload["prompt"], "x");
        assert_eq!(started.trace_id, h.item("wi_only").trace_id);
        let announced: Vec<String> =
            h.drain_events().iter().map(|event| event.event_type.as_str()).collect();
        assert_eq!(announced, vec!["work_item_started"]);
    }

    #[tokio::test]
    async fn the_started_root_names_the_item_and_opens_under_the_admitted_span() {
        let admitted = root("alpha", &serde_json::json!({"campaign": "tidy", "campaign_cycle": 2}))
            .with_source(Some(WorkSource::campaign("tidy", 2)))
            .with_gather_id(Some("gth_1".to_string()));
        let started = build_started_root(&admitted, "wi_cycle");
        assert_ne!(started.id, admitted.id, "a fresh event, never the admitted root twice");
        assert_eq!(started.payload[ADMITTED_WORK_ITEM_KEY], "wi_cycle");
        assert_eq!(started.payload["campaign"], "tidy");
        assert_eq!(started.trace_id, admitted.trace_id);
        assert_eq!(started.parent_span_id, admitted.span_id);
        assert_ne!(started.span_id, admitted.span_id);
        assert_eq!(started.gather_id.as_deref(), Some("gth_1"));
        assert_eq!(started.source, admitted.source);
        assert_eq!(started.throttle, admitted.throttle);
    }

    // --- the second review statement: --after and --not-before --------------

    #[tokio::test]
    async fn a_dependent_waits_on_its_dependency_and_starts_once_it_lands() {
        let mut dependent = task("wi_b", "beta", 2);
        dependent.depends_on = vec!["wi_a".to_string()];
        let h = Harness::new(vec![task("wi_a", "alpha", 1), dependent]);

        h.scheduler.tick().await;
        assert_eq!(h.dispatched_ids(), vec!["wi_a"]);
        assert_eq!(h.item("wi_b").reason, "waits on wi_a");

        h.settle("wi_a", WorkItemState::Landed);
        h.scheduler.tick().await;
        assert_eq!(h.dispatched_ids(), vec!["wi_a", "wi_b"]);
    }

    #[tokio::test]
    async fn a_dependency_settling_other_than_landed_moves_the_dependent_to_needs_decision() {
        for state in [
            WorkItemState::Preserved,
            WorkItemState::Failed,
            WorkItemState::Cancelled,
        ] {
            let mut dependent = task("wi_b", "beta", 2);
            dependent.depends_on = vec!["wi_a".to_string()];
            let mut h = Harness::new(vec![task("wi_a", "alpha", 1), dependent]);
            h.scheduler.tick().await;
            h.drain_events();
            h.settle("wi_a", state);

            let outcome = h.scheduler.tick().await;

            assert_eq!(outcome.settled.len(), 1, "{state:?}");
            let item = h.item("wi_b");
            assert_eq!(item.state, WorkItemState::NeedsDecision);
            assert_eq!(item.reason, format!("waits on wi_a, which settled {}", state.tag()));
            assert!(item.settled_at.is_some());
            assert_eq!(item.started_at, None, "it never started");
            assert_eq!(h.dispatched_ids(), vec!["wi_a"], "nothing was dispatched for it");
            let announced = h.drain_events();
            assert_eq!(announced.len(), 1);
            assert_eq!(announced[0].event_type, EventType::WorkItemSettled);
            assert_eq!(announced[0].payload["state"], "needs_decision");
            assert_eq!(announced[0].payload["item_id"], "wi_b");
        }
    }

    #[tokio::test]
    async fn not_before_holds_the_item_until_the_time_and_the_tick_reports_when_to_wake() {
        let mut item = task("wi_late", "alpha", 1);
        item.not_before = Some(at(45));
        let h = Harness::new(vec![item]);

        let outcome = h.scheduler.tick().await;
        assert!(outcome.started.is_empty());
        assert_eq!(outcome.next_not_before, Some(at(45)));
        assert_eq!(h.item("wi_late").reason, "not before 2026-10-10T12:00:45+00:00");

        h.set_clock(at(45));
        let outcome = h.scheduler.tick().await;
        assert_eq!(outcome.started.len(), 1);
        assert_eq!(outcome.next_not_before, None);
        assert_eq!(h.dispatched_ids(), vec!["wi_late"]);
    }

    // --- the third and fourth review statements: pause, restart ---------------

    #[tokio::test]
    async fn a_paused_lane_stops_new_starts_leaves_running_items_alone_and_survives_a_restart() {
        let h = Harness::new(vec![
            running("wi_running", "gamma", WorkItemKind::Task),
            task("wi_a", "alpha", 1),
        ]);
        let mut pauses = PauseState::default();
        pauses.pause(&[WorkLane::Interactive]);
        pauses.save(&Harness::pacing_paths(h.dir.path()).state).expect("save pause");

        h.scheduler.tick().await;
        assert!(h.dispatched_ids().is_empty());
        assert_eq!(h.item("wi_a").reason, "lane paused");
        assert_eq!(h.item("wi_running").state, WorkItemState::Running, "untouched");

        // A restart: a new scheduler over the same files is still paused.
        let dir = h.dir;
        let ledger = dir.path().join("work-items.json");
        let (event_tx, _events) = broadcast::channel(16);
        let ctx = RuntimeContext {
            engine: Arc::new(Engine::new().with_event_broadcaster(event_tx.clone())),
            trace_store: Arc::new(crate::trace_store::TraceStore::new(Duration::from_secs(60))),
            workflow_tracker: Arc::new(crate::workflow_tracker::WorkflowTracker::new()),
            trace_writer: Arc::new(foundry_blocks::trace_writer::TraceWriter::new(
                dir.path().join("traces").to_str().expect("utf-8"),
            )),
            event_tx,
            registry: Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        };
        let dispatched: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&dispatched);
        let restarted = PacingScheduler::new(
            ctx,
            ledger.clone(),
            Harness::pacing_paths(dir.path()),
            Arc::new(Notify::new()),
        )
        .with_dispatch(Arc::new(move |event| recorder.lock().expect("lock").push(event)));
        restarted.tick().await;
        assert!(dispatched.lock().expect("lock").is_empty(), "the pause is still in force");

        let mut pauses = PauseState::load(&Harness::pacing_paths(dir.path()).state).expect("load");
        pauses.resume(&[WorkLane::Interactive]);
        pauses.save(&Harness::pacing_paths(dir.path()).state).expect("save");
        restarted.tick().await;
        assert_eq!(dispatched.lock().expect("lock").len(), 1, "resume starts the next item");
        assert_eq!(
            WorkItemStore::load(&ledger).expect("load").find("wi_a").expect("a").state,
            WorkItemState::Running
        );
    }

    #[tokio::test]
    async fn queued_and_held_items_survive_a_restart_and_are_re_evaluated_on_the_first_tick() {
        let mut held = task("wi_held", "beta", 2);
        held.hold();
        let h = Harness::new(vec![
            running("wi_was_running", "gamma", WorkItemKind::Task),
            task("wi_queued", "alpha", 1),
            held,
        ]);

        // The restart sweep runs before the scheduler's first tick.
        settle_running_work_items_on_start(&h.scheduler.ctx, &h.ledger_path()).await;
        assert_eq!(h.item("wi_was_running").state, WorkItemState::Failed);
        assert_eq!(h.item("wi_was_running").reason, "daemon restarted");
        assert_eq!(h.item("wi_queued").state, WorkItemState::Queued);
        assert_eq!(h.item("wi_held").state, WorkItemState::Held);

        h.scheduler.tick().await;

        assert_eq!(h.dispatched_ids(), vec!["wi_queued"]);
        assert_eq!(h.item("wi_held").state, WorkItemState::Held, "held is the operator's call");
    }

    // --- the fifth review statement: every task-shaped kind, one scheduler ----

    #[tokio::test]
    async fn a_campaign_cycle_a_majors_task_and_a_resume_child_pass_through_the_same_scheduler() {
        let mut cycle =
            queued("wi_cycle", "alpha", WorkItemKind::CampaignCycle, WorkLane::Campaign, 1);
        cycle.source = Some(WorkSource::campaign("tidy", 1));
        let major =
            queued("wi_major", "beta", WorkItemKind::MajorUpgrade, WorkLane::Maintenance, 2);
        let mut child = task("wi_child", "gamma", 3);
        child.resumes = Some("wi_parent".to_string());
        let h = Harness::new(vec![cycle, major, child]);
        std::fs::write(Harness::pacing_paths(h.dir.path()).limits, r#"{"max_running": 3}"#)
            .expect("limits");

        h.scheduler.tick().await;

        assert_eq!(
            h.dispatched_ids(),
            vec!["wi_child", "wi_cycle", "wi_major"],
            "interactive first, then oldest"
        );
        for id in ["wi_cycle", "wi_major", "wi_child"] {
            assert_eq!(h.item(id).state, WorkItemState::Running, "{id}");
        }
    }

    #[tokio::test]
    async fn a_running_maintenance_item_blocks_an_interactive_task_on_the_same_repository() {
        let h = Harness::new(vec![
            running("wi_nightly", "alpha", WorkItemKind::Maintenance),
            task("wi_task", "alpha", 1),
        ]);

        h.scheduler.tick().await;

        assert!(h.dispatched_ids().is_empty());
        assert_eq!(h.item("wi_task").reason, "repository busy: wi_nightly");
    }

    #[tokio::test]
    async fn the_limits_file_raises_the_host_cap_without_a_restart() {
        let h = Harness::new(vec![
            task("wi_a", "alpha", 1),
            task("wi_b", "beta", 2),
            task("wi_c", "gamma", 3),
        ]);
        h.scheduler.tick().await;
        assert_eq!(h.dispatched_ids().len(), 2);
        std::fs::write(
            Harness::pacing_paths(h.dir.path()).limits,
            serde_json::to_string(&Limits { max_running: 3 }).expect("json"),
        )
        .expect("limits");
        h.scheduler.tick().await;
        assert_eq!(h.dispatched_ids().len(), 3);
    }

    // --- faults --------------------------------------------------------------

    #[tokio::test]
    async fn a_queued_item_with_no_root_settles_failed_and_names_why() {
        let mut orphan = task("wi_orphan", "alpha", 1);
        orphan.pending_root = None;
        let h = Harness::new(vec![orphan]);

        let outcome = h.scheduler.tick().await;

        assert!(outcome.started.is_empty());
        assert_eq!(h.item("wi_orphan").state, WorkItemState::Failed);
        assert_eq!(h.item("wi_orphan").reason, NO_ROOT_REASON);
        assert!(h.dispatched_ids().is_empty());
    }

    #[tokio::test]
    async fn an_unwritable_ledger_dispatches_nothing() {
        let h = Harness::new(vec![task("wi_a", "alpha", 1)]);
        // A directory where the temp file would go makes the save fail.
        std::fs::create_dir(h.ledger_path().with_extension("json.tmp")).expect("block the save");

        let outcome = h.scheduler.tick().await;

        assert!(outcome.started.is_empty());
        assert!(h.dispatched_ids().is_empty());
        assert_eq!(h.item("wi_a").state, WorkItemState::Queued, "the ledger is as it was");
    }

    #[tokio::test]
    async fn the_wake_handle_ticks_the_loop_without_an_event() {
        let h = Harness::new(vec![]);
        let ledger = h.ledger_path();
        let dispatched = Arc::clone(&h.dispatched);
        let wake = Arc::clone(&h.scheduler.wake);
        tokio::spawn(h.scheduler.run());
        tokio::time::sleep(Duration::from_millis(50)).await;
        {
            let _guard = ledger_write_gate().lock().expect("gate");
            let mut store = WorkItemStore::load(&ledger).expect("load");
            store.upsert(task("wi_quiet", "alpha", 1));
            store.save(&ledger).expect("save");
        }
        wake.notify_one();
        assert!(wait_until(|| dispatched.lock().expect("lock").len() == 1).await);
    }

    #[test]
    fn only_ledger_and_pacing_events_wake_the_scheduler() {
        assert!(wakes_scheduler(&EventType::WorkItemSubmitted));
        assert!(wakes_scheduler(&EventType::WorkItemSettled));
        assert!(wakes_scheduler(&EventType::WorkItemReleased));
        assert!(wakes_scheduler(&EventType::PacingResumed));
        assert!(!wakes_scheduler(&EventType::TaskRunStarted));
        assert!(!wakes_scheduler(&EventType::ExecutionRequested));
    }

    /// Poll until `done` holds or five seconds pass.
    async fn wait_until(done: impl Fn() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            if done() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        done()
    }

    #[tokio::test]
    async fn the_loop_starts_an_item_admitted_after_it_began_waiting() {
        let h = Harness::new(vec![]);
        let ledger = h.ledger_path();
        let dispatched = Arc::clone(&h.dispatched);
        let event_tx = h.scheduler.ctx.event_tx.clone();
        tokio::spawn(h.scheduler.run());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(dispatched.lock().expect("lock").is_empty());

        // An admission: the ledger gains a queued item and the submitted event
        // reaches the broadcast, exactly as the admission block does it.
        let item = task("wi_new", "alpha", 1);
        {
            let _guard = ledger_write_gate().lock().expect("gate");
            let mut store = WorkItemStore::load(&ledger).expect("load");
            store.upsert(item.clone());
            store.save(&ledger).expect("save");
        }
        event_tx
            .send(lifecycle_event(EventType::WorkItemSubmitted, &item))
            .expect("broadcast");
        assert!(wait_until(|| !dispatched.lock().expect("lock").is_empty()).await);
        assert_eq!(
            dispatched
                .lock()
                .expect("lock")
                .iter()
                .map(|e| e.payload[ADMITTED_WORK_ITEM_KEY].clone())
                .collect::<Vec<_>>(),
            vec![serde_json::json!("wi_new")]
        );
    }
}
