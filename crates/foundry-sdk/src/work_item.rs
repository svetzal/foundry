//! The work-item ledger — one durable record per unit of work Foundry
//! dispatches.
//!
//! Foundry runs engineering work but, before this ledger, kept no record of
//! the work *as a unit*: `foundry task` executed at once, a campaign derived
//! one objective at a time, and the nightly fanned out per project. Nothing
//! answered "what is running, what still needs a person" without reading raw
//! events, worktrees on disk and remote branches.
//!
//! A [`WorkItem`] is that record. The store is a single JSON file
//! ([`crate::paths::work_items_path`]), owned by the daemon and authoritative:
//! every mutation loads the file, applies the change, and saves it. Nothing
//! caches items between mutations.
//!
//! This first slice records the work; it paces nothing. Every item goes
//! `submitted` → `queued` → `running` immediately, exactly matching the
//! dispatch behaviour that already exists.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::payload::{TaskRunCompletedPayload, TaskVerdict};

/// Current work-item store format version. Bumped on schema-breaking changes.
pub const WORK_ITEM_STORE_VERSION: u32 = 1;

/// What sort of work a [`WorkItem`] represents.
///
/// All six kinds are recorded: the three task-shaped kinds (`Task`,
/// `CampaignCycle`, `MajorUpgrade`) from a task workflow's root
/// `ExecutionRequested`, and the three run-shaped kinds (`Maintenance`,
/// `Release`, `Remediation`) from the root event of the run each one names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemKind {
    /// A one-shot `foundry task` execution.
    Task,
    /// One cycle of a durable campaign.
    CampaignCycle,
    /// A per-project maintenance run.
    Maintenance,
    /// A major dependency upgrade dispatched by the nightly majors lane.
    MajorUpgrade,
    /// A release run.
    Release,
    /// A pipeline or vulnerability remediation run.
    Remediation,
}

/// Which queue a [`WorkItem`] belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkLane {
    /// Submitted by a person at the CLI.
    Interactive,
    /// Derived by a campaign.
    Campaign,
    /// Dispatched by a scheduled maintenance run.
    Maintenance,
}

/// Where a [`WorkItem`] stands.
///
/// `Landed` and `Cancelled` are terminal. `Preserved`, `NeedsDecision` and
/// `Failed` are *open*: they are settled, but they still hold an obligation a
/// person has to discharge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemState {
    /// Admitted to the ledger, not yet queued.
    Submitted,
    /// Waiting to start.
    Queued,
    /// An agent is working on it.
    Running,
    /// The work reached trunk.
    Landed,
    /// The work is held on a durable ref for a later cycle.
    Preserved,
    /// The work stopped on a question only a person can answer.
    NeedsDecision,
    /// The work stopped on a fault.
    Failed,
    /// An operator stopped the work.
    Cancelled,
}

impl WorkItemState {
    /// Whether this state still holds an obligation for a person.
    #[must_use]
    pub fn is_open(self) -> bool {
        matches!(self, Self::Preserved | Self::NeedsDecision | Self::Failed)
    }

    /// The serialized tag this state is written to disk and to the wire as.
    ///
    /// Kept in lockstep with the `snake_case` serde renaming above so a caller
    /// filtering on a state never has to round-trip through `serde_json`.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Submitted => "submitted",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Landed => "landed",
            Self::Preserved => "preserved",
            Self::NeedsDecision => "needs_decision",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Parse a serialized state tag, returning `None` for anything unknown.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        [
            Self::Submitted,
            Self::Queued,
            Self::Running,
            Self::Landed,
            Self::Preserved,
            Self::NeedsDecision,
            Self::Failed,
            Self::Cancelled,
        ]
        .into_iter()
        .find(|state| state.tag() == tag)
    }
}

impl WorkItemKind {
    /// The serialized tag this kind is written to disk and to the wire as.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::CampaignCycle => "campaign_cycle",
            Self::Maintenance => "maintenance",
            Self::MajorUpgrade => "major_upgrade",
            Self::Release => "release",
            Self::Remediation => "remediation",
        }
    }
}

impl WorkLane {
    /// The serialized tag this lane is written to disk and to the wire as.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Campaign => "campaign",
            Self::Maintenance => "maintenance",
        }
    }
}

/// How a settled [`WorkItem`] ended.
///
/// The trace id is deliberately *not* repeated here: it is known at submission
/// and lives once, on [`WorkItem::trace_id`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkDisposition {
    /// The reviewer's typed verdict tag (see [`TaskVerdict::tag`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// The trunk commit the work landed as, when it landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_commit: Option<String>,
    /// The durable ref (branch or `bundle:<path>`) holding unlanded work.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preservation_ref: Option<String>,
    /// The isolated worktree the work ran in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// Whether that worktree was gone by the time the item settled. `None`
    /// when the item records no worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_removed: Option<bool>,
}

/// An owner action, separate from the item's original submission and settlement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItemOperatorAction {
    /// `close` or `cancel`.
    pub command: String,
    /// CLI hostname and optional operator context.
    pub origin: String,
    /// State before the action.
    pub previous_state: WorkItemState,
    /// Reason before the action.
    pub previous_reason: String,
    /// Earlier settlement timestamp, retained as evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_settled_at: Option<DateTime<Utc>>,
}

/// One durable unit of work Foundry dispatched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkItem {
    /// Stable identity, minted at submission.
    pub id: String,
    /// Registry project name.
    pub project: String,
    /// The task description or campaign objective the work serves.
    pub objective: String,
    /// What sort of work this is.
    pub kind: WorkItemKind,
    /// Which queue it belongs to.
    pub lane: WorkLane,
    /// Opaque submitter text. Foundry never interprets it.
    pub origin: String,
    /// When the item entered the ledger.
    pub submitted_at: DateTime<Utc>,
    /// When an agent started on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// When it settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settled_at: Option<DateTime<Utc>>,
    /// Where it stands.
    pub state: WorkItemState,
    /// Why it is in that state, in one line.
    pub reason: String,
    /// The workflow trace the item belongs to. This is what correlates the
    /// item with the events its run produced, so it is recorded at submission
    /// rather than at settlement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// How it ended. `None` until it settles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<WorkDisposition>,
    /// Exact preserved item this task continues.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumes: Option<String>,
    /// Latest owner action. Absent on records predating owner controls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_action: Option<WorkItemOperatorAction>,
}

/// Generate a fresh work-item id as `wi_` followed by 24 lowercase hex
/// characters.
///
/// The `wi_` prefix makes the id self-describing and visually distinct from
/// `evt_` event ids, `gth_` gather ids and the bare-hex trace ids.
#[must_use]
pub fn mint_work_item_id() -> String {
    use rand::Rng as _;
    let mut bytes = [0u8; 12];
    rand::rng().fill_bytes(&mut bytes);
    format!("wi_{}", hex::encode(bytes))
}

/// What a dispatch knows about the work before it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemSpec {
    /// Registry project name.
    pub project: String,
    /// The task description or campaign objective.
    pub objective: String,
    /// What sort of work this is.
    pub kind: WorkItemKind,
    /// Which queue it belongs to.
    pub lane: WorkLane,
    /// Opaque submitter text.
    pub origin: String,
    /// The workflow trace the work runs under.
    pub trace_id: Option<String>,
}

impl WorkItem {
    /// Admit `spec` to the ledger as `submitted`.
    #[must_use]
    pub fn submitted(spec: WorkItemSpec, at: DateTime<Utc>) -> Self {
        Self {
            id: mint_work_item_id(),
            project: spec.project,
            objective: spec.objective,
            kind: spec.kind,
            lane: spec.lane,
            origin: spec.origin,
            submitted_at: at,
            started_at: None,
            settled_at: None,
            state: WorkItemState::Submitted,
            reason: "submitted".to_string(),
            trace_id: spec.trace_id,
            disposition: None,
            operator_action: None,
            resumes: None,
        }
    }

    /// Move the item to `queued`.
    pub fn queue(&mut self) {
        self.state = WorkItemState::Queued;
        self.reason = "queued".to_string();
    }

    /// Move the item to `running`.
    pub fn start(&mut self, at: DateTime<Utc>) {
        self.state = WorkItemState::Running;
        self.started_at = Some(at);
        self.reason = "running".to_string();
    }

    /// Admit, queue and start an item in one step.
    ///
    /// This slice paces nothing: a dispatch that reaches the ledger is already
    /// under way, so the three transitions happen together rather than a
    /// scheduler moving the item between them.
    #[must_use]
    pub fn dispatched(spec: WorkItemSpec, at: DateTime<Utc>) -> Self {
        let mut item = Self::submitted(spec, at);
        item.queue();
        item.start(at);
        item
    }

    /// Whether the item is still `running`.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.state == WorkItemState::Running
    }

    /// Settle the item from the task runner's typed terminal result.
    ///
    /// The mapping is fixed: `complete` or a landing `remainder` landed; a
    /// non-landing `remainder` or a `defect` is preserved work; a
    /// `blocked_on_decision` needs a person; a `runner_error` failed.
    ///
    /// A `complete` whose landing was blocked (`land_blocked` set — trunk
    /// moved and the rebase conflicted, say) is preserved work to reconcile:
    /// the reviewer accepted it, but it is not on trunk. A `complete` with no
    /// deliverable reports no landing and no block, and settles `landed` as
    /// before.
    ///
    /// `worktree_removed` reports whether the run's isolated worktree was gone
    /// by settlement time. The finalize step removes it best-effort and
    /// reports nothing, so this is observed rather than asked for.
    pub fn settle_from_task_run(
        &mut self,
        result: &TaskRunCompletedPayload,
        worktree_removed: Option<bool>,
        at: DateTime<Utc>,
    ) {
        let (state, reason) = match &result.verdict {
            TaskVerdict::Complete if result.land_blocked.is_some() => {
                (WorkItemState::Preserved, result.summary.clone())
            }
            TaskVerdict::Complete => (WorkItemState::Landed, result.summary.clone()),
            TaskVerdict::Remainder { .. } if result.landed => {
                (WorkItemState::Landed, result.summary.clone())
            }
            TaskVerdict::Remainder { .. } => (WorkItemState::Preserved, result.summary.clone()),
            TaskVerdict::Defect { diagnosis } => (WorkItemState::Preserved, diagnosis.clone()),
            TaskVerdict::BlockedOnDecision { finding, .. } => {
                (WorkItemState::NeedsDecision, finding.clone())
            }
            TaskVerdict::RunnerError { detail } => (WorkItemState::Failed, detail.clone()),
        };
        self.state = state;
        self.reason = one_line(&reason);
        self.settled_at = Some(at);
        self.disposition = Some(WorkDisposition {
            verdict: Some(result.verdict.tag().to_string()),
            landed_commit: if result.landed {
                result.preservation_ref.clone()
            } else {
                None
            },
            preservation_ref: if result.landed {
                None
            } else {
                result.preservation_ref.clone()
            },
            worktree: result.context.task_worktree.clone(),
            worktree_removed,
        });
    }

    /// Settle the item `landed` with `reason`, keeping whatever disposition
    /// fields are already known.
    ///
    /// Used by the run-shaped kinds, whose terminals report success as a
    /// boolean with no typed verdict to map: a maintenance run, a release or
    /// a remediation that reports success has reached trunk.
    pub fn settle_landed(&mut self, reason: &str, at: DateTime<Utc>) {
        self.state = WorkItemState::Landed;
        self.reason = one_line(reason);
        self.settled_at = Some(at);
    }

    /// Settle the item `cancelled` with `reason`, recording `disposition`.
    ///
    /// Used when an operator stops the work outright — today only
    /// `foundry campaign cancel --now`, which aborts the in-flight cycle so no
    /// typed task result ever arrives to settle the item. `reason` is the
    /// operator's own cancellation reason, recorded verbatim (collapsed to one
    /// line) rather than paraphrased: it is the only account of why the work
    /// stopped.
    ///
    /// `disposition` is what disposal observed afterwards — the worktree the
    /// cycle ran in, whether it is gone, and the ref holding any preserved
    /// work. `None` when the cancellation found no worktree to dispose of.
    ///
    /// `Cancelled` is terminal and not *open*: an operator who stopped the work
    /// has already discharged the decision, so the item carries no further
    /// obligation.
    pub fn settle_cancelled(
        &mut self,
        reason: &str,
        disposition: Option<WorkDisposition>,
        at: DateTime<Utc>,
    ) {
        self.state = WorkItemState::Cancelled;
        self.reason = one_line(reason);
        self.settled_at = Some(at);
        self.disposition = disposition;
    }

    /// Settle the item `failed` with `reason`, keeping whatever disposition
    /// fields are already known.
    ///
    /// Used when a dispatch faults with no typed task result to read — most
    /// importantly on daemon start, where every item left `running` by the
    /// previous process is settled so a restart never leaves work visibly
    /// running.
    pub fn settle_failed(&mut self, reason: &str, at: DateTime<Utc>) {
        self.state = WorkItemState::Failed;
        self.reason = one_line(reason);
        self.settled_at = Some(at);
    }
}

/// Collapse `text` to a single line, so a `reason` never breaks a one-item-
/// per-line rendering.
fn one_line(text: &str) -> String {
    let collapsed: String = text.split_whitespace().collect::<Vec<&str>>().join(" ");
    if collapsed.is_empty() {
        "no reason recorded".to_string()
    } else {
        collapsed
    }
}

/// The on-disk work-item ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkItemStore {
    /// Format version.
    pub version: u32,
    /// Every item recorded, in submission order.
    pub items: Vec<WorkItem>,
}

impl Default for WorkItemStore {
    fn default() -> Self {
        Self {
            version: WORK_ITEM_STORE_VERSION,
            items: Vec::new(),
        }
    }
}

/// The same-directory temp file a save writes through before renaming.
///
/// Same directory matters: a rename across filesystems is not atomic, and the
/// ledger must never be observed half-written.
fn temp_path(path: &Path) -> PathBuf {
    path.with_extension("json.tmp")
}

impl WorkItemStore {
    /// Load the ledger from `path`.
    ///
    /// A missing file is an empty ledger — the daemon's first dispatch creates
    /// it, so absence is the normal starting state rather than a fault.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] on read failure and [`StoreError::Parse`]
    /// when the file contains malformed JSON.
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

    /// Save the ledger to `path` through a same-directory temp file rename.
    ///
    /// A failed save leaves the previous contents byte-for-byte intact and no
    /// temp file behind.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] if the directory, write or rename fails, and
    /// [`StoreError::Parse`] if serialization fails (which it does not for
    /// this type).
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
        let tmp = temp_path(path);
        std::fs::write(&tmp, content).map_err(|source| StoreError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| {
            // Best-effort: the save already failed and the caller is being told
            // so; all that is left is not to leave a half-written sibling of the
            // ledger on disk for the next reader to trip over.
            if let Err(cleanup) = std::fs::remove_file(&tmp) {
                tracing::warn!(
                    path = %tmp.display(),
                    error = %cleanup,
                    "could not remove the work-item ledger temp file after a failed rename"
                );
            }
            StoreError::Io {
                path: path.to_owned(),
                source,
            }
        })
    }

    /// Look up an item by id.
    #[must_use]
    pub fn find(&self, id: &str) -> Option<&WorkItem> {
        self.items.iter().find(|item| item.id == id)
    }

    /// Look up an item by id for mutation.
    pub fn find_mut(&mut self, id: &str) -> Option<&mut WorkItem> {
        self.items.iter_mut().find(|item| item.id == id)
    }

    /// Record a new item, or replace an existing one with the same id.
    pub fn upsert(&mut self, item: WorkItem) {
        if let Some(existing) = self.find_mut(&item.id) {
            *existing = item;
        } else {
            self.items.push(item);
        }
    }

    /// Every item still `running`.
    pub fn running(&self) -> impl Iterator<Item = &WorkItem> {
        self.items.iter().filter(|item| item.is_running())
    }

    /// The `running` item a task run's terminal result settles.
    ///
    /// A run's events all carry its `trace_id`, so that is the correlation, and
    /// it is the *only* correlation whenever the result has one: a result that
    /// names a trace no running item carries settles nothing. Falling back to
    /// the project's newest running item there would settle a concurrent
    /// workflow's item with another run's verdict, which is worse than leaving
    /// the item running until the next restart closes it.
    ///
    /// The project-newest-running fallback therefore applies only to a result
    /// that carries no trace at all — a run that predates trace stamping, where
    /// the project is the only correlation left.
    ///
    /// Only the task-shaped kinds are eligible. A maintenance, release or
    /// remediation item can share a trace — and a project — with the task run
    /// this result belongs to, and settling one of those with a task verdict
    /// would report the wrong unit of work as finished. Those kinds settle
    /// through [`WorkItemStore::running_of_kind`] from their own terminal
    /// instead. For a task item the correlation is unchanged.
    pub fn running_for_settlement(
        &mut self,
        trace_id: Option<&str>,
        project: &str,
    ) -> Option<&mut WorkItem> {
        if let Some(trace) = trace_id {
            let index = self.items.iter().position(|item| {
                item.is_running()
                    && settles_from_task_run(item.kind)
                    && item.trace_id.as_deref() == Some(trace)
            })?;
            return self.items.get_mut(index);
        }
        let index = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.is_running() && settles_from_task_run(item.kind) && item.project == project
            })
            .max_by_key(|(_, item)| item.started_at)
            .map(|(index, _)| index)?;
        self.items.get_mut(index)
    }

    /// The `running` item of `kind` that a run-shaped terminal settles.
    ///
    /// The run-shaped kinds nest and fan out: a maintenance cycle puts one
    /// `Maintenance` item per project on one trace, and a single per-project
    /// run can hold a `Maintenance`, a `Remediation` and a `Release` item at
    /// once, all on that same trace *and* project. Neither the trace nor the
    /// project alone identifies which item a terminal closes, so the kind is
    /// part of the correlation and all three must match exactly — a terminal
    /// carrying no trace settles only an item that carries none either.
    ///
    /// When more than one running item still matches (a run that remediates
    /// twice), the oldest is settled first, so a sequence of terminals closes
    /// the items in the order their roots opened them.
    pub fn running_of_kind(
        &mut self,
        trace_id: Option<&str>,
        project: &str,
        kind: WorkItemKind,
    ) -> Option<&mut WorkItem> {
        let index = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.is_running()
                    && item.kind == kind
                    && item.project == project
                    && item.trace_id.as_deref() == trace_id
            })
            .min_by_key(|(_, item)| item.started_at)
            .map(|(index, _)| index)?;
        self.items.get_mut(index)
    }
}

/// Whether a task run's terminal result is what settles items of `kind`.
///
/// The task-shaped kinds all reach the engine through one root
/// (`ExecutionRequested`) and leave through one terminal
/// (`TaskRunCompleted`); the run-shaped kinds have their own roots and their
/// own terminals.
fn settles_from_task_run(kind: WorkItemKind) -> bool {
    match kind {
        WorkItemKind::Task | WorkItemKind::CampaignCycle | WorkItemKind::MajorUpgrade => true,
        WorkItemKind::Maintenance | WorkItemKind::Release | WorkItemKind::Remediation => false,
    }
}

/// The process-wide lock that orders read-modify-write sequences against the
/// ledger file.
///
/// The file stays the single source of truth — this lock holds no ledger state
/// and caches nothing. It exists because a `load` → apply → `save` is not
/// atomic: two concurrent workflows, one recording a dispatch and another
/// settling a different one, would otherwise both load the same bytes and the
/// later save would silently drop the earlier change. One gate per mutation
/// site is not enough; every mutation in the process has to take the *same*
/// gate, so it lives here beside the store rather than inside any one caller.
///
/// Callers must absorb a poisoned lock rather than unwrap it: `foundryd` is
/// long-lived state, and losing the ledger must never take the daemon down.
#[must_use]
pub fn ledger_write_gate() -> &'static std::sync::Mutex<()> {
    static GATE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &GATE
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::LoopContext;

    fn spec() -> WorkItemSpec {
        WorkItemSpec {
            project: "alpha".to_string(),
            objective: "Add a --quiet flag.".to_string(),
            kind: WorkItemKind::Task,
            lane: WorkLane::Interactive,
            origin: "foundry task".to_string(),
            trace_id: Some("a".repeat(32)),
        }
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn run_result(verdict: TaskVerdict, landed: bool) -> TaskRunCompletedPayload {
        TaskRunCompletedPayload {
            project: "alpha".to_string(),
            success: verdict.is_complete(),
            landed,
            summary: "task summary".to_string(),
            preservation_ref: Some("ref-or-commit".to_string()),
            land_blocked: None,
            trunk_arrivals: Vec::new(),
            verdict,
            context: LoopContext {
                task_worktree: Some("/tmp/worktrees/alpha/abc".to_string()),
                task_branch: Some("foundry-task/abc".to_string()),
                ..LoopContext::default()
            },
        }
    }

    // --- store -------------------------------------------------------------

    #[test]
    fn a_store_round_trips_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let mut store = WorkItemStore::default();
        store.upsert(WorkItem::dispatched(spec(), now()));
        store.save(&path).unwrap();

        let loaded = WorkItemStore::load(&path).unwrap();
        assert_eq!(loaded.version, WORK_ITEM_STORE_VERSION);
        assert_eq!(loaded.items, store.items);
    }

    #[test]
    fn a_save_writes_through_a_temp_file_in_the_same_directory_and_leaves_none_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let tmp = temp_path(&path);
        assert_eq!(tmp.parent(), path.parent(), "the temp file must be a sibling");

        WorkItemStore::default().save(&path).unwrap();

        assert!(path.exists());
        assert!(!tmp.exists(), "a successful save must leave no temp file behind");
        let strays: Vec<PathBuf> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| p.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "stray temp files: {strays:?}");
    }

    #[test]
    fn loading_a_missing_path_yields_an_empty_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkItemStore::load(&dir.path().join("absent.json")).unwrap();
        assert!(store.items.is_empty());
        assert_eq!(store.version, WORK_ITEM_STORE_VERSION);
    }

    #[test]
    fn loading_malformed_json_is_a_typed_parse_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        std::fs::write(&path, "{ not json").unwrap();
        let err = WorkItemStore::load(&path).unwrap_err();
        assert!(matches!(err, StoreError::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn a_failed_save_leaves_the_prior_bytes_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let mut store = WorkItemStore::default();
        store.upsert(WorkItem::dispatched(spec(), now()));
        store.save(&path).unwrap();
        let before = std::fs::read(&path).unwrap();

        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            perms.set_mode(0o500);
        }
        std::fs::set_permissions(dir.path(), perms).unwrap();

        let mut changed = store.clone();
        changed.items.clear();
        let result = changed.save(&path);

        let mut restore = std::fs::metadata(dir.path()).unwrap().permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            restore.set_mode(0o700);
        }
        std::fs::set_permissions(dir.path(), restore).unwrap();

        assert!(result.is_err(), "a read-only directory must fail the save");
        assert_eq!(std::fs::read(&path).unwrap(), before, "prior bytes must be intact");
        assert!(!temp_path(&path).exists(), "no stray temp file");
    }

    #[test]
    fn upsert_replaces_an_item_with_the_same_id() {
        let mut store = WorkItemStore::default();
        let item = WorkItem::dispatched(spec(), now());
        let id = item.id.clone();
        store.upsert(item.clone());
        let mut settled = item;
        settled.settle_failed("daemon restarted", now());
        store.upsert(settled);
        assert_eq!(store.items.len(), 1);
        assert_eq!(store.find(&id).unwrap().state, WorkItemState::Failed);
    }

    #[test]
    fn settlement_finds_the_running_item_by_trace() {
        let mut store = WorkItemStore::default();
        let mut other = spec();
        other.trace_id = Some("b".repeat(32));
        store.upsert(WorkItem::dispatched(other, now()));
        let wanted = WorkItem::dispatched(spec(), now());
        let wanted_id = wanted.id.clone();
        store.upsert(wanted);

        let found = store.running_for_settlement(Some(&"a".repeat(32)), "alpha").unwrap();
        assert_eq!(found.id, wanted_id);
    }

    #[test]
    fn settlement_without_a_trace_falls_back_to_the_projects_newest_running_item() {
        let mut store = WorkItemStore::default();
        let mut traceless = spec();
        traceless.trace_id = None;
        let older = WorkItem::dispatched(traceless.clone(), now() - chrono::Duration::minutes(5));
        let newer = WorkItem::dispatched(traceless, now());
        let newest_id = newer.id.clone();
        store.upsert(older);
        store.upsert(newer);

        assert_eq!(
            store.running_for_settlement(None, "alpha").unwrap().id,
            newest_id,
            "with no trace to correlate by, the project's newest running item is the candidate"
        );
        assert!(store.running_for_settlement(None, "beta").is_none());
    }

    #[test]
    fn an_unmatched_trace_settles_nothing_and_leaves_every_item_running() {
        let mut store = WorkItemStore::default();
        let mut under_b = spec();
        under_b.trace_id = Some("b".repeat(32));
        store.upsert(WorkItem::dispatched(spec(), now()));
        store.upsert(WorkItem::dispatched(under_b, now()));

        assert!(
            store.running_for_settlement(Some(&"c".repeat(32)), "alpha").is_none(),
            "a result from a third trace belongs to neither running item"
        );
        assert_eq!(
            store.running().count(),
            2,
            "both concurrent runs must still be running afterwards"
        );
    }

    #[test]
    fn a_settled_item_is_no_longer_a_settlement_candidate() {
        let mut store = WorkItemStore::default();
        let item = WorkItem::dispatched(spec(), now());
        store.upsert(item);
        let trace = "a".repeat(32);
        store
            .running_for_settlement(Some(&trace), "alpha")
            .unwrap()
            .settle_failed("boom", now());
        assert!(store.running_for_settlement(Some(&trace), "alpha").is_none());
        assert_eq!(store.running().count(), 0);
    }

    // --- lifecycle ---------------------------------------------------------

    #[test]
    fn a_dispatched_item_is_running_with_its_start_recorded() {
        let at = now();
        let item = WorkItem::dispatched(spec(), at);
        assert_eq!(item.state, WorkItemState::Running);
        assert_eq!(item.started_at, Some(at));
        assert_eq!(item.submitted_at, at);
        assert_eq!(item.settled_at, None);
        assert!(item.id.starts_with("wi_"), "id: {}", item.id);
        assert_eq!(item.disposition, None);
    }

    #[test]
    fn minted_ids_are_distinct() {
        assert_ne!(mint_work_item_id(), mint_work_item_id());
    }

    #[test]
    fn every_state_kind_and_lane_tag_matches_its_serde_representation() {
        for state in [
            WorkItemState::Submitted,
            WorkItemState::Queued,
            WorkItemState::Running,
            WorkItemState::Landed,
            WorkItemState::Preserved,
            WorkItemState::NeedsDecision,
            WorkItemState::Failed,
            WorkItemState::Cancelled,
        ] {
            let serialized = serde_json::to_value(state).unwrap();
            assert_eq!(serialized, state.tag(), "tag must match serde for {state:?}");
            assert_eq!(WorkItemState::from_tag(state.tag()), Some(state));
        }
        for kind in [
            WorkItemKind::Task,
            WorkItemKind::CampaignCycle,
            WorkItemKind::Maintenance,
            WorkItemKind::MajorUpgrade,
            WorkItemKind::Release,
            WorkItemKind::Remediation,
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), kind.tag());
        }
        for lane in [
            WorkLane::Interactive,
            WorkLane::Campaign,
            WorkLane::Maintenance,
        ] {
            assert_eq!(serde_json::to_value(lane).unwrap(), lane.tag());
        }
    }

    #[test]
    fn an_unknown_state_tag_does_not_parse() {
        assert_eq!(WorkItemState::from_tag("nonsense"), None);
        assert_eq!(WorkItemState::from_tag(""), None);
        assert_eq!(WorkItemState::from_tag("Running"), None);
    }

    #[test]
    fn open_states_are_the_ones_holding_an_obligation() {
        assert!(WorkItemState::Preserved.is_open());
        assert!(WorkItemState::NeedsDecision.is_open());
        assert!(WorkItemState::Failed.is_open());
        assert!(!WorkItemState::Landed.is_open());
        assert!(!WorkItemState::Cancelled.is_open());
        assert!(!WorkItemState::Running.is_open());
    }

    // --- settlement mapping, one test per row ------------------------------
    //
    // Each row asserts the whole settled record, not just the state: the
    // reason, whether the ref was recorded as a landed commit or as
    // preserved work, the worktree, whether that worktree was gone, and the
    // trace the item stays correlated by. A row that only checked `state`
    // would pass while the disposition said something wrong.

    /// Every field a settlement row has to pin down, read off the item.
    #[derive(Debug, PartialEq, Eq)]
    struct Settled {
        state: WorkItemState,
        reason: String,
        landed_commit: Option<String>,
        preservation_ref: Option<String>,
        worktree: Option<String>,
        worktree_removed: Option<bool>,
        trace_id: Option<String>,
    }

    fn settled(item: &WorkItem) -> Settled {
        let d = item.disposition.clone().expect("a settled item has a disposition");
        Settled {
            state: item.state,
            reason: item.reason.clone(),
            landed_commit: d.landed_commit,
            preservation_ref: d.preservation_ref,
            worktree: d.worktree,
            worktree_removed: d.worktree_removed,
            trace_id: item.trace_id.clone(),
        }
    }

    const TRACE: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const WORKTREE: &str = "/tmp/worktrees/alpha/abc";

    #[test]
    fn complete_and_landed_settles_landed_against_its_commit() {
        let mut item = WorkItem::dispatched(spec(), now());
        item.settle_from_task_run(&run_result(TaskVerdict::Complete, true), Some(true), now());

        assert_eq!(
            settled(&item),
            Settled {
                state: WorkItemState::Landed,
                reason: "task summary".to_string(),
                landed_commit: Some("ref-or-commit".to_string()),
                preservation_ref: None,
                worktree: Some(WORKTREE.to_string()),
                worktree_removed: Some(true),
                trace_id: Some(TRACE.to_string()),
            }
        );
        assert_eq!(item.disposition.unwrap().verdict.as_deref(), Some("complete"));
    }

    /// The reviewer accepted the work but trunk moved under it: that is
    /// preserved work to reconcile, and the verdict stays `complete`.
    #[test]
    fn a_complete_whose_landing_was_blocked_settles_preserved_not_as_a_defect() {
        let mut item = WorkItem::dispatched(spec(), now());
        let mut result = run_result(TaskVerdict::Complete, false);
        result.land_blocked = Some(crate::payload::LandBlocked::TrunkMovedConflict);
        result.summary = "complete work preserved; land blocked (trunk_moved_conflict)".to_string();
        item.settle_from_task_run(&result, Some(true), now());

        assert_eq!(
            settled(&item),
            Settled {
                state: WorkItemState::Preserved,
                reason: "complete work preserved; land blocked (trunk_moved_conflict)".to_string(),
                landed_commit: None,
                preservation_ref: Some("ref-or-commit".to_string()),
                worktree: Some(WORKTREE.to_string()),
                worktree_removed: Some(true),
                trace_id: Some(TRACE.to_string()),
            }
        );
        assert_eq!(item.disposition.unwrap().verdict.as_deref(), Some("complete"));
    }

    #[test]
    fn a_landing_remainder_settles_landed_against_its_commit() {
        let mut item = WorkItem::dispatched(spec(), now());
        let verdict = TaskVerdict::Remainder {
            gaps: vec!["docs".to_string()],
        };
        item.settle_from_task_run(&run_result(verdict, true), Some(true), now());

        assert_eq!(
            settled(&item),
            Settled {
                state: WorkItemState::Landed,
                reason: "task summary".to_string(),
                landed_commit: Some("ref-or-commit".to_string()),
                preservation_ref: None,
                worktree: Some(WORKTREE.to_string()),
                worktree_removed: Some(true),
                trace_id: Some(TRACE.to_string()),
            }
        );
        assert_eq!(item.disposition.unwrap().verdict.as_deref(), Some("remainder"));
    }

    #[test]
    fn a_non_landing_remainder_settles_preserved_against_its_ref() {
        let mut item = WorkItem::dispatched(spec(), now());
        let verdict = TaskVerdict::Remainder {
            gaps: vec!["docs".to_string()],
        };
        item.settle_from_task_run(&run_result(verdict, false), Some(false), now());

        assert_eq!(
            settled(&item),
            Settled {
                state: WorkItemState::Preserved,
                reason: "task summary".to_string(),
                landed_commit: None,
                preservation_ref: Some("ref-or-commit".to_string()),
                worktree: Some(WORKTREE.to_string()),
                worktree_removed: Some(false),
                trace_id: Some(TRACE.to_string()),
            }
        );
        assert_eq!(item.disposition.unwrap().verdict.as_deref(), Some("remainder"));
    }

    #[test]
    fn a_defect_settles_preserved_with_its_diagnosis_as_the_reason() {
        let mut item = WorkItem::dispatched(spec(), now());
        let verdict = TaskVerdict::Defect {
            diagnosis: "gates went red\nand stayed red".to_string(),
        };
        item.settle_from_task_run(&run_result(verdict, false), Some(false), now());

        assert_eq!(
            settled(&item),
            Settled {
                state: WorkItemState::Preserved,
                reason: "gates went red and stayed red".to_string(),
                landed_commit: None,
                preservation_ref: Some("ref-or-commit".to_string()),
                worktree: Some(WORKTREE.to_string()),
                worktree_removed: Some(false),
                trace_id: Some(TRACE.to_string()),
            },
            "the reason must be the diagnosis, on one line"
        );
        assert_eq!(item.disposition.unwrap().verdict.as_deref(), Some("defect"));
    }

    #[test]
    fn blocked_on_decision_settles_needs_decision_with_its_finding() {
        let mut item = WorkItem::dispatched(spec(), now());
        let verdict = TaskVerdict::BlockedOnDecision {
            finding: "two schemas disagree".to_string(),
            options: vec!["a".to_string(), "b".to_string()],
        };
        item.settle_from_task_run(&run_result(verdict, false), Some(true), now());

        assert_eq!(
            settled(&item),
            Settled {
                state: WorkItemState::NeedsDecision,
                reason: "two schemas disagree".to_string(),
                landed_commit: None,
                preservation_ref: Some("ref-or-commit".to_string()),
                worktree: Some(WORKTREE.to_string()),
                worktree_removed: Some(true),
                trace_id: Some(TRACE.to_string()),
            }
        );
        assert_eq!(item.disposition.unwrap().verdict.as_deref(), Some("blocked_on_decision"));
    }

    #[test]
    fn a_runner_error_settles_failed_with_its_detail() {
        let mut item = WorkItem::dispatched(spec(), now());
        let verdict = TaskVerdict::RunnerError {
            detail: "task worktree already exists".to_string(),
        };
        let mut result = run_result(verdict, false);
        result.context.task_worktree = None;
        item.settle_from_task_run(&result, None, now());

        assert_eq!(
            settled(&item),
            Settled {
                state: WorkItemState::Failed,
                reason: "task worktree already exists".to_string(),
                landed_commit: None,
                preservation_ref: Some("ref-or-commit".to_string()),
                worktree: None,
                worktree_removed: None,
                trace_id: Some(TRACE.to_string()),
            },
            "a fault before the worktree existed records no worktree and no removal flag"
        );
        assert_eq!(item.disposition.unwrap().verdict.as_deref(), Some("runner_error"));
    }

    #[test]
    fn settling_failed_records_the_reason_and_the_time() {
        let at = now();
        let mut item = WorkItem::dispatched(spec(), at);
        item.settle_failed("daemon restarted", at);
        assert_eq!(item.state, WorkItemState::Failed);
        assert_eq!(item.reason, "daemon restarted");
        assert_eq!(item.settled_at, Some(at));
    }

    #[test]
    fn cancelling_records_the_operator_reason_verbatim_and_its_disposal() {
        let at = now();
        let mut item = WorkItem::dispatched(spec(), at);
        item.settle_cancelled(
            "superseded by the rewrite",
            Some(WorkDisposition {
                verdict: None,
                landed_commit: None,
                preservation_ref: Some("foundry-task/alpha-tidy-c3-abcdef".to_string()),
                worktree: Some(WORKTREE.to_string()),
                worktree_removed: Some(true),
            }),
            at,
        );

        assert_eq!(
            settled(&item),
            Settled {
                state: WorkItemState::Cancelled,
                reason: "superseded by the rewrite".to_string(),
                landed_commit: None,
                preservation_ref: Some("foundry-task/alpha-tidy-c3-abcdef".to_string()),
                worktree: Some(WORKTREE.to_string()),
                worktree_removed: Some(true),
                trace_id: Some(TRACE.to_string()),
            }
        );
        assert_eq!(item.settled_at, Some(at));
        assert!(!item.is_running());
        assert!(!WorkItemState::Cancelled.is_open(), "a cancellation holds no obligation");
    }

    #[test]
    fn cancelling_with_nothing_to_dispose_of_records_no_disposition() {
        let mut item = WorkItem::dispatched(spec(), now());
        item.settle_cancelled("stopping immediately", None, now());
        assert_eq!(item.state, WorkItemState::Cancelled);
        assert_eq!(item.reason, "stopping immediately");
        assert!(item.disposition.is_none());
    }

    #[test]
    fn an_empty_reason_still_says_something() {
        let mut item = WorkItem::dispatched(spec(), now());
        item.settle_failed("   ", now());
        assert_eq!(item.reason, "no reason recorded");
    }

    #[test]
    fn states_kinds_and_lanes_serialize_as_snake_case() {
        let cases = [
            (WorkItemState::Submitted, "submitted"),
            (WorkItemState::Queued, "queued"),
            (WorkItemState::Running, "running"),
            (WorkItemState::Landed, "landed"),
            (WorkItemState::Preserved, "preserved"),
            (WorkItemState::NeedsDecision, "needs_decision"),
            (WorkItemState::Failed, "failed"),
            (WorkItemState::Cancelled, "cancelled"),
        ];
        for (state, expected) in cases {
            assert_eq!(serde_json::to_value(state).unwrap(), expected);
        }
        let kinds = [
            (WorkItemKind::Task, "task"),
            (WorkItemKind::CampaignCycle, "campaign_cycle"),
            (WorkItemKind::Maintenance, "maintenance"),
            (WorkItemKind::MajorUpgrade, "major_upgrade"),
            (WorkItemKind::Release, "release"),
            (WorkItemKind::Remediation, "remediation"),
        ];
        for (kind, expected) in kinds {
            assert_eq!(serde_json::to_value(kind).unwrap(), expected);
        }
        let lanes = [
            (WorkLane::Interactive, "interactive"),
            (WorkLane::Campaign, "campaign"),
            (WorkLane::Maintenance, "maintenance"),
        ];
        for (lane, expected) in lanes {
            assert_eq!(serde_json::to_value(lane).unwrap(), expected);
        }
    }

    /// A run-shaped item of `kind` for `project`, running on `trace`.
    fn run_item(kind: WorkItemKind, project: &str, trace: Option<&str>) -> WorkItem {
        WorkItem::dispatched(
            WorkItemSpec {
                project: project.to_string(),
                objective: "a run".to_string(),
                kind,
                lane: WorkLane::Maintenance,
                origin: "maintenance cycle".to_string(),
                trace_id: trace.map(str::to_string),
            },
            now(),
        )
    }

    #[test]
    fn a_run_terminal_settles_only_its_own_kind_on_a_shared_trace_and_project() {
        let trace = "c".repeat(32);
        let mut store = WorkItemStore::default();
        for kind in [
            WorkItemKind::Maintenance,
            WorkItemKind::Remediation,
            WorkItemKind::Release,
        ] {
            store.upsert(run_item(kind, "alpha", Some(&trace)));
        }
        let ids: Vec<String> = store.items.iter().map(|item| item.id.clone()).collect();

        let settled = store
            .running_of_kind(Some(&trace), "alpha", WorkItemKind::Remediation)
            .expect("the remediation item is running on this trace");
        settled.settle_landed("fixed", now());

        assert_eq!(store.find(&ids[1]).unwrap().state, WorkItemState::Landed);
        assert!(store.find(&ids[0]).unwrap().is_running(), "the run itself is untouched");
        assert!(store.find(&ids[2]).unwrap().is_running(), "the release is untouched");
    }

    #[test]
    fn a_fan_out_siblings_terminal_settles_only_its_own_project() {
        let trace = "d".repeat(32);
        let mut store = WorkItemStore::default();
        store.upsert(run_item(WorkItemKind::Maintenance, "alpha", Some(&trace)));
        store.upsert(run_item(WorkItemKind::Maintenance, "beta", Some(&trace)));
        let ids: Vec<String> = store.items.iter().map(|item| item.id.clone()).collect();

        store
            .running_of_kind(Some(&trace), "beta", WorkItemKind::Maintenance)
            .unwrap()
            .settle_landed("done", now());

        assert!(store.find(&ids[0]).unwrap().is_running());
        assert_eq!(store.find(&ids[1]).unwrap().state, WorkItemState::Landed);
    }

    #[test]
    fn a_run_terminal_naming_a_trace_no_item_carries_settles_nothing() {
        let mut store = WorkItemStore::default();
        store.upsert(run_item(WorkItemKind::Maintenance, "alpha", Some(&"e".repeat(32))));
        assert!(
            store
                .running_of_kind(Some(&"f".repeat(32)), "alpha", WorkItemKind::Maintenance)
                .is_none()
        );
        assert!(store.running_of_kind(None, "alpha", WorkItemKind::Maintenance).is_none());
    }

    #[test]
    fn a_task_result_never_settles_a_run_shaped_item_sharing_its_trace() {
        let trace = "a".repeat(32);
        let mut store = WorkItemStore::default();
        store.upsert(run_item(WorkItemKind::Maintenance, "alpha", Some(&trace)));
        let run_id = store.items[0].id.clone();
        store.upsert(WorkItem::dispatched(spec(), now()));
        let task_id = store.items[1].id.clone();

        let settled = store
            .running_for_settlement(Some(&trace), "alpha")
            .expect("the task item is the one a task result settles");
        assert_eq!(settled.id, task_id);
        assert!(store.find(&run_id).unwrap().is_running());
    }

    #[test]
    fn the_trace_less_task_fallback_skips_run_shaped_items() {
        let mut store = WorkItemStore::default();
        store.upsert(run_item(WorkItemKind::Maintenance, "alpha", None));
        assert!(
            store.running_for_settlement(None, "alpha").is_none(),
            "no task-shaped item is running for this project"
        );
    }
}
