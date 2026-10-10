//! Pacing — the rules that decide when a queued work item starts.
//!
//! Admission records a task-shaped item `queued` (see [`crate::work_item`]);
//! the daemon's scheduler ticks over the ledger and starts items as these
//! rules allow. [`evaluate`] is the whole decision, pure over a snapshot of
//! the ledger, so the scheduler, the admission block and the owner controls
//! all compute a waiting item's reason the same way.
//!
//! The rules, in the order they are checked for each candidate:
//!
//! 1. One mutating item per repository. A `running` item of any kind, a
//!    maintenance run included, occupies its repository.
//! 2. At most [`Limits::max_running`] items running on this host.
//! 3. The item's `not_before` has passed.
//! 4. Every item in `depends_on` has settled `landed`. One that settled any
//!    other way, or that is not in the ledger, moves the dependent to
//!    `needs_decision` instead; that is decided before the rules above, since
//!    no amount of waiting will change it.
//! 5. The item's lane is not paused.
//!
//! Candidates are taken in priority order: the `interactive` lane first,
//! then oldest `submitted_at` first. A start made earlier in the same tick
//! counts against the repository and host rules for every later candidate.
//!
//! Two small files feed the rules. [`Limits`] is operator-edited
//! (`~/.foundry/pacing.json`) and never written by Foundry; [`PauseState`]
//! is daemon-owned (`~/.foundry/pacing-state.json`) and saved through a
//! same-directory temp-file rename, so a pause is still in force after a
//! restart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::registry::Registry;
use crate::work_item::{WorkItem, WorkItemKind, WorkItemState, WorkLane};

/// How many items may run at once on one host when `pacing.json` says
/// nothing.
pub const DEFAULT_MAX_RUNNING: usize = 2;

/// Current pacing state file format version.
pub const PACING_STATE_VERSION: u32 = 1;

/// The reason a queued item carries when nothing holds it back: the next
/// scheduler tick starts it.
pub const READY_REASON: &str = "ready";

/// The reason a queued item carries while its lane is paused.
pub const LANE_PAUSED_REASON: &str = "lane paused";

/// The operator-edited limits in force on this host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// How many items may run at once on this host.
    #[serde(default = "default_max_running")]
    pub max_running: usize,
}

fn default_max_running() -> usize {
    DEFAULT_MAX_RUNNING
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_running: DEFAULT_MAX_RUNNING,
        }
    }
}

impl Limits {
    /// Load the limits from `path`. A missing file is the defaults.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] on read failure and [`StoreError::Parse`]
    /// when the file holds malformed JSON.
    pub fn load(path: &Path) -> Result<Self, StoreError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path).map_err(|source| StoreError::Io {
            path: path.to_owned(),
            source,
        })?;
        serde_json::from_str(&content).map_err(|source| StoreError::Parse {
            path: path.to_owned(),
            source,
        })
    }
}

/// The limits in force at `path`, absorbing a fault.
///
/// A malformed or unreadable `pacing.json` must not stall every start: the
/// fault is logged on every read and the defaults apply until it is fixed.
#[must_use]
pub fn limits_in_force(path: &Path) -> Limits {
    match Limits::load(path) {
        Ok(limits) => limits,
        Err(error) => {
            // Best-effort: the limits are operator tuning, not a precondition
            // for running work; the defaults are the documented fallback.
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "could not read the pacing limits; using the defaults"
            );
            Limits::default()
        }
    }
}

/// The pause state in force at `path`, absorbing a fault by failing closed.
///
/// A malformed or unreadable state file might hide an operator's pause, so
/// every lane counts as paused until the file is fixed: nothing starts that a
/// person may have meant to stop.
#[must_use]
pub fn pauses_in_force(path: &Path) -> PauseState {
    match PauseState::load(path) {
        Ok(state) => state,
        Err(error) => {
            // Best-effort: there is no caller to propagate to on a scheduler
            // tick, and silently un-pausing would be the dangerous reading.
            tracing::error!(
                path = %path.display(),
                error = %error,
                "could not read the pacing state; treating every lane as paused"
            );
            PauseState {
                version: PACING_STATE_VERSION,
                paused: WorkLane::ALL.to_vec(),
            }
        }
    }
}

/// Which lanes an operator has paused. Daemon-owned and persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PauseState {
    /// Format version.
    pub version: u32,
    /// The paused lanes, in [`WorkLane::ALL`] order.
    #[serde(default)]
    pub paused: Vec<WorkLane>,
}

impl Default for PauseState {
    fn default() -> Self {
        Self {
            version: PACING_STATE_VERSION,
            paused: Vec::new(),
        }
    }
}

impl PauseState {
    /// Load the state from `path`. A missing file means nothing is paused.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] on read failure and [`StoreError::Parse`]
    /// when the file holds malformed JSON.
    pub fn load(path: &Path) -> Result<Self, StoreError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path).map_err(|source| StoreError::Io {
            path: path.to_owned(),
            source,
        })?;
        serde_json::from_str(&content).map_err(|source| StoreError::Parse {
            path: path.to_owned(),
            source,
        })
    }

    /// Save the state to `path` through a same-directory temp-file rename, so
    /// it is never observed half-written. A failed save leaves the previous
    /// contents intact.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] if the directory, write or rename fails.
    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_owned(),
                source,
            })?;
        }
        let content = serde_json::to_string_pretty(self).map_err(|source| StoreError::Parse {
            path: path.to_owned(),
            source,
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, content).map_err(|source| StoreError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| {
            // Best-effort: the save already failed and the caller is being told
            // so; all that is left is not to leave a half-written sibling on
            // disk for the next reader to trip over.
            if let Err(cleanup) = std::fs::remove_file(&tmp) {
                tracing::warn!(
                    path = %tmp.display(),
                    error = %cleanup,
                    "could not remove the pacing state temp file after a failed rename"
                );
            }
            StoreError::Io {
                path: path.to_owned(),
                source,
            }
        })
    }

    /// Whether `lane` is paused.
    #[must_use]
    pub fn is_paused(&self, lane: WorkLane) -> bool {
        self.paused.contains(&lane)
    }

    /// Pause `lanes`. Returns the lanes that were not already paused.
    pub fn pause(&mut self, lanes: &[WorkLane]) -> Vec<WorkLane> {
        let newly: Vec<WorkLane> =
            lanes.iter().copied().filter(|lane| !self.is_paused(*lane)).collect();
        self.paused = WorkLane::ALL
            .into_iter()
            .filter(|lane| self.is_paused(*lane) || newly.contains(lane))
            .collect();
        newly
    }

    /// Resume `lanes`. Returns the lanes that were paused.
    pub fn resume(&mut self, lanes: &[WorkLane]) -> Vec<WorkLane> {
        let resumed: Vec<WorkLane> =
            lanes.iter().copied().filter(|lane| self.is_paused(*lane)).collect();
        self.paused.retain(|lane| !resumed.contains(lane));
        resumed
    }
}

/// The lanes a `pacing pause` or `pacing resume` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneSelection {
    /// Every lane.
    All,
    /// One lane.
    One(WorkLane),
}

impl LaneSelection {
    /// Parse `interactive`, `campaign`, `maintenance` or `all`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text == "all" {
            return Some(Self::All);
        }
        WorkLane::from_tag(text).map(Self::One)
    }

    /// The lanes selected, in [`WorkLane::ALL`] order.
    #[must_use]
    pub fn lanes(self) -> Vec<WorkLane> {
        match self {
            Self::All => WorkLane::ALL.to_vec(),
            Self::One(lane) => vec![lane],
        }
    }
}

/// Where the two pacing files live.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PacingPaths {
    /// The operator-edited [`Limits`].
    pub limits: PathBuf,
    /// The daemon-owned [`PauseState`].
    pub state: PathBuf,
}

impl PacingPaths {
    /// The paths the environment names (see [`crate::paths::pacing_path`]
    /// and [`crate::paths::pacing_state_path`]).
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            limits: crate::paths::pacing_path(),
            state: crate::paths::pacing_state_path(),
        }
    }
}

/// What one tick decides for one queued item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Every rule allows it: start it now.
    Start,
    /// A rule holds it back, for this reason.
    Wait(String),
    /// A dependency can never land; only an owner can say what happens next.
    NeedsDecision(String),
}

/// One tick's decision for one queued item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The item decided on.
    pub id: String,
    /// What was decided.
    pub decision: Decision,
}

/// The reason a queued item waits while `holder` runs on its repository.
#[must_use]
pub fn repository_busy(holder: &str) -> String {
    format!("repository busy: {holder}")
}

/// The reason a queued item waits while the host runs `running` of `max`.
#[must_use]
pub fn host_at_capacity(running: usize, max: usize) -> String {
    format!("host at capacity {running}/{max}")
}

/// The reason a queued item waits until `at`.
#[must_use]
pub fn not_before(at: DateTime<Utc>) -> String {
    format!("not before {}", at.to_rfc3339())
}

/// The reason a queued item waits on `dependency`.
#[must_use]
pub fn waits_on(dependency: &str) -> String {
    format!("waits on {dependency}")
}

/// The reason a dependent moves to `needs_decision` when `dependency` settled
/// `state`, which is not `landed`.
#[must_use]
pub fn waits_on_settled(dependency: &str, state: WorkItemState) -> String {
    format!("waits on {dependency}, which settled {}", state.tag())
}

/// The reason a dependent moves to `needs_decision` when `dependency` is not
/// in the ledger at all.
#[must_use]
pub fn waits_on_unknown(dependency: &str) -> String {
    format!("waits on {dependency}, which is not in the ledger")
}

/// The repository a project's work mutates, as the per-repository rule keys
/// it: the registered GitHub slug when there is one, otherwise the registered
/// checkout path, otherwise the project name itself. Two projects on one slug
/// or one checkout share a repository.
#[must_use]
pub fn repository_key(registry: &Registry, project: &str) -> String {
    registry.find_project(project).map_or_else(
        || project.to_string(),
        |entry| {
            let slug = entry.repo.trim();
            if slug.is_empty() {
                entry.path.clone()
            } else {
                slug.to_string()
            }
        },
    )
}

/// Decide every `queued` item in `items`, in priority order.
///
/// `repository_of` maps a project to the repository it mutates (see
/// [`repository_key`]). The verdicts come back in the order the candidates
/// were considered; a `Start` earlier in the list already counts against the
/// repository and host rules for the verdicts after it.
#[must_use]
pub fn evaluate(
    items: &[WorkItem],
    limits: &Limits,
    pauses: &PauseState,
    now: DateTime<Utc>,
    repository_of: &dyn Fn(&str) -> String,
) -> Vec<Verdict> {
    let mut busy: HashMap<String, String> = HashMap::new();
    for item in items.iter().filter(|item| item.is_running()) {
        busy.entry(repository_of(&item.project)).or_insert_with(|| item.id.clone());
    }
    let mut running = items.iter().filter(|item| item.is_running()).count();

    let mut candidates: Vec<&WorkItem> = items.iter().filter(|item| item.is_queued()).collect();
    candidates.sort_by(|left, right| {
        (left.lane != WorkLane::Interactive, left.submitted_at, &left.id).cmp(&(
            right.lane != WorkLane::Interactive,
            right.submitted_at,
            &right.id,
        ))
    });

    let mut verdicts = Vec::with_capacity(candidates.len());
    for item in candidates {
        let repository = repository_of(&item.project);
        let decision = decide(item, items, &busy, running, limits, pauses, now, &repository);
        if decision == Decision::Start {
            busy.insert(repository, item.id.clone());
            running += 1;
        }
        verdicts.push(Verdict {
            id: item.id.clone(),
            decision,
        });
    }
    verdicts
}

/// One item as `pacing show` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotItem {
    /// The item's id.
    pub id: String,
    /// Registry project name.
    pub project: String,
    /// The repository the per-repository rule keys the item on.
    pub repository: String,
    /// What sort of work it is.
    pub kind: WorkItemKind,
    /// Which lane it is in.
    pub lane: WorkLane,
    /// Where it stands.
    pub state: WorkItemState,
    /// Why, in one line: what it occupies, or why it waits.
    pub reason: String,
    /// `started_at` for a running item, `submitted_at` for a waiting one.
    pub since: Option<DateTime<Utc>>,
}

/// The pacing stage as it stands: the limits in force, what is running, what
/// waits and why, and which lanes are paused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The host running cap in force.
    pub max_running: usize,
    /// The paused lanes, in [`WorkLane::ALL`] order.
    pub paused: Vec<WorkLane>,
    /// Every running item, oldest start first.
    pub running: Vec<SnapshotItem>,
    /// Every queued item in the scheduler's priority order, then every held
    /// item, oldest submission first.
    pub waiting: Vec<SnapshotItem>,
}

/// Read the pacing stage off a ledger snapshot and the files in force.
///
/// Pure, so the daemon's `GetPacing` and `foundry pacing show --offline` list
/// the same items in the same order. Reasons are the ones the ledger records:
/// the scheduler keeps them current on every tick.
#[must_use]
pub fn snapshot(
    items: &[WorkItem],
    limits: &Limits,
    pauses: &PauseState,
    repository_of: &dyn Fn(&str) -> String,
) -> Snapshot {
    let entry = |item: &WorkItem, since: Option<DateTime<Utc>>| SnapshotItem {
        id: item.id.clone(),
        project: item.project.clone(),
        repository: repository_of(&item.project),
        kind: item.kind,
        lane: item.lane,
        state: item.state,
        reason: item.reason.clone(),
        since,
    };
    let mut running: Vec<&WorkItem> = items.iter().filter(|item| item.is_running()).collect();
    running.sort_by(|left, right| (left.started_at, &left.id).cmp(&(right.started_at, &right.id)));
    let mut queued: Vec<&WorkItem> = items.iter().filter(|item| item.is_queued()).collect();
    queued.sort_by(|left, right| {
        (left.lane != WorkLane::Interactive, left.submitted_at, &left.id).cmp(&(
            right.lane != WorkLane::Interactive,
            right.submitted_at,
            &right.id,
        ))
    });
    let mut held: Vec<&WorkItem> =
        items.iter().filter(|item| item.state == WorkItemState::Held).collect();
    held.sort_by(|left, right| (left.submitted_at, &left.id).cmp(&(right.submitted_at, &right.id)));
    Snapshot {
        max_running: limits.max_running,
        paused: pauses.paused.clone(),
        running: running.into_iter().map(|item| entry(item, item.started_at)).collect(),
        waiting: queued
            .into_iter()
            .chain(held)
            .map(|item| entry(item, Some(item.submitted_at)))
            .collect(),
    }
}

#[allow(clippy::too_many_arguments)]
fn decide(
    item: &WorkItem,
    items: &[WorkItem],
    busy: &HashMap<String, String>,
    running: usize,
    limits: &Limits,
    pauses: &PauseState,
    now: DateTime<Utc>,
    repository: &str,
) -> Decision {
    // A dependency that can never land takes the item out of the queue
    // whatever else holds it: waiting would not change the answer.
    for dependency in &item.depends_on {
        match items.iter().find(|candidate| candidate.id == *dependency) {
            None => return Decision::NeedsDecision(waits_on_unknown(dependency)),
            Some(found) if found.state == WorkItemState::Landed => {}
            Some(found) if found.state.is_waiting() || found.is_running() => {}
            Some(found) => {
                return Decision::NeedsDecision(waits_on_settled(dependency, found.state));
            }
        }
    }
    if let Some(holder) = busy.get(repository) {
        return Decision::Wait(repository_busy(holder));
    }
    if running >= limits.max_running {
        return Decision::Wait(host_at_capacity(running, limits.max_running));
    }
    if let Some(at) = item.not_before
        && now < at
    {
        return Decision::Wait(not_before(at));
    }
    if let Some(dependency) = item.depends_on.iter().find(|dependency| {
        items
            .iter()
            .find(|candidate| candidate.id == **dependency)
            .is_none_or(|found| found.state != WorkItemState::Landed)
    }) {
        return Decision::Wait(waits_on(dependency));
    }
    if pauses.is_paused(item.lane) {
        return Decision::Wait(LANE_PAUSED_REASON.to_string());
    }
    Decision::Start
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone as _;

    use super::*;
    use crate::event::{Event, EventType};
    use crate::throttle::Throttle;
    use crate::work_item::{WorkItemKind, WorkItemSpec};

    fn at(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 10, 12, 0, second).single().expect("valid date")
    }

    fn root(project: &str) -> Event {
        Event::new(
            EventType::ExecutionRequested,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({"project": project, "workflow": "task", "prompt": "x"}),
        )
    }

    fn queued(id: &str, project: &str, lane: WorkLane, submitted: u32) -> WorkItem {
        let mut item = WorkItem::queued(
            WorkItemSpec {
                project: project.to_string(),
                objective: "o".to_string(),
                kind: WorkItemKind::Task,
                lane,
                origin: "test".to_string(),
                trace_id: None,
            },
            root(project),
            at(submitted),
        );
        item.id = id.to_string();
        item
    }

    fn running(id: &str, project: &str, kind: WorkItemKind) -> WorkItem {
        let mut item = WorkItem::dispatched(
            WorkItemSpec {
                project: project.to_string(),
                objective: "o".to_string(),
                kind,
                lane: WorkLane::Interactive,
                origin: "test".to_string(),
                trace_id: None,
            },
            at(0),
        );
        item.id = id.to_string();
        item
    }

    fn settled(id: &str, state: WorkItemState) -> WorkItem {
        let mut item = running(id, "alpha", WorkItemKind::Task);
        item.state = state;
        item.settled_at = Some(at(1));
        item
    }

    fn by_project(project: &str) -> String {
        project.to_string()
    }

    fn decisions(
        items: &[WorkItem],
        limits: &Limits,
        pauses: &PauseState,
    ) -> Vec<(String, Decision)> {
        evaluate(items, limits, pauses, at(30), &by_project)
            .into_iter()
            .map(|verdict| (verdict.id, verdict.decision))
            .collect()
    }

    // --- the rules, in order ---------------------------------------------

    #[test]
    fn a_single_queued_item_on_an_idle_host_starts() {
        let items = vec![queued("wi_a", "alpha", WorkLane::Interactive, 1)];
        assert_eq!(
            decisions(&items, &Limits::default(), &PauseState::default()),
            vec![("wi_a".to_string(), Decision::Start)]
        );
    }

    #[test]
    fn two_items_on_one_repository_run_one_after_the_other() {
        let items = vec![
            queued("wi_first", "alpha", WorkLane::Interactive, 1),
            queued("wi_second", "alpha", WorkLane::Interactive, 2),
        ];
        assert_eq!(
            decisions(&items, &Limits::default(), &PauseState::default()),
            vec![
                ("wi_first".to_string(), Decision::Start),
                ("wi_second".to_string(), Decision::Wait("repository busy: wi_first".to_string())),
            ]
        );
    }

    #[test]
    fn a_running_maintenance_item_occupies_its_repository() {
        let items = vec![
            running("wi_nightly", "alpha", WorkItemKind::Maintenance),
            queued("wi_task", "alpha", WorkLane::Interactive, 1),
        ];
        assert_eq!(
            decisions(&items, &Limits::default(), &PauseState::default()),
            vec![(
                "wi_task".to_string(),
                Decision::Wait("repository busy: wi_nightly".to_string())
            )]
        );
    }

    #[test]
    fn two_projects_sharing_a_repository_key_share_the_rule() {
        let items = vec![
            queued("wi_a", "alpha", WorkLane::Interactive, 1),
            queued("wi_b", "alpha-docs", WorkLane::Interactive, 2),
        ];
        let shared = |_: &str| "owner/one-repo".to_string();
        let verdicts =
            evaluate(&items, &Limits::default(), &PauseState::default(), at(30), &shared);
        assert_eq!(verdicts[1].decision, Decision::Wait("repository busy: wi_a".to_string()));
    }

    #[test]
    fn the_host_cap_holds_a_third_project_at_capacity() {
        let items = vec![
            queued("wi_a", "alpha", WorkLane::Interactive, 1),
            queued("wi_b", "beta", WorkLane::Interactive, 2),
            queued("wi_c", "gamma", WorkLane::Interactive, 3),
        ];
        assert_eq!(
            decisions(&items, &Limits::default(), &PauseState::default()),
            vec![
                ("wi_a".to_string(), Decision::Start),
                ("wi_b".to_string(), Decision::Start),
                ("wi_c".to_string(), Decision::Wait("host at capacity 2/2".to_string())),
            ]
        );
    }

    #[test]
    fn running_items_of_every_kind_count_toward_the_host_cap() {
        let items = vec![
            running("wi_m", "alpha", WorkItemKind::Maintenance),
            running("wi_r", "beta", WorkItemKind::Release),
            queued("wi_c", "gamma", WorkLane::Interactive, 3),
        ];
        assert_eq!(
            decisions(&items, &Limits::default(), &PauseState::default()),
            vec![("wi_c".to_string(), Decision::Wait("host at capacity 2/2".to_string()))]
        );
    }

    #[test]
    fn a_raised_limit_lets_more_run() {
        let items = vec![
            queued("wi_a", "alpha", WorkLane::Interactive, 1),
            queued("wi_b", "beta", WorkLane::Interactive, 2),
            queued("wi_c", "gamma", WorkLane::Interactive, 3),
        ];
        let limits = Limits { max_running: 3 };
        assert!(
            decisions(&items, &limits, &PauseState::default())
                .iter()
                .all(|(_, decision)| *decision == Decision::Start)
        );
    }

    #[test]
    fn not_before_holds_the_item_until_the_time_and_names_it() {
        let mut item = queued("wi_a", "alpha", WorkLane::Interactive, 1);
        item.not_before = Some(at(45));
        let items = vec![item];
        assert_eq!(
            decisions(&items, &Limits::default(), &PauseState::default()),
            vec![(
                "wi_a".to_string(),
                Decision::Wait("not before 2026-10-10T12:00:45+00:00".to_string())
            )]
        );
        let later =
            evaluate(&items, &Limits::default(), &PauseState::default(), at(45), &by_project);
        assert_eq!(later[0].decision, Decision::Start, "the boundary itself is past");
    }

    #[test]
    fn a_dependency_still_running_or_queued_is_waited_on() {
        let mut dependent = queued("wi_b", "beta", WorkLane::Interactive, 2);
        dependent.depends_on = vec!["wi_a".to_string()];
        let items = vec![
            running("wi_a", "alpha", WorkItemKind::Task),
            dependent.clone(),
        ];
        assert_eq!(
            decisions(&items, &Limits::default(), &PauseState::default()),
            vec![("wi_b".to_string(), Decision::Wait("waits on wi_a".to_string()))]
        );

        let items = vec![queued("wi_a", "alpha", WorkLane::Campaign, 1), dependent];
        let verdicts = decisions(&items, &Limits::default(), &PauseState::default());
        assert_eq!(verdicts[0], ("wi_b".to_string(), Decision::Wait("waits on wi_a".to_string())));
        assert_eq!(verdicts[1], ("wi_a".to_string(), Decision::Start));
    }

    #[test]
    fn a_landed_dependency_releases_the_dependent() {
        let mut dependent = queued("wi_b", "beta", WorkLane::Interactive, 2);
        dependent.depends_on = vec!["wi_a".to_string()];
        let items = vec![settled("wi_a", WorkItemState::Landed), dependent];
        assert_eq!(
            decisions(&items, &Limits::default(), &PauseState::default()),
            vec![("wi_b".to_string(), Decision::Start)]
        );
    }

    #[test]
    fn a_dependency_that_settled_any_other_way_needs_a_decision_naming_the_state() {
        for state in [
            WorkItemState::Preserved,
            WorkItemState::Failed,
            WorkItemState::Cancelled,
            WorkItemState::NeedsDecision,
        ] {
            let mut dependent = queued("wi_b", "beta", WorkLane::Interactive, 2);
            dependent.depends_on = vec!["wi_a".to_string()];
            // Even with the repository busy, the decision comes first.
            let items = vec![
                settled("wi_a", state),
                running("wi_busy", "beta", WorkItemKind::Task),
                dependent,
            ];
            assert_eq!(
                decisions(&items, &Limits::default(), &PauseState::default()),
                vec![(
                    "wi_b".to_string(),
                    Decision::NeedsDecision(format!(
                        "waits on wi_a, which settled {}",
                        state.tag()
                    ))
                )],
                "{state:?}"
            );
        }
    }

    #[test]
    fn a_dependency_missing_from_the_ledger_needs_a_decision() {
        let mut dependent = queued("wi_b", "beta", WorkLane::Interactive, 2);
        dependent.depends_on = vec!["wi_gone".to_string()];
        assert_eq!(
            decisions(&[dependent], &Limits::default(), &PauseState::default()),
            vec![(
                "wi_b".to_string(),
                Decision::NeedsDecision("waits on wi_gone, which is not in the ledger".to_string())
            )]
        );
    }

    #[test]
    fn a_paused_lane_holds_only_its_own_items() {
        let items = vec![
            queued("wi_i", "alpha", WorkLane::Interactive, 1),
            queued("wi_c", "beta", WorkLane::Campaign, 2),
        ];
        let mut pauses = PauseState::default();
        pauses.pause(&[WorkLane::Campaign]);
        assert_eq!(
            decisions(&items, &Limits::default(), &pauses),
            vec![
                ("wi_i".to_string(), Decision::Start),
                ("wi_c".to_string(), Decision::Wait("lane paused".to_string())),
            ]
        );
    }

    #[test]
    fn the_rules_are_reported_in_their_documented_order() {
        // Everything holds the item at once; the repository rule names it.
        let mut item = queued("wi_b", "alpha", WorkLane::Campaign, 2);
        item.not_before = Some(at(45));
        item.depends_on = vec!["wi_dep".to_string()];
        let mut pauses = PauseState::default();
        pauses.pause(&[WorkLane::Campaign]);
        let mut limits = Limits { max_running: 1 };
        let mut items = vec![
            running("wi_a", "alpha", WorkItemKind::Task),
            running("wi_dep", "delta", WorkItemKind::Task),
            item,
        ];
        let decision = |items: &[WorkItem], limits: &Limits, pauses: &PauseState| {
            decisions(items, limits, pauses).remove(0).1
        };
        let wait = |reason: &str| Decision::Wait(reason.to_string());
        assert_eq!(decision(&items, &limits, &pauses), wait("repository busy: wi_a"));
        items[0].project = "elsewhere".to_string();
        assert_eq!(decision(&items, &limits, &pauses), wait("host at capacity 2/1"));
        limits.max_running = 3;
        assert_eq!(
            decision(&items, &limits, &pauses),
            wait("not before 2026-10-10T12:00:45+00:00")
        );
        items[2].not_before = None;
        assert_eq!(decision(&items, &limits, &pauses), wait("waits on wi_dep"));
        items[1].state = WorkItemState::Landed;
        assert_eq!(decision(&items, &limits, &pauses), wait("lane paused"));
        pauses.resume(&[WorkLane::Campaign]);
        assert_eq!(decision(&items, &limits, &pauses), Decision::Start);
    }

    #[test]
    fn interactive_items_go_first_then_oldest_first() {
        let items = vec![
            queued("wi_old_campaign", "a", WorkLane::Campaign, 1),
            queued("wi_new_interactive", "b", WorkLane::Interactive, 5),
            queued("wi_old_interactive", "c", WorkLane::Interactive, 2),
        ];
        let order: Vec<String> = decisions(&items, &Limits::default(), &PauseState::default())
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(
            order,
            vec![
                "wi_old_interactive",
                "wi_new_interactive",
                "wi_old_campaign"
            ]
        );
        let verdicts = decisions(&items, &Limits::default(), &PauseState::default());
        assert_eq!(verdicts[2].1, Decision::Wait("host at capacity 2/2".to_string()));
    }

    #[test]
    fn a_snapshot_lists_running_then_queued_in_priority_order_then_held() {
        let mut held = queued("wi_held", "d", WorkLane::Interactive, 0);
        held.hold();
        let items = vec![
            queued("wi_campaign", "a", WorkLane::Campaign, 1),
            held,
            running("wi_run", "b", WorkItemKind::Maintenance),
            queued("wi_interactive", "c", WorkLane::Interactive, 5),
        ];
        let mut pauses = PauseState::default();
        pauses.pause(&[WorkLane::Campaign]);
        let snap = snapshot(&items, &Limits { max_running: 3 }, &pauses, &by_project);
        assert_eq!(snap.max_running, 3);
        assert_eq!(snap.paused, vec![WorkLane::Campaign]);
        let running_ids: Vec<&str> = snap.running.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(running_ids, vec!["wi_run"]);
        assert_eq!(snap.running[0].repository, "b");
        assert_eq!(snap.running[0].since, items[2].started_at);
        let waiting_ids: Vec<&str> = snap.waiting.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(waiting_ids, vec!["wi_interactive", "wi_campaign", "wi_held"]);
        assert_eq!(snap.waiting[2].reason, crate::work_item::HELD_REASON);
        assert_eq!(snap.waiting[2].since, Some(at(0)));
    }

    #[test]
    fn held_items_are_not_candidates() {
        let mut held = queued("wi_h", "alpha", WorkLane::Interactive, 1);
        held.hold();
        assert!(decisions(&[held], &Limits::default(), &PauseState::default()).is_empty());
    }

    // --- the files -------------------------------------------------------

    #[test]
    fn limits_default_to_two_and_read_the_file_when_present() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pacing.json");
        assert_eq!(Limits::load(&path).unwrap(), Limits { max_running: 2 });
        std::fs::write(&path, r#"{"max_running": 4}"#).unwrap();
        assert_eq!(Limits::load(&path).unwrap(), Limits { max_running: 4 });
        std::fs::write(&path, "{}").unwrap();
        assert_eq!(Limits::load(&path).unwrap().max_running, 2, "an empty object is the defaults");
        std::fs::write(&path, "{ nope").unwrap();
        assert!(matches!(Limits::load(&path).unwrap_err(), StoreError::Parse { .. }));
    }

    #[test]
    fn pause_state_round_trips_through_an_atomic_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pacing-state.json");
        assert_eq!(PauseState::load(&path).unwrap(), PauseState::default());

        let mut state = PauseState::default();
        assert_eq!(
            state.pause(&[WorkLane::Campaign, WorkLane::Interactive]),
            vec![WorkLane::Campaign, WorkLane::Interactive]
        );
        assert_eq!(
            state.paused,
            vec![WorkLane::Interactive, WorkLane::Campaign],
            "stored in lane order"
        );
        assert!(state.pause(&[WorkLane::Campaign]).is_empty(), "already paused");
        state.save(&path).unwrap();
        assert!(!path.with_extension("json.tmp").exists());

        let loaded = PauseState::load(&path).unwrap();
        assert_eq!(loaded, state);
        assert!(loaded.is_paused(WorkLane::Interactive));
        assert!(!loaded.is_paused(WorkLane::Maintenance));

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["paused"], serde_json::json!(["interactive", "campaign"]));

        let mut resumed = loaded;
        assert_eq!(resumed.resume(&WorkLane::ALL), vec![WorkLane::Interactive, WorkLane::Campaign]);
        assert!(resumed.paused.is_empty());
        assert!(resumed.resume(&[WorkLane::Campaign]).is_empty());
    }

    #[test]
    fn faulty_files_fall_back_to_defaults_for_limits_and_fail_closed_for_pauses() {
        let dir = tempfile::tempdir().unwrap();
        let limits = dir.path().join("pacing.json");
        let state = dir.path().join("pacing-state.json");
        std::fs::write(&limits, "nope").unwrap();
        std::fs::write(&state, "nope").unwrap();
        assert_eq!(limits_in_force(&limits), Limits::default());
        assert_eq!(pauses_in_force(&state).paused, WorkLane::ALL.to_vec());
        assert_eq!(pauses_in_force(&dir.path().join("absent.json")), PauseState::default());
    }

    #[test]
    fn a_lane_selection_parses_each_lane_and_all() {
        assert_eq!(LaneSelection::parse("all"), Some(LaneSelection::All));
        assert_eq!(LaneSelection::parse("campaign"), Some(LaneSelection::One(WorkLane::Campaign)));
        assert_eq!(LaneSelection::parse("nope"), None);
        assert_eq!(LaneSelection::All.lanes(), WorkLane::ALL.to_vec());
        assert_eq!(LaneSelection::One(WorkLane::Maintenance).lanes(), vec![WorkLane::Maintenance]);
    }

    #[test]
    fn the_repository_key_is_the_slug_then_the_path_then_the_name() {
        use crate::registry::{ActionFlags, ProjectEntry, Stack};
        let entry = |name: &str, path: &str, repo: &str| ProjectEntry {
            name: name.to_string(),
            path: path.to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: repo.to_string(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: ActionFlags::default(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
            update_policy: None,
        };
        let registry = Registry {
            version: 2,
            projects: vec![
                entry("alpha", "/src/alpha", "owner/alpha"),
                entry("alpha-docs", "/src/alpha", "owner/alpha"),
                entry("beta", "/src/beta", ""),
            ],
        };
        assert_eq!(repository_key(&registry, "alpha"), "owner/alpha");
        assert_eq!(repository_key(&registry, "alpha-docs"), "owner/alpha");
        assert_eq!(repository_key(&registry, "beta"), "/src/beta");
        assert_eq!(repository_key(&registry, "unknown"), "unknown");
    }
}
