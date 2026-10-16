//! Durable cross-project intent, owner decisions and findings in the campaign store.

use std::collections::BTreeSet;
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::campaign::{CampaignStatus, CampaignStore, OwnerDecision, Transition};
use crate::gateway::StageProviders;

pub mod attention;
pub mod derive;
mod occupancy;
pub mod outcome;
pub use outcome::*;
pub mod precheck;
pub use precheck::{BlockedMember, SteeringPrecheck};
pub mod steering;
pub use steering::*;

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum InitiativeError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    AlreadyExists(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Precondition(String),
    #[error("{0}")]
    Persistence(String),
}

type Result<T> = std::result::Result<T, InitiativeError>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Initiative {
    pub name: String,
    pub outcome: String,
    /// Known creation time; legacy records have no inferred age.
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub intent_refs: Vec<String>,
    pub projects: Vec<String>,
    #[serde(default)]
    pub brief: Option<InitiativeBrief>,
    #[serde(default)]
    pub members: Vec<InitiativeMember>,
    pub done_evidence: Vec<DoneEvidence>,
    #[serde(default)]
    pub budget: InitiativeBudget,
    #[serde(default)]
    pub authorized_by: Option<String>,
    #[serde(default)]
    pub agent_provider: Option<String>,
    #[serde(default)]
    pub providers: StageProviders,
    #[serde(default)]
    pub status: InitiativeStatus,
    #[serde(default)]
    pub owner_decisions: Vec<InitiativeOwnerDecision>,
    #[serde(default)]
    pub open_questions: Vec<InitiativeQuestion>,
    /// Unanswered facts waiting for room on the bounded owner attention surface.
    #[serde(default)]
    pub deferred_questions: Vec<InitiativeQuestion>,
    #[serde(default)]
    pub derive_source: Option<DeriveSource>,
    #[serde(default)]
    pub provider_pairings: Vec<StageProviders>,
    #[serde(default)]
    pub holds: Vec<MemberHold>,
    #[serde(default)]
    pub milestones: Vec<MemberMilestone>,
    #[serde(default)]
    pub task_dispatches: Vec<TaskDispatch>,
    #[serde(default)]
    pub reservations: Vec<MemberReservation>,
    #[serde(default)]
    pub steering: Option<SteeringLease>,
    #[serde(default)]
    pub steering_history: Vec<SteeringRecord>,
    #[serde(default)]
    pub pending_actions: Vec<DurableSteeringAction>,
    #[serde(default)]
    pub close_proposals: Vec<EtCloseProposal>,
    #[serde(default)]
    pub steering_enabled: bool,
    #[serde(default)]
    pub intent_revision: String,
    #[serde(default)]
    pub work_admissions: Vec<WorkAdmission>,
    #[serde(default)]
    pub pairing_base_cycles: Option<u64>,
    #[serde(default)]
    pub integration_results: Vec<IntegrationGateResult>,
    #[serde(default)]
    pub completion_proposal: Option<CompletionProposal>,
    #[serde(default)]
    pub completion_confirmation: Option<CompletionConfirmation>,
    #[serde(default)]
    pub completion_receipts: Vec<EtCloseReceipt>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeBrief {
    pub path: String,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitiativeMember {
    pub kind: MemberKind,
    pub name: String,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub intent_ref: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub intent_state: Option<String>,
    /// The classified evidence that licenses activation, invalidated by revised intent.
    #[serde(default)]
    pub audit_finding_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberKind {
    Campaign,
    /// Intent awaiting the slice 1 audit and project assignment; never dispatched.
    Candidate,
    /// A standing slot whose actual work state lives in the work-item ledger.
    Task,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InitiativeBudget {
    pub max_cycles: u64,
    pub max_active_members: usize,
    pub max_open_questions: usize,
}

impl Default for InitiativeBudget {
    fn default() -> Self {
        Self {
            max_cycles: 20,
            max_active_members: 2,
            max_open_questions: 3,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitiativeStatus {
    #[default]
    Staged,
    Active,
    Escalated,
    Completed,
    Cancelled,
}

impl MemberKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Campaign => "campaign",
            Self::Candidate => "candidate",
            Self::Task => "task",
        }
    }
}

impl std::fmt::Display for MemberKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContractChanged => "contract_changed",
            Self::Regression => "regression",
            Self::Blocker => "blocker",
            Self::Evidence => "evidence",
            Self::OwnerNote => "owner_note",
        }
    }
}

impl std::fmt::Display for FindingKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl InitiativeStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }
}

impl std::fmt::Display for InitiativeStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Staged => "staged",
            Self::Active => "active",
            Self::Escalated => "escalated",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DoneEvidence {
    Gate {
        command: String,
        #[serde(default)]
        host: Option<String>,
        #[serde(default = "required")]
        required: bool,
        #[serde(default)]
        artifacts: Vec<String>,
    },
    Review {
        statement: String,
    },
}

fn required() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeOwnerDecision {
    pub decision: String,
    pub authorized_by: String,
    pub decided_at: DateTime<Utc>,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeQuestion {
    pub id: String,
    pub question: String,
    pub members: Vec<String>,
    pub opened_at: DateTime<Utc>,
    /// Derived from current campaign escalations; rebuilt rather than held as policy.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_members: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    ContractChanged,
    Regression,
    Blocker,
    Evidence,
    OwnerNote,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingSource {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindingConsumption {
    pub member: String,
    pub work_item: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub initiative: String,
    pub from: FindingSource,
    #[serde(default)]
    pub to: Vec<String>,
    pub kind: FindingKind,
    pub text: String,
    #[serde(default)]
    pub refs: Vec<String>,
    #[serde(default)]
    pub consumed_by: Vec<FindingConsumption>,
    #[serde(default = "Utc::now")]
    pub recorded_at: DateTime<Utc>,
    #[serde(default)]
    pub audit: Option<AuditEvidence>,
}

impl Finding {
    pub fn new(
        initiative: &str,
        from: FindingSource,
        to: Vec<String>,
        kind: FindingKind,
        text: &str,
        refs: Vec<String>,
    ) -> Self {
        Self {
            id: format!("fnd_{}", uuid::Uuid::new_v4()),
            initiative: initiative.into(),
            from,
            to,
            kind,
            text: text.into(),
            refs,
            consumed_by: vec![],
            recorded_at: Utc::now(),
            audit: None,
        }
    }
}

/// Snapshot retained alongside formation so changes invalidate stale output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitiativeContext {
    pub initiative: String,
    pub status: InitiativeStatus,
    pub budget: InitiativeBudget,
    pub agent_provider: Option<String>,
    pub brief: Option<InitiativeBrief>,
    pub providers: StageProviders,
    pub owner_decisions: Vec<InitiativeOwnerDecision>,
    pub findings: Vec<Finding>,
}

impl InitiativeContext {
    pub fn finding_ids(&self) -> Vec<String> {
        self.findings.iter().map(|f| f.id.clone()).collect()
    }
    pub fn render_findings(&self) -> String {
        let mut text = String::from("Findings from the initiative\n");
        for finding in &self.findings {
            use std::fmt::Write;
            let _ = writeln!(
                text,
                "{} ({:?}): {}\nReferences: {}",
                finding.id,
                finding.kind,
                finding.text,
                finding.refs.join(", ")
            );
        }
        text
    }
}

impl Initiative {
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty()
            || self.name == "."
            || self.name == ".."
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(InitiativeError::Invalid(
                "initiative name must contain only letters, digits, '.', '-' or '_'".into(),
            ));
        }
        if self.outcome.trim().is_empty()
            || self.projects.is_empty()
            || self.done_evidence.is_empty()
        {
            return Err(InitiativeError::Invalid(
                "initiative outcome, projects and done_evidence are required".into(),
            ));
        }
        if self.budget.max_cycles == 0
            || self.budget.max_active_members == 0
            || self.budget.max_open_questions == 0
        {
            return Err(InitiativeError::Invalid(
                "initiative budgets must be greater than zero".into(),
            ));
        }
        let mut names = BTreeSet::new();
        for member in &self.members {
            if member.name.trim().is_empty() || !names.insert(&member.name) {
                return Err(InitiativeError::Invalid(
                    "initiative members must have distinct non-empty names".into(),
                ));
            }
        }
        for member in &self.members {
            for after in &member.after {
                let prerequisite = after.split(':').next().unwrap_or(after);
                if prerequisite == member.name || !names.contains(&prerequisite.to_owned()) {
                    return Err(InitiativeError::Invalid(format!(
                        "unknown or self prerequisite '{after}' for member '{}'",
                        member.name
                    )));
                }
            }
        }
        for member in &self.members {
            let mut pending = member.after.clone();
            let mut visited = BTreeSet::new();
            while let Some(dependency) = pending.pop() {
                let dependency = dependency.split(':').next().unwrap_or(&dependency);
                if dependency == member.name {
                    return Err(InitiativeError::Invalid(format!(
                        "member '{}' has a dependency cycle",
                        member.name
                    )));
                }
                if visited.insert(dependency.to_owned())
                    && let Some(prerequisite) =
                        self.members.iter().find(|member| member.name == dependency)
                {
                    pending.extend(prerequisite.after.clone());
                }
            }
        }
        Ok(())
    }
}

impl CampaignStore {
    pub fn initiative_view(&self, name: &str) -> Result<Initiative> {
        self.require_initiative(name)?;
        let mut snapshot = self.clone();
        snapshot.refresh_questions(name)?;
        snapshot.require_initiative(name).cloned()
    }
    pub fn find_finding(&self, id: &str) -> Option<&Finding> {
        self.findings.iter().find(|f| f.id == id)
    }
    pub fn find_initiative(&self, name: &str) -> Option<&Initiative> {
        self.initiatives.iter().find(|i| i.name == name)
    }
    pub fn find_initiative_mut(&mut self, name: &str) -> Option<&mut Initiative> {
        self.initiatives.iter_mut().find(|i| i.name == name)
    }
    fn require_initiative(&self, name: &str) -> Result<&Initiative> {
        self.find_initiative(name)
            .ok_or_else(|| InitiativeError::NotFound(format!("initiative '{name}' not found")))
    }
    pub fn add_initiative(
        &mut self,
        mut initiative: Initiative,
        registered_projects: &[String],
    ) -> Result<()> {
        initiative.validate()?;
        if initiative.steering.is_some()
            || !initiative.open_questions.is_empty()
            || !initiative.deferred_questions.is_empty()
            || !initiative.steering_history.is_empty()
            || !initiative.pending_actions.is_empty()
            || !initiative.reservations.is_empty()
            || !initiative.task_dispatches.is_empty()
            || !initiative.work_admissions.is_empty()
            || !initiative.close_proposals.is_empty()
            || !initiative.milestones.is_empty()
            || !initiative.integration_results.is_empty()
            || initiative.completion_proposal.is_some()
            || initiative.completion_confirmation.is_some()
            || !initiative.completion_receipts.is_empty()
            || initiative.members.iter().any(|member| member.audit_finding_id.is_some())
        {
            return Err(InitiativeError::Invalid(
                "initiative definitions must omit durable steering records and evidence".into(),
            ));
        }
        if initiative.brief.is_some() {
            return Err(InitiativeError::Invalid(
                "initiative definitions must omit brief; import it with 'foundry initiative brief set'".into(),
            ));
        }
        if self.find_initiative(&initiative.name).is_some() {
            return Err(InitiativeError::AlreadyExists(format!(
                "initiative '{}' already exists",
                initiative.name
            )));
        }
        for project in &initiative.projects {
            if !registered_projects.contains(project) {
                return Err(InitiativeError::Invalid(format!(
                    "project '{project}' is not registered"
                )));
            }
        }
        for member in initiative.members.iter().filter(|m| m.kind == MemberKind::Campaign) {
            let campaign = self.find(&member.name).ok_or_else(|| {
                InitiativeError::Invalid(format!("member campaign '{}' not found", member.name))
            })?;
            if !initiative.projects.contains(&campaign.project) {
                return Err(InitiativeError::Invalid(format!(
                    "member '{}' project '{}' is outside the initiative",
                    member.name, campaign.project
                )));
            }
            if campaign.initiative.as_deref().is_some_and(|name| name != initiative.name) {
                return Err(InitiativeError::Invalid(format!(
                    "member '{}' already belongs to an initiative",
                    member.name
                )));
            }
        }
        for member in initiative.members.iter().filter(|m| m.kind == MemberKind::Campaign) {
            if let Some(campaign) = self.find_mut(&member.name) {
                campaign.initiative = Some(initiative.name.clone());
                campaign.budget.max_cycles = initiative.budget.max_cycles;
            }
        }
        initiative.created_at = Some(Utc::now());
        self.initiatives.push(initiative);
        Ok(())
    }
    fn applicable_members(&self, name: &str, members: &[String]) -> Result<Vec<String>> {
        let initiative = self.require_initiative(name)?;
        let all: Vec<_> = initiative.members.iter().map(|m| m.name.clone()).collect();
        if members.iter().any(|m| !all.contains(m)) {
            return Err(InitiativeError::Invalid(
                "decision or finding names a campaign outside the initiative".into(),
            ));
        }
        Ok(if members.is_empty() {
            all
        } else {
            let mut unique = Vec::new();
            for member in members {
                if !unique.contains(member) {
                    unique.push(member.clone());
                }
            }
            unique
        })
    }
    /// The owner and the members an owner decision applies to, or the reason
    /// the decision is refused: the initiative is unknown, terminal or
    /// unauthorized, the decision is empty, or it names an outside member.
    /// Writes nothing.
    fn decision_scope(
        &self,
        name: &str,
        decision: &str,
        members: &[String],
    ) -> Result<(String, Vec<String>)> {
        let initiative = self.require_initiative(name)?;
        if initiative.status.is_terminal() {
            return Err(InitiativeError::Precondition(format!("initiative '{name}' is terminal")));
        }
        let owner =
            initiative
                .authorized_by
                .clone()
                .filter(|o| !o.trim().is_empty())
                .ok_or_else(|| {
                    InitiativeError::Precondition(format!(
                        "initiative '{name}' has not been authorized"
                    ))
                })?;
        if decision.trim().is_empty() {
            return Err(InitiativeError::Invalid("decision must be non-empty".into()));
        }
        Ok((owner, self.applicable_members(name, members)?))
    }
    pub fn decide_initiative(
        &mut self,
        name: &str,
        decision: &str,
        members: &[String],
        at: DateTime<Utc>,
    ) -> Result<InitiativeOwnerDecision> {
        let (owner, applicable) = self.decision_scope(name, decision, members)?;
        self.refresh_questions(name)?;
        let record = InitiativeOwnerDecision {
            decision: decision.into(),
            authorized_by: owner.clone(),
            decided_at: at,
            members: applicable.clone(),
        };
        for member in &applicable {
            if let Some(campaign) = self.find_mut(member) {
                campaign.owner_decisions.push(OwnerDecision {
                    decision: decision.into(),
                    authorized_by: owner.clone(),
                    decided_at: at,
                });
                if campaign.status == CampaignStatus::Escalated
                    && campaign.cycles_completed < campaign.budget.max_cycles
                {
                    campaign.status = CampaignStatus::Active;
                    campaign.open_escalation = None;
                }
            }
        }
        if let Some(initiative) = self.find_initiative_mut(name) {
            initiative.owner_decisions.push(record.clone());
            for question in
                initiative.open_questions.iter_mut().chain(&mut initiative.deferred_questions)
            {
                question.members.retain(|m| !applicable.contains(m));
            }
            initiative.open_questions.retain(|q| !q.members.is_empty());
            initiative.deferred_questions.retain(|q| !q.members.is_empty());
            if initiative.open_questions.is_empty() {
                initiative.status = InitiativeStatus::Active;
            }
        }
        self.refresh_questions(name)?;
        Ok(record)
    }
    pub fn complete_initiative(
        &mut self,
        name: &str,
        reason: &str,
        at: DateTime<Utc>,
    ) -> Result<Transition> {
        let initiative = self.require_initiative(name)?;
        if initiative.status == InitiativeStatus::Completed {
            return Ok(Transition::AlreadySettled);
        }
        if initiative.status == InitiativeStatus::Cancelled {
            return Err(InitiativeError::Precondition(
                "a cancelled initiative cannot be recorded as complete".into(),
            ));
        }
        self.decide_initiative(name, reason, &[], at)?;
        let cancellation_reason = format!("initiative {name} completed: {reason}");
        for member in self.applicable_members(name, &[])? {
            if let Some(campaign) = self.find_mut(&member)
                && !campaign.status.is_terminal()
            {
                let _transition = campaign
                    .cancel(&cancellation_reason, at)
                    .map_err(|error| InitiativeError::Precondition(error.to_string()))?;
            }
        }
        if let Some(initiative) = self.find_initiative_mut(name) {
            initiative.status = InitiativeStatus::Completed;
            if let Some(confirmation) = &mut initiative.completion_confirmation {
                confirmation.last_error = None;
            }
            initiative.open_questions.clear();
            initiative.deferred_questions.clear();
            initiative.steering = None;
            initiative.pending_actions.clear();
            initiative.reservations.clear();
        }
        Ok(Transition::Applied)
    }
    pub fn cancel_initiative(
        &mut self,
        name: &str,
        reason: &str,
        at: DateTime<Utc>,
    ) -> Result<Transition> {
        let initiative = self.require_initiative(name)?;
        if initiative.status == InitiativeStatus::Cancelled {
            return Ok(Transition::AlreadySettled);
        }
        if initiative.status == InitiativeStatus::Completed {
            return Err(InitiativeError::Precondition(
                "a completed initiative cannot be cancelled".into(),
            ));
        }
        if reason.trim().is_empty() {
            return Err(InitiativeError::Invalid("cancellation reason must be non-empty".into()));
        }
        for member in self.applicable_members(name, &[])? {
            if let Some(campaign) = self.find_mut(&member)
                && !campaign.status.is_terminal()
            {
                let _transition = campaign
                    .cancel(reason, at)
                    .map_err(|error| InitiativeError::Precondition(error.to_string()))?;
            }
        }
        if let Some(initiative) = self.find_initiative_mut(name) {
            initiative.status = InitiativeStatus::Cancelled;
            initiative.open_questions.clear();
            initiative.deferred_questions.clear();
            initiative.steering = None;
            initiative.pending_actions.clear();
            initiative.reservations.clear();
        }
        Ok(Transition::Applied)
    }
    pub fn record_question(
        &mut self,
        name: &str,
        question: &str,
        members: &[String],
    ) -> Result<InitiativeQuestion> {
        self.record_question_inner(name, question, members, false)
    }

    /// Retain an execution's unanswered fact even when the attention surface is full.
    pub fn record_required_question(
        &mut self,
        name: &str,
        question: &str,
        members: &[String],
    ) -> Result<InitiativeQuestion> {
        self.record_question_inner(name, question, members, true)
    }

    fn record_question_inner(
        &mut self,
        name: &str,
        question: &str,
        members: &[String],
        required: bool,
    ) -> Result<InitiativeQuestion> {
        if question.trim().is_empty() {
            return Err(InitiativeError::Invalid("question must be non-empty".into()));
        }
        if self.require_initiative(name)?.status.is_terminal() {
            return Err(InitiativeError::Precondition(
                "terminal initiative cannot ask questions".into(),
            ));
        }
        let applicable = self.applicable_members(name, members)?;
        self.refresh_questions(name)?;
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        if let Some(existing) = initiative
            .open_questions
            .iter_mut()
            .chain(&mut initiative.deferred_questions)
            .find(|q| q.question.trim() == question.trim())
        {
            for member in applicable {
                if !existing.members.contains(&member) {
                    existing.members.push(member);
                }
            }
            existing.from_members = false;
            initiative.status = InitiativeStatus::Escalated;
            return Ok(existing.clone());
        }
        if !required && initiative.open_questions.len() >= initiative.budget.max_open_questions {
            return Err(InitiativeError::Precondition(
                "initiative is waiting on its owner; open question budget exhausted".into(),
            ));
        }
        let record = InitiativeQuestion {
            id: question_id(name, question),
            question: question.trim().into(),
            members: applicable,
            opened_at: Utc::now(),
            from_members: false,
        };
        if initiative.open_questions.len() < initiative.budget.max_open_questions {
            initiative.open_questions.push(record.clone());
        } else {
            initiative.deferred_questions.push(record.clone());
        }
        initiative.status = InitiativeStatus::Escalated;
        Ok(record)
    }

    /// Coalesce live facts, retaining their oldest age and enforcing the owner attention cap.
    /// Deferred facts still block their members and become visible as answers free slots.
    pub fn refresh_questions(&mut self, name: &str) -> Result<()> {
        if self.require_initiative(name)?.status.is_terminal() {
            return Ok(());
        }
        let prior: Vec<_> = self
            .require_initiative(name)?
            .open_questions
            .iter()
            .chain(&self.require_initiative(name)?.deferred_questions)
            .cloned()
            .collect();
        let members = self.applicable_members(name, &[])?;
        let has_progress = members.iter().filter_map(|member| self.find(member)).any(|campaign| {
            campaign.status == CampaignStatus::Active || campaign.cycles_completed > 0
        });
        let mut questions: Vec<_> =
            prior.iter().filter(|question| !question.from_members).cloned().collect();
        for member in members {
            let Some(campaign) = self.find(&member) else {
                continue;
            };
            if campaign.status != CampaignStatus::Escalated {
                continue;
            }
            let text = campaign.open_escalation.as_ref().map_or_else(
                || "Member requires an owner decision".into(),
                |escalation| {
                    escalation.question.clone().unwrap_or_else(|| escalation.reason.clone())
                },
            );
            let prior_question =
                prior.iter().find(|question| question.question.trim() == text.trim());
            let opened_at = campaign.open_escalation.as_ref().map_or_else(
                || prior_question.map_or_else(Utc::now, |question| question.opened_at),
                |escalation| escalation.escalated_at,
            );
            let opened_at =
                prior_question.map_or(opened_at, |question| question.opened_at.min(opened_at));
            if let Some(question) = questions.iter_mut().find(|q| q.question.trim() == text.trim())
            {
                if !question.members.contains(&member) {
                    question.members.push(member);
                }
                question.opened_at = question.opened_at.min(opened_at);
            } else {
                questions.push(InitiativeQuestion {
                    id: question_id(name, &text),
                    question: text.trim().into(),
                    members: vec![member],
                    opened_at,
                    from_members: true,
                });
            }
        }
        // Stable ordering prevents a refresh or restart from rotating the owner's questions.
        questions.sort_by(|left, right| {
            left.opened_at.cmp(&right.opened_at).then(left.id.cmp(&right.id))
        });
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        let deferred =
            questions.split_off(questions.len().min(initiative.budget.max_open_questions));
        initiative.open_questions = questions;
        initiative.deferred_questions = deferred;
        if !initiative.open_questions.is_empty() || initiative.completion_proposal.is_some() {
            initiative.status = InitiativeStatus::Escalated;
        } else if has_progress || initiative.status == InitiativeStatus::Active {
            initiative.status = InitiativeStatus::Active;
        } else {
            initiative.status = InitiativeStatus::Staged;
        }
        Ok(())
    }

    pub fn enforce_can_form(&self, member: &str, active_members: &[String]) -> Result<()> {
        let Some(campaign) = self.find(member) else {
            return Err(InitiativeError::NotFound(format!("campaign '{member}' not found")));
        };
        let Some(name) = &campaign.initiative else {
            return Ok(());
        };
        self.enforce_member_eligibility(name, member, active_members)?;
        Ok(())
    }
    /// Before dispatch, also enforce the initiative's shared cycle budget.
    /// A spent budget still permits read-only formation to assess landed evidence.
    pub fn enforce_can_advance(&self, member: &str, active_members: &[String]) -> Result<()> {
        self.enforce_can_form(member, active_members)?;
        let Some(name) = self.find(member).and_then(|campaign| campaign.initiative.as_ref()) else {
            return Ok(());
        };
        let initiative = self.require_initiative(name)?;
        let used = self.shared_cycles_used(name)?;
        if used >= initiative.budget.max_cycles {
            return Err(InitiativeError::Precondition(format!(
                "initiative '{name}' exhausted its cycle budget"
            )));
        }
        Ok(())
    }

    pub fn set_brief(&mut self, name: &str, home: &Path, content: &str) -> Result<InitiativeBrief> {
        self.require_initiative(name)?;
        let path = home.join("initiatives").join(name).join("BRIEF.md");
        let parent = path
            .parent()
            .ok_or_else(|| InitiativeError::Invalid("invalid brief path".into()))?;
        std::fs::create_dir_all(parent)
            .map_err(|e| InitiativeError::Persistence(format!("create brief directory: {e}")))?;
        let tmp = parent.join(format!(".BRIEF.{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, content)
            .and_then(|()| std::fs::rename(&tmp, &path))
            .map_err(|e| InitiativeError::Persistence(format!("write initiative brief: {e}")))?;
        let brief = InitiativeBrief {
            path: path.to_string_lossy().into_owned(),
            hash: hex::encode(Sha256::digest(content.as_bytes())),
        };
        if let Some(initiative) = self.find_initiative_mut(name) {
            initiative.brief = Some(brief.clone());
        }
        Ok(brief)
    }
    pub fn show_brief(&self, name: &str) -> Result<String> {
        let brief = self.require_initiative(name)?.brief.as_ref().ok_or_else(|| {
            InitiativeError::NotFound(format!("initiative '{name}' has no brief"))
        })?;
        let content = std::fs::read_to_string(&brief.path)
            .map_err(|e| InitiativeError::Persistence(format!("read initiative brief: {e}")))?;
        if hex::encode(Sha256::digest(content.as_bytes())) != brief.hash {
            return Err(InitiativeError::Precondition(
                "initiative brief content differs from its recorded hash".into(),
            ));
        }
        Ok(content)
    }
    pub fn add_finding(&mut self, finding: Finding) -> Result<Finding> {
        self.require_initiative(&finding.initiative)?;
        self.applicable_members(&finding.initiative, &finding.to)?;
        if finding.id.trim().is_empty()
            || finding.text.trim().is_empty()
            || finding.from.kind.trim().is_empty()
            || finding.from.id.trim().is_empty()
        {
            return Err(InitiativeError::Invalid(
                "finding id, text and source must be non-empty".into(),
            ));
        }
        if self.findings.iter().any(|f| f.id == finding.id) {
            return Err(InitiativeError::AlreadyExists(format!(
                "finding '{}' already exists",
                finding.id
            )));
        }
        self.findings.push(finding.clone());
        Ok(finding)
    }
    pub fn list_findings(&self, name: &str) -> Result<Vec<Finding>> {
        self.require_initiative(name)?;
        let mut findings: Vec<_> =
            self.findings.iter().filter(|f| f.initiative == name).cloned().collect();
        findings.sort_by_key(|f| std::cmp::Reverse(f.recorded_at));
        Ok(findings)
    }
    pub fn formation_context(
        &self,
        member: &str,
        max_bytes: usize,
    ) -> Result<Option<InitiativeContext>> {
        let campaign = self
            .find(member)
            .ok_or_else(|| InitiativeError::NotFound(format!("campaign '{member}' not found")))?;
        let Some(name) = &campaign.initiative else {
            return Ok(None);
        };
        let initiative = self.require_initiative(name)?;
        let mut selected = vec![];
        let mut bytes = 0;
        for finding in self.list_findings(name)? {
            if (!finding.to.is_empty() && !finding.to.iter().any(|m| m == member))
                || finding.consumed_by.iter().any(|c| c.member == member)
            {
                continue;
            }
            let size = serde_json::to_vec(&finding)
                .map_err(|e| InitiativeError::Invalid(e.to_string()))?
                .len();
            if bytes + size > max_bytes {
                continue;
            }
            bytes += size;
            selected.push(finding);
        }
        Ok(Some(InitiativeContext {
            initiative: name.clone(),
            status: initiative.status,
            budget: initiative.budget.clone(),
            agent_provider: initiative.agent_provider.clone(),
            brief: initiative.brief.clone(),
            providers: initiative.providers,
            owner_decisions: initiative.owner_decisions.clone(),
            findings: selected,
        }))
    }
    pub fn consume_findings(
        &mut self,
        name: &str,
        member: &str,
        work_item: &str,
        ids: &[String],
    ) -> Result<Vec<Finding>> {
        self.applicable_members(name, &[member.into()])?;
        let mut consumed = vec![];
        for finding in &mut self.findings {
            if finding.initiative == name
                && ids.contains(&finding.id)
                && (finding.to.is_empty() || finding.to.iter().any(|m| m == member))
                && !finding.consumed_by.iter().any(|c| c.member == member)
            {
                finding.consumed_by.push(FindingConsumption {
                    member: member.into(),
                    work_item: work_item.into(),
                });
                consumed.push(finding.clone());
            }
        }
        Ok(consumed)
    }
}

#[cfg(test)]
mod tests;

fn question_id(name: &str, question: &str) -> String {
    format!(
        "question_{}",
        hex::encode(Sha256::digest(format!("{name}:{}", question.trim()).as_bytes()))
    )
}
