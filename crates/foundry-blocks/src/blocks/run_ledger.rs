//! The blocks that keep the work-item ledger in step with Foundry's
//! *run-shaped* dispatches: a per-project maintenance run, a release, and a
//! remediation.
//!
//! These differ from the task-shaped kinds ([`super::work_ledger`]) in that
//! they have no single root event and no single terminal. Each one is opened
//! from the root of its own chain and closed from that chain's own typed
//! terminal:
//!
//! | Kind | Recorded at | Settled from |
//! |------|-------------|--------------|
//! | `maintenance` | `ProjectRunStarted` inside a cycle | `ProjectRunCompleted` |
//! | `release` | `ReleaseRequested`, or a clean `MainBranchAudited` | `ReleaseCompleted` |
//! | `remediation` | a dirty `MainBranchAudited`, or a failing `PipelineChecked` | `RemediationCompleted` |
//!
//! Recording is decided by the *same* predicate the dispatching block's
//! `accepts()` uses, shared from that block rather than re-derived here, so an
//! item exists exactly when the run it names exists. Nothing here changes a
//! dispatch: a ledger fault is absorbed, never propagated, exactly as in
//! [`super::work_ledger`], and every mutation takes the same process-wide gate
//! ([`foundry_sdk::work_item::ledger_write_gate`]).

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use chrono::Utc;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    ProjectRunCompletedPayload, ReleaseCompletedPayload, ReleaseRequestedPayload,
    RemediationCompletedPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::{WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkLane};

use super::SimulatedSuccess;
use super::work_ledger::{ledger_lock, load_ledger, save_ledger, work_item_event};

/// The lane a run-shaped dispatch belongs to.
///
/// A `gather_id` marks an event as descending from the maintenance cycle's
/// fan-out; anything else was rooted at the CLI. This is the only lane
/// distinction the existing payloads can carry, and adding one to them is out
/// of scope — so a remediation started from `foundry pipeline` is recorded
/// `interactive` and one nested in the nightly run is recorded `maintenance`.
fn lane_for(trigger: &Event) -> WorkLane {
    if super::complete_project_run::in_maintenance_cycle(trigger) {
        WorkLane::Maintenance
    } else {
        WorkLane::Interactive
    }
}

/// Build a spec for `trigger` with the fields every run-shaped item shares.
fn spec(
    trigger: &Event,
    kind: WorkItemKind,
    lane: WorkLane,
    objective: String,
    origin: &str,
) -> WorkItemSpec {
    WorkItemSpec {
        project: trigger.project.clone(),
        objective,
        kind,
        lane,
        origin: origin.to_string(),
        trace_id: trigger.trace_id.clone(),
    }
}

/// Records a maintenance run, a release or a remediation in the ledger,
/// `running`, from the root event of its chain.
///
/// Mutator — sinks on the four roots that start a run-shaped dispatch. A
/// `DryRun` throttle dispatches nothing, so it records nothing.
pub struct RecordRunWorkItem {
    store_path: PathBuf,
    /// Read to answer "will this run actually happen?" — the release action
    /// flag, and whether the project is one Foundry runs at all. An item
    /// recorded for a run that never starts has no terminal to settle it.
    registry: Arc<RwLock<Registry>>,
}

impl RecordRunWorkItem {
    /// Record run-shaped dispatches in the ledger at `store_path`.
    #[must_use]
    pub fn new(store_path: PathBuf, registry: Arc<RwLock<Registry>>) -> Self {
        Self {
            store_path,
            registry,
        }
    }

    /// Whether the dispatch names a project in the registry.
    fn project_is_registered(&self, project: &str) -> bool {
        match super::read_registry(&self.registry) {
            Ok(guard) => guard.find_project(project).is_some(),
            Err(error) => {
                // Best-effort: a poisoned registry lock is not the ledger's
                // fault, and declining every record would hide real work.
                // Record and let the restart sweep be the backstop.
                tracing::warn!(
                    error = %error,
                    "could not read the registry to check a run dispatch; recording it anyway"
                );
                true
            }
        }
    }

    /// The item `trigger` opens, or `None` when it opens none.
    ///
    /// Every arm delegates to the dispatching block's own `accepts()`
    /// predicate, so the ledger cannot drift out of step with what runs.
    fn planned(&self, trigger: &Event) -> Option<WorkItemSpec> {
        if !trigger.throttle.permits_mutation() {
            return None;
        }
        if !self.project_is_registered(&trigger.project) {
            return None;
        }
        let project = trigger.project.as_str();
        match trigger.event_type {
            EventType::ProjectRunStarted => {
                super::complete_project_run::in_maintenance_cycle(trigger).then(|| {
                    spec(
                        trigger,
                        WorkItemKind::Maintenance,
                        WorkLane::Maintenance,
                        format!("Maintenance run for {project}"),
                        "maintenance cycle",
                    )
                })
            }
            EventType::ReleaseRequested => {
                if !super::release::release_enabled(&self.registry, project) {
                    return None;
                }
                let bump = trigger
                    .parse_payload::<ReleaseRequestedPayload>()
                    .ok()
                    .and_then(|p| p.bump)
                    .unwrap_or_else(|| "auto".to_string());
                Some(spec(
                    trigger,
                    WorkItemKind::Release,
                    WorkLane::Interactive,
                    format!("Release {project} ({bump} bump)"),
                    "foundry release",
                ))
            }
            EventType::MainBranchAudited => {
                if let Some(cve) = super::remediate::remediation_target(trigger) {
                    return Some(spec(
                        trigger,
                        WorkItemKind::Remediation,
                        lane_for(trigger),
                        format!("Remediate vulnerability {cve} in {project}"),
                        "vulnerability remediation",
                    ));
                }
                super::release::cut_release_target(trigger).map(|cve| {
                    spec(
                        trigger,
                        WorkItemKind::Release,
                        WorkLane::Maintenance,
                        format!("Cut a release for {project} fixing {cve}"),
                        "vulnerability release",
                    )
                })
            }
            EventType::PipelineChecked => {
                super::remediate_pipeline::pipeline_remediation_will_proceed(trigger).then(|| {
                    spec(
                        trigger,
                        WorkItemKind::Remediation,
                        lane_for(trigger),
                        format!("Remediate the failing pipeline in {project}"),
                        "pipeline remediation",
                    )
                })
            }
            _ => None,
        }
    }

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

impl SimulatedSuccess for RecordRunWorkItem {
    type Outcome = Option<WorkItem>;

    fn simulate(&self, trigger: &Event) -> Option<WorkItem> {
        self.planned(trigger).map(|spec| WorkItem::dispatched(spec, Utc::now()))
    }

    fn success_events(&self, trigger: &Event, outcome: &Option<WorkItem>) -> Vec<Event> {
        let Some(item) = outcome.as_ref() else {
            return vec![];
        };
        let mut submitted = item.clone();
        submitted.state = WorkItemState::Submitted;
        submitted.reason = "submitted".to_string();
        vec![
            work_item_event(EventType::WorkItemSubmitted, trigger, &submitted),
            work_item_event(EventType::WorkItemStarted, trigger, item),
        ]
    }
}

impl TaskBlock for RecordRunWorkItem {
    task_block_meta! {
        name: "Record Run Work Item",
        kind: Mutator,
        sinks_on: [ProjectRunStarted, ReleaseRequested, MainBranchAudited, PipelineChecked],
    }

    dry_run_via_simulation!();

    fn accepts(&self, trigger: &Event) -> bool {
        self.planned(trigger).is_some()
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        // Defensive: accepts() filters every trigger that opens no item.
        let Some(spec) = self.planned(trigger) else {
            let summary = format!("{}: no run to record", trigger.project);
            return skip!(summary);
        };
        let item = WorkItem::dispatched(spec, Utc::now());
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

/// What a run-shaped terminal settles, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Settlement {
    /// The kind of item this terminal closes. Part of the correlation: a run,
    /// its nested remediation and its automatic release can all be running on
    /// one trace and one project at once.
    kind: WorkItemKind,
    /// Where the item lands.
    state: WorkItemState,
    /// The one-line reason recorded with it.
    reason: String,
}

/// Read the settlement `trigger` carries, or `None` when it carries none.
fn settlement(trigger: &Event) -> Option<Settlement> {
    match trigger.event_type {
        EventType::ProjectRunCompleted => {
            let p = trigger.parse_payload::<ProjectRunCompletedPayload>().ok()?;
            Some(if p.success {
                Settlement {
                    kind: WorkItemKind::Maintenance,
                    state: WorkItemState::Landed,
                    reason: "maintenance run completed".to_string(),
                }
            } else {
                Settlement {
                    kind: WorkItemKind::Maintenance,
                    state: WorkItemState::Failed,
                    reason: "maintenance run did not complete successfully".to_string(),
                }
            })
        }
        EventType::ReleaseCompleted => {
            let p = trigger.parse_payload::<ReleaseCompletedPayload>().ok()?;
            Some(if p.success {
                Settlement {
                    kind: WorkItemKind::Release,
                    state: WorkItemState::Landed,
                    reason: p.new_tag.as_ref().map_or_else(
                        || "released, no new tag reported".to_string(),
                        |tag| format!("released {tag}"),
                    ),
                }
            } else {
                Settlement {
                    kind: WorkItemKind::Release,
                    state: WorkItemState::Failed,
                    reason: "release failed".to_string(),
                }
            })
        }
        EventType::RemediationCompleted => {
            let p = trigger.parse_payload::<RemediationCompletedPayload>().ok()?;
            if p.success {
                return Some(Settlement {
                    kind: WorkItemKind::Remediation,
                    state: WorkItemState::Landed,
                    reason: p.summary.unwrap_or_else(|| "remediation completed".to_string()),
                });
            }
            // A remediation Foundry stopped for review is failed: its commits
            // are not pushed and no release follows, so the review text — not
            // the agent's own summary — is what a person needs to read.
            Some(Settlement {
                kind: WorkItemKind::Remediation,
                state: WorkItemState::Failed,
                reason: p
                    .needs_review
                    .or(p.summary)
                    .unwrap_or_else(|| "remediation failed".to_string()),
            })
        }
        _ => None,
    }
}

/// Settles a maintenance, release or remediation item from its own typed
/// terminal.
///
/// Mutator — sinks on the three run-shaped terminals. `ReleasePipelineCompleted`
/// and `LocalInstallCompleted` are downstream observation of a release that has
/// already settled, so they are deliberately absent.
pub struct SettleRunWorkItem {
    store_path: PathBuf,
}

impl SettleRunWorkItem {
    /// Settle run-shaped items in the ledger at `store_path`.
    #[must_use]
    pub fn new(store_path: PathBuf) -> Self {
        Self { store_path }
    }

    /// Apply `settlement` to the one running item it names, and return it.
    fn settle(&self, trigger: &Event, settlement: &Settlement) -> Option<WorkItem> {
        let _guard = ledger_lock()?;
        let mut store = load_ledger(&self.store_path)?;
        let item = store.running_of_kind(
            trigger.trace_id.as_deref(),
            &trigger.project,
            settlement.kind,
        )?;
        apply(item, settlement);
        let settled = item.clone();
        save_ledger(&store, &self.store_path).then_some(settled)
    }
}

/// Move `item` to the settled state `settlement` names.
fn apply(item: &mut WorkItem, settlement: &Settlement) {
    let now = Utc::now();
    match settlement.state {
        WorkItemState::Landed => item.settle_landed(&settlement.reason, now),
        // Every settlement this block produces is one of these two, and the
        // failed mapping is the safe reading of anything else: an item that
        // reached a terminal is not still running.
        _ => item.settle_failed(&settlement.reason, now),
    }
}

impl SimulatedSuccess for SettleRunWorkItem {
    type Outcome = Option<WorkItem>;

    fn simulate(&self, trigger: &Event) -> Option<WorkItem> {
        let settlement = settlement(trigger)?;
        let mut item = WorkItem::dispatched(
            spec(trigger, settlement.kind, lane_for(trigger), String::new(), "simulated"),
            Utc::now(),
        );
        apply(&mut item, &settlement);
        Some(item)
    }

    fn success_events(&self, trigger: &Event, outcome: &Option<WorkItem>) -> Vec<Event> {
        outcome
            .as_ref()
            .map(|item| vec![work_item_event(EventType::WorkItemSettled, trigger, item)])
            .unwrap_or_default()
    }
}

impl TaskBlock for SettleRunWorkItem {
    task_block_meta! {
        name: "Settle Run Work Item",
        kind: Mutator,
        sinks_on: [ProjectRunCompleted, ReleaseCompleted, RemediationCompleted],
    }

    dry_run_via_simulation!();

    fn accepts(&self, trigger: &Event) -> bool {
        trigger.throttle.permits_mutation() && settlement(trigger).is_some()
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        // Defensive: accepts() filters every terminal that settles nothing.
        let Some(settlement) = settlement(trigger) else {
            let summary = format!("{}: no run terminal to settle", trigger.project);
            return skip!(summary);
        };
        let settled = self.settle(trigger, &settlement);
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

/// The ledger at `path`. Test-only convenience so the unit tests below read
/// the same file the blocks write.
#[cfg(test)]
fn read_items(path: &std::path::Path) -> Vec<WorkItem> {
    foundry_sdk::work_item::WorkItemStore::load(path)
        .map(|store| store.items)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::work_item::{WorkItemKind, WorkItemState, WorkLane};

    use super::super::test_helpers;
    use super::{RecordRunWorkItem, SettleRunWorkItem, read_items};

    const TRACE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn recorder(path: &std::path::Path) -> RecordRunWorkItem {
        RecordRunWorkItem::new(
            path.to_path_buf(),
            test_helpers::registry_with_project("alpha", "/tmp/alpha"),
        )
    }

    fn settler(path: &std::path::Path) -> SettleRunWorkItem {
        SettleRunWorkItem::new(path.to_path_buf())
    }

    fn event(event_type: EventType, payload: serde_json::Value) -> Event {
        Event::new(event_type, "alpha".to_string(), Throttle::Full, payload)
            .with_trace_id(Some(TRACE.to_string()))
    }

    fn cycle_run_started() -> Event {
        event(EventType::ProjectRunStarted, serde_json::json!({}))
            .with_gather_id(Some("gth_cycle".to_string()))
    }

    #[test]
    fn record_run_work_item_sinks_on_the_four_run_roots() {
        let dir = tempfile::tempdir().unwrap();
        let block = recorder(&dir.path().join("work-items.json"));
        assert_eq!(block.name(), "Record Run Work Item");
        assert_eq!(block.kind(), foundry_sdk::task_block::BlockKind::Mutator);
        assert_eq!(
            block.sinks_on(),
            &[
                EventType::ProjectRunStarted,
                EventType::ReleaseRequested,
                EventType::MainBranchAudited,
                EventType::PipelineChecked
            ]
        );
    }

    #[test]
    fn settle_run_work_item_sinks_on_the_three_run_terminals() {
        let dir = tempfile::tempdir().unwrap();
        let block = settler(&dir.path().join("work-items.json"));
        assert_eq!(block.name(), "Settle Run Work Item");
        assert_eq!(block.kind(), foundry_sdk::task_block::BlockKind::Mutator);
        assert_eq!(
            block.sinks_on(),
            &[
                EventType::ProjectRunCompleted,
                EventType::ReleaseCompleted,
                EventType::RemediationCompleted
            ]
        );
    }

    #[test]
    fn accepts_returns_false_for_a_project_run_outside_a_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let block = recorder(&dir.path().join("work-items.json"));
        assert!(!block.accepts(&event(EventType::ProjectRunStarted, serde_json::json!({}))));
    }

    #[test]
    fn accepts_returns_true_for_a_project_run_inside_a_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let block = recorder(&dir.path().join("work-items.json"));
        assert!(block.accepts(&cycle_run_started()));
    }

    #[test]
    fn accepts_returns_false_for_an_unregistered_project() {
        let dir = tempfile::tempdir().unwrap();
        let block = recorder(&dir.path().join("work-items.json"));
        let trigger = Event::new(
            EventType::ProjectRunStarted,
            "unknown".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_gather_id(Some("gth_cycle".to_string()));
        assert!(!block.accepts(&trigger));
    }

    #[test]
    fn accepts_returns_false_at_a_dry_run_throttle() {
        let dir = tempfile::tempdir().unwrap();
        let block = recorder(&dir.path().join("work-items.json"));
        let trigger = Event::new(
            EventType::ProjectRunStarted,
            "alpha".to_string(),
            Throttle::DryRun,
            serde_json::json!({}),
        )
        .with_gather_id(Some("gth_cycle".to_string()));
        assert!(!block.accepts(&trigger));
    }

    #[test]
    fn dry_run_and_accepts_agree_on_skip_for_a_run_outside_a_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let block = recorder(&dir.path().join("work-items.json"));
        let trigger = event(EventType::ProjectRunStarted, serde_json::json!({}));
        assert!(!block.accepts(&trigger));
        assert!(block.dry_run_events(&trigger).is_empty());
        assert!(super::SimulatedSuccess::simulate(&block, &trigger).is_none());
    }

    #[test]
    fn a_dirty_main_branch_records_a_remediation_and_a_clean_one_records_a_release() {
        let dir = tempfile::tempdir().unwrap();
        let block = recorder(&dir.path().join("work-items.json"));
        let dirty = event(
            EventType::MainBranchAudited,
            serde_json::json!({"project": "alpha", "cve": "CVE-2026-1", "vulnerable": true, "dirty": true}),
        );
        let clean = event(
            EventType::MainBranchAudited,
            serde_json::json!({"project": "alpha", "cve": "CVE-2026-1", "vulnerable": true, "dirty": false}),
        );
        let remediation = super::SimulatedSuccess::simulate(&block, &dirty).unwrap();
        assert_eq!(remediation.kind, WorkItemKind::Remediation);
        assert_eq!(remediation.lane, WorkLane::Interactive);
        let release = super::SimulatedSuccess::simulate(&block, &clean).unwrap();
        assert_eq!(release.kind, WorkItemKind::Release);
        assert_eq!(release.lane, WorkLane::Maintenance);
    }

    #[test]
    fn a_pipeline_remediation_nested_in_a_cycle_records_the_maintenance_lane() {
        let dir = tempfile::tempdir().unwrap();
        let block = recorder(&dir.path().join("work-items.json"));
        let trigger = event(
            EventType::PipelineChecked,
            serde_json::json!({"passing": false, "conclusion": "failure"}),
        )
        .with_gather_id(Some("gth_cycle".to_string()));
        let item = super::SimulatedSuccess::simulate(&block, &trigger).unwrap();
        assert_eq!(item.kind, WorkItemKind::Remediation);
        assert_eq!(item.lane, WorkLane::Maintenance);
        assert_eq!(item.origin, "pipeline remediation");
    }

    #[test]
    fn a_passing_pipeline_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let block = recorder(&dir.path().join("work-items.json"));
        let trigger = event(
            EventType::PipelineChecked,
            serde_json::json!({"passing": true, "conclusion": "success"}),
        );
        assert!(!block.accepts(&trigger));
    }

    #[tokio::test]
    async fn a_maintenance_item_settles_from_its_own_project_run_completed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let recorder = recorder(&path);
        let settler = settler(&path);

        recorder.execute(&cycle_run_started()).await.unwrap();
        let opened = read_items(&path);
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].kind, WorkItemKind::Maintenance);
        assert_eq!(opened[0].lane, WorkLane::Maintenance);
        assert_eq!(opened[0].origin, "maintenance cycle");
        assert_eq!(opened[0].state, WorkItemState::Running);

        settler
            .execute(&event(EventType::ProjectRunCompleted, serde_json::json!({"success": true})))
            .await
            .unwrap();
        let settled = read_items(&path);
        assert_eq!(settled[0].id, opened[0].id);
        assert_eq!(settled[0].state, WorkItemState::Landed);
        assert_eq!(settled[0].reason, "maintenance run completed");
    }

    #[tokio::test]
    async fn a_remediation_stopped_for_review_settles_failed_with_that_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let recorder = recorder(&path);
        let settler = settler(&path);

        recorder
            .execute(&event(
                EventType::MainBranchAudited,
                serde_json::json!({"project": "alpha", "cve": "CVE-2026-1", "vulnerable": true, "dirty": true}),
            ))
            .await
            .unwrap();

        settler
            .execute(&event(
                EventType::RemediationCompleted,
                serde_json::json!({
                    "cve": "CVE-2026-1",
                    "success": false,
                    "summary": "an agent summary that must not win",
                    "needs_review": "the agent added an advisory suppression",
                }),
            ))
            .await
            .unwrap();

        let items = read_items(&path);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].state, WorkItemState::Failed);
        assert_eq!(items[0].reason, "the agent added an advisory suppression");
    }

    #[tokio::test]
    async fn a_release_terminal_names_the_new_tag_and_leaves_other_kinds_running() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let recorder = recorder(&path);
        let settler = settler(&path);

        recorder.execute(&cycle_run_started()).await.unwrap();
        recorder
            .execute(&event(
                EventType::MainBranchAudited,
                serde_json::json!({"project": "alpha", "cve": "CVE-2026-1", "vulnerable": true, "dirty": false}),
            ))
            .await
            .unwrap();

        settler
            .execute(&event(
                EventType::ReleaseCompleted,
                serde_json::json!({"release": "patch", "new_tag": "v1.2.3", "success": true}),
            ))
            .await
            .unwrap();

        let items = read_items(&path);
        let maintenance = items.iter().find(|item| item.kind == WorkItemKind::Maintenance).unwrap();
        let release = items.iter().find(|item| item.kind == WorkItemKind::Release).unwrap();
        assert_eq!(maintenance.state, WorkItemState::Running);
        assert_eq!(release.state, WorkItemState::Landed);
        assert_eq!(release.reason, "released v1.2.3");
    }
}
