//! The trunk CI watcher: what the branch's GitHub CI said about each commit a
//! task landed.
//!
//! Foundry's gates run on the Foundry host. A landing that passed them all
//! can still turn the branch's CI red, through a tool the CI runner lacks or
//! a platform only CI covers. `SettleWorkItem` records a pending
//! [`TrunkCiWatch`] on every task it settles `landed` at a commit, for a
//! project with a registered GitHub repository. This watcher concludes those
//! watches.
//!
//! ## A tick
//!
//! 1. Under the shared ledger write gate, read every pending watch.
//! 2. Outside the gate, ask GitHub ([`TrunkCiGateway`]) for each commit's runs
//!    and conclude them with [`foundry_sdk::trunk_ci::conclude`]. A red one
//!    also fetches its failed jobs and the end of its failed-job log.
//! 3. Under the gate again, reload the ledger and apply each conclusion with
//!    [`WorkItemStore::apply_trunk_ci`], which re-checks that the watch is
//!    still pending on the same commit: red opens a `trunk_ci` follow-up,
//!    green settles the earlier red follow-ups it carries. Save once.
//! 4. Outside the gate, announce `trunk_ci_checked` for every concluded
//!    watch, and `work_item_submitted` / `work_item_settled` for every
//!    follow-up opened or settled.
//! 5. For every project with no watch left pending, ask the campaigns whose
//!    latest advance stood down awaiting trunk CI to advance again, once per
//!    stood-down advance.
//!
//! ## When it ticks
//!
//! On every `work_item_settled` on the broadcast (a landing has just become
//! watchable), at the next grace or limit deadline of a pending watch, and at
//! least every [`POLL`]. Tests call [`TrunkCiWatcher::tick`] directly, with an
//! injected clock and an in-process gateway: nothing here reads the system
//! clock or GitHub in a test.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;

use foundry_blocks::gateway::TrunkCiGateway;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{TrunkCiCheckedPayload, WorkItemEventPayload};
use foundry_sdk::throttle::Throttle;
use foundry_sdk::trunk_ci::{
    CiRun, CiVerdict, TRUNK_CI_GRACE, TRUNK_CI_LIMIT, TrunkCiConclusion, TrunkCiWatch,
};
use foundry_sdk::work_item::{TrunkCiApplied, WorkItem, WorkItemStore, ledger_write_gate};

use crate::pacing::ClockFn;
use crate::service::RuntimeContext;

/// Asks a campaign to advance again. In production this is the
/// `AdvanceCampaign` RPC's own function, with operator origin
/// [`ADVANCE_ORIGIN`]; tests record the names.
pub type AdvanceHook = Arc<dyn Fn(String) + Send + Sync + 'static>;

/// The longest the watcher waits between ticks.
pub const POLL: Duration = Duration::from_secs(60);

/// The operator origin a campaign advance the watcher asks for carries.
pub const ADVANCE_ORIGIN: &str = "trunk CI watch";

/// What one tick did.
#[derive(Debug, Default)]
pub struct CiTickOutcome {
    /// Every watched item whose watch concluded, as it now stands.
    pub concluded: Vec<WorkItem>,
    /// Every trunk CI follow-up a red watch opened.
    pub opened: Vec<WorkItem>,
    /// Every item a green watch settled.
    pub settled: Vec<WorkItem>,
    /// The campaigns asked to advance again.
    pub advanced: Vec<String>,
    /// The earliest grace or limit deadline of a watch still pending.
    pub next_deadline: Option<DateTime<Utc>>,
}

/// One pending watch read from the ledger.
struct Pending {
    id: String,
    watch: TrunkCiWatch,
}

/// What the gated apply step decided.
#[derive(Default)]
struct Applied {
    results: Vec<TrunkCiApplied>,
    /// `(campaign, stood-down advance item)` to ask to advance again.
    awaiting: Vec<(String, String)>,
    next_deadline: Option<DateTime<Utc>>,
}

/// The trunk CI watcher. `run` consumes it and loops forever.
pub struct TrunkCiWatcher {
    ctx: RuntimeContext,
    work_items_path: PathBuf,
    gateway: Arc<dyn TrunkCiGateway>,
    advance: AdvanceHook,
    clock: ClockFn,
    /// Stood-down advances already asked to advance again, so each is asked
    /// once per daemon process.
    prodded: Mutex<HashSet<String>>,
}

impl TrunkCiWatcher {
    /// A watcher over the ledger at `work_items_path`, asking `gateway` and
    /// `advance`, reading the system clock.
    #[must_use]
    pub fn new(
        ctx: RuntimeContext,
        work_items_path: PathBuf,
        gateway: Arc<dyn TrunkCiGateway>,
        advance: AdvanceHook,
    ) -> Self {
        Self {
            ctx,
            work_items_path,
            gateway,
            advance,
            clock: Arc::new(Utc::now),
            prodded: Mutex::new(HashSet::new()),
        }
    }

    /// Read "now" from `clock` instead of the system clock. Test-only.
    #[cfg(test)]
    #[must_use]
    pub fn with_clock(mut self, clock: ClockFn) -> Self {
        self.clock = clock;
        self
    }

    /// Run the watcher loop. Never returns under normal operation.
    pub async fn run(self) {
        let mut events = self.ctx.event_tx.subscribe();
        tracing::info!(path = %self.work_items_path.display(), "trunk CI watcher started");
        loop {
            let outcome = self.tick().await;
            let now = (self.clock)();
            let wait = outcome
                .next_deadline
                .and_then(|at| (at - now).to_std().ok())
                .map_or(POLL, |until| until.min(POLL));
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = wake_on_settlement(&mut events) => {}
            }
        }
    }

    /// One tick: read the pending watches, ask GitHub outside the gate, apply
    /// under it, then announce and ask stood-down campaigns to advance.
    pub async fn tick(&self) -> CiTickOutcome {
        let path = self.work_items_path.clone();
        let pending = tokio::task::spawn_blocking(move || pending_watches(&path))
            .await
            .unwrap_or_else(|error| {
                // Best-effort: a read that did not finish concludes nothing;
                // the next tick reads again.
                tracing::warn!(%error, "trunk CI watch read did not finish");
                Vec::new()
            });
        let mut conclusions = Vec::with_capacity(pending.len());
        for watched in pending {
            let conclusion = self.conclusion(&watched.watch).await;
            conclusions.push((watched.id, watched.watch.commit, conclusion));
        }
        let path = self.work_items_path.clone();
        let now = (self.clock)();
        let applied = tokio::task::spawn_blocking(move || apply(&path, conclusions, now))
            .await
            .unwrap_or_else(|error| {
                // Best-effort: nothing was saved, so nothing is announced;
                // the next tick decides again from the ledger as it was.
                tracing::warn!(%error, "trunk CI watch apply did not finish");
                Applied::default()
            });

        let mut outcome = CiTickOutcome {
            next_deadline: applied.next_deadline,
            ..CiTickOutcome::default()
        };
        for result in applied.results {
            if let Some(item) = result.concluded {
                if let Some(event) = checked_event(&item) {
                    self.announce(event).await;
                }
                outcome.concluded.push(item);
            }
            if let Some(item) = result.opened {
                self.announce(lifecycle_event(EventType::WorkItemSubmitted, &item)).await;
                self.announce(lifecycle_event(EventType::WorkItemSettled, &item)).await;
                outcome.opened.push(item);
            }
            for item in result.settled {
                self.announce(lifecycle_event(EventType::WorkItemSettled, &item)).await;
                outcome.settled.push(item);
            }
        }
        for (campaign, advance) in applied.awaiting {
            if self.first_prod(&advance) {
                tracing::info!(%campaign, %advance, "trunk CI concluded; asking the campaign to advance");
                (self.advance)(campaign.clone());
                outcome.advanced.push(campaign);
            }
        }
        outcome
    }

    /// Whether the stood-down advance `id` has not been asked about yet, and
    /// record that it now has.
    fn first_prod(&self, id: &str) -> bool {
        self.prodded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.to_string())
    }

    async fn announce(&self, event: Event) {
        self.ctx.engine.process(event).await;
    }

    /// Ask GitHub about `watch` and conclude it, fetching a red run's
    /// evidence.
    async fn conclusion(&self, watch: &TrunkCiWatch) -> TrunkCiConclusion {
        let now = (self.clock)();
        let runs = self.gateway.runs_for_commit(&watch.repo, &watch.commit).await;
        let error = runs.as_ref().err().map(ToString::to_string);
        let observed = match (&runs, &error) {
            (Ok(runs), _) => Ok(runs.as_slice()),
            (Err(_), Some(error)) => Err(error.as_str()),
            (Err(_), None) => Err("unknown error"),
        };
        match foundry_sdk::trunk_ci::conclude(observed, &watch.branch, watch.requested_at, now) {
            CiVerdict::Pending { detail } => TrunkCiConclusion::Pending { detail },
            CiVerdict::Passed { run_url } => TrunkCiConclusion::Passed { run_url },
            CiVerdict::NoRun { detail } => TrunkCiConclusion::NoRun { detail },
            CiVerdict::Unavailable { detail } => TrunkCiConclusion::Unavailable { detail },
            CiVerdict::Failed { runs } => self.red(&watch.repo, &runs).await,
        }
    }

    /// The evidence of a red conclusion: the first failed run, every failed
    /// run's failed jobs, and the end of the first one's failed-job log. A
    /// fault fetching evidence is recorded, never a reason to wait.
    async fn red(&self, repo: &str, runs: &[CiRun]) -> TrunkCiConclusion {
        let mut failing_jobs: Vec<String> = Vec::new();
        let mut faults = Vec::new();
        for run in runs {
            match self.gateway.failed_jobs(repo, run.database_id).await {
                Ok(jobs) => {
                    for job in jobs {
                        if !failing_jobs.contains(&job) {
                            failing_jobs.push(job);
                        }
                    }
                }
                Err(error) => {
                    faults.push(format!("failed jobs of run {}: {error}", run.database_id));
                }
            }
        }
        let first = runs.first();
        let log_tail = match first {
            Some(run) => match self.gateway.failed_log_tail(repo, run.database_id).await {
                Ok(tail) => Some(tail).filter(|tail| !tail.trim().is_empty()),
                Err(error) => {
                    faults.push(format!("failed-job log of run {}: {error}", run.database_id));
                    None
                }
            },
            None => None,
        };
        TrunkCiConclusion::Failed {
            run_url: first.map(|run| run.url.clone()).filter(|url| !url.is_empty()),
            failing_jobs,
            log_tail,
            detail: (!faults.is_empty()).then(|| format!("could not read {}", faults.join("; "))),
        }
    }
}

/// Every pending watch in the ledger at `path`, oldest first.
fn pending_watches(path: &std::path::Path) -> Vec<Pending> {
    let Ok(_guard) = ledger_write_gate().lock() else {
        // Best-effort: a poisoned gate means another mutation panicked
        // mid-sequence; this tick watches nothing and the next one retries.
        tracing::warn!("work-item ledger lock poisoned; trunk CI tick reads nothing");
        return Vec::new();
    };
    let store = match WorkItemStore::load(path) {
        Ok(store) => store,
        Err(error) => {
            // Best-effort: an unreadable ledger is logged on every tick and
            // watches nothing until it is readable again.
            tracing::warn!(path = %path.display(), %error, "could not read the work-item ledger; trunk CI tick reads nothing");
            return Vec::new();
        }
    };
    let mut pending: Vec<Pending> = store
        .items
        .iter()
        .filter_map(|item| {
            let watch = item.ci.as_ref().filter(|watch| watch.is_pending())?;
            Some(Pending {
                id: item.id.clone(),
                watch: watch.clone(),
            })
        })
        .collect();
    pending.sort_by_key(|pending| pending.watch.requested_at);
    pending
}

/// The gated half of a tick: reload, apply each conclusion, save once, and
/// find the stood-down campaigns whose project has nothing pending left.
///
/// A ledger that cannot be read or saved applies nothing, so nothing is
/// announced or advanced that the ledger does not record.
fn apply(
    path: &std::path::Path,
    conclusions: Vec<(String, String, TrunkCiConclusion)>,
    now: DateTime<Utc>,
) -> Applied {
    let Ok(_guard) = ledger_write_gate().lock() else {
        // Best-effort: as in `pending_watches`.
        tracing::warn!("work-item ledger lock poisoned; trunk CI tick applies nothing");
        return Applied::default();
    };
    let mut store = match WorkItemStore::load(path) {
        Ok(store) => store,
        Err(error) => {
            // Best-effort: as in `pending_watches`.
            tracing::warn!(path = %path.display(), %error, "could not read the work-item ledger; trunk CI tick applies nothing");
            return Applied::default();
        }
    };
    let mut results = Vec::new();
    for (id, commit, conclusion) in conclusions {
        let result = store.apply_trunk_ci(&id, &commit, conclusion, now);
        if result.changed {
            results.push(result);
        }
    }
    if !results.is_empty()
        && let Err(error) = store.save(path)
    {
        // Best-effort: nothing was written, so nothing is announced; the
        // next tick concludes again from the ledger as it was.
        tracing::warn!(path = %path.display(), %error, "could not write the work-item ledger; trunk CI tick applies nothing");
        return Applied::default();
    }
    let projects: BTreeSet<&str> = store.items.iter().map(|item| item.project.as_str()).collect();
    let awaiting = projects
        .into_iter()
        .filter(|project| store.trunk_ci_pending(project).is_empty())
        .flat_map(|project| store.campaigns_awaiting_trunk_ci(project))
        .collect();
    let next_deadline = store
        .items
        .iter()
        .filter_map(|item| item.ci.as_ref().filter(|watch| watch.is_pending()))
        .flat_map(|watch| {
            [
                watch.requested_at + TRUNK_CI_GRACE,
                watch.requested_at + TRUNK_CI_LIMIT,
            ]
        })
        .filter(|at| *at > now)
        .min();
    Applied {
        results: results.into_iter().filter(|result| result.concluded.is_some()).collect(),
        awaiting,
        next_deadline,
    }
}

/// Block until a `work_item_settled` arrives on the broadcast: a landing
/// may just have become watchable. A lagged receiver counts as a wake.
async fn wake_on_settlement(events: &mut broadcast::Receiver<Event>) {
    loop {
        match events.recv().await {
            Ok(event) if event.event_type == EventType::WorkItemSettled => return,
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => return,
            Err(broadcast::error::RecvError::Closed) => {
                // Nothing will ever wake us again; fall back to the interval.
                tokio::time::sleep(POLL).await;
                return;
            }
        }
    }
}

/// `trunk_ci_checked` for the concluded watch on `item`, on its trace, or
/// `None` when the item holds no watch.
fn checked_event(item: &WorkItem) -> Option<Event> {
    let checked = TrunkCiCheckedPayload::from_item(item)?;
    #[allow(
        clippy::expect_used,
        reason = "TrunkCiCheckedPayload is infallibly serializable (Payload Conventions, AGENTS.md)"
    )]
    let payload = Event::serialize_payload(&checked)
        .expect("TrunkCiCheckedPayload is infallibly serializable");
    Some(
        Event::new(EventType::TrunkCiChecked, item.project.clone(), Throttle::Full, payload)
            .with_trace_id(item.trace_id.clone()),
    )
}

/// The `work_item_*` event recording one transition of `item`, on its trace.
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
    use std::sync::RwLock;

    use chrono::TimeZone as _;
    use foundry_blocks::gateway::trunk_ci::FakeTrunkCiGateway;
    use foundry_engine::engine::Engine;
    use foundry_sdk::registry::Registry;
    use foundry_sdk::trunk_ci::{TrunkCiState, awaiting_reason};
    use foundry_sdk::work_item::{WorkItemKind, WorkItemSpec, WorkLane};

    use super::*;

    fn at(minutes: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 20, 12, 0, 0).single().expect("valid date")
            + chrono::Duration::minutes(minutes)
    }

    /// A task of `project` that landed as `commit`, watched from minute 0.
    fn landed(id: &str, project: &str, commit: &str) -> WorkItem {
        let mut item = WorkItem::dispatched(
            WorkItemSpec {
                project: project.to_string(),
                objective: format!("objective of {id}"),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "test".to_string(),
                trace_id: Some(id.repeat(4)),
            },
            at(0),
        );
        item.id = id.to_string();
        item.settle_landed("landed", at(0));
        item.ci = Some(TrunkCiWatch::pending("o/r", "main", commit, at(0)));
        item
    }

    /// A campaign advance of `campaign` on `project` that settled with `reason`.
    fn advance(id: &str, project: &str, campaign: &str, reason: &str) -> WorkItem {
        let root = Event::new(
            EventType::CampaignAdvanceRequested,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({"campaign": campaign}),
        );
        let mut item = WorkItem::queued(
            WorkItemSpec {
                project: project.to_string(),
                objective: "advance".to_string(),
                kind: WorkItemKind::Workflow,
                lane: WorkLane::Campaign,
                origin: "test".to_string(),
                trace_id: None,
            },
            root.clone(),
            at(0),
        );
        item.id = id.to_string();
        item.start(at(0));
        item.started_root = Some(root);
        item.settle_landed(reason, at(1));
        item
    }

    struct Harness {
        dir: tempfile::TempDir,
        watcher: TrunkCiWatcher,
        gateway: Arc<FakeTrunkCiGateway>,
        advanced: Arc<Mutex<Vec<String>>>,
        events: broadcast::Receiver<Event>,
        clock: Arc<Mutex<DateTime<Utc>>>,
    }

    impl Harness {
        fn new(items: Vec<WorkItem>) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let ledger = dir.path().join("work-items.json");
            WorkItemStore { version: 1, items }.save(&ledger).expect("seed ledger");
            let (event_tx, events) = broadcast::channel(256);
            let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));
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
            let gateway = FakeTrunkCiGateway::new();
            let advanced: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
            let recorder = Arc::clone(&advanced);
            let clock = Arc::new(Mutex::new(at(1)));
            let now = Arc::clone(&clock);
            let watcher = TrunkCiWatcher::new(
                ctx,
                ledger,
                gateway.clone(),
                Arc::new(move |campaign| recorder.lock().expect("lock").push(campaign)),
            )
            .with_clock(Arc::new(move || *now.lock().expect("lock")));
            Self {
                dir,
                watcher,
                gateway,
                advanced,
                events,
                clock,
            }
        }

        fn item(&self, id: &str) -> WorkItem {
            WorkItemStore::load(&self.dir.path().join("work-items.json"))
                .expect("ledger")
                .find(id)
                .cloned()
                .expect("item")
        }

        fn set_clock(&self, minutes: i64) {
            *self.clock.lock().expect("lock") = at(minutes);
        }

        fn announced(&mut self) -> Vec<EventType> {
            let mut out = Vec::new();
            while let Ok(event) = self.events.try_recv() {
                out.push(event.event_type);
            }
            out
        }
    }

    #[tokio::test]
    async fn a_red_run_records_every_failed_runs_jobs_and_the_first_runs_log_tail() {
        let mut h = Harness::new(vec![landed("wi_a", "alpha", "aaaaaaa1")]);
        h.gateway.set_runs(
            "aaaaaaa1",
            vec![
                FakeTrunkCiGateway::run(1, "main", "completed", "failure"),
                FakeTrunkCiGateway::run(2, "main", "completed", "timed_out"),
            ],
        );
        h.gateway.set_failed_jobs(1, &["lint", "test"]);
        h.gateway.set_failed_jobs(2, &["test", "slow"]);
        h.gateway.set_log_tail(1, "boom");

        let outcome = h.watcher.tick().await;

        assert_eq!(outcome.opened.len(), 1);
        let watch = h.item("wi_a").ci.expect("watch");
        assert_eq!(watch.state, TrunkCiState::Failed);
        assert_eq!(watch.failing_jobs, vec!["lint", "test", "slow"]);
        assert_eq!(watch.log_tail.as_deref(), Some("boom"));
        assert_eq!(watch.run_url.as_deref(), Some("https://github.com/o/r/actions/runs/1"));
        assert_eq!(watch.detail, None);
        assert_eq!(
            h.gateway.calls(),
            vec!["runs o/r aaaaaaa1", "jobs o/r 1", "jobs o/r 2", "log o/r 1"]
        );
        assert_eq!(
            h.announced(),
            vec![
                EventType::TrunkCiChecked,
                EventType::WorkItemSubmitted,
                EventType::WorkItemSettled
            ]
        );
    }

    #[tokio::test]
    async fn a_watch_that_concluded_is_not_asked_about_again() {
        let h = Harness::new(vec![landed("wi_a", "alpha", "aaaaaaa1")]);
        h.gateway
            .set_runs("aaaaaaa1", vec![FakeTrunkCiGateway::run(1, "main", "completed", "success")]);

        assert_eq!(h.watcher.tick().await.concluded.len(), 1);
        let second = h.watcher.tick().await;

        assert!(second.concluded.is_empty());
        assert_eq!(h.gateway.calls(), vec!["runs o/r aaaaaaa1"], "one question per pending watch");
        assert_eq!(second.next_deadline, None, "nothing pending, nothing due");
    }

    #[tokio::test]
    async fn a_pending_watch_reports_its_next_grace_or_limit_deadline() {
        let h = Harness::new(vec![landed("wi_a", "alpha", "aaaaaaa1")]);

        assert_eq!(h.watcher.tick().await.next_deadline, Some(at(15)));
        h.gateway
            .set_runs("aaaaaaa1", vec![FakeTrunkCiGateway::run(1, "main", "in_progress", "")]);
        h.set_clock(30);
        assert_eq!(h.watcher.tick().await.next_deadline, Some(at(120)));
    }

    #[tokio::test]
    async fn a_stood_down_campaign_is_asked_to_advance_once_when_nothing_is_pending() {
        let awaiting = awaiting_reason("aaaaaaa1", "wi_a");
        let h = Harness::new(vec![
            landed("wi_a", "alpha", "aaaaaaa1"),
            advance("wi_adv", "alpha", "tidy", &awaiting),
            advance("wi_other", "beta", "grow", "workflow completed"),
        ]);

        let pending = h.watcher.tick().await;
        assert!(pending.advanced.is_empty(), "the watch on alpha is still pending");

        h.gateway
            .set_runs("aaaaaaa1", vec![FakeTrunkCiGateway::run(1, "main", "completed", "success")]);
        let concluded = h.watcher.tick().await;
        assert_eq!(concluded.advanced, vec!["tidy"]);
        assert!(h.watcher.tick().await.advanced.is_empty(), "asked once per stood-down advance");
        assert_eq!(*h.advanced.lock().expect("lock"), vec!["tidy"]);
    }
}
