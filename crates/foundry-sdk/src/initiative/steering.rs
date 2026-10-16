//! Read-only steering decisions and durable, guarded admission into member work.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    CampaignStore, Finding, FindingKind, FindingSource, Initiative, InitiativeBrief,
    InitiativeError, MemberKind, OutcomeReview, Result, StageCampaignDefinition,
};
use crate::campaign::{Campaign, CampaignStatus};
use crate::gateway::{AgentProvider, StageProviders};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeriveSource {
    pub repo: String,
    pub from: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditClassification {
    Realised,
    Partial,
    Absent,
    Misplaced,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEvidence {
    pub intent_ref: String,
    pub classification: AuditClassification,
    pub project: String,
    pub paths: Vec<String>,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtCloseProposal {
    pub intent_ref: String,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberHold {
    pub member: String,
    #[serde(default)]
    pub until: Vec<String>,
    pub reason: String,
    /// An indefinite owner hold can only be released by an owner operation.
    #[serde(default)]
    pub owner: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberMilestone {
    pub member: String,
    pub name: String,
    pub evidence: Vec<String>,
}

/// A receipt counts dispatched tasks without copying their live ledger state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskDispatch {
    pub member: String,
    pub work_item: String,
    pub project: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReservationKind {
    Formation,
    Task,
}

/// Accepted work admission records the selected pair without copying live work state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkAdmission {
    pub action_id: String,
    pub member: String,
    pub providers: StageProviders,
    pub work_item: Option<String>,
}

/// Admission ownership, separate from campaign and ledger execution states.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberReservation {
    pub member: String,
    pub operation_id: String,
    pub kind: ReservationKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringLease {
    pub id: String,
    pub fingerprint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SteeringSnapshot {
    pub initiative: Initiative,
    pub id: String,
    pub fingerprint: String,
    /// Set when this pass is the follow-up of an earlier steering step. The
    /// commit records it on the steering record.
    #[serde(default)]
    pub follow_up_of: Option<SteeringFollowUp>,
    /// Why integration did not run this pass, when it was skipped. The commit
    /// records it on the steering record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_skipped: Option<String>,
}

/// What a steering record carries about its pass beside the decision.
#[derive(Debug, Clone, Default)]
struct PassNotes {
    follow_up_of: Option<SteeringFollowUp>,
    integration_skipped: Option<String>,
}

impl PassNotes {
    fn of(snapshot: &SteeringSnapshot) -> Self {
        Self {
            follow_up_of: snapshot.follow_up_of.clone(),
            integration_skipped: snapshot.integration_skipped.clone(),
        }
    }
}

/// Why a steering pass was requested as a follow-up: an earlier steer applied
/// a step that changed state but dispatched nothing while a member was
/// eligible. A follow-up pass never requests another.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringFollowUp {
    /// The steer whose step caused the follow-up.
    pub steer_id: String,
    /// That steer's decision tag, such as `relay` or `hold`.
    pub step: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum SteeringDecision {
    Advance {
        member: String,
        #[serde(default)]
        project: Option<String>,
    },
    Dispatch {
        project: String,
        objective: String,
        #[serde(default)]
        member: Option<String>,
    },
    Hold {
        member: String,
        until: Vec<String>,
        reason: String,
    },
    Relay {
        finding: Box<Finding>,
        to: Vec<String>,
    },
    Ask {
        question: String,
        #[serde(default)]
        members: Vec<String>,
    },
    Escalate {
        reason: String,
    },
    Stage {
        campaign: Box<StageCampaignDefinition>,
        #[serde(default)]
        member: Option<String>,
    },
    Done {
        reviews: Vec<OutcomeReview>,
    },
    /// Nothing should move until running work settles. Recorded with its
    /// reason; it opens no question and changes no initiative state.
    Wait {
        reason: String,
    },
}

/// What committing a steering decision produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteeringCommit {
    /// The decision changes initiative state; its durable action awaits application.
    Action(Box<DurableSteeringAction>),
    /// The agent chose `wait`. Only the steering record changed.
    Wait { reason: String },
    /// The decision is already in force: a hold on a member the owner already
    /// holds, or a relay to members the finding already reaches. Only the
    /// steering record changed.
    NoOp { reason: String },
}

impl SteeringCommit {
    /// The durable action, when the decision needs one.
    #[must_use]
    pub fn into_action(self) -> Option<DurableSteeringAction> {
        match self {
            Self::Action(action) => Some(*action),
            Self::Wait { .. } | Self::NoOp { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringRecord {
    pub id: String,
    pub prompt: String,
    pub decision: SteeringDecision,
    pub provider: Option<AgentProvider>,
    pub recorded_at: DateTime<Utc>,
    /// The earlier steer and step this pass followed up, when it was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follow_up_of: Option<SteeringFollowUp>,
    /// Why this pass ran without integration: it started without every
    /// initiative trunk, so integration results predate it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integration_skipped: Option<String>,
}

impl SteeringDecision {
    /// The decision's serialized tag, such as `relay` or `advance`.
    #[must_use]
    pub fn tag(&self) -> &'static str {
        match self {
            Self::Advance { .. } => "advance",
            Self::Dispatch { .. } => "dispatch",
            Self::Hold { .. } => "hold",
            Self::Relay { .. } => "relay",
            Self::Ask { .. } => "ask",
            Self::Escalate { .. } => "escalate",
            Self::Stage { .. } => "stage",
            Self::Done { .. } => "done",
            Self::Wait { .. } => "wait",
        }
    }

    /// Whether an applied step of this kind changes initiative state without
    /// dispatching work, so an eligible member would otherwise wait for the
    /// next settled fact. `done` hands the initiative to its owner and `wait`
    /// changes nothing, so neither qualifies.
    #[must_use]
    pub fn changes_state_without_dispatch(&self) -> bool {
        matches!(
            self,
            Self::Hold { .. }
                | Self::Relay { .. }
                | Self::Ask { .. }
                | Self::Escalate { .. }
                | Self::Stage { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringRepositoryIdentity {
    pub project: String,
    pub root: String,
    pub registry: String,
    pub trunk: String,
    pub head: String,
    pub status: String,
    pub diff: String,
}

/// Read-only evidence that must still match before a pending work action is admitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SteeringExternalContext {
    pub repositories: Vec<SteeringRepositoryIdentity>,
    pub derive_source: Option<DeriveSource>,
    pub intent: Option<String>,
    pub brief: Option<InitiativeBrief>,
    pub brief_content: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableSteeringAction {
    pub id: String,
    pub initiative: String,
    pub decision: SteeringDecision,
    pub providers: StageProviders,
    pub fingerprint: String,
    #[serde(default)]
    pub external_context: Option<SteeringExternalContext>,
}

impl CampaignStore {
    pub fn enable_steering(&mut self, name: &str) -> Result<()> {
        let initiative = self.require_initiative(name)?;
        require_authorized(initiative)?;
        self.find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?
            .steering_enabled = true;
        Ok(())
    }

    pub fn begin_steering(&mut self, name: &str, id: &str) -> Result<SteeringSnapshot> {
        self.claim_steering(name, id, true)
    }

    fn claim_steering(
        &mut self,
        name: &str,
        id: &str,
        require_enabled: bool,
    ) -> Result<SteeringSnapshot> {
        let initiative = self.require_initiative(name)?;
        require_authorized(initiative)?;
        if (require_enabled && !initiative.steering_enabled) || id.trim().is_empty() {
            return Err(InitiativeError::Precondition(
                "steering must be explicitly enabled and identify its operation".into(),
            ));
        }
        if initiative.steering_history.iter().any(|record| record.id == id) {
            return Err(InitiativeError::AlreadyExists(
                "steering operation already committed".into(),
            ));
        }
        if initiative.completion_proposal.is_some() {
            return Err(InitiativeError::Precondition(
                "done proposal awaits owner confirmation".into(),
            ));
        }
        if initiative.steering.is_some() || !initiative.pending_actions.is_empty() {
            return Err(InitiativeError::Precondition(
                "an initiative steering operation is already in flight".into(),
            ));
        }
        let fingerprint = self.steering_fingerprint(name)?;
        let snapshot = SteeringSnapshot {
            initiative: initiative.clone(),
            id: id.into(),
            fingerprint: fingerprint.clone(),
            follow_up_of: None,
            integration_skipped: None,
        };
        self.find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?
            .steering = Some(SteeringLease {
            id: id.into(),
            fingerprint,
        });
        Ok(snapshot)
    }

    /// Apply a typed owner operation without enabling automatic steering.
    pub fn commit_owner_action(
        &mut self,
        name: &str,
        id: &str,
        decision: SteeringDecision,
        prompt: String,
    ) -> Result<DurableSteeringAction> {
        let mut candidate = self.clone();
        let snapshot = candidate.claim_steering(name, id, false)?;
        candidate.validate_steering(&snapshot)?;
        let mut action = candidate.bind_steering_action(
            name,
            id,
            decision,
            prompt,
            None,
            PassNotes::default(),
        )?;
        if let SteeringDecision::Hold { member, .. } = &action.decision {
            if let Some(hold) = candidate.find_initiative_mut(name).and_then(|initiative| {
                initiative.holds.iter_mut().find(|hold| hold.member == *member)
            }) {
                hold.owner = true;
            }
            action.fingerprint = candidate.steering_fingerprint(name)?;
            if let Some(pending) = candidate.find_initiative_mut(name).and_then(|initiative| {
                initiative.pending_actions.iter_mut().find(|pending| pending.id == id)
            }) {
                pending.fingerprint.clone_from(&action.fingerprint);
            }
        }
        *self = candidate;
        Ok(action)
    }

    fn steering_fingerprint(&self, name: &str) -> Result<String> {
        let mut initiative = self.require_initiative(name)?.clone();
        initiative.steering = None;
        initiative.pending_actions.clear();
        let campaigns: Vec<_> = self
            .campaigns
            .iter()
            .filter(|campaign| campaign.initiative.as_deref() == Some(name))
            .collect();
        let findings: Vec<_> =
            self.findings.iter().filter(|finding| finding.initiative == name).collect();
        let bytes = serde_json::to_vec(&(initiative, campaigns, findings))
            .map_err(|error| InitiativeError::Persistence(error.to_string()))?;
        Ok(hex::encode(Sha256::digest(bytes)))
    }

    pub fn validate_steering(&self, snapshot: &SteeringSnapshot) -> Result<()> {
        let initiative = self.require_initiative(&snapshot.initiative.name)?;
        let matches = initiative.steering.as_ref().is_some_and(|lease| {
            lease.id == snapshot.id && lease.fingerprint == snapshot.fingerprint
        });
        if !matches || self.steering_fingerprint(&initiative.name)? != snapshot.fingerprint {
            return Err(InitiativeError::Precondition(
                "initiative changed while steering; discard the stale decision".into(),
            ));
        }
        require_authorized(initiative)
    }

    pub fn finish_steering(&mut self, name: &str, id: &str) -> Result<()> {
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        if initiative.steering.as_ref().is_some_and(|lease| lease.id == id) {
            initiative.steering = None;
        }
        Ok(())
    }

    /// Restart recovery uses authoritative operation identities, never elapsed time.
    pub fn recover_steering(&mut self, name: &str, live_operation_ids: &[String]) -> Result<()> {
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        if initiative
            .steering
            .as_ref()
            .is_some_and(|lease| !live_operation_ids.contains(&lease.id))
        {
            initiative.steering = None;
        }
        initiative.reservations.retain(|reservation| {
            live_operation_ids.contains(&reservation.operation_id)
                || initiative
                    .pending_actions
                    .iter()
                    .any(|action| action.id == reservation.operation_id)
        });
        Ok(())
    }

    /// Commit into a copy first, so a rejected action cannot leave audit or budget changes.
    ///
    /// A `wait`, and a decision already in force (see
    /// [`Self::steering_no_op`]), is recorded in the steering history and
    /// nothing else: no pending action, no reservation, no budget.
    pub fn commit_steering(
        &mut self,
        snapshot: &SteeringSnapshot,
        steer_id: &str,
        decision: SteeringDecision,
        audits: Vec<AuditEvidence>,
        prompt: String,
        provider: Option<AgentProvider>,
    ) -> Result<SteeringCommit> {
        if snapshot.id != steer_id {
            return Err(InitiativeError::Invalid("steering operation identity changed".into()));
        }
        self.validate_steering(snapshot)?;
        let mut candidate = self.clone();
        let name = &snapshot.initiative.name;
        if snapshot.initiative.derive_source.is_some()
            && matches!(
                decision,
                SteeringDecision::Advance { .. }
                    | SteeringDecision::Dispatch { .. }
                    | SteeringDecision::Stage { .. }
            )
        {
            for member in snapshot.initiative.members.iter().filter(|member| {
                member.intent_ref.is_some()
                    && member
                        .intent_state
                        .as_deref()
                        .is_none_or(|state| matches!(state, "create" | "update" | "delete"))
            }) {
                if !audits.iter().any(|audit| Some(&audit.intent_ref) == member.intent_ref.as_ref())
                    && !member.audit_finding_id.as_ref().is_some_and(|id| {
                        self.find_finding(id).is_some_and(|finding| {
                            finding.audit.as_ref().is_some_and(|audit| {
                                Some(&audit.intent_ref) == member.intent_ref.as_ref()
                            })
                        })
                    })
                {
                    return Err(InitiativeError::Precondition(
                        "derived candidates require a codebase audit before work advances".into(),
                    ));
                }
            }
        }
        candidate.record_audits(name, steer_id, audits)?;
        let committed = if let SteeringDecision::Wait { reason } = &decision {
            if reason.trim().is_empty() {
                return Err(InitiativeError::Invalid("wait requires a reason".into()));
            }
            let reason = reason.trim().to_owned();
            candidate.record_steering_decision(
                name,
                steer_id,
                decision,
                prompt,
                provider,
                PassNotes::of(snapshot),
            )?;
            SteeringCommit::Wait { reason }
        } else if let Some(reason) = candidate.steering_no_op(name, &decision)? {
            candidate.record_steering_decision(
                name,
                steer_id,
                decision,
                prompt,
                provider,
                PassNotes::of(snapshot),
            )?;
            SteeringCommit::NoOp { reason }
        } else {
            SteeringCommit::Action(Box::new(candidate.bind_steering_action(
                name,
                steer_id,
                decision,
                prompt,
                provider,
                PassNotes::of(snapshot),
            )?))
        };
        *self = candidate;
        Ok(committed)
    }

    /// Why a steering `hold` or `relay` would change nothing, or `None` when it
    /// would change initiative state (or be refused).
    ///
    /// A hold on a member the owner already holds keeps the owner hold: steering
    /// cannot replace it, and the member is already held. A hold identical to
    /// the member's current steering hold changes nothing either. A relay of a
    /// recorded finding to members it already reaches adds no recipient. A
    /// relay that would alter the finding's evidence is not a no-op; the commit
    /// refuses it.
    pub fn steering_no_op(
        &self,
        name: &str,
        decision: &SteeringDecision,
    ) -> Result<Option<String>> {
        let initiative = self.require_initiative(name)?;
        Ok(match decision {
            SteeringDecision::Hold { member, until, .. } => {
                initiative.holds.iter().find(|hold| hold.member == *member).and_then(|hold| {
                    if hold.owner {
                        Some(format!(
                            "member '{member}' already has an owner hold, which steering keeps: {}",
                            hold.reason
                        ))
                    } else {
                        (hold.until == *until)
                            .then(|| format!("member '{member}' is already held: {}", hold.reason))
                    }
                })
            }
            SteeringDecision::Relay { finding, to } => self
                .find_finding(&finding.id)
                .filter(|existing| {
                    existing.initiative == name
                        && existing.from == finding.from
                        && existing.kind == finding.kind
                        && existing.text == finding.text
                        && existing.refs == finding.refs
                        && existing.audit == finding.audit
                        && (existing.to.is_empty()
                            || to.is_empty()
                            || to.iter().all(|member| existing.to.contains(member)))
                })
                .map(|existing| {
                    let reach = if existing.to.is_empty() {
                        "every member".to_owned()
                    } else {
                        existing.to.join(", ")
                    };
                    format!("finding '{}' already reaches {reach}", existing.id)
                }),
            _ => None,
        })
    }

    /// Record a decision that needs no action, and release the steering lease.
    fn record_steering_decision(
        &mut self,
        name: &str,
        steer_id: &str,
        decision: SteeringDecision,
        prompt: String,
        provider: Option<AgentProvider>,
        notes: PassNotes,
    ) -> Result<()> {
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        initiative.steering_history.push(SteeringRecord {
            id: steer_id.into(),
            prompt,
            decision,
            provider,
            recorded_at: Utc::now(),
            follow_up_of: notes.follow_up_of,
            integration_skipped: notes.integration_skipped,
        });
        initiative.steering = None;
        Ok(())
    }

    fn bind_steering_action(
        &mut self,
        name: &str,
        steer_id: &str,
        decision: SteeringDecision,
        prompt: String,
        provider: Option<AgentProvider>,
        notes: PassNotes,
    ) -> Result<DurableSteeringAction> {
        if matches!(
            decision,
            SteeringDecision::Advance { .. }
                | SteeringDecision::Dispatch { .. }
                | SteeringDecision::Stage { .. }
        ) && self.require_initiative(name)?.derive_source.is_some()
        {
            for member in self.require_initiative(name)?.members.iter().filter(|member| {
                member.intent_ref.is_some()
                    && member
                        .intent_state
                        .as_deref()
                        .is_none_or(|state| matches!(state, "create" | "update" | "delete"))
            }) {
                if !member.audit_finding_id.as_ref().is_some_and(|id| {
                    self.find_finding(id).is_some_and(|finding| finding.audit.is_some())
                }) {
                    return Err(InitiativeError::Precondition(
                        "derived intent requires classified audit evidence before work advances"
                            .into(),
                    ));
                }
            }
        }
        let mut decision = decision;
        self.prepare_steering_action(name, steer_id, &mut decision)?;
        let reserved_task = u64::from(matches!(decision, SteeringDecision::Dispatch { .. }));
        let current_cycles = self.shared_cycles_used(name)?.saturating_sub(reserved_task);
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        let pairing_index = initiative
            .pairing_base_cycles
            .unwrap_or(current_cycles)
            .saturating_add(u64::try_from(initiative.work_admissions.len()).unwrap_or(u64::MAX));
        if !initiative.provider_pairings.is_empty()
            && matches!(
                decision,
                SteeringDecision::Advance { .. } | SteeringDecision::Dispatch { .. }
            )
        {
            initiative.pairing_base_cycles.get_or_insert(current_cycles);
        }
        let providers = if initiative.provider_pairings.is_empty() {
            initiative.providers
        } else {
            let index = usize::try_from(
                pairing_index
                    % u64::try_from(initiative.provider_pairings.len()).unwrap_or(u64::MAX),
            )
            .map_err(|error| InitiativeError::Invalid(error.to_string()))?;
            initiative.provider_pairings[index].inherit(&initiative.providers)
        };
        let action = DurableSteeringAction {
            id: steer_id.into(),
            initiative: name.into(),
            decision: decision.clone(),
            providers,
            fingerprint: String::new(),
            external_context: None,
        };
        initiative.steering_history.push(SteeringRecord {
            id: steer_id.into(),
            prompt,
            decision,
            provider,
            recorded_at: Utc::now(),
            follow_up_of: notes.follow_up_of,
            integration_skipped: notes.integration_skipped,
        });
        initiative.pending_actions.push(action.clone());
        initiative.steering = None;
        let mut action = action;
        action.fingerprint = self.steering_fingerprint(name)?;
        if let Some(pending) = self
            .find_initiative_mut(name)
            .and_then(|initiative| initiative.pending_actions.last_mut())
        {
            pending.fingerprint.clone_from(&action.fingerprint);
        }
        Ok(action)
    }

    fn record_audits(
        &mut self,
        name: &str,
        steer_id: &str,
        audits: Vec<AuditEvidence>,
    ) -> Result<()> {
        for (index, audit) in audits.into_iter().enumerate() {
            let initiative = self.require_initiative(name)?;
            if !initiative.intent_refs.contains(&audit.intent_ref)
                || !initiative.projects.contains(&audit.project)
                || audit.explanation.trim().is_empty()
                || audit.paths.is_empty()
                || audit.paths.iter().any(|path| path.trim().is_empty())
            {
                return Err(InitiativeError::Invalid(
                    "audit evidence must identify initiative intent, project and supporting paths"
                        .into(),
                ));
            }
            let targets: Vec<_> = initiative
                .members
                .iter()
                .filter(|member| member.intent_ref.as_ref() == Some(&audit.intent_ref))
                .map(|member| member.name.clone())
                .collect();
            let current_targets: Vec<_> = initiative
                .members
                .iter()
                .filter(|member| {
                    member.intent_ref.as_ref() == Some(&audit.intent_ref)
                        && member
                            .intent_state
                            .as_deref()
                            .is_none_or(|state| matches!(state, "create" | "update" | "delete"))
                })
                .map(|member| member.name.clone())
                .collect();
            let mut finding = Finding::new(
                name,
                FindingSource {
                    kind: "initiative_steering".into(),
                    id: steer_id.into(),
                },
                targets.clone(),
                FindingKind::Evidence,
                &audit.explanation,
                audit.paths.clone(),
            );
            finding.id = format!("fnd_{steer_id}_audit_{index}");
            finding.audit = Some(audit.clone());
            let finding_id = finding.id.clone();
            // Candidate targets are valid intent recipients before activation.
            self.findings.push(finding);
            if let Some(initiative) = self.find_initiative_mut(name) {
                for member in &mut initiative.members {
                    if current_targets.contains(&member.name) {
                        member.audit_finding_id = Some(finding_id.clone());
                    }
                }
            }
            if audit.classification == AuditClassification::Realised && !current_targets.is_empty()
            {
                for member in &current_targets {
                    self.record_milestone(name, member, "landed", audit.paths.clone())?;
                }
                let initiative = self
                    .find_initiative_mut(name)
                    .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
                if !initiative
                    .close_proposals
                    .iter()
                    .any(|proposal| proposal.intent_ref == audit.intent_ref)
                {
                    initiative.close_proposals.push(EtCloseProposal {
                        intent_ref: audit.intent_ref,
                        evidence: audit.paths,
                    });
                }
            }
        }
        Ok(())
    }

    fn prepare_steering_action(
        &mut self,
        name: &str,
        id: &str,
        decision: &mut SteeringDecision,
    ) -> Result<()> {
        match decision {
            SteeringDecision::Advance { member, project } => {
                if self
                    .require_initiative(name)?
                    .members
                    .iter()
                    .any(|m| m.name == *member && m.kind == MemberKind::Candidate)
                {
                    let project = project.as_deref().ok_or_else(|| {
                        InitiativeError::Invalid(
                            "candidate advance requires a project assignment".into(),
                        )
                    })?;
                    self.activate_candidate_campaign(name, member, project)?;
                }
                self.grant_member_budget(member, &[])?;
                self.reserve_member(name, member, id, ReservationKind::Formation)?;
            }
            SteeringDecision::Dispatch {
                project,
                objective,
                member,
            } => self.prepare_task_action(name, id, project, objective, member)?,
            SteeringDecision::Hold {
                member,
                until,
                reason,
            } => {
                self.set_member_hold(name, member, until, reason, false)?;
            }
            SteeringDecision::Relay { finding, to } => self.relay_finding(name, finding, to)?,
            SteeringDecision::Ask { question, members } => {
                self.record_question(name, question, members)?;
            }
            SteeringDecision::Escalate { reason } => {
                if reason.trim().is_empty() {
                    return Err(InitiativeError::Invalid(
                        "escalation reason must be non-empty".into(),
                    ));
                }
                self.refresh_questions(name)?;
                let initiative = self.require_initiative(name)?;
                if initiative.open_questions.len() < initiative.budget.max_open_questions
                    || initiative
                        .open_questions
                        .iter()
                        .chain(&initiative.deferred_questions)
                        .any(|question| question.question.trim() == reason.trim())
                {
                    self.record_question(name, reason, &[])?;
                } else {
                    self.find_initiative_mut(name)
                        .ok_or_else(|| InitiativeError::NotFound(name.into()))?
                        .status = super::InitiativeStatus::Escalated;
                }
            }
            SteeringDecision::Stage { campaign, member } => {
                self.stage_initiative_campaign(name, campaign, member.as_deref())?;
            }
            SteeringDecision::Done { reviews } => self.validate_outcome_reviews(name, reviews)?,
            SteeringDecision::Wait { .. } => {
                // Defensive: commit_steering records a wait without binding an action,
                // and no owner operation produces one.
                return Err(InitiativeError::Invalid(
                    "wait is a steering record, not an action".into(),
                ));
            }
        }
        Ok(())
    }

    fn prepare_task_action(
        &mut self,
        name: &str,
        id: &str,
        project: &str,
        objective: &str,
        member: &mut Option<String>,
    ) -> Result<()> {
        if objective.trim().is_empty()
            || !self
                .require_initiative(name)?
                .projects
                .iter()
                .any(|candidate| candidate == project)
        {
            return Err(InitiativeError::Invalid(
                "task objective and an initiative project are required".into(),
            ));
        }
        let slot = member.clone().unwrap_or_else(|| format!("task_{id}"));
        if let Some(definition) =
            self.require_initiative(name)?.members.iter().find(|m| m.name == slot)
        {
            if definition
                .intent_state
                .as_deref()
                .is_some_and(|state| !matches!(state, "create" | "update" | "delete"))
            {
                return Err(InitiativeError::Precondition(
                    "member is no longer approved intent".into(),
                ));
            }
            if definition.kind == MemberKind::Candidate
                && self.require_initiative(name)?.derive_source.is_some()
                && !definition.audit_finding_id.as_ref().is_some_and(|id| {
                    self.find_finding(id).is_some_and(|finding| {
                        finding.audit.as_ref().is_some_and(|audit| {
                            Some(&audit.intent_ref) == definition.intent_ref.as_ref()
                        })
                    })
                })
            {
                return Err(InitiativeError::Precondition(
                    "candidate dispatch requires current classified audit evidence".into(),
                ));
            }
            if definition.kind == MemberKind::Campaign {
                return Err(InitiativeError::Invalid(
                    "dispatch requires a candidate or task slot".into(),
                ));
            }
            if definition.kind == MemberKind::Candidate
                && self
                    .require_initiative(name)?
                    .close_proposals
                    .iter()
                    .any(|proposal| Some(&proposal.intent_ref) == definition.intent_ref.as_ref())
            {
                return Err(InitiativeError::Precondition(
                    "realised intent awaits an owner close proposal, not new work".into(),
                ));
            }
        } else if member.is_some() {
            return Err(InitiativeError::NotFound(format!("candidate '{slot}' not found")));
        }
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        if let Some(definition) = initiative.members.iter_mut().find(|m| m.name == slot) {
            definition.kind = MemberKind::Task;
        } else {
            initiative.members.push(super::InitiativeMember {
                kind: MemberKind::Task,
                name: slot.clone(),
                after: vec![],
                intent_ref: None,
                description: Some(objective.into()),
                intent_state: None,
                audit_finding_id: None,
            });
        }
        *member = Some(slot.clone());
        self.enforce_task_dispatch(name, &slot, &[])?;
        self.reserve_member(name, &slot, id, ReservationKind::Task)?;
        Ok(())
    }

    fn relay_finding(&mut self, name: &str, finding: &mut Finding, to: &[String]) -> Result<()> {
        if finding.initiative != name {
            return Err(InitiativeError::Invalid(
                "relayed finding belongs to another initiative".into(),
            ));
        }
        self.applicable_members(name, to)?;
        if let Some(existing) = self.findings.iter_mut().find(|existing| existing.id == finding.id)
        {
            if existing.initiative != name
                || existing.from != finding.from
                || existing.kind != finding.kind
                || existing.text != finding.text
                || existing.refs != finding.refs
                || existing.audit != finding.audit
            {
                return Err(InitiativeError::Invalid(
                    "relay cannot replace a finding's recorded evidence or provenance".into(),
                ));
            }
            if !existing.to.is_empty() {
                for member in to {
                    if !existing.to.contains(member) {
                        existing.to.push(member.clone());
                    }
                }
            }
        } else {
            finding.to = to.to_vec();
            self.add_finding(finding.clone())?;
        }
        Ok(())
    }

    pub fn pending_steering_action(
        &self,
        name: &str,
        id: &str,
    ) -> Result<Option<&DurableSteeringAction>> {
        Ok(self
            .require_initiative(name)?
            .pending_actions
            .iter()
            .find(|action| action.id == id))
    }

    pub fn pending_action_is_current(&self, name: &str, id: &str) -> Result<bool> {
        let Some(action) = self.pending_steering_action(name, id)? else {
            return Ok(false);
        };
        Ok(!self.require_initiative(name)?.status.is_terminal()
            && self.steering_fingerprint(name)? == action.fingerprint)
    }

    /// Bind the checked external evidence to the exact pending action before saving it.
    /// Replays may repeat the same binding, but cannot replace its original evidence.
    pub fn bind_external_context(
        &mut self,
        name: &str,
        id: &str,
        context: SteeringExternalContext,
    ) -> Result<DurableSteeringAction> {
        let action = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?
            .pending_actions
            .iter_mut()
            .find(|action| action.id == id)
            .ok_or_else(|| InitiativeError::NotFound("steering action no longer pending".into()))?;
        if action.external_context.as_ref().is_some_and(|original| original != &context) {
            return Err(InitiativeError::Precondition(
                "pending action external evidence is already bound".into(),
            ));
        }
        action.external_context = Some(context);
        Ok(action.clone())
    }

    /// Release an admission slot after its authoritative ledger item settles.
    pub fn release_member_reservation(&mut self, name: &str, operation_id: &str) -> Result<()> {
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        initiative
            .reservations
            .retain(|reservation| reservation.operation_id != operation_id);
        Ok(())
    }

    /// Release every admission reservation held by one of `work_items`, in any
    /// initiative, and report whether one was released.
    ///
    /// A queued formation item that is cancelled never runs, so its completion
    /// never releases the slot it reserved. The cancel path calls this instead.
    pub fn release_reservations_held_by(&mut self, work_items: &[String]) -> bool {
        let mut released = false;
        for initiative in &mut self.initiatives {
            let before = initiative.reservations.len();
            initiative
                .reservations
                .retain(|reservation| !work_items.contains(&reservation.operation_id));
            released |= initiative.reservations.len() != before;
        }
        released
    }

    pub fn enforce_steering_action(
        &self,
        name: &str,
        id: &str,
        active_members: &[String],
    ) -> Result<()> {
        let action = self
            .pending_steering_action(name, id)?
            .ok_or_else(|| InitiativeError::NotFound("steering action no longer pending".into()))?;
        require_authorized(self.require_initiative(name)?)?;
        if self.steering_fingerprint(name)? != action.fingerprint {
            return Err(InitiativeError::Precondition(
                "initiative changed after steering; discard the stale action".into(),
            ));
        }
        match &action.decision {
            SteeringDecision::Advance { member, .. } => {
                self.enforce_can_advance(member, active_members)
            }
            SteeringDecision::Dispatch {
                member: Some(member),
                ..
            } => self.enforce_task_dispatch(name, member, active_members),
            _ => Ok(()),
        }
    }

    pub fn acknowledge_steering_action(
        &mut self,
        name: &str,
        id: &str,
        work_item: Option<&str>,
    ) -> Result<()> {
        let action = self.pending_steering_action(name, id)?.cloned();
        let Some(action) = action else {
            return Ok(());
        };
        if work_item.is_some_and(|work_item| {
            self.find_initiative(name).is_some_and(|initiative| {
                initiative.work_admissions.iter().any(|receipt| {
                    receipt.work_item.as_deref() == Some(work_item) && receipt.action_id != id
                })
            })
        }) {
            return Err(InitiativeError::Precondition(
                "work-item ID already belongs to another initiative admission".into(),
            ));
        }
        if let SteeringDecision::Dispatch {
            project,
            member: Some(member),
            ..
        } = &action.decision
        {
            let work_item = work_item.filter(|id| !id.trim().is_empty()).ok_or_else(|| {
                InitiativeError::Invalid(
                    "dispatch acknowledgement requires its durable work-item ID".into(),
                )
            })?;
            let initiative = self
                .find_initiative_mut(name)
                .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
            if !initiative.task_dispatches.iter().any(|receipt| receipt.work_item == work_item) {
                initiative.task_dispatches.push(TaskDispatch {
                    member: member.clone(),
                    project: project.clone(),
                    work_item: work_item.into(),
                });
            }
        }
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        let member = match &action.decision {
            SteeringDecision::Advance { member, .. }
            | SteeringDecision::Dispatch {
                member: Some(member),
                ..
            } => Some(member.clone()),
            _ => None,
        };
        if let Some(member) = member
            && !initiative.work_admissions.iter().any(|receipt| receipt.action_id == id)
        {
            initiative.work_admissions.push(WorkAdmission {
                action_id: id.into(),
                member,
                providers: action.providers,
                work_item: work_item.map(str::to_owned),
            });
        }
        initiative.pending_actions.retain(|action| action.id != id);
        let work_action = matches!(
            action.decision,
            SteeringDecision::Advance { .. } | SteeringDecision::Dispatch { .. }
        );
        if work_action {
            if let Some(work_item) = work_item {
                for reservation in &mut initiative.reservations {
                    if reservation.operation_id == id {
                        reservation.operation_id = work_item.into();
                    }
                }
            }
        } else {
            initiative.reservations.retain(|reservation| reservation.operation_id != id);
        }
        Ok(())
    }

    pub fn finish_steering_action(&mut self, name: &str, id: &str) -> Result<()> {
        self.acknowledge_steering_action(name, id, None)
    }

    /// Abandon a stale admission without claiming a dispatched task or consuming budget.
    pub fn discard_steering_action(&mut self, name: &str, id: &str) -> Result<()> {
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        initiative.pending_actions.retain(|action| action.id != id);
        initiative.reservations.retain(|reservation| reservation.operation_id != id);
        Ok(())
    }

    pub fn activate_candidate_campaign(
        &mut self,
        name: &str,
        member: &str,
        project: &str,
    ) -> Result<()> {
        let initiative = self.require_initiative(name)?;
        require_authorized(initiative)?;
        let definition = initiative
            .members
            .iter()
            .find(|m| m.name == member && m.kind == MemberKind::Candidate)
            .ok_or_else(|| {
                InitiativeError::Precondition(
                    "only an existing intent candidate can be activated".into(),
                )
            })?;
        if !initiative.projects.iter().any(|p| p == project) || self.find(member).is_some() {
            return Err(InitiativeError::Invalid(
                "candidate assignment must use an initiative project and a distinct campaign name"
                    .into(),
            ));
        }
        if initiative
            .close_proposals
            .iter()
            .any(|proposal| Some(&proposal.intent_ref) == definition.intent_ref.as_ref())
        {
            return Err(InitiativeError::Precondition(
                "realised intent awaits an owner close proposal, not new work".into(),
            ));
        }
        if definition
            .intent_state
            .as_deref()
            .is_some_and(|state| !matches!(state, "create" | "update" | "delete"))
        {
            return Err(InitiativeError::Precondition(
                "candidate is no longer approved intent".into(),
            ));
        }
        if initiative.derive_source.is_some()
            && !definition.audit_finding_id.as_ref().is_some_and(|id| {
                self.find_finding(id).is_some_and(|finding| {
                    finding.audit.as_ref().is_some_and(|audit| {
                        Some(&audit.intent_ref) == definition.intent_ref.as_ref()
                    })
                })
            })
        {
            return Err(InitiativeError::Precondition(
                "candidate activation requires current classified audit evidence".into(),
            ));
        }
        let description = definition
            .description
            .as_ref()
            .filter(|text| !text.trim().is_empty())
            .ok_or_else(|| InitiativeError::Invalid("candidate description is required".into()))?;
        let campaign: Campaign = serde_json::from_value(serde_json::json!({"name":member,"project":project,"mission":description,
            "initiative":name,"intent_refs":definition.intent_ref.iter().collect::<Vec<_>>(),"done_evidence":[{"kind":"review","statement":description}],
            "authorized_by":initiative.authorized_by,"agent_provider":initiative.agent_provider,"providers":StageProviders::default(),
            "budget":{"max_cycles":initiative.budget.max_cycles}})).map_err(|error| InitiativeError::Invalid(error.to_string()))?;
        self.add(campaign)
            .map_err(|error| InitiativeError::Invalid(error.to_string()))?;
        if let Some(definition) = self
            .find_initiative_mut(name)
            .and_then(|initiative| initiative.members.iter_mut().find(|m| m.name == member))
        {
            definition.kind = MemberKind::Campaign;
        }
        Ok(())
    }

    pub fn shared_cycles_used(&self, name: &str) -> Result<u64> {
        let initiative = self.require_initiative(name)?;
        let used = self
            .campaigns
            .iter()
            .filter(|campaign| campaign.initiative.as_deref() == Some(name))
            .fold(0u64, |used, campaign| used.saturating_add(campaign.cycles_completed));
        let tasks = u64::try_from(initiative.task_dispatches.len()).unwrap_or(u64::MAX);
        let pending = u64::try_from(
            initiative
                .reservations
                .iter()
                .filter(|reservation| {
                    reservation.kind == ReservationKind::Task
                        && !initiative
                            .task_dispatches
                            .iter()
                            .any(|receipt| receipt.work_item == reservation.operation_id)
                })
                .count(),
        )
        .unwrap_or(u64::MAX);
        Ok(used.saturating_add(tasks).saturating_add(pending))
    }

    pub fn grant_member_budget(&mut self, member: &str, active_members: &[String]) -> Result<()> {
        let mut candidate = self.clone();
        candidate.grant_member_budget_inner(member, active_members)?;
        *self = candidate;
        Ok(())
    }

    fn grant_member_budget_inner(&mut self, member: &str, active_members: &[String]) -> Result<()> {
        let name = self
            .find(member)
            .and_then(|campaign| campaign.initiative.clone())
            .ok_or_else(|| InitiativeError::Invalid("member has no initiative".into()))?;
        let ceiling = self.require_initiative(&name)?.budget.max_cycles;
        let exhausted = self
            .find(member)
            .is_some_and(|campaign| campaign.cycles_completed >= campaign.budget.max_cycles);
        let budget_escalation = exhausted
            && self.find(member).is_some_and(|campaign| {
                campaign.open_escalation.as_ref().is_some_and(|escalation| {
                    escalation.reason.starts_with("campaign cycle budget exhausted")
                })
            });
        if budget_escalation {
            if let Some(campaign) = self.find_mut(member) {
                campaign.status = CampaignStatus::Active;
                campaign.open_escalation = None;
            }
            self.refresh_questions(&name)?;
        }
        self.enforce_can_advance(member, active_members)?;
        let campaign =
            self.find_mut(member).ok_or_else(|| InitiativeError::NotFound(member.into()))?;
        if campaign.status.is_terminal() {
            return Err(InitiativeError::Precondition("terminal member cannot advance".into()));
        }
        if campaign.cycles_completed >= campaign.budget.max_cycles {
            campaign.budget.max_cycles = ceiling;
        }
        Ok(())
    }

    fn reserve_member(
        &mut self,
        name: &str,
        member: &str,
        id: &str,
        kind: ReservationKind,
    ) -> Result<()> {
        self.enforce_member_eligibility(name, member, &[])?;
        if self.find(member).is_some_and(Campaign::has_cycle_in_flight) {
            return Err(InitiativeError::Precondition(
                "member already has a cycle in flight".into(),
            ));
        }
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        if initiative.reservations.iter().any(|reservation| reservation.member == member) {
            return Err(InitiativeError::Precondition(
                "member already has an admission in flight".into(),
            ));
        }
        initiative.reservations.push(MemberReservation {
            member: member.into(),
            operation_id: id.into(),
            kind,
        });
        Ok(())
    }

    pub fn enforce_task_dispatch(
        &self,
        name: &str,
        member: &str,
        active_members: &[String],
    ) -> Result<()> {
        self.enforce_member_eligibility(name, member, active_members)?;
        let initiative = self.require_initiative(name)?;
        let owns_reservation = initiative.reservations.iter().any(|reservation| {
            reservation.member == member && reservation.kind == ReservationKind::Task
        });
        let used = self.shared_cycles_used(name)?;
        if used >= initiative.budget.max_cycles
            && !(owns_reservation && used == initiative.budget.max_cycles)
        {
            return Err(InitiativeError::Precondition(
                "initiative exhausted its shared cycle budget".into(),
            ));
        }
        Ok(())
    }

    pub fn enforce_member_eligibility(
        &self,
        name: &str,
        member: &str,
        active_members: &[String],
    ) -> Result<()> {
        let initiative = self.initiative_view(name)?;
        require_authorized(&initiative)?;
        let definition = initiative
            .members
            .iter()
            .find(|definition| definition.name == member)
            .ok_or_else(|| InitiativeError::NotFound(format!("member '{member}' not found")))?;
        if definition
            .intent_state
            .as_deref()
            .is_some_and(|state| matches!(state, "discarded" | "plan" | "revoked"))
        {
            return Err(InitiativeError::Precondition(
                "member intent was discarded or revoked".into(),
            ));
        }
        if initiative
            .open_questions
            .iter()
            .chain(&initiative.deferred_questions)
            .any(|question| {
                question.members.is_empty() || question.members.iter().any(|m| m == member)
            })
        {
            return Err(InitiativeError::Precondition(format!(
                "member '{member}' waits on an initiative owner question"
            )));
        }
        for hold in initiative.holds.iter().filter(|hold| hold.member == member) {
            if hold.owner
                || hold.until.is_empty()
                || hold.until.iter().any(|milestone| !self.milestone_satisfied(name, milestone))
            {
                return Err(InitiativeError::Precondition(format!(
                    "member '{member}' is held: {}",
                    hold.reason
                )));
            }
        }
        for after in &definition.after {
            if !self.milestone_satisfied(name, after) {
                return Err(InitiativeError::Precondition(format!(
                    "member '{member}' waits on '{after}'"
                )));
            }
        }
        let active = self.occupied_member_slots(name, active_members)?;
        if !active.contains(member) && active.len() >= initiative.budget.max_active_members {
            return Err(InitiativeError::Precondition(format!(
                "initiative '{name}' has reached its active member limit"
            )));
        }
        Ok(())
    }

    fn milestone_satisfied(&self, name: &str, prerequisite: &str) -> bool {
        let Some(initiative) = self.find_initiative(name) else {
            return false;
        };
        if let Some((member, milestone)) = prerequisite.split_once(':') {
            return initiative.milestones.iter().any(|proof| {
                proof.member == member && proof.name == milestone && !proof.evidence.is_empty()
            });
        }
        self.find(prerequisite).is_some_and(|campaign| {
            campaign.cycles_landed > 0 || campaign.status == CampaignStatus::Completed
        }) || initiative.milestones.iter().any(|proof| {
            proof.member == prerequisite && proof.name == "landed" && !proof.evidence.is_empty()
        })
    }

    fn set_member_hold(
        &mut self,
        name: &str,
        member: &str,
        until: &[String],
        reason: &str,
        owner: bool,
    ) -> Result<()> {
        let initiative = self.require_initiative(name)?;
        require_authorized(initiative)?;
        if reason.trim().is_empty()
            || !initiative.members.iter().any(|definition| definition.name == member)
        {
            return Err(InitiativeError::Invalid(
                "hold requires an initiative member and a reason".into(),
            ));
        }
        for prerequisite in until {
            let (dependency, milestone) =
                prerequisite.split_once(':').unwrap_or((prerequisite, "landed"));
            if dependency == member
                || milestone.trim().is_empty()
                || !initiative.members.iter().any(|definition| definition.name == dependency)
            {
                return Err(InitiativeError::Invalid(
                    "hold prerequisite must name another member's milestone".into(),
                ));
            }
        }
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        if !owner && initiative.holds.iter().any(|hold| hold.member == member && hold.owner) {
            return Err(InitiativeError::Precondition(
                "steering cannot replace an owner hold".into(),
            ));
        }
        initiative.holds.retain(|hold| hold.member != member || (hold.owner && !owner));
        initiative.holds.push(MemberHold {
            member: member.into(),
            until: until.to_vec(),
            reason: reason.into(),
            owner,
        });
        Ok(())
    }

    pub fn hold_member(&mut self, name: &str, member: &str, reason: &str) -> Result<()> {
        self.set_member_hold(name, member, &[], reason, true)
    }
    pub fn release_member(&mut self, name: &str, member: &str) -> Result<()> {
        require_authorized(self.require_initiative(name)?)?;
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        if !initiative.members.iter().any(|definition| definition.name == member) {
            return Err(InitiativeError::NotFound(member.into()));
        }
        initiative
            .holds
            .retain(|hold| hold.member != member || !hold.owner || !hold.until.is_empty());
        Ok(())
    }

    pub fn record_milestone(
        &mut self,
        name: &str,
        member: &str,
        milestone: &str,
        evidence: Vec<String>,
    ) -> Result<()> {
        let initiative = self.require_initiative(name)?;
        require_authorized(initiative)?;
        if !initiative.members.iter().any(|definition| definition.name == member)
            || milestone.trim().is_empty()
            || evidence.is_empty()
            || evidence.iter().any(|path| path.trim().is_empty())
        {
            return Err(InitiativeError::Invalid(
                "milestone requires a member, name and recorded proof".into(),
            ));
        }
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        initiative
            .milestones
            .retain(|proof| proof.member != member || proof.name != milestone);
        initiative.milestones.push(MemberMilestone {
            member: member.into(),
            name: milestone.into(),
            evidence,
        });
        Ok(())
    }

    /// Record an owner decision, extending the shared cycle budget by
    /// `add_cycles` in the same change.
    ///
    /// When the shared budget is spent, `add_cycles` must be positive: a
    /// decision without cycles cannot unblock a budget-escalated member, and
    /// the same question would reopen. The decision's own refusals come first,
    /// and every refusal writes nothing.
    pub fn decide_initiative_with_budget(
        &mut self,
        name: &str,
        decision: &str,
        members: &[String],
        add_cycles: u64,
        at: DateTime<Utc>,
    ) -> Result<super::InitiativeOwnerDecision> {
        self.decision_scope(name, decision, members)?;
        if add_cycles == 0
            && self.shared_cycles_used(name)? >= self.require_initiative(name)?.budget.max_cycles
        {
            return Err(InitiativeError::Precondition(format!(
                "initiative '{name}' exhausted its cycle budget; pass add_cycles > 0 to authorize more work"
            )));
        }
        let mut candidate = self.clone();
        let ceiling = candidate
            .require_initiative(name)?
            .budget
            .max_cycles
            .checked_add(add_cycles)
            .ok_or_else(|| InitiativeError::Invalid("initiative cycle budget overflow".into()))?;
        candidate
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?
            .budget
            .max_cycles = ceiling;
        if add_cycles > 0 {
            for campaign in candidate.campaigns.iter_mut().filter(|campaign| {
                campaign.initiative.as_deref() == Some(name) && !campaign.status.is_terminal()
            }) {
                campaign.budget.max_cycles = ceiling;
            }
        }
        let record = candidate.decide_initiative(name, decision, members, at)?;
        if let Some(initiative) = candidate.find_initiative_mut(name) {
            initiative.completion_proposal = None;
            initiative.completion_confirmation = None;
        }
        *self = candidate;
        Ok(record)
    }

    fn reconcile_derived_members(
        &mut self,
        name: &str,
        projection: &super::derive::IntentProjection,
        derived: &Initiative,
    ) -> Result<Vec<String>> {
        let mut cancelled = Vec::new();
        let existing = self.require_initiative(name)?.members.clone();
        for member in &existing {
            if member.intent_ref.is_none() {
                continue;
            }
            let interaction = member
                .intent_ref
                .as_deref()
                .and_then(|reference| reference.strip_prefix("et:interaction:"))
                .and_then(|id| {
                    projection.interactions.iter().find(|interaction| interaction.id == id)
                });
            let in_scope =
                derived.members.iter().any(|incoming| incoming.intent_ref == member.intent_ref);
            let realised = interaction.is_some_and(|interaction| {
                matches!(interaction.state.as_str(), "created" | "deleted")
            });
            let revoked = !realised
                && (!in_scope
                    || interaction.is_none_or(|interaction| {
                        matches!(interaction.state.as_str(), "discarded" | "plan")
                    }));
            let next_state = Some(if revoked {
                "revoked".into()
            } else {
                interaction
                    .map_or_else(|| "revoked".into(), |interaction| interaction.state.clone())
            });
            if member.intent_state != next_state
                && let Some(initiative) = self.find_initiative_mut(name)
            {
                initiative
                    .close_proposals
                    .retain(|proposal| Some(&proposal.intent_ref) != member.intent_ref.as_ref());
            }
            if let Some(definition) = self.find_initiative_mut(name).and_then(|initiative| {
                initiative.members.iter_mut().find(|definition| definition.name == member.name)
            }) {
                if definition.intent_state != next_state {
                    definition.audit_finding_id = None;
                }
                definition.intent_state = next_state;
            }
            if revoked {
                if let Some(campaign) = self.find_mut(&member.name)
                    && !campaign.status.is_terminal()
                {
                    let _transition = campaign
                        .cancel("initiative intent was discarded or revoked", Utc::now())
                        .map_err(|error| InitiativeError::Precondition(error.to_string()))?;
                }
                cancelled.push(member.name.clone());
            }
        }
        for incoming in &derived.members {
            if let Some(previous) =
                existing.iter().find(|member| member.intent_ref == incoming.intent_ref)
                && previous.description != incoming.description
                && let Some(campaign) = self.find_mut(&previous.name)
                && let Some(description) = &incoming.description
            {
                if Some(&campaign.mission) == previous.description.as_ref() {
                    campaign.mission.clone_from(description);
                }
                for evidence in &mut campaign.done_evidence {
                    if let crate::campaign::DoneEvidence::Review { statement } = evidence
                        && Some(&*statement) == previous.description.as_ref()
                    {
                        statement.clone_from(description);
                    }
                }
            }
        }
        Ok(cancelled)
    }

    /// Re-read intent while retaining linked campaigns, decisions and consumed findings.
    pub fn refresh_derived_intent(
        &mut self,
        name: &str,
        projection: &super::derive::IntentProjection,
    ) -> Result<Vec<String>> {
        let initiative = self.require_initiative(name)?;
        let source = initiative.derive_source.as_ref().ok_or_else(|| {
            InitiativeError::Precondition("initiative has no recorded derivation source".into())
        })?;
        let derived = super::derive::derive_from_projection(
            projection,
            &source.from,
            initiative.projects.clone(),
        )?;
        let mut candidate = self.clone();
        let cancelled = candidate.reconcile_derived_members(name, projection, &derived)?;
        let initiative = candidate
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        initiative.outcome = derived.outcome;
        initiative.intent_revision = hex::encode(Sha256::digest(
            serde_json::to_vec(projection)
                .map_err(|error| InitiativeError::Invalid(error.to_string()))?,
        ));
        // Retain refs for historical members so their findings remain addressable.
        for reference in derived.intent_refs {
            if !initiative.intent_refs.contains(&reference) {
                initiative.intent_refs.push(reference);
            }
        }
        for incoming in derived.members {
            if let Some(member) = initiative
                .members
                .iter_mut()
                .find(|member| member.intent_ref == incoming.intent_ref)
            {
                if member.description != incoming.description
                    || member.intent_state != incoming.intent_state
                {
                    member.audit_finding_id = None;
                    initiative.close_proposals.retain(|proposal| {
                        Some(&proposal.intent_ref) != incoming.intent_ref.as_ref()
                    });
                }
                member.description = incoming.description;
                member.after = incoming.after;
                member.intent_state = incoming.intent_state;
            } else {
                initiative.members.push(incoming);
            }
        }
        // Host gates are owner declarations, while Tracker owns the review text.
        // Keep gates first so added or removed interactions never renumber them.
        let mut evidence: Vec<_> = initiative
            .done_evidence
            .iter()
            .filter(|evidence| matches!(evidence, super::DoneEvidence::Gate { .. }))
            .cloned()
            .collect();
        evidence.extend(derived.done_evidence);
        initiative.done_evidence = evidence;
        initiative
            .reservations
            .retain(|reservation| !cancelled.contains(&reservation.member));
        candidate.refresh_questions(name)?;
        *self = candidate;
        Ok(cancelled)
    }
}

fn require_authorized(initiative: &Initiative) -> Result<()> {
    if initiative.status.is_terminal()
        || initiative.authorized_by.as_ref().is_none_or(|owner| owner.trim().is_empty())
    {
        return Err(InitiativeError::Precondition(
            "initiative is terminal or has not been authorized".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
