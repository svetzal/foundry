//! `PlanMajorUpgrades` — the majors lane's decision step.
//!
//! Sinks on:
//!
//! - `MaintenanceSummaryRequested` (nightly): reads every project's trace for
//!   its dependency classification and maintenance outcome, reads prior
//!   upgrade tasks from the event log for dedupe, and plans each major with
//!   [`crate::dependency_updates::majors::plan`]. It emits
//!   `MajorUpgradesPlanned`, which carries the summary fields forward so
//!   `Generate Summary` runs on it. The service dispatches the `dispatch`
//!   entries as separate `foundry task` workflows once the summary is written,
//!   and only under full throttle.
//! - `DependencyUpdatesClassified` in the `review` phase: plans one project's
//!   majors so a review shows what the lane would do. It never dispatches.
//!
//! Observer. It records problems (an unreadable event log, a trace that is
//! missing) in the payload rather than failing, because the summary runs on
//! the event it emits.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Duration, Utc};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    ClassificationPhase, DependencyUpdatesClassifiedPayload, MaintenanceSummaryRequestedPayload,
    MajorUpgradeStatus, MajorUpgradesPlannedPayload, ProjectCompletedPayload,
    TaskRunCompletedPayload, TaskRunStartedPayload, TaskVerdict,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::trace::ProcessResult;

use crate::dependency_updates::majors::{self, Caps, PriorTask, ProjectMajors};
use crate::gateway::ShellGateway;
use crate::trace_writer::TraceWriter;

/// How far back the event log is searched for earlier upgrade tasks.
const HISTORY_DAYS: i64 = 45;
/// A task started this long ago with no completion is treated as abandoned
/// (a daemon restart mid-run), not in flight.
const IN_FLIGHT_HOURS: i64 = 24;

pub struct PlanMajorUpgrades {
    trace_writer: Arc<TraceWriter>,
    registry: Arc<RwLock<Registry>>,
    shell: Arc<dyn ShellGateway>,
    events_dir: PathBuf,
    caps: Caps,
}

impl PlanMajorUpgrades {
    pub fn new(
        trace_writer: Arc<TraceWriter>,
        registry: Arc<RwLock<Registry>>,
        shell: Arc<dyn ShellGateway>,
    ) -> Self {
        Self {
            trace_writer,
            registry,
            shell,
            events_dir: foundry_sdk::paths::events_dir(),
            caps: Caps::from_env(),
        }
    }

    /// Construct with an explicit event-log directory and caps.
    pub fn with_config(
        trace_writer: Arc<TraceWriter>,
        registry: Arc<RwLock<Registry>>,
        shell: Arc<dyn ShellGateway>,
        events_dir: PathBuf,
        caps: Caps,
    ) -> Self {
        Self {
            trace_writer,
            registry,
            shell,
            events_dir,
            caps,
        }
    }
}

/// An upgrade task as the event log records it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LoggedTask {
    pub project: String,
    pub objective: String,
    pub started_at: DateTime<Utc>,
    pub completed: Option<TaskRunCompletedPayload>,
}

/// Every major-upgrade task started since `since`, paired with its completion
/// by trace id.
pub(crate) fn logged_upgrade_tasks(
    events_dir: &Path,
    since: DateTime<Utc>,
) -> Result<Vec<LoggedTask>, String> {
    let mut months: Vec<String> = Vec::new();
    let mut day = since;
    while day <= Utc::now() + Duration::days(1) {
        let m = day.format("%Y-%m").to_string();
        if !months.contains(&m) {
            months.push(m);
        }
        day += Duration::days(20);
    }
    let now_month = Utc::now().format("%Y-%m").to_string();
    if !months.contains(&now_month) {
        months.push(now_month);
    }

    let mut started: BTreeMap<String, LoggedTask> = BTreeMap::new();
    let mut completed: HashMap<String, TaskRunCompletedPayload> = HashMap::new();
    for month in months {
        let path = events_dir.join(format!("{month}.jsonl"));
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("event log {} unreadable: {e}", path.display())),
        };
        for line in text.lines() {
            let is_start = line.contains("\"task_run_started\"");
            if !is_start && !line.contains("\"task_run_completed\"") {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Event>(line) else {
                continue;
            };
            let Some(trace) = event.trace_id.clone() else {
                continue;
            };
            if event.occurred_at < since {
                continue;
            }
            match event.event_type {
                EventType::TaskRunStarted => {
                    if let Ok(p) = event.parse_payload::<TaskRunStartedPayload>()
                        && majors::parse_objective(&p.objective).is_some()
                    {
                        started.insert(
                            trace,
                            LoggedTask {
                                project: p.project,
                                objective: p.objective,
                                started_at: event.occurred_at,
                                completed: None,
                            },
                        );
                    }
                }
                EventType::TaskRunCompleted => {
                    if let Ok(p) = event.parse_payload::<TaskRunCompletedPayload>() {
                        completed.insert(trace, p);
                    }
                }
                _ => {}
            }
        }
    }
    Ok(started
        .into_iter()
        .map(|(trace, mut task)| {
            task.completed = completed.remove(&trace);
            task
        })
        .collect())
}

/// Why a logged task still blocks a new dispatch, before checking that its
/// preserved work exists. `None` when it does not block.
pub(crate) fn blocking_reason(
    task: &LoggedTask,
    now: DateTime<Utc>,
) -> Option<(String, Option<String>)> {
    match &task.completed {
        None if now - task.started_at < Duration::hours(IN_FLIGHT_HOURS) => Some((
            format!(
                "a task for this upgrade is in flight (started {})",
                task.started_at.format("%Y-%m-%d %H:%M UTC")
            ),
            None,
        )),
        None => None,
        Some(done) if done.landed => None,
        Some(done) => {
            let verdict = match &done.verdict {
                TaskVerdict::Remainder { .. } => "remainder",
                TaskVerdict::Defect { .. } => "defect",
                TaskVerdict::BlockedOnDecision { .. } => "blocked-on-decision",
                TaskVerdict::Complete | TaskVerdict::RunnerError { .. } => return None,
            };
            let reference = done.preservation_ref.clone()?;
            Some((
                format!("an earlier task left a preserved {verdict} at {reference}"),
                Some(reference),
            ))
        }
    }
}

/// Whether a preserved ref still exists: a bundle file, or a branch on `origin`.
async fn preserved_ref_exists(shell: &dyn ShellGateway, repo: &Path, reference: &str) -> bool {
    if let Some(bundle) = reference.strip_prefix("bundle:") {
        return Path::new(bundle).exists();
    }
    match shell
        .run(
            repo,
            "git",
            &["ls-remote", "--exit-code", "--heads", "origin", reference],
            None,
            None,
        )
        .await
    {
        Ok(result) => result.success,
        Err(e) => {
            // Best-effort: when the remote cannot be asked, keep the dedupe
            // (assume the work is there) rather than risk a duplicate task.
            tracing::warn!(error = %e, reference, "could not check preserved ref; keeping dedupe");
            true
        }
    }
}

/// The dependency classification a project's trace carries: the `after`
/// phase when present (what is left after maintenance), otherwise `before`.
fn classification_from_trace(trace: &ProcessResult) -> Option<DependencyUpdatesClassifiedPayload> {
    let all: Vec<DependencyUpdatesClassifiedPayload> = trace
        .parsed_events_of::<DependencyUpdatesClassifiedPayload>(
            EventType::DependencyUpdatesClassified,
        )
        .collect();
    let after = all.iter().find(|p| p.phase == ClassificationPhase::After).cloned();
    after.or_else(|| all.into_iter().find(|p| p.phase == ClassificationPhase::Before))
}

fn maintain_succeeded(trace: &ProcessResult) -> bool {
    trace
        .parsed_events_of::<ProjectCompletedPayload>(EventType::ProjectMaintenanceCompleted)
        .last()
        .is_some_and(|p| p.success)
}

/// Render the plan for a block's output and a review.
pub(crate) fn render_plan(payload: &MajorUpgradesPlannedPayload) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    if payload.upgrades.is_empty() {
        out.push_str("No major upgrades available.\n");
        return out;
    }
    let _ = writeln!(
        out,
        "Major upgrades (caps: {} per project, {} per night; {}):",
        payload.per_project_cap,
        payload.per_night_cap,
        if payload.dispatch_enabled {
            "dispatching"
        } else {
            "not dispatching: plan only"
        }
    );
    for m in &payload.upgrades {
        let status = match (m.status, payload.dispatch_enabled) {
            (MajorUpgradeStatus::Dispatch, false) => "would dispatch",
            (status, _) => status.as_str(),
        };
        let _ = write!(
            out,
            "- [{status}] {}: {} {} -> {} ({})",
            m.project, m.package, m.from, m.to, m.ecosystem
        );
        if let Some(id) = &m.security {
            let _ = write!(out, " security {id}");
        }
        if let Some(reason) = &m.reason {
            let _ = write!(out, " — {reason}");
        }
        out.push('\n');
        if m.status != MajorUpgradeStatus::Dispatch || !payload.dispatch_enabled {
            let _ = writeln!(out, "    {}", m.command);
        }
    }
    if let Some(w) = &payload.history_warning {
        let _ = writeln!(out, "Warning: {w}");
    }
    out
}

impl TaskBlock for PlanMajorUpgrades {
    task_block_meta! {
        name: "Plan Major Upgrades",
        kind: Observer,
        sinks_on: [MaintenanceSummaryRequested, DependencyUpdatesClassified],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        match trigger.event_type {
            EventType::MaintenanceSummaryRequested => true,
            EventType::DependencyUpdatesClassified => {
                trigger.payload.get("phase").and_then(serde_json::Value::as_str) == Some("review")
            }
            _ => false,
        }
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let throttle = trigger.throttle;
        let project = trigger.project.clone();
        let review = trigger.event_type == EventType::DependencyUpdatesClassified;
        let caps = self.caps;
        let shell = Arc::clone(&self.shell);
        let events_dir = self.events_dir.clone();

        // Gather each project's majors before any await (no lock across awaits).
        let gathered = if review {
            let p = parse_payload!(trigger, DependencyUpdatesClassifiedPayload);
            self.review_inputs(p)
        } else {
            let p = parse_payload!(trigger, MaintenanceSummaryRequestedPayload);
            self.nightly_inputs(p)
        };
        let Gathered {
            inputs,
            summary_fields,
            paths,
        } = match gathered {
            Ok(g) => g,
            Err(e) => return Box::pin(async move { Err(e) }),
        };

        Box::pin(async move {
            let now = Utc::now();
            let (prior, history_warning) =
                prior_tasks(events_dir, &paths, shell.as_ref(), now).await;
            let upgrades = majors::plan(&inputs, &prior, caps);
            let dispatch_enabled = !review && throttle.permits_mutation();
            let payload = MajorUpgradesPlannedPayload {
                upgrades,
                per_project_cap: caps.per_project,
                per_night_cap: caps.per_night,
                dispatch_enabled,
                review,
                history_warning,
                project_trace_ids: summary_fields.project_trace_ids,
                skipped_projects: summary_fields.skipped_projects,
                total_duration_ms: summary_fields.total_duration_ms,
                root_event_id: summary_fields.root_event_id,
            };
            let summary = plan_summary(&payload);
            let rendered = render_plan(&payload);
            let result: TaskBlockResult = super::emit_result(
                summary,
                EventType::MajorUpgradesPlanned,
                &project,
                throttle,
                &payload,
            )?;
            Ok(result.with_output(Some(rendered), None))
        })
    }
}

/// What the plan is built from, gathered before any await.
struct Gathered {
    inputs: Vec<ProjectMajors>,
    summary_fields: MaintenanceSummaryRequestedPayload,
    /// Project checkouts, for checking that preserved work still exists.
    paths: HashMap<String, PathBuf>,
}

impl PlanMajorUpgrades {
    /// One project's majors, from its review classification.
    fn review_inputs(&self, p: DependencyUpdatesClassifiedPayload) -> anyhow::Result<Gathered> {
        let guard = super::read_registry(&self.registry)?;
        let paths = guard
            .find_project(&p.project)
            .map(|e| (p.project.clone(), PathBuf::from(&e.path)))
            .into_iter()
            .collect();
        drop(guard);
        Ok(Gathered {
            inputs: vec![ProjectMajors {
                project: p.project,
                policy: p.brief.policy,
                maintain_succeeded: true,
                majors: p.brief.majors,
            }],
            summary_fields: MaintenanceSummaryRequestedPayload::default(),
            paths,
        })
    }

    /// Every project's majors, from the traces of tonight's run.
    fn nightly_inputs(&self, p: MaintenanceSummaryRequestedPayload) -> anyhow::Result<Gathered> {
        let guard = super::read_registry(&self.registry)?;
        let mut inputs = Vec::new();
        let mut paths = HashMap::new();
        for (name, trace_id) in &p.project_trace_ids {
            let Some(trace) = self.trace_writer.read(trace_id) else {
                continue;
            };
            let Some(classified) = classification_from_trace(&trace) else {
                continue;
            };
            if let Some(entry) = guard.find_project(name) {
                paths.insert(name.clone(), PathBuf::from(&entry.path));
            }
            inputs.push(ProjectMajors {
                project: name.clone(),
                policy: classified.brief.policy,
                maintain_succeeded: maintain_succeeded(&trace),
                majors: classified.brief.majors,
            });
        }
        drop(guard);
        Ok(Gathered {
            inputs,
            summary_fields: p,
            paths,
        })
    }
}

/// Earlier upgrade tasks that still block a dispatch, and a warning when the
/// history could not be read (dedupe was then blind, and the plan says so).
async fn prior_tasks(
    events_dir: PathBuf,
    paths: &HashMap<String, PathBuf>,
    shell: &dyn ShellGateway,
    now: DateTime<Utc>,
) -> (Vec<PriorTask>, Option<String>) {
    let history = tokio::task::spawn_blocking(move || {
        logged_upgrade_tasks(&events_dir, now - Duration::days(HISTORY_DAYS))
    })
    .await
    .unwrap_or_else(|e| Err(format!("history lookup failed: {e}")));
    let logged = match history {
        Ok(tasks) => tasks,
        Err(w) => {
            tracing::warn!(warning = %w, "major-upgrade dedupe cannot read task history");
            return (Vec::new(), Some(format!("{w}; dedupe could not check earlier tasks")));
        }
    };
    let mut prior = Vec::new();
    for task in &logged {
        let Some((reason, reference)) = blocking_reason(task, now) else {
            continue;
        };
        if let Some(reference) = reference {
            let exists = match paths.get(&task.project) {
                Some(repo) => preserved_ref_exists(shell, repo, &reference).await,
                None => true,
            };
            if !exists {
                continue;
            }
        }
        prior.push(PriorTask {
            project: task.project.clone(),
            objective: task.objective.clone(),
            blocking: reason,
        });
    }
    (prior, None)
}

fn plan_summary(payload: &MajorUpgradesPlannedPayload) -> String {
    let count = |s: MajorUpgradeStatus| payload.upgrades.iter().filter(|m| m.status == s).count();
    format!(
        "{} major upgrade(s): {} dispatch, {} deduped, {} overflow, {} deferred, {} proposed{}",
        payload.upgrades.len(),
        count(MajorUpgradeStatus::Dispatch),
        count(MajorUpgradeStatus::Deduped),
        count(MajorUpgradeStatus::Overflow),
        count(MajorUpgradeStatus::Deferred),
        count(MajorUpgradeStatus::Proposed),
        if payload.dispatch_enabled {
            ""
        } else {
            " (plan only)"
        },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::{Duration, Utc};
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::gateway::fakes::FakeShellGateway;
    use foundry_sdk::payload::{
        ChainContext, ChangeKind, ClassificationPhase, DependencyBrief, DependencyClassification,
        DependencyUpdatesClassifiedPayload, Ecosystem, LoopContext, MajorUpgradeStatus,
        MajorUpgradesPlannedPayload, PlannedUpdate, TaskRunCompletedPayload, TaskVerdict,
        UpdateClass,
    };
    use foundry_sdk::registry::UpdatePolicy;
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::trace::ProcessResult;

    use super::super::test_helpers;
    use super::{LoggedTask, PlanMajorUpgrades, blocking_reason, logged_upgrade_tasks};
    use crate::dependency_updates::majors::{Caps, objective};
    use crate::trace_writer::TraceWriter;

    fn major(package: &str) -> PlannedUpdate {
        PlannedUpdate {
            ecosystem: Ecosystem::Npm,
            manifest: ".".to_string(),
            package: package.to_string(),
            from: "1.0.0".to_string(),
            to: "2.0.0".to_string(),
            class: UpdateClass::Major,
            change: ChangeKind::Manifest,
            security: None,
            beyond_policy: false,
            beyond_hold: false,
        }
    }

    fn classified(
        project: &str,
        phase: ClassificationPhase,
        policy: UpdatePolicy,
        majors: Vec<PlannedUpdate>,
    ) -> Event {
        let payload = DependencyUpdatesClassifiedPayload {
            project: project.to_string(),
            phase,
            workflow: Some("maintain".to_string()),
            success: None,
            classification: DependencyClassification::default(),
            brief: DependencyBrief {
                policy,
                policy_set: true,
                apply: vec![],
                held_by_policy: vec![],
                held_by_hold: vec![],
                majors,
            },
            chain: ChainContext::default(),
        };
        Event::new(
            EventType::DependencyUpdatesClassified,
            project.to_string(),
            Throttle::Full,
            serde_json::to_value(payload).unwrap(),
        )
    }

    fn maintenance_done(project: &str, success: bool) -> Event {
        test_event!(EventType::ProjectMaintenanceCompleted, project, {
            "project": project, "success": success, "summary": "", "workflow": "maintain",
        })
    }

    fn block(traces: &std::path::Path, events: &std::path::Path, caps: Caps) -> PlanMajorUpgrades {
        PlanMajorUpgrades::with_config(
            Arc::new(TraceWriter::new(traces.to_str().unwrap())),
            test_helpers::empty_registry(),
            FakeShellGateway::success(),
            events.to_path_buf(),
            caps,
        )
    }

    fn write_trace(tw: &TraceWriter, id: &str, events: Vec<Event>) {
        tw.write(
            id,
            &ProcessResult {
                events,
                block_executions: vec![],
                total_duration_ms: 1,
            },
        )
        .unwrap();
    }

    fn summary_request(ids: &[(&str, &str)], throttle: Throttle) -> Event {
        let map: std::collections::HashMap<String, String> =
            ids.iter().map(|(p, i)| ((*p).to_string(), (*i).to_string())).collect();
        Event::new(
            EventType::MaintenanceSummaryRequested,
            "system".to_string(),
            throttle,
            serde_json::json!({"project_trace_ids": map, "skipped_projects": ["off"], "total_duration_ms": 5000}),
        )
    }

    assert_block_meta!(
        PlanMajorUpgrades::with_config(
            Arc::new(TraceWriter::new("/nonexistent")),
            test_helpers::empty_registry(),
            FakeShellGateway::success(),
            std::path::PathBuf::from("/nonexistent"),
            Caps::default(),
        ),
        kind: Observer,
        sinks_on: [MaintenanceSummaryRequested, DependencyUpdatesClassified],
    );

    #[test]
    fn accepts_returns_false_when_the_classification_is_not_a_review() {
        let dir = tempfile::tempdir().unwrap();
        let b = block(dir.path(), dir.path(), Caps::default());
        assert!(!b.accepts(&classified(
            "p",
            ClassificationPhase::Before,
            UpdatePolicy::Major,
            vec![]
        )));
        assert!(!b.accepts(&classified(
            "p",
            ClassificationPhase::After,
            UpdatePolicy::Major,
            vec![]
        )));
        assert!(b.accepts(&classified(
            "p",
            ClassificationPhase::Review,
            UpdatePolicy::Major,
            vec![]
        )));
    }

    #[tokio::test]
    async fn the_nightly_plan_uses_after_classifications_and_forwards_summary_fields() {
        let traces = tempfile::tempdir().unwrap();
        let events = tempfile::tempdir().unwrap();
        let tw = TraceWriter::new(traces.path().to_str().unwrap());
        write_trace(
            &tw,
            "evt_a",
            vec![
                classified(
                    "alpha",
                    ClassificationPhase::Before,
                    UpdatePolicy::Major,
                    vec![major("x"), major("gone")],
                ),
                maintenance_done("alpha", true),
                classified(
                    "alpha",
                    ClassificationPhase::After,
                    UpdatePolicy::Major,
                    vec![major("x")],
                ),
            ],
        );
        write_trace(
            &tw,
            "evt_b",
            vec![
                classified(
                    "beta",
                    ClassificationPhase::Before,
                    UpdatePolicy::Minor,
                    vec![major("y")],
                ),
                maintenance_done("beta", true),
            ],
        );
        let b = block(traces.path(), events.path(), Caps::default());

        let result = b
            .execute(&summary_request(&[("alpha", "evt_a"), ("beta", "evt_b")], Throttle::Full))
            .await
            .unwrap();

        assert!(result.success);
        let p: MajorUpgradesPlannedPayload = result.events[0].parse_payload().unwrap();
        let got: Vec<(&str, &str, MajorUpgradeStatus)> = p
            .upgrades
            .iter()
            .map(|m| (m.project.as_str(), m.package.as_str(), m.status))
            .collect();
        assert_eq!(
            got,
            [
                ("alpha", "x", MajorUpgradeStatus::Dispatch),
                ("beta", "y", MajorUpgradeStatus::Proposed)
            ]
        );
        assert!(p.dispatch_enabled);
        assert!(!p.review);
        assert_eq!(p.project_trace_ids.len(), 2);
        assert_eq!(p.skipped_projects, ["off"]);
        assert_eq!(p.total_duration_ms, 5000);
    }

    #[tokio::test]
    async fn a_dry_run_plans_without_dispatching() {
        let traces = tempfile::tempdir().unwrap();
        let events = tempfile::tempdir().unwrap();
        let tw = TraceWriter::new(traces.path().to_str().unwrap());
        write_trace(
            &tw,
            "evt_a",
            vec![
                classified(
                    "alpha",
                    ClassificationPhase::Before,
                    UpdatePolicy::Major,
                    vec![major("x")],
                ),
                maintenance_done("alpha", true),
            ],
        );
        let b = block(traces.path(), events.path(), Caps::default());

        let result = b
            .execute(&summary_request(&[("alpha", "evt_a")], Throttle::DryRun))
            .await
            .unwrap();

        let p: MajorUpgradesPlannedPayload = result.events[0].parse_payload().unwrap();
        assert!(!p.dispatch_enabled);
        assert_eq!(p.upgrades[0].status, MajorUpgradeStatus::Dispatch);
        assert!(result.raw_output.unwrap().contains("[would dispatch] alpha: x 1.0.0 -> 2.0.0"));
    }

    #[tokio::test]
    async fn a_review_plans_one_project_and_never_dispatches() {
        let dir = tempfile::tempdir().unwrap();
        let b = block(dir.path(), dir.path(), Caps::default());
        let trigger =
            classified("alpha", ClassificationPhase::Review, UpdatePolicy::Major, vec![major("x")]);

        let result = b.execute(&trigger).await.unwrap();

        let p: MajorUpgradesPlannedPayload = result.events[0].parse_payload().unwrap();
        assert!(p.review);
        assert!(!p.dispatch_enabled);
        assert_eq!(p.upgrades.len(), 1);
        let text = result.raw_output.unwrap();
        assert!(
            text.contains("foundry task alpha 'Upgrade x from 1.0.0 to 2.0.0 in alpha:"),
            "{text}"
        );
    }

    fn logged(completed: Option<TaskRunCompletedPayload>, hours_ago: i64) -> LoggedTask {
        LoggedTask {
            project: "alpha".to_string(),
            objective: objective("alpha", &major("x")),
            started_at: Utc::now() - Duration::hours(hours_ago),
            completed,
        }
    }

    fn done(
        landed: bool,
        verdict: TaskVerdict,
        reference: Option<&str>,
    ) -> TaskRunCompletedPayload {
        TaskRunCompletedPayload {
            project: "alpha".to_string(),
            success: landed,
            landed,
            summary: String::new(),
            preservation_ref: reference.map(str::to_string),
            verdict,
            context: LoopContext::default(),
        }
    }

    #[test]
    fn blocking_reasons_follow_the_dedupe_rules() {
        let now = Utc::now();
        assert!(blocking_reason(&logged(None, 2), now).unwrap().0.contains("in flight"));
        assert!(blocking_reason(&logged(None, 30), now).is_none(), "abandoned after a day");
        assert!(
            blocking_reason(&logged(Some(done(true, TaskVerdict::Complete, Some("abc"))), 1), now)
                .is_none()
        );
        let remainder =
            done(false, TaskVerdict::Remainder { gaps: vec![] }, Some("foundry-task/alpha-1"));
        let (reason, reference) = blocking_reason(&logged(Some(remainder), 1), now).unwrap();
        assert!(reason.contains("preserved remainder at foundry-task/alpha-1"));
        assert_eq!(reference.as_deref(), Some("foundry-task/alpha-1"));
        let defect = done(
            false,
            TaskVerdict::Defect {
                diagnosis: String::new(),
            },
            Some("bundle:/x"),
        );
        assert!(blocking_reason(&logged(Some(defect), 1), now).is_some());
        let runner = done(
            false,
            TaskVerdict::RunnerError {
                detail: String::new(),
            },
            None,
        );
        assert!(
            blocking_reason(&logged(Some(runner), 1), now).is_none(),
            "a runner error may be retried"
        );
    }

    #[test]
    fn logged_tasks_pair_starts_with_completions_by_trace() {
        let dir = tempfile::tempdir().unwrap();
        let obj = objective("alpha", &major("x"));
        let mut start =
            test_event!(EventType::TaskRunStarted, "alpha", {"project": "alpha", "objective": obj});
        start.trace_id = Some("t1".to_string());
        let mut other = test_event!(EventType::TaskRunStarted, "alpha", {"project": "alpha", "objective": "Fix a bug"});
        other.trace_id = Some("t2".to_string());
        let mut end = test_event!(EventType::TaskRunCompleted, "alpha", {
            "project": "alpha", "success": false, "landed": false, "summary": "",
            "preservation_ref": "foundry-task/alpha-1", "verdict": "remainder", "gaps": [],
        });
        end.trace_id = Some("t1".to_string());
        let text: String = [start, other, end]
            .iter()
            .map(|e| serde_json::to_string(e).unwrap() + "\n")
            .collect();
        std::fs::write(dir.path().join(format!("{}.jsonl", Utc::now().format("%Y-%m"))), text)
            .unwrap();

        let tasks = logged_upgrade_tasks(dir.path(), Utc::now() - Duration::days(1)).unwrap();

        assert_eq!(tasks.len(), 1, "only upgrade objectives count");
        assert!(tasks[0].completed.as_ref().is_some_and(|c| !c.landed));
    }

    #[tokio::test]
    async fn a_blocking_history_dedupes_the_nightly_dispatch() {
        let traces = tempfile::tempdir().unwrap();
        let events = tempfile::tempdir().unwrap();
        let tw = TraceWriter::new(traces.path().to_str().unwrap());
        write_trace(
            &tw,
            "evt_a",
            vec![
                classified(
                    "alpha",
                    ClassificationPhase::Before,
                    UpdatePolicy::Major,
                    vec![major("x")],
                ),
                maintenance_done("alpha", true),
            ],
        );
        let mut start = test_event!(EventType::TaskRunStarted, "alpha", {"project": "alpha", "objective": objective("alpha", &major("x"))});
        start.trace_id = Some("t1".to_string());
        std::fs::write(
            events.path().join(format!("{}.jsonl", Utc::now().format("%Y-%m"))),
            serde_json::to_string(&start).unwrap() + "\n",
        )
        .unwrap();
        let b = block(traces.path(), events.path(), Caps::default());

        let result = b
            .execute(&summary_request(&[("alpha", "evt_a")], Throttle::Full))
            .await
            .unwrap();

        let p: MajorUpgradesPlannedPayload = result.events[0].parse_payload().unwrap();
        assert_eq!(p.upgrades[0].status, MajorUpgradeStatus::Deduped);
        assert!(p.upgrades[0].reason.as_deref().unwrap().contains("in flight"));
    }
}
