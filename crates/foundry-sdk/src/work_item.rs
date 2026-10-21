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
//! Every engineering workflow is admitted `queued` with its root event.
//! The daemon scheduler starts it when [`crate::pacing`] allows. Downstream
//! work retains that slot until the traversal, including cleanup, completes.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::event::Event;
use crate::payload::{TaskRunCompletedPayload, TaskVerdict};
use crate::work_source::WorkSource;

/// Current work-item store format version. Bumped on schema-breaking changes.
pub const WORK_ITEM_STORE_VERSION: u32 = 1;

/// What sort of work a [`WorkItem`] represents.
///
/// Task kinds retain typed task verdicts. Maintenance, release, remediation
/// and general workflows share the same durable admission and scheduler. A
/// follow-up is never admitted to run: it records an obligation for a person.

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
    /// An admitted engineering workflow such as formation, validation or iteration.
    Workflow,
    /// An obligation a landed item left behind (see [`FollowUp`]). It never
    /// runs itself: it is settled open, and `foundry queue resume` admits a
    /// task that continues it.
    FollowUp,
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
/// person has to discharge. `Queued` and `Held` are *waiting*: the item has
/// not started, and the scheduler (for `Queued`) or an operator (for `Held`)
/// decides when it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemState {
    /// Admitted to the ledger, not yet queued.
    Submitted,
    /// Waiting for the scheduler to start it.
    Queued,
    /// Taken out of the scheduler's hands by an operator; `queue release`
    /// returns it to `Queued`.
    Held,
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

    /// Whether the item has been admitted but has not started: an operator
    /// may still cancel it, and nothing of its work exists yet.
    #[must_use]
    pub fn is_waiting(self) -> bool {
        matches!(self, Self::Submitted | Self::Queued | Self::Held)
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
            Self::Held => "held",
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
        Self::ALL.into_iter().find(|state| state.tag() == tag)
    }

    /// Every state, in declaration order.
    pub const ALL: [Self; 9] = [
        Self::Submitted,
        Self::Queued,
        Self::Held,
        Self::Running,
        Self::Landed,
        Self::Preserved,
        Self::NeedsDecision,
        Self::Failed,
        Self::Cancelled,
    ];
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
            Self::Workflow => "workflow",
            Self::FollowUp => "follow_up",
        }
    }
}

impl WorkLane {
    /// Every lane, in declaration order.
    pub const ALL: [Self; 3] = [Self::Interactive, Self::Campaign, Self::Maintenance];

    /// The serialized tag this lane is written to disk and to the wire as.
    #[must_use]
    pub fn tag(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Campaign => "campaign",
            Self::Maintenance => "maintenance",
        }
    }

    /// Parse a serialized lane tag, returning `None` for anything unknown.
    #[must_use]
    pub fn from_tag(tag: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|lane| lane.tag() == tag)
    }
}

/// Deletion evidence for an owned branch after its work reached trunk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchCleanup {
    /// Exact branch ref examined.
    pub reference: String,
    /// Commit observed before deletion, retained even after the ref is gone.
    pub commit: Option<String>,
    /// Whether this is the origin ref rather than the local ref.
    pub remote: bool,
    /// Whether the guarded deletion succeeded.
    pub deleted: bool,
    /// Why the ref was retained when cleanup could not succeed.
    pub error: Option<String>,
}

/// How a settled [`WorkItem`] ended.
///
/// The trace id is deliberately *not* repeated here: it is known at submission
/// and lives once, on [`WorkItem::trace_id`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkDisposition {
    /// The task branch owned by this item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_branch: Option<String>,
    /// Observed cleanup results, also carried by the settlement event.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub branch_cleanup: Vec<BranchCleanup>,
    /// The reviewer's typed verdict tag (see [`TaskVerdict::tag`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
    /// The trunk commit the work landed as, when it landed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landed_commit: Option<String>,
    /// The durable preservation ref (branch or `bundle:<path>`), retained
    /// as historical evidence after landing and any guarded branch deletion.
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
    /// `close`, `cancel`, `resume`, `hold` or `release`.
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

/// An obligation a landed item left behind, held by a [`WorkItemKind::FollowUp`]
/// item so it stays visible in the open group until someone discharges it.
///
/// A landing settles its own item `landed`, so whatever the landing still owes
/// (gaps the reviewer reported, say) would otherwise exist only in that run's
/// event payloads. The follow-up item records it in the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FollowUp {
    /// The landed item this obligation follows.
    pub of: String,
    /// The trunk commit the obligation refers to. Absent when none was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// What is owed, typed. Flattened, so the record carries an `obligation`
    /// tag beside the obligation's own fields.
    #[serde(flatten)]
    pub obligation: FollowUpObligation,
}

/// What a [`FollowUp`] owes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "obligation", rename_all = "snake_case")]
pub enum FollowUpObligation {
    /// The reviewer let the work land with a `remainder` verdict: these gaps
    /// are still to do.
    Remainder {
        /// The gaps, one line each, as the reviewer reported them.
        gaps: Vec<String>,
    },
}

impl FollowUpObligation {
    /// The serialized tag this obligation is written to disk and to the wire as.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Remainder { .. } => "remainder",
        }
    }

    /// The state a new follow-up holding this obligation opens in.
    #[must_use]
    pub fn opening_state(&self) -> WorkItemState {
        match self {
            Self::Remainder { .. } => WorkItemState::NeedsDecision,
        }
    }

    /// The objective a task that discharges this obligation runs: `parent`'s
    /// objective, then exactly what is still owed.
    #[must_use]
    pub fn objective(&self, parent: &WorkItem, commit: Option<&str>) -> String {
        let landed = commit.map_or_else(
            || format!("{} landed", parent.id),
            |commit| format!("{} landed as {commit}", parent.id),
        );
        match self {
            Self::Remainder { gaps } => {
                let mut objective = format!(
                    "{}\n\nRemaining gaps reported when {landed}; the rest of the objective above is on trunk:",
                    parent.objective.trim_end()
                );
                for gap in gaps {
                    objective.push_str("\n- ");
                    objective.push_str(&one_line(gap));
                }
                objective
            }
        }
    }

    /// The one-line reason a new follow-up holding this obligation opens with.
    #[must_use]
    pub fn reason(&self, parent_id: &str) -> String {
        match self {
            Self::Remainder { gaps } => {
                let first = gaps.first().map_or("", String::as_str);
                format!("remainder of {parent_id}: {} gap(s): {first}", gaps.len())
            }
        }
    }
}

impl FollowUp {
    /// The follow-up a task run's terminal result leaves behind, if any.
    ///
    /// Only a standalone task (`result.context.campaign` is `None`) that
    /// `landed` with a `remainder` verdict naming at least one nonblank gap
    /// leaves one: a campaign or initiative cycle carries its remainder into
    /// the campaign's next formation instead. Blank gaps are dropped and the
    /// rest are trimmed. `parent` is the item that result settled.
    #[must_use]
    pub fn from_landed_remainder(
        parent: &WorkItem,
        result: &TaskRunCompletedPayload,
    ) -> Option<Self> {
        if parent.state != WorkItemState::Landed || result.context.campaign.is_some() {
            return None;
        }
        let TaskVerdict::Remainder { gaps } = &result.verdict else {
            return None;
        };
        let gaps: Vec<String> = gaps
            .iter()
            .map(|gap| gap.trim())
            .filter(|gap| !gap.is_empty())
            .map(str::to_string)
            .collect();
        if gaps.is_empty() {
            return None;
        }
        let commit = parent
            .disposition
            .as_ref()
            .and_then(|disposition| disposition.landed_commit.clone())
            .filter(|commit| !commit.trim().is_empty());
        Some(Self {
            of: parent.id.clone(),
            commit,
            obligation: FollowUpObligation::Remainder { gaps },
        })
    }

    /// The open ledger item holding this follow-up of `parent`, settled at `at`
    /// in the state, with the objective and the reason the obligation implies.
    #[must_use]
    pub fn into_item(self, parent: &WorkItem, at: DateTime<Utc>) -> WorkItem {
        let objective = self.obligation.objective(parent, self.commit.as_deref());
        let reason = self.obligation.reason(&parent.id);
        let state = self.obligation.opening_state();
        WorkItem::follow_up(parent, self, objective, state, &reason, at)
    }
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
    /// What dispatched the work, typed: a campaign cycle, a sentinel, a person
    /// at a client, or a parent work item. Recorded at submission and never
    /// changed. Absent on records written before the source existed, which
    /// every reader treats as "not recorded".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<WorkSource>,
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
    /// Exact preserved item this task continues. When this item lands, that
    /// item settles `landed` too, transitively (see
    /// [`WorkItemStore::settle_lineage`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumes: Option<String>,
    /// Open items this task follows up on (`foundry task --continues`).
    /// Lineage only: it changes neither the task's base nor its objective.
    /// When this item lands, each one still open settles `landed` as
    /// superseded by it, and so on transitively through the lineage of every
    /// item settled that way (see [`WorkItemStore::settle_lineage`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub continues: Vec<String>,
    /// The obligation a [`WorkItemKind::FollowUp`] item holds. Absent on every
    /// other kind, and on records written before follow-ups existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up: Option<FollowUp>,
    /// Latest owner action. Absent on records predating owner controls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_action: Option<WorkItemOperatorAction>,
    /// Items this one waits on. It starts only once every one of them has
    /// settled `landed`; one that settles any other way moves this item to
    /// `needs_decision` instead (see [`crate::pacing`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// The earliest time the scheduler may start the item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<DateTime<Utc>>,
    /// The root event that starts the item, held while it waits. The
    /// scheduler builds the started root from it and clears it on start, so a
    /// queued item survives a daemon restart with everything it needs to run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_root: Option<Event>,
    /// The exact root dispatched by the scheduler, retained as admission proof.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_root: Option<Event>,
    /// Latest distinct steering trigger arriving during an admitted steer.
    /// Drained into one fresh queued steer after the current traversal ends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coalesced_root: Option<Event>,
    /// Additional projects whose repositories this workflow uses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reserved_projects: Vec<String>,
    /// Whether [`Self::reserved_projects`] are taken only if every one is
    /// free when the item starts. If one is busy then, the item starts with
    /// its own project's repository alone and the scheduler clears the
    /// reservation, so a running item's record names exactly what it holds
    /// (see [`crate::pacing`]). The scheduler clears this flag on every start.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reservation_if_free: bool,
    /// Whether the entire admitted traversal has ended, including cleanup.
    #[serde(default)]
    pub execution_completed: bool,
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
    /// The member campaign recorded on its admitted root, with legacy source fallback.
    pub fn campaign_name(&self) -> Option<&str> {
        self.started_root
            .as_ref()
            .or(self.pending_root.as_ref())
            .and_then(|root| root.payload.get("campaign"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                self.source
                    .as_ref()
                    .filter(|source| source.kind == crate::work_source::WorkSourceKind::Campaign)
                    .map(|source| source.reference.as_str())
            })
    }

    /// The cycle of the member campaign, independent of the initiative source.
    pub fn campaign_cycle(&self) -> Option<u64> {
        self.started_root
            .as_ref()
            .or(self.pending_root.as_ref())
            .and_then(|root| root.payload.get("campaign_cycle"))
            .and_then(serde_json::Value::as_u64)
            .or_else(|| {
                self.source
                    .as_ref()
                    .filter(|source| source.kind == crate::work_source::WorkSourceKind::Campaign)
                    .and_then(|source| source.cycle)
            })
    }

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
            source: None,
            submitted_at: at,
            started_at: None,
            settled_at: None,
            state: WorkItemState::Submitted,
            reason: "submitted".to_string(),
            trace_id: spec.trace_id,
            disposition: None,
            operator_action: None,
            resumes: None,
            continues: Vec::new(),
            follow_up: None,
            depends_on: Vec::new(),
            not_before: None,
            pending_root: None,
            started_root: None,
            reserved_projects: Vec::new(),
            reservation_if_free: false,
            execution_completed: false,
            coalesced_root: None,
        }
    }

    /// Move the item to `queued`.
    pub fn queue(&mut self) {
        self.state = WorkItemState::Queued;
        self.reason = "queued".to_string();
    }

    /// Admit `spec` to the ledger as `queued`, holding `root` to start it
    /// from. The caller sets the reason it waits with (see
    /// [`crate::pacing::evaluate`]).
    #[must_use]
    pub fn queued(spec: WorkItemSpec, root: Event, at: DateTime<Utc>) -> Self {
        let mut item = Self::submitted(spec, at);
        item.queue();
        item.pending_root = Some(root);
        item
    }

    /// Whether this item still occupies execution capacity. A typed task
    /// verdict can settle before its cleanup finishes; admission keeps the
    /// slot until the complete traversal returns.
    #[must_use]
    pub fn occupies_slot(&self) -> bool {
        self.is_running() || (self.started_root.is_some() && !self.execution_completed)
    }

    /// Move the item to `running`, dropping the held root. The scheduler
    /// retains the actual dispatched event in [`Self::started_root`].
    pub fn start(&mut self, at: DateTime<Utc>) {
        self.state = WorkItemState::Running;
        self.started_at = Some(at);
        self.reason = "running".to_string();
        self.pending_root = None;
    }

    /// Take a `queued` item out of the scheduler's hands.
    pub fn hold(&mut self) {
        self.state = WorkItemState::Held;
        self.reason = HELD_REASON.to_string();
    }

    /// Return a held item, or one the scheduler moved to `needs_decision` for
    /// a dependency, to `queued`. The caller recomputes the reason it waits
    /// with; `settled_at` is cleared because the item is unsettled again.
    pub fn release(&mut self) {
        self.state = WorkItemState::Queued;
        self.reason = "released".to_string();
        self.settled_at = None;
    }

    /// Admit, queue and start an item in one step.
    ///
    /// Compatibility constructor for already-running records and test fixtures.
    /// Daemon execution uses [`Self::queued`] and the shared scheduler instead.
    #[must_use]
    pub fn dispatched(spec: WorkItemSpec, at: DateTime<Utc>) -> Self {
        let mut item = Self::submitted(spec, at);
        item.queue();
        item.start(at);
        item
    }

    /// Settle a waiting item `needs_decision` with `reason`, before it ever
    /// started: a dependency settled some way other than `landed`, and only
    /// the owner can say whether the item should run regardless.
    pub fn settle_needs_decision(&mut self, reason: &str, at: DateTime<Utc>) {
        self.state = WorkItemState::NeedsDecision;
        self.reason = one_line(reason);
        self.settled_at = Some(at);
    }

    /// Open a [`WorkItemKind::FollowUp`] item holding `follow_up`, an
    /// obligation `parent` left behind when it landed.
    ///
    /// The item belongs to `parent`'s project and trace, sits in the
    /// interactive lane, names `parent` as its typed source and in its origin,
    /// and is settled `state` with `reason` at `at`. It never starts and holds
    /// no root: nothing schedules it. `foundry queue resume` admits a task that
    /// continues it, and that task's landing settles it (see
    /// [`WorkItemStore::settle_lineage`]).
    #[must_use]
    pub fn follow_up(
        parent: &Self,
        follow_up: FollowUp,
        objective: String,
        state: WorkItemState,
        reason: &str,
        at: DateTime<Utc>,
    ) -> Self {
        let mut item = Self::submitted(
            WorkItemSpec {
                project: parent.project.clone(),
                objective,
                kind: WorkItemKind::FollowUp,
                lane: WorkLane::Interactive,
                origin: format!("follow-up of {}", parent.id),
                trace_id: parent.trace_id.clone(),
            },
            at,
        )
        .with_source(Some(WorkSource::work_item(parent.id.clone())));
        item.state = state;
        item.reason = one_line(reason);
        item.settled_at = Some(at);
        item.follow_up = Some(follow_up);
        item
    }

    /// Record what dispatched the item (builder pattern).
    ///
    /// `None` leaves the source unrecorded, which is what an item whose
    /// dispatching event named no source carries.
    #[must_use]
    pub fn with_source(mut self, source: Option<WorkSource>) -> Self {
        self.source = source;
        self
    }

    /// Whether the item is still `running`.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.state == WorkItemState::Running
    }

    /// Whether the item is `queued`: admitted, not started, and the
    /// scheduler's to start.
    #[must_use]
    pub fn is_queued(&self) -> bool {
        self.state == WorkItemState::Queued
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
            TaskVerdict::BlockedOnRecovery { detail, .. } => {
                (WorkItemState::Preserved, detail.clone())
            }
            TaskVerdict::RunnerError { detail } => (WorkItemState::Failed, detail.clone()),
        };
        self.state = state;
        self.reason = one_line(&reason);
        self.settled_at = Some(at);
        self.disposition = Some(WorkDisposition {
            task_branch: result.context.task_branch.clone(),
            branch_cleanup: Vec::new(),
            verdict: Some(result.verdict.tag().to_string()),
            landed_commit: if result.landed {
                result.preservation_ref.clone()
            } else {
                None
            },
            preservation_ref: if result.landed {
                result.context.task_preservation_ref.clone()
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

/// The reason a held item carries. Stable text: an operator reading the ledger
/// sees the same phrase every time.
pub const HELD_REASON: &str = "held by operator";

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

    /// The follow-up item recorded for the landed item `of`, if there is one.
    #[must_use]
    pub fn follow_up_of(&self, of: &str) -> Option<&WorkItem> {
        self.items
            .iter()
            .find(|item| item.follow_up.as_ref().is_some_and(|follow_up| follow_up.of == of))
    }

    /// Every item still `running`.
    pub fn running(&self) -> impl Iterator<Item = &WorkItem> {
        self.items.iter().filter(|item| item.is_running())
    }

    /// Every item waiting for the scheduler (`queued`).
    pub fn queued(&self) -> impl Iterator<Item = &WorkItem> {
        self.items.iter().filter(|item| item.is_queued())
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
    /// would report the wrong unit of work as finished. The admission owner
    /// settles other workflows after the whole traversal ends.
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

    /// Settle `landed` everything the landing of `landed_id` discharges,
    /// following lineage transitively, and return the items settled, in
    /// settlement order (`landed_id` itself is not among them).
    ///
    /// The caller has already settled `landed_id` `landed` at `commit`. For it,
    /// and then for every item this call settles, in turn:
    ///
    /// - its [`WorkItem::resumes`] parent settles `landed` with reason
    ///   `resumed work landed` when it is `preserved` and has a disposition;
    /// - each item in its [`WorkItem::continues`] that is still open settles
    ///   `landed` with reason `superseded by <commit> via <id>`, naming the
    ///   item that listed it (a disposition is created if it has none).
    ///
    /// Every item settled records `commit` as its landed commit. Only items in
    /// `landed_id`'s project settle. Only open items ever settle, so a cycle
    /// in the recorded lineage terminates, and an item an owner already closed
    /// or cancelled stays exactly as it is. Preservation evidence is kept.
    ///
    /// Pure over the loaded store: callers apply it under
    /// [`ledger_write_gate`] to the ledger as loaded there, so an owner's
    /// close that reached the ledger first wins.
    pub fn settle_lineage(
        &mut self,
        landed_id: &str,
        commit: &str,
        at: DateTime<Utc>,
    ) -> Vec<WorkItem> {
        let Some(project) = self.find(landed_id).map(|item| item.project.clone()) else {
            return Vec::new();
        };
        let mut settled = Vec::new();
        let mut worklist = VecDeque::from([landed_id.to_string()]);
        while let Some(id) = worklist.pop_front() {
            let Some((resumes, continues)) =
                self.find(&id).map(|item| (item.resumes.clone(), item.continues.clone()))
            else {
                continue;
            };
            if let Some(parent) = resumes.as_deref().and_then(|parent| self.find_mut(parent))
                && parent.project == project
                && parent.state == WorkItemState::Preserved
                && let Some(disposition) = parent.disposition.as_mut()
            {
                disposition.landed_commit = Some(commit.to_string());
                parent.settle_landed(RESUMED_WORK_LANDED_REASON, at);
                worklist.push_back(parent.id.clone());
                settled.push(parent.clone());
            }
            let reason = format!("superseded by {commit} via {id}");
            for item in self.items.iter_mut().filter(|item| {
                continues.contains(&item.id) && item.project == project && item.state.is_open()
            }) {
                item.disposition.get_or_insert_with(WorkDisposition::default).landed_commit =
                    Some(commit.to_string());
                item.settle_landed(&reason, at);
                worklist.push_back(item.id.clone());
                settled.push(item.clone());
            }
        }
        settled
    }
}

/// The reason a preserved item settles `landed` with when the task that
/// resumed it lands.
const RESUMED_WORK_LANDED_REASON: &str = "resumed work landed";

/// Whether a task run's terminal result is what settles items of `kind`.
///
/// The task-shaped kinds all reach the engine through one root
/// (`ExecutionRequested`) and leave through one terminal
/// (`TaskRunCompleted`); the run-shaped kinds have their own roots and their
/// own terminals.
fn settles_from_task_run(kind: WorkItemKind) -> bool {
    match kind {
        WorkItemKind::Task | WorkItemKind::CampaignCycle | WorkItemKind::MajorUpgrade => true,
        WorkItemKind::Maintenance
        | WorkItemKind::Release
        | WorkItemKind::Remediation
        | WorkItemKind::Workflow
        | WorkItemKind::FollowUp => false,
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
            proof_evidence: None,
            uncommitted_artifacts: None,
            trunk_arrivals: Vec::new(),
            verdict,
            context: LoopContext {
                task_worktree: Some("/tmp/worktrees/alpha/abc".to_string()),
                task_branch: Some("foundry-task/abc".to_string()),
                ..LoopContext::default()
            },
            failure: crate::gateway::AgentFailureMetadata::default(),
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
    fn a_recorded_source_round_trips_through_the_file_and_an_absent_one_stays_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let mut store = WorkItemStore::default();
        let sourced = WorkItem::dispatched(spec(), now())
            .with_source(Some(WorkSource::campaign("tidy-cli", 3)));
        let unsourced = WorkItem::dispatched(spec(), now());
        store.upsert(sourced.clone());
        store.upsert(unsourced.clone());
        store.save(&path).unwrap();

        let loaded = WorkItemStore::load(&path).unwrap();
        assert_eq!(
            loaded.find(&sourced.id).unwrap().source,
            Some(WorkSource::campaign("tidy-cli", 3))
        );
        assert_eq!(loaded.find(&unsourced.id).unwrap().source, None);

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let records = json["items"].as_array().unwrap();
        assert_eq!(records[0]["source"]["kind"], "campaign");
        assert_eq!(records[0]["source"]["ref"], "tidy-cli");
        assert_eq!(records[0]["source"]["cycle"], 3);
        assert!(records[1].get("source").is_none(), "an unrecorded source writes no key");
    }

    #[test]
    fn a_record_written_before_the_source_existed_loads_with_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let mut store = WorkItemStore::default();
        store.upsert(WorkItem::dispatched(spec(), now()));
        let mut json: serde_json::Value = serde_json::to_value(&store).unwrap();
        json["items"][0].as_object_mut().unwrap().remove("source");
        std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        let loaded = WorkItemStore::load(&path).unwrap();
        assert_eq!(loaded.items[0].source, None, "absence reads as not recorded");
    }

    #[test]
    fn loading_a_missing_path_yields_an_empty_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkItemStore::load(&dir.path().join("absent.json")).unwrap();
        assert!(store.items.is_empty(), "expected `store.items` to be empty: {:?}", store.items);
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
    fn a_queued_item_holds_its_root_until_it_starts() {
        let root = Event::new(
            crate::event::EventType::ExecutionRequested,
            "alpha".to_string(),
            crate::throttle::Throttle::Full,
            serde_json::json!({"project": "alpha", "workflow": "task", "prompt": "x"}),
        );
        let at = now();
        let mut item = WorkItem::queued(spec(), root.clone(), at);
        assert_eq!(item.state, WorkItemState::Queued);
        assert_eq!(item.started_at, None);
        assert_eq!(item.pending_root.as_ref().map(|e| e.id.as_str()), Some(root.id.as_str()));

        item.start(at);
        assert_eq!(item.state, WorkItemState::Running);
        assert_eq!(item.started_at, Some(at));
        assert_eq!(item.pending_root, None, "a started item no longer needs its root");
    }

    #[test]
    fn hold_and_release_move_between_held_and_queued_without_a_start() {
        let mut item = WorkItem::queued(spec(), sample_root(), now());
        item.hold();
        assert_eq!(item.state, WorkItemState::Held);
        assert_eq!(item.reason, HELD_REASON);
        item.release();
        assert_eq!(item.state, WorkItemState::Queued);
        assert_eq!(item.started_at, None);
        assert!(item.pending_root.is_some(), "the root survives a hold");
    }

    #[test]
    fn a_dependency_decision_settles_a_waiting_item_and_release_unsettles_it() {
        let mut item = WorkItem::queued(spec(), sample_root(), now());
        item.settle_needs_decision("waits on wi_x, which settled failed", now());
        assert_eq!(item.state, WorkItemState::NeedsDecision);
        assert!(item.settled_at.is_some());
        assert_eq!(item.started_at, None);
        item.release();
        assert_eq!(item.state, WorkItemState::Queued);
        assert_eq!(item.settled_at, None);
    }

    #[test]
    fn waiting_states_are_the_ones_that_never_started() {
        assert!(WorkItemState::Submitted.is_waiting());
        assert!(WorkItemState::Queued.is_waiting());
        assert!(WorkItemState::Held.is_waiting());
        assert!(!WorkItemState::Running.is_waiting());
        assert!(!WorkItemState::NeedsDecision.is_waiting());
        assert!(!WorkItemState::Landed.is_waiting());
    }

    fn sample_root() -> Event {
        Event::new(
            crate::event::EventType::ExecutionRequested,
            "alpha".to_string(),
            crate::throttle::Throttle::Full,
            serde_json::json!({"project": "alpha", "workflow": "task", "prompt": "x"}),
        )
    }

    #[test]
    fn a_queued_item_round_trips_its_root_dependencies_and_not_before_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let mut item = WorkItem::queued(spec(), sample_root(), now());
        item.depends_on = vec!["wi_a".to_string()];
        item.not_before = Some(now());
        let mut store = WorkItemStore::default();
        store.upsert(item.clone());
        store.save(&path).unwrap();

        let loaded = WorkItemStore::load(&path).unwrap();
        assert_eq!(loaded.items, vec![item]);

        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(json["items"][0]["state"], "queued");
        assert_eq!(json["items"][0]["depends_on"], serde_json::json!(["wi_a"]));
        assert!(json["items"][0]["pending_root"].is_object());

        let plain = WorkItem::dispatched(spec(), now());
        let json = serde_json::to_value(&plain).unwrap();
        for absent in ["depends_on", "not_before", "pending_root"] {
            assert!(json.get(absent).is_none(), "{absent} writes no key when unset");
        }
    }

    #[test]
    fn every_state_kind_and_lane_tag_matches_its_serde_representation() {
        for state in WorkItemState::ALL {
            let serialized = serde_json::to_value(state).unwrap();
            assert_eq!(serialized, state.tag(), "tag must match serde for {state:?}");
            assert_eq!(WorkItemState::from_tag(state.tag()), Some(state));
        }
        assert_eq!(WorkItemState::from_tag("held"), Some(WorkItemState::Held));
        for kind in [
            WorkItemKind::Task,
            WorkItemKind::CampaignCycle,
            WorkItemKind::Maintenance,
            WorkItemKind::MajorUpgrade,
            WorkItemKind::Release,
            WorkItemKind::Remediation,
            WorkItemKind::Workflow,
            WorkItemKind::FollowUp,
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), kind.tag());
        }
        for lane in WorkLane::ALL {
            assert_eq!(serde_json::to_value(lane).unwrap(), lane.tag());
            assert_eq!(WorkLane::from_tag(lane.tag()), Some(lane));
        }
        assert_eq!(WorkLane::from_tag("all"), None);
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
    fn operational_blocker_preserves_work_without_owner_decision() {
        let mut item = WorkItem::dispatched(spec(), now());
        let verdict = TaskVerdict::BlockedOnRecovery {
            kind: crate::recovery::RecoveryKind::CapabilityUnavailable,
            detail: "macOS runner missing".into(),
        };
        item.settle_from_task_run(&run_result(verdict, false), Some(true), now());
        assert_eq!(item.state, WorkItemState::Preserved);
        assert_eq!(
            item.disposition.as_ref().unwrap().verdict.as_deref(),
            Some("blocked_on_recovery")
        );
        assert_eq!(
            item.disposition.as_ref().unwrap().preservation_ref.as_deref(),
            Some("ref-or-commit")
        );
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

    /// A refused continuation ran nothing and preserved nothing of its own:
    /// the item fails with the refusal, which names the base and the files,
    /// and claims no preservation ref. The continued work's own item keeps it.
    #[test]
    fn a_refused_continuation_settles_failed_naming_the_conflict_and_no_ref() {
        let mut item = WorkItem::dispatched(spec(), now());
        let refusal = "continuation refused before any agent session: the preserved work at \
                       foundry-task/a (abc123) is 3 commit(s) behind trunk (def456) and \
                       replaying it onto trunk conflicts in: README.md";
        let mut result = run_result(
            TaskVerdict::RunnerError {
                detail: refusal.to_string(),
            },
            false,
        );
        result.preservation_ref = None;
        result.land_blocked = Some(crate::payload::LandBlocked::ContinuationConflict);
        result.context.task_worktree = None;
        item.settle_from_task_run(&result, None, now());

        assert_eq!(item.state, WorkItemState::Failed);
        assert_eq!(item.reason, refusal);
        let disposition = item.disposition.unwrap();
        assert_eq!(disposition.preservation_ref, None);
        assert_eq!(disposition.verdict.as_deref(), Some("runner_error"));
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
    fn every_started_root_retains_ownership_until_explicit_completion() {
        let mut item = WorkItem::dispatched(spec(), now());
        item.started_root = Some(Event::new(
            crate::event::EventType::ExecutionRequested,
            item.project.clone(),
            crate::throttle::Throttle::Full,
            serde_json::json!({"workflow": "task"}),
        ));
        for state in [
            WorkItemState::Landed,
            WorkItemState::Preserved,
            WorkItemState::NeedsDecision,
            WorkItemState::Failed,
            WorkItemState::Cancelled,
        ] {
            item.state = state;
            item.reason = "daemon restarted".into();
            item.execution_completed = false;
            assert!(item.occupies_slot(), "unfinished root lost ownership in {state:?}");
            item.execution_completed = true;
            assert!(!item.occupies_slot(), "finished root retained ownership in {state:?}");
        }
    }

    #[test]
    fn cancelling_records_the_operator_reason_verbatim_and_its_disposal() {
        let at = now();
        let mut item = WorkItem::dispatched(spec(), at);
        item.settle_cancelled(
            "superseded by the rewrite",
            Some(WorkDisposition {
                task_branch: None,
                branch_cleanup: Vec::new(),
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
            (WorkItemKind::FollowUp, "follow_up"),
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

    // --- lineage settlement -------------------------------------------------

    fn lineage_item(id: &str, project: &str, state: WorkItemState) -> WorkItem {
        let mut item = WorkItem::dispatched(
            WorkItemSpec {
                project: project.to_string(),
                ..spec()
            },
            now(),
        );
        item.id = id.to_string();
        item.state = state;
        item.disposition = Some(WorkDisposition {
            preservation_ref: Some(format!("foundry-task/{id}")),
            ..WorkDisposition::default()
        });
        item
    }

    fn lineage_store(items: Vec<WorkItem>) -> WorkItemStore {
        WorkItemStore {
            version: WORK_ITEM_STORE_VERSION,
            items,
        }
    }

    fn settled_ids(settled: &[WorkItem]) -> Vec<&str> {
        settled.iter().map(|item| item.id.as_str()).collect()
    }

    #[test]
    fn lineage_settles_a_chain_of_resumes_and_continues_transitively() {
        // A <- continued by B <- resumed as C <- continued by D (landed).
        let a = lineage_item("wi_a", "alpha", WorkItemState::Failed);
        let mut b = lineage_item("wi_b", "alpha", WorkItemState::Preserved);
        b.continues = vec!["wi_a".to_string()];
        let mut c = lineage_item("wi_c", "alpha", WorkItemState::NeedsDecision);
        c.resumes = Some("wi_b".to_string());
        let mut d = lineage_item("wi_d", "alpha", WorkItemState::Landed);
        d.continues = vec!["wi_c".to_string()];
        let mut store = lineage_store(vec![a, b, c, d]);
        let at = now();

        let settled = store.settle_lineage("wi_d", "abc123", at);

        assert_eq!(settled_ids(&settled), vec!["wi_c", "wi_b", "wi_a"]);
        for (id, reason) in [
            ("wi_c", "superseded by abc123 via wi_d"),
            ("wi_b", "resumed work landed"),
            ("wi_a", "superseded by abc123 via wi_b"),
        ] {
            let item = store.find(id).unwrap();
            assert_eq!(item.state, WorkItemState::Landed, "{id}");
            assert_eq!(item.reason, reason, "{id}");
            assert_eq!(item.settled_at, Some(at), "{id}");
            let disposition = item.disposition.as_ref().unwrap();
            assert_eq!(disposition.landed_commit.as_deref(), Some("abc123"), "{id}");
            assert_eq!(
                disposition.preservation_ref.as_deref(),
                Some(format!("foundry-task/{id}").as_str()),
                "preservation evidence is kept for {id}"
            );
        }
        assert_eq!(
            settled,
            vec![
                store.find("wi_c").unwrap().clone(),
                store.find("wi_b").unwrap().clone(),
                store.find("wi_a").unwrap().clone(),
            ]
        );
    }

    #[test]
    fn lineage_creates_a_disposition_for_a_continued_item_that_has_none() {
        let mut a = lineage_item("wi_a", "alpha", WorkItemState::Failed);
        a.disposition = None;
        let mut b = lineage_item("wi_b", "alpha", WorkItemState::Landed);
        b.continues = vec!["wi_a".to_string()];
        let mut store = lineage_store(vec![a, b]);

        let settled = store.settle_lineage("wi_b", "abc123", now());

        assert_eq!(settled_ids(&settled), vec!["wi_a"]);
        let disposition = store.find("wi_a").unwrap().disposition.clone().unwrap();
        assert_eq!(disposition.landed_commit.as_deref(), Some("abc123"));
        assert_eq!(disposition.preservation_ref, None);
    }

    #[test]
    fn lineage_terminates_on_a_cycle_and_settles_each_item_once() {
        let mut a = lineage_item("wi_a", "alpha", WorkItemState::Preserved);
        a.continues = vec!["wi_b".to_string(), "wi_c".to_string()];
        let mut b = lineage_item("wi_b", "alpha", WorkItemState::Preserved);
        b.continues = vec!["wi_a".to_string(), "wi_c".to_string()];
        let mut c = lineage_item("wi_c", "alpha", WorkItemState::Landed);
        c.continues = vec!["wi_a".to_string()];
        c.resumes = Some("wi_b".to_string());
        let mut store = lineage_store(vec![a, b, c]);

        let settled = store.settle_lineage("wi_c", "abc123", now());

        assert_eq!(settled_ids(&settled), vec!["wi_b", "wi_a"]);
        assert_eq!(store.find("wi_b").unwrap().reason, "resumed work landed");
        assert_eq!(store.find("wi_a").unwrap().reason, "superseded by abc123 via wi_c");
        assert!(store.items.iter().all(|item| item.state == WorkItemState::Landed));
    }

    #[test]
    fn lineage_leaves_a_cancelled_item_and_what_lies_behind_it_alone() {
        let a = lineage_item("wi_a", "alpha", WorkItemState::Failed);
        let mut b = lineage_item("wi_b", "alpha", WorkItemState::Cancelled);
        b.continues = vec!["wi_a".to_string()];
        let mut c = lineage_item("wi_c", "alpha", WorkItemState::Landed);
        c.continues = vec!["wi_b".to_string()];
        let mut store = lineage_store(vec![a.clone(), b.clone(), c]);

        let settled = store.settle_lineage("wi_c", "abc123", now());

        assert!(settled.is_empty());
        assert_eq!(store.find("wi_b").unwrap(), &b, "an owner's cancel wins");
        assert_eq!(store.find("wi_a").unwrap(), &a, "nothing landed through it");
    }

    #[test]
    fn lineage_leaves_an_item_in_another_project_alone() {
        let other = lineage_item("wi_other", "beta", WorkItemState::Preserved);
        let mut c = lineage_item("wi_c", "alpha", WorkItemState::Landed);
        c.continues = vec!["wi_other".to_string()];
        c.resumes = Some("wi_other".to_string());
        let mut store = lineage_store(vec![other.clone(), c]);

        let settled = store.settle_lineage("wi_c", "abc123", now());

        assert!(settled.is_empty());
        assert_eq!(store.find("wi_other").unwrap(), &other);
    }

    #[test]
    fn lineage_leaves_a_resume_parent_that_is_not_preserved_or_has_no_disposition_alone() {
        let failed = lineage_item("wi_failed", "alpha", WorkItemState::Failed);
        let mut bare = lineage_item("wi_bare", "alpha", WorkItemState::Preserved);
        bare.disposition = None;
        let mut one = lineage_item("wi_one", "alpha", WorkItemState::Landed);
        one.resumes = Some("wi_failed".to_string());
        let mut two = lineage_item("wi_two", "alpha", WorkItemState::Landed);
        two.resumes = Some("wi_bare".to_string());
        let mut store = lineage_store(vec![failed.clone(), bare.clone(), one, two]);

        assert!(store.settle_lineage("wi_one", "abc123", now()).is_empty());
        assert!(store.settle_lineage("wi_two", "abc123", now()).is_empty());
        assert_eq!(store.find("wi_failed").unwrap(), &failed);
        assert_eq!(store.find("wi_bare").unwrap(), &bare);
    }

    #[test]
    fn lineage_of_an_unknown_item_settles_nothing() {
        let mut store = lineage_store(vec![lineage_item("wi_a", "alpha", WorkItemState::Failed)]);
        assert!(store.settle_lineage("wi_missing", "abc123", now()).is_empty());
    }

    // --- follow-ups ----------------------------------------------------------

    fn fixed(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_790_000_000 + seconds, 0).expect("in-range timestamp")
    }

    /// A standalone task that landed as `landedcommit` with `gaps` remaining.
    fn landed_remainder(gaps: &[&str]) -> (WorkItem, TaskRunCompletedPayload) {
        let mut parent = WorkItem::dispatched(spec(), fixed(0));
        parent.id = "wi_parent".to_string();
        let mut result = run_result(
            TaskVerdict::Remainder {
                gaps: gaps.iter().map(|gap| (*gap).to_string()).collect(),
            },
            true,
        );
        result.preservation_ref = Some("landedcommit".to_string());
        parent.settle_from_task_run(&result, Some(true), fixed(1));
        (parent, result)
    }

    #[test]
    fn a_landed_remainder_opens_a_follow_up_needing_a_decision_with_its_gaps() {
        let (parent, result) =
            landed_remainder(&["  Document the flag. ", "", "Add a test\nfor it."]);

        let follow_up = FollowUp::from_landed_remainder(&parent, &result).expect("a follow-up");
        assert_eq!(
            follow_up,
            FollowUp {
                of: "wi_parent".to_string(),
                commit: Some("landedcommit".to_string()),
                obligation: FollowUpObligation::Remainder {
                    gaps: vec![
                        "Document the flag.".to_string(),
                        "Add a test\nfor it.".to_string()
                    ],
                },
            }
        );
        let item = follow_up.clone().into_item(&parent, fixed(2));

        assert_eq!(item.kind, WorkItemKind::FollowUp);
        assert_eq!(item.lane, WorkLane::Interactive);
        assert_eq!(item.project, parent.project);
        assert_eq!(item.origin, "follow-up of wi_parent");
        assert_eq!(item.source, Some(WorkSource::work_item("wi_parent")));
        assert_eq!(item.trace_id, parent.trace_id);
        assert_eq!(item.state, WorkItemState::NeedsDecision);
        assert_eq!(item.reason, "remainder of wi_parent: 2 gap(s): Document the flag.");
        assert_eq!(item.submitted_at, fixed(2));
        assert_eq!(item.settled_at, Some(fixed(2)));
        assert_eq!(item.started_at, None, "a follow-up never starts");
        assert_eq!(item.pending_root, None, "nothing schedules it");
        assert_eq!(item.disposition, None);
        assert!(!item.occupies_slot());
        assert_eq!(item.follow_up, Some(follow_up));
        assert_eq!(
            item.objective,
            "Add a --quiet flag.\n\nRemaining gaps reported when wi_parent landed as landedcommit; \
             the rest of the objective above is on trunk:\n- Document the flag.\n- Add a test for it."
        );
    }

    #[test]
    fn only_a_standalone_landing_with_a_nonblank_gap_leaves_a_follow_up() {
        let (parent, result) = landed_remainder(&["one gap"]);
        assert!(FollowUp::from_landed_remainder(&parent, &result).is_some());

        let mut cycle = result.clone();
        cycle.context.campaign = Some("tidy-cli".to_string());
        assert_eq!(FollowUp::from_landed_remainder(&parent, &cycle), None, "a campaign carries it");

        let (blank, blank_result) = landed_remainder(&[" ", ""]);
        assert_eq!(FollowUp::from_landed_remainder(&blank, &blank_result), None);

        let (complete, mut complete_result) = landed_remainder(&["one gap"]);
        complete_result.verdict = TaskVerdict::Complete;
        assert_eq!(FollowUp::from_landed_remainder(&complete, &complete_result), None);

        let mut preserved = WorkItem::dispatched(spec(), fixed(0));
        let unlanded = run_result(
            TaskVerdict::Remainder {
                gaps: vec!["one gap".to_string()],
            },
            false,
        );
        preserved.settle_from_task_run(&unlanded, Some(true), fixed(1));
        assert_eq!(preserved.state, WorkItemState::Preserved);
        assert_eq!(FollowUp::from_landed_remainder(&preserved, &unlanded), None);
    }

    #[test]
    fn a_follow_up_without_a_commit_says_only_that_its_parent_landed() {
        let (mut parent, result) = landed_remainder(&["one gap"]);
        parent.disposition.as_mut().unwrap().landed_commit = None;
        let follow_up = FollowUp::from_landed_remainder(&parent, &result).unwrap();
        assert_eq!(follow_up.commit, None);
        assert!(
            follow_up
                .into_item(&parent, fixed(2))
                .objective
                .contains("when wi_parent landed;"),
            "no commit is named"
        );
    }

    #[test]
    fn a_follow_up_round_trips_with_a_flat_obligation_tag_and_older_records_read_without_one() {
        let (parent, result) = landed_remainder(&["one gap"]);
        let item = FollowUp::from_landed_remainder(&parent, &result)
            .unwrap()
            .into_item(&parent, fixed(2));

        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["kind"], "follow_up");
        assert_eq!(
            json["follow_up"],
            serde_json::json!({"of": "wi_parent", "commit": "landedcommit",
                "obligation": "remainder", "gaps": ["one gap"]})
        );
        assert_eq!(serde_json::from_value::<WorkItem>(json).unwrap(), item);

        let older = serde_json::to_value(&parent).unwrap();
        assert!(older.get("follow_up").is_none(), "an item without one writes no key");
        assert_eq!(serde_json::from_value::<WorkItem>(older).unwrap().follow_up, None);
    }

    #[test]
    fn the_store_finds_the_follow_up_recorded_for_a_landed_item() {
        let (parent, result) = landed_remainder(&["one gap"]);
        let item = FollowUp::from_landed_remainder(&parent, &result)
            .unwrap()
            .into_item(&parent, fixed(2));
        let id = item.id.clone();
        let store = lineage_store(vec![parent, item]);

        assert_eq!(
            store.follow_up_of("wi_parent").map(|found| found.id.as_str()),
            Some(id.as_str())
        );
        assert!(store.follow_up_of(&id).is_none(), "a follow-up follows its parent, not itself");
    }
}
