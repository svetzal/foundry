//! In-process scheduler that fires sentinels (declarative, named, scheduled
//! triggers) by emitting their configured event into the engine when due.
//!
//! Replaces the launchd-driven nightly maintenance run with a scheduler that
//! lives inside `foundryd`. The trace store, audit logs, and event history
//! now see *why* a maintenance cycle started — because a sentinel fired —
//! rather than receiving an unexplained `MaintenanceCycleStarted` from
//! outside the daemon.
//!
//! ## Design
//!
//! The hot path is:
//!
//! 1. Read the [`SentinelStore`] (briefly — drop the lock before awaiting).
//! 2. For each enabled sentinel, compute its next firing in the local
//!    timezone via the standard 5-field cron expression (auto-padded to the
//!    6-field shape the `cron` crate expects).
//! 3. Pick the soonest deadline.
//! 4. `tokio::select!` between sleeping until that deadline and a reload
//!    `Notify`. On fire, hand the event to the configured emit callback.
//!    On reload, re-compute deadlines.
//!
//! ## Catch-up policy
//!
//! Missed firings are silently skipped. If the daemon was down at 02:00 and
//! starts at 05:00, the next firing is the *next* 02:00 — not the missed
//! one. This is documented in `book/src/guide/sentinels.md`.

use std::str::FromStr;
use std::sync::{Arc, RwLock};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Local};
use cron::Schedule as CronSchedule;
use tokio::sync::Notify;
use tracing::{info, warn};

use foundry_sdk::event::{Event, mint_span_id, mint_trace_id};
use foundry_sdk::sentinel::{Schedule as SentinelSchedule, SentinelEntry, SentinelStore};

/// Fire-and-forget hook for handing a freshly-built root event to whatever
/// machinery actually runs it. In production this calls
/// [`crate::service::spawn_workflow`]; tests use a recording closure.
pub type EmitFn = Arc<dyn Fn(Event) + Send + Sync + 'static>;

/// Source of the current local-timezone wall-clock time. Indirected so tests
/// can pin a deterministic "now" without touching the global clock.
pub type ClockFn = Arc<dyn Fn() -> DateTime<Local> + Send + Sync + 'static>;

/// Default clock — returns the real `chrono::Local::now()`.
fn system_clock() -> ClockFn {
    Arc::new(Local::now)
}

/// The scheduler. Owns shared references to the sentinel store and a reload
/// notify handle; `run` consumes `self` and loops forever.
pub struct Scheduler {
    store: Arc<RwLock<SentinelStore>>,
    reload: Arc<Notify>,
    emit: EmitFn,
    clock: ClockFn,
}

impl Scheduler {
    /// Construct a scheduler using the real system clock.
    pub fn new(store: Arc<RwLock<SentinelStore>>, reload: Arc<Notify>, emit: EmitFn) -> Self {
        Self {
            store,
            reload,
            emit,
            clock: system_clock(),
        }
    }

    /// Override the clock used to compute "now". Test-only knob.
    #[cfg(test)]
    fn with_clock(mut self, clock: ClockFn) -> Self {
        self.clock = clock;
        self
    }

    /// Run the scheduler loop. Never returns under normal operation.
    pub async fn run(self) {
        info!("scheduler started");
        loop {
            let now = (self.clock)();
            let next = compute_next_firing(&self.store, now);

            match next {
                None => {
                    // No enabled sentinels — block until a mutation reloads us.
                    self.reload.notified().await;
                }
                Some(NextFiring { entry, fire_at }) => {
                    let wait =
                        (fire_at - now).to_std().unwrap_or(std::time::Duration::from_secs(0));
                    info!(
                        sentinel = %entry.name,
                        fire_at = %fire_at,
                        wait_secs = wait.as_secs(),
                        wait_ms = %wait.as_millis(),
                        "scheduler armed",
                    );
                    tokio::select! {
                        () = tokio::time::sleep(wait) => {
                            let woke_at = (self.clock)();
                            if !sleep_reached_deadline(woke_at, fire_at) {
                                // TODO: Replace this defensive guard with a scheduler design that
                                // tracks claimed scheduled instants explicitly instead of relying on
                                // recomputing cron after a timer wake.
                                warn!(
                                    sentinel = %entry.name,
                                    fire_at = %fire_at,
                                    woke_at = %woke_at,
                                    "scheduler woke before scheduled boundary; rearming"
                                );
                                continue;
                            }
                            let event = build_event(&entry);
                            info!(
                                sentinel = %entry.name,
                                event_type = %event.event_type,
                                project = %event.project,
                                "sentinel firing",
                            );
                            (self.emit)(event);
                        }
                        () = self.reload.notified() => {
                            info!("scheduler reload signal received");
                        }
                    }
                }
            }
        }
    }
}

/// The earliest pending firing across the store, returned together with the
/// cloned entry so callers can release the read lock immediately.
#[derive(Debug, Clone)]
struct NextFiring {
    entry: SentinelEntry,
    fire_at: DateTime<Local>,
}

fn compute_next_firing(
    store: &Arc<RwLock<SentinelStore>>,
    now: DateTime<Local>,
) -> Option<NextFiring> {
    let snapshot: Vec<SentinelEntry> = {
        let guard = store.read().expect("sentinel store lock poisoned");
        guard.sentinels.iter().filter(|s| s.enabled).cloned().collect()
    };
    next_firing_among(&snapshot, now)
}

/// Pure helper: choose the soonest firing among a list of enabled entries.
fn next_firing_among(entries: &[SentinelEntry], now: DateTime<Local>) -> Option<NextFiring> {
    let mut best: Option<NextFiring> = None;
    for entry in entries {
        match next_firing_for(&entry.schedule, now) {
            Ok(fire_at) => {
                if best.as_ref().is_none_or(|b| fire_at < b.fire_at) {
                    best = Some(NextFiring {
                        entry: entry.clone(),
                        fire_at,
                    });
                }
            }
            Err(e) => {
                warn!(
                    sentinel = %entry.name,
                    error = %e,
                    "invalid schedule; skipping sentinel"
                );
            }
        }
    }
    best
}

fn sleep_reached_deadline(woke_at: DateTime<Local>, fire_at: DateTime<Local>) -> bool {
    woke_at >= fire_at
}

/// Pure helper: compute the next firing for a given schedule after `now`.
fn next_firing_for(schedule: &SentinelSchedule, now: DateTime<Local>) -> Result<DateTime<Local>> {
    match schedule {
        SentinelSchedule::Cron(expr) => {
            let canonical = expand_to_six_field(expr);
            let parsed = CronSchedule::from_str(&canonical)
                .map_err(|e| anyhow!("invalid cron expression '{expr}': {e}"))?;
            parsed
                .after(&now)
                .next()
                .ok_or_else(|| anyhow!("cron expression '{expr}' produced no future firings"))
        }
    }
}

/// The `cron` crate expects 6 fields (`sec min hour dom month dow`); standard
/// cron is 5 fields (`min hour dom month dow`). Prepend `"0"` for seconds
/// when a 5-field expression is given so users can paste vanilla cron.
fn expand_to_six_field(expr: &str) -> String {
    let trimmed = expr.trim();
    let field_count = trimmed.split_whitespace().count();
    if field_count == 5 {
        format!("0 {trimmed}")
    } else {
        trimmed.to_string()
    }
}

/// Build the root event a sentinel fires. Mints fresh trace and span IDs so
/// downstream stamping treats this as a root, exactly like the gRPC `emit()`
/// path does for events with no incoming trace context.
fn build_event(entry: &SentinelEntry) -> Event {
    Event::new(
        entry.emit.event_type.clone(),
        entry.emit.project.clone(),
        entry.emit.throttle,
        entry.emit.payload.clone(),
    )
    .with_trace_id(Some(mint_trace_id()))
    .with_span_ids(Some(mint_span_id()), None)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::TimeZone;
    use foundry_sdk::event::EventType;
    use foundry_sdk::sentinel::{EmitSpec, Schedule, SentinelEntry, SentinelStore};
    use foundry_sdk::throttle::Throttle;

    use super::*;

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, minute, 0)
            .single()
            .expect("constructable local time")
    }

    fn entry(name: &str, cron: &str, enabled: bool) -> SentinelEntry {
        SentinelEntry {
            name: name.to_string(),
            schedule: Schedule::Cron(cron.to_string()),
            emit: EmitSpec {
                event_type: EventType::MaintenanceCycleStarted,
                project: "system".to_string(),
                throttle: Throttle::Full,
                payload: serde_json::json!({}),
            },
            enabled,
        }
    }

    // ---------------------------------------------------------------------
    // expand_to_six_field
    // ---------------------------------------------------------------------

    #[test]
    fn expand_pads_five_field_with_leading_zero() {
        assert_eq!(expand_to_six_field("0 2 * * *"), "0 0 2 * * *");
    }

    #[test]
    fn expand_leaves_six_field_alone() {
        assert_eq!(expand_to_six_field("30 0 2 * * *"), "30 0 2 * * *");
    }

    #[test]
    fn expand_trims_whitespace_before_counting_fields() {
        assert_eq!(expand_to_six_field("  0 2 * * *  "), "0 0 2 * * *");
    }

    // ---------------------------------------------------------------------
    // next_firing_for
    // ---------------------------------------------------------------------

    #[test]
    fn next_firing_for_daily_02_when_now_is_01() {
        let now = at(2026, 5, 27, 1, 0);
        let fire = next_firing_for(&Schedule::Cron("0 2 * * *".to_string()), now).unwrap();
        assert_eq!(fire, at(2026, 5, 27, 2, 0));
    }

    #[test]
    fn next_firing_for_daily_02_when_now_is_03_rolls_to_next_day() {
        let now = at(2026, 5, 27, 3, 0);
        let fire = next_firing_for(&Schedule::Cron("0 2 * * *".to_string()), now).unwrap();
        assert_eq!(fire, at(2026, 5, 28, 2, 0));
    }

    #[test]
    fn next_firing_for_accepts_six_field_expression_directly() {
        let now = at(2026, 5, 27, 1, 0);
        let fire = next_firing_for(&Schedule::Cron("0 0 2 * * *".to_string()), now).unwrap();
        assert_eq!(fire, at(2026, 5, 27, 2, 0));
    }

    #[test]
    fn next_firing_for_invalid_cron_returns_error() {
        let now = at(2026, 5, 27, 1, 0);
        let err =
            next_firing_for(&Schedule::Cron("definitely not cron".to_string()), now).unwrap_err();
        assert!(err.to_string().contains("invalid cron"));
    }

    // ---------------------------------------------------------------------
    // next_firing_among
    // ---------------------------------------------------------------------

    #[test]
    fn next_firing_among_returns_none_for_empty_input() {
        let now = at(2026, 5, 27, 1, 0);
        assert!(next_firing_among(&[], now).is_none());
    }

    #[test]
    fn next_firing_among_picks_soonest_of_two_entries() {
        let now = at(2026, 5, 27, 1, 0);
        let earlier = entry("earlier", "0 2 * * *", true); // 02:00
        let later = entry("later", "0 9 * * *", true); // 09:00
        let result = next_firing_among(&[later.clone(), earlier.clone()], now).unwrap();
        assert_eq!(result.entry.name, "earlier");
        assert_eq!(result.fire_at, at(2026, 5, 27, 2, 0));
    }

    #[test]
    fn next_firing_among_skips_invalid_schedules() {
        let now = at(2026, 5, 27, 1, 0);
        let bad = entry("broken", "not a cron", true);
        let good = entry("good", "0 2 * * *", true);
        let result = next_firing_among(&[bad, good], now).unwrap();
        assert_eq!(result.entry.name, "good");
    }

    #[test]
    fn next_firing_among_returns_none_when_all_invalid() {
        let now = at(2026, 5, 27, 1, 0);
        let bad = entry("bad", "not a cron", true);
        assert!(next_firing_among(&[bad], now).is_none());
    }

    // ---------------------------------------------------------------------
    // compute_next_firing (through the shared store)
    // ---------------------------------------------------------------------

    #[test]
    fn compute_next_firing_skips_disabled_entries() {
        let store = SentinelStore {
            version: 1,
            sentinels: vec![
                entry("disabled-early", "0 2 * * *", false),
                entry("enabled-later", "0 9 * * *", true),
            ],
        };
        let store = Arc::new(RwLock::new(store));
        let now = at(2026, 5, 27, 1, 0);
        let result = compute_next_firing(&store, now).unwrap();
        assert_eq!(result.entry.name, "enabled-later");
    }

    #[test]
    fn compute_next_firing_returns_none_when_all_disabled() {
        let store = SentinelStore {
            version: 1,
            sentinels: vec![entry("off", "0 2 * * *", false)],
        };
        let store = Arc::new(RwLock::new(store));
        let now = at(2026, 5, 27, 1, 0);
        assert!(compute_next_firing(&store, now).is_none());
    }

    // ---------------------------------------------------------------------
    // build_event
    // ---------------------------------------------------------------------

    #[test]
    fn build_event_uses_emit_spec_fields_and_mints_trace_context() {
        let entry = entry("nightly", "0 2 * * *", true);
        let event = build_event(&entry);
        assert_eq!(event.event_type, EventType::MaintenanceCycleStarted);
        assert_eq!(event.project, "system");
        assert_eq!(event.throttle, Throttle::Full);
        assert_eq!(event.payload, serde_json::json!({}));
        let trace_id = event.trace_id.as_ref().expect("trace_id minted");
        assert_eq!(trace_id.len(), 32, "trace_id should be 32 hex chars");
        let span_id = event.span_id.as_ref().expect("span_id minted");
        assert_eq!(span_id.len(), 16, "span_id should be 16 hex chars");
        assert!(event.parent_span_id.is_none(), "root events have no parent span");
    }

    // ---------------------------------------------------------------------
    // run() — async smoke tests with paused tokio time
    // ---------------------------------------------------------------------

    /// Test sink that records every emitted event.
    fn recording_sink() -> (EmitFn, Arc<Mutex<Vec<Event>>>) {
        let captured: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
        let sink_captured = Arc::clone(&captured);
        let sink: EmitFn = Arc::new(move |e| {
            sink_captured.lock().expect("sink lock not poisoned").push(e);
        });
        (sink, captured)
    }

    fn fixed_clock(t: DateTime<Local>) -> ClockFn {
        Arc::new(move || t)
    }

    fn mutable_clock(t: DateTime<Local>) -> (ClockFn, Arc<Mutex<DateTime<Local>>>) {
        let current = Arc::new(Mutex::new(t));
        let clock_current = Arc::clone(&current);
        let clock: ClockFn = Arc::new(move || *clock_current.lock().expect("clock lock poisoned"));
        (clock, current)
    }

    fn set_clock(clock: &Arc<Mutex<DateTime<Local>>>, t: DateTime<Local>) {
        *clock.lock().expect("clock lock poisoned") = t;
    }

    #[test]
    fn sleep_reached_deadline_rejects_early_wake() {
        assert!(!sleep_reached_deadline(at(2026, 5, 27, 1, 59), at(2026, 5, 27, 2, 0)));
    }

    #[test]
    fn sleep_reached_deadline_accepts_boundary_or_later_wake() {
        let fire_at = at(2026, 5, 27, 2, 0);
        assert!(sleep_reached_deadline(fire_at, fire_at));
        assert!(sleep_reached_deadline(at(2026, 5, 27, 2, 1), fire_at));
    }

    #[tokio::test(start_paused = true)]
    async fn run_emits_when_scheduled_time_arrives() {
        let now = at(2026, 5, 27, 1, 59);
        let store = Arc::new(RwLock::new(SentinelStore {
            version: 1,
            sentinels: vec![entry("nightly", "0 2 * * *", true)],
        }));
        let reload = Arc::new(Notify::new());
        let (sink, captured) = recording_sink();
        let (clock, clock_state) = mutable_clock(now);

        let scheduler = Scheduler::new(store, Arc::clone(&reload), sink).with_clock(clock);

        tokio::spawn(scheduler.run());

        // Yield once so the scheduler reaches its sleep point before we
        // advance virtual time past the deadline.
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        set_clock(&clock_state, at(2026, 5, 27, 2, 0));
        tokio::task::yield_now().await;

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1, "exactly one sentinel firing expected");
        assert_eq!(events[0].event_type, EventType::MaintenanceCycleStarted);
        assert_eq!(events[0].project, "system");
    }

    #[tokio::test(start_paused = true)]
    async fn run_rearms_instead_of_emitting_on_early_wake() {
        let now = at(2026, 5, 27, 1, 59);
        let store = Arc::new(RwLock::new(SentinelStore {
            version: 1,
            sentinels: vec![entry("nightly", "0 2 * * *", true)],
        }));
        let reload = Arc::new(Notify::new());
        let (sink, captured) = recording_sink();
        let (clock, clock_state) = mutable_clock(now);

        let scheduler = Scheduler::new(store, Arc::clone(&reload), sink).with_clock(clock);

        tokio::spawn(scheduler.run());

        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        set_clock(&clock_state, at(2026, 5, 27, 1, 59));
        tokio::task::yield_now().await;

        assert!(captured.lock().unwrap().is_empty());

        set_clock(&clock_state, at(2026, 5, 27, 2, 0));
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        tokio::task::yield_now().await;

        let events = captured.lock().unwrap();
        assert_eq!(events.len(), 1, "event should fire after the boundary");
        assert_eq!(events[0].event_type, EventType::MaintenanceCycleStarted);
    }

    #[tokio::test(start_paused = true)]
    async fn run_with_no_enabled_sentinels_blocks_on_reload() {
        let store = Arc::new(RwLock::new(SentinelStore {
            version: 1,
            sentinels: vec![entry("off", "0 2 * * *", false)],
        }));
        let reload = Arc::new(Notify::new());
        let (sink, captured) = recording_sink();
        let now = at(2026, 5, 27, 1, 59);

        let scheduler =
            Scheduler::new(store, Arc::clone(&reload), sink).with_clock(fixed_clock(now));

        tokio::spawn(scheduler.run());

        // Even after advancing a full day, no sentinel is enabled so nothing fires.
        tokio::time::advance(std::time::Duration::from_secs(60 * 60 * 24)).await;
        tokio::task::yield_now().await;

        assert!(captured.lock().unwrap().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn run_reload_signal_wakes_loop_to_recompute() {
        let store = Arc::new(RwLock::new(SentinelStore {
            version: 1,
            sentinels: vec![entry("off", "0 2 * * *", false)],
        }));
        let reload = Arc::new(Notify::new());
        let (sink, captured) = recording_sink();
        let now = at(2026, 5, 27, 1, 59);
        let store_for_mutation = Arc::clone(&store);
        let (clock, clock_state) = mutable_clock(now);

        let scheduler = Scheduler::new(store, Arc::clone(&reload), sink).with_clock(clock);

        tokio::spawn(scheduler.run());

        // Let the loop reach its "no work, wait on reload" state.
        tokio::task::yield_now().await;

        // Enable the sentinel and signal reload.
        {
            let mut guard = store_for_mutation.write().unwrap();
            guard.sentinels[0].enabled = true;
        }
        reload.notify_one();
        tokio::task::yield_now().await;

        // Now the loop is armed for 02:00 (60s from frozen now). Advance past it.
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        set_clock(&clock_state, at(2026, 5, 27, 2, 0));
        tokio::task::yield_now().await;

        assert_eq!(captured.lock().unwrap().len(), 1);
    }
}
