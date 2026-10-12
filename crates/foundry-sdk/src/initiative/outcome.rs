//! Outcome evidence and explicit, resumable owner confirmation.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    CampaignStore, DoneEvidence, EtCloseProposal, Finding, FindingKind, FindingSource,
    InitiativeError, MemberKind, Result, SteeringRepositoryIdentity,
};
use crate::campaign::{Campaign, CampaignBudget, CampaignStatus};
use crate::gateway::StageProviders;

/// Only declarations may enter through steering, never invented execution history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageCampaignDefinition {
    pub name: String,
    pub project: String,
    pub mission: String,
    #[serde(default)]
    pub intent_refs: Vec<String>,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub context_paths: Vec<String>,
    #[serde(default)]
    pub writable_repositories: Vec<String>,
    pub done_evidence: Vec<crate::campaign::DoneEvidence>,
    #[serde(default)]
    pub budget: CampaignBudget,
    #[serde(default)]
    pub agent_provider: Option<String>,
    #[serde(default)]
    pub providers: StageProviders,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeSnapshot {
    pub repositories: Vec<SteeringRepositoryIdentity>,
    pub intent_revision: String,
    pub policy_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationGateResult {
    pub id: String,
    pub gate_index: usize,
    pub host: String,
    pub command: String,
    pub snapshot: OutcomeSnapshot,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub report: String,
    pub artifacts: Vec<String>,
    pub recorded_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutcomeReview {
    pub statement: String,
    pub affirmed: bool,
    pub evidence: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionProposal {
    pub id: String,
    pub snapshot: OutcomeSnapshot,
    pub reviews: Vec<OutcomeReview>,
    pub proposed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompletionConfirmation {
    pub proposal_id: String,
    pub reason: String,
    pub confirmed_at: DateTime<Utc>,
    pub last_error: Option<String>,
    #[serde(default)]
    pub intent_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtCloseReceipt {
    #[serde(default)]
    pub proposal_id: String,
    pub intent_ref: String,
    pub completed_at: DateTime<Utc>,
}

impl CampaignStore {
    /// Bind evidence to the complete registered project set and current policy.
    pub fn outcome_snapshot(
        &self,
        name: &str,
        mut repositories: Vec<SteeringRepositoryIdentity>,
    ) -> Result<OutcomeSnapshot> {
        let initiative = self.require_initiative(name)?;
        repositories.sort_by(|left, right| left.project.cmp(&right.project));
        let mut projects = initiative.projects.clone();
        projects.sort();
        if repositories.iter().map(|repo| repo.project.clone()).collect::<Vec<_>>() != projects
            || repositories.iter().any(|repo| {
                repo.root.trim().is_empty()
                    || repo.trunk.trim().is_empty()
                    || repo.head.trim().is_empty()
            })
        {
            return Err(InitiativeError::Invalid(
                "outcome evidence must identify every initiative trunk exactly once".into(),
            ));
        }
        let bytes = serde_json::to_vec(&(
            &initiative.outcome,
            &initiative.intent_refs,
            &initiative.projects,
            &initiative.brief,
            &initiative.members,
            &initiative.done_evidence,
            &initiative.budget,
            &initiative.authorized_by,
            &initiative.agent_provider,
            initiative.providers,
            &initiative.provider_pairings,
            &initiative.owner_decisions,
            &initiative.holds,
            &initiative.derive_source,
        ))
        .map_err(|error| InitiativeError::Persistence(error.to_string()))?;
        Ok(OutcomeSnapshot {
            repositories,
            intent_revision: initiative.intent_revision.clone(),
            policy_hash: hex::encode(Sha256::digest(bytes)),
        })
    }

    fn validate_outcome_snapshot(&self, name: &str, snapshot: &OutcomeSnapshot) -> Result<()> {
        if self.outcome_snapshot(name, snapshot.repositories.clone())? != *snapshot {
            return Err(InitiativeError::Precondition(
                "outcome evidence is stale after an intent or policy change".into(),
            ));
        }
        Ok(())
    }

    /// An operation identity may be replayed, but its original evidence cannot change.
    pub fn record_integration_result(
        &mut self,
        name: &str,
        result: IntegrationGateResult,
    ) -> Result<()> {
        let initiative = self.require_initiative(name)?;
        if let Some(existing) =
            initiative.integration_results.iter().find(|item| item.id == result.id)
        {
            return if existing == &result {
                Ok(())
            } else {
                Err(InitiativeError::Precondition(
                    "integration operation already has different evidence".into(),
                ))
            };
        }
        self.validate_outcome_snapshot(name, &result.snapshot)?;
        let Some(DoneEvidence::Gate {
            command,
            host,
            artifacts,
            ..
        }) = initiative.done_evidence.get(result.gate_index)
        else {
            return Err(InitiativeError::Invalid(
                "integration result must name a declared gate".into(),
            ));
        };
        if result.id.trim().is_empty()
            || result.command != *command
            || result.host.trim().is_empty()
            || host.as_ref().is_some_and(|host| host != &result.host)
            || (result.passed
                && (result.exit_code != Some(0)
                    || result.report.trim().is_empty()
                    || artifacts.iter().any(|path| !result.artifacts.contains(path))
                    || result.snapshot.repositories.iter().any(|repo| {
                        !repo.status.trim().is_empty()
                            || !repo.diff.trim().is_empty()
                            || repo.head.trim() != repo.trunk.trim()
                    })))
        {
            return Err(InitiativeError::Invalid(
                "successful integration evidence requires the declared host, command, report, artifacts and clean trunks".into(),
            ));
        }
        if !result.passed {
            let mut finding = Finding::new(
                name,
                FindingSource {
                    kind: "integration_gate".into(),
                    id: result.id.clone(),
                },
                vec![],
                FindingKind::Regression,
                &format!(
                    "Integration gate {} on {} failed: {}",
                    result.gate_index, result.host, result.report
                ),
                result.artifacts.clone(),
            );
            finding.id = format!("finding_{}", result.id);
            self.add_finding(finding)?;
        }
        self.find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?
            .integration_results
            .push(result);
        Ok(())
    }

    pub fn validate_outcome_reviews(&self, name: &str, reviews: &[OutcomeReview]) -> Result<()> {
        let initiative = self.require_initiative(name)?;
        for evidence in &initiative.done_evidence {
            if let DoneEvidence::Review { statement } = evidence
                && !reviews.iter().any(|review| {
                    review.statement == *statement
                        && review.affirmed
                        && !review.evidence.is_empty()
                        && review.evidence.iter().all(|evidence| !evidence.trim().is_empty())
                })
            {
                return Err(InitiativeError::Precondition(format!(
                    "outcome review is not affirmed: {statement}"
                )));
            }
        }
        Ok(())
    }

    fn require_settled_outcome(&self, name: &str) -> Result<()> {
        let initiative = self.require_initiative(name)?;
        if initiative.status.is_terminal()
            || initiative.authorized_by.as_ref().is_none_or(|owner| owner.trim().is_empty())
            || !initiative.open_questions.is_empty()
            || !initiative.reservations.is_empty()
            || self.campaigns.iter().any(|campaign| {
                campaign.initiative.as_deref() == Some(name)
                    && (campaign.status == CampaignStatus::Escalated
                        || campaign
                            .objective_history
                            .last()
                            .is_some_and(|cycle| cycle.outcome.is_none()))
            })
        {
            return Err(InitiativeError::Precondition(
                "outcome requires settled member work and no open owner questions".into(),
            ));
        }
        Ok(())
    }

    fn validate_gate_results(&self, name: &str, snapshot: &OutcomeSnapshot) -> Result<()> {
        let initiative = self.require_initiative(name)?;
        for (index, evidence) in initiative.done_evidence.iter().enumerate() {
            if matches!(evidence, DoneEvidence::Gate { required: true, .. })
                && !initiative
                    .integration_results
                    .iter()
                    .rev()
                    .find(|result| result.gate_index == index)
                    .is_some_and(|result| {
                        result.passed && result.exit_code == Some(0) && result.snapshot == *snapshot
                    })
            {
                return Err(InitiativeError::Precondition(format!(
                    "required initiative gate {index} has no current passing evidence"
                )));
            }
        }
        Ok(())
    }

    fn require_current_trunks(snapshot: &OutcomeSnapshot) -> Result<()> {
        if snapshot.repositories.iter().any(|repository| {
            !repository.status.trim().is_empty()
                || !repository.diff.trim().is_empty()
                || repository.head.trim() != repository.trunk.trim()
        }) {
            return Err(InitiativeError::Precondition(
                "completion requires clean checkouts at every registered trunk".into(),
            ));
        }
        Ok(())
    }

    /// Proposing done never changes the terminal state or writes to Tracker.
    pub fn propose_completion(
        &mut self,
        name: &str,
        id: &str,
        snapshot: OutcomeSnapshot,
        reviews: Vec<OutcomeReview>,
    ) -> Result<()> {
        if id.trim().is_empty() {
            return Err(InitiativeError::Invalid(
                "completion proposal requires an operation identity".into(),
            ));
        }
        Self::require_current_trunks(&snapshot)?;
        self.require_settled_outcome(name)?;
        self.validate_outcome_snapshot(name, &snapshot)?;
        self.validate_gate_results(name, &snapshot)?;
        self.validate_outcome_reviews(name, &reviews)?;
        let initiative = self.require_initiative(name)?;
        if let Some(proposal) = &initiative.completion_proposal {
            return if proposal.id == id
                && proposal.snapshot == snapshot
                && proposal.reviews == reviews
            {
                Ok(())
            } else {
                Err(InitiativeError::Precondition(
                    "completion already awaits owner confirmation".into(),
                ))
            };
        }
        let mut proposals = initiative.close_proposals.clone();
        if initiative.derive_source.is_some() {
            for member in &initiative.members {
                if member
                    .intent_state
                    .as_deref()
                    .is_some_and(|state| matches!(state, "create" | "update" | "delete"))
                {
                    let Some(intent_ref) = &member.intent_ref else {
                        continue;
                    };
                    if !proposals.iter().any(|proposal| proposal.intent_ref == *intent_ref) {
                        let review = reviews
                            .iter()
                            .find(|review| {
                                member.description.as_ref() == Some(&review.statement)
                                    && review.affirmed
                                    && !review.evidence.is_empty()
                            })
                            .ok_or_else(|| {
                                InitiativeError::Precondition(format!(
                                    "intent '{intent_ref}' requires an affirmed interaction review"
                                ))
                            })?;
                        proposals.push(EtCloseProposal {
                            intent_ref: intent_ref.clone(),
                            evidence: review.evidence.clone(),
                        });
                    }
                }
            }
            for reference in initiative
                .intent_refs
                .iter()
                .filter(|reference| reference.starts_with("et:goal:"))
            {
                if !proposals.iter().any(|proposal| proposal.intent_ref == *reference) {
                    let evidence =
                        reviews.iter().flat_map(|review| review.evidence.clone()).collect();
                    proposals.push(EtCloseProposal {
                        intent_ref: reference.clone(),
                        evidence,
                    });
                }
            }
        }
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        initiative.close_proposals = proposals;
        initiative.status = super::InitiativeStatus::Escalated;
        initiative.completion_proposal = Some(CompletionProposal {
            id: id.into(),
            snapshot,
            reviews,
            proposed_at: Utc::now(),
        });
        Ok(())
    }

    pub fn validate_completion(&self, name: &str, snapshot: &OutcomeSnapshot) -> Result<()> {
        Self::require_current_trunks(snapshot)?;
        self.require_settled_outcome(name)?;
        self.validate_outcome_snapshot(name, snapshot)?;
        let proposal =
            self.require_initiative(name)?.completion_proposal.as_ref().ok_or_else(|| {
                InitiativeError::Precondition(
                    "initiative has no done proposal awaiting confirmation".into(),
                )
            })?;
        if proposal.snapshot != *snapshot {
            return Err(InitiativeError::Precondition(
                "completion evidence is stale after a trunk change".into(),
            ));
        }
        self.validate_gate_results(name, snapshot)?;
        self.validate_outcome_reviews(name, &proposal.reviews)
    }

    pub fn confirm_completion(
        &mut self,
        name: &str,
        reason: &str,
        snapshot: &OutcomeSnapshot,
        at: DateTime<Utc>,
    ) -> Result<()> {
        self.validate_completion(name, snapshot)?;
        if reason.trim().is_empty() {
            return Err(InitiativeError::Invalid(
                "completion confirmation requires a reason".into(),
            ));
        }
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        let proposal_id = initiative
            .completion_proposal
            .as_ref()
            .ok_or_else(|| InitiativeError::Precondition("missing completion proposal".into()))?
            .id
            .clone();
        if let Some(confirmation) = &initiative.completion_confirmation {
            if confirmation.proposal_id != proposal_id {
                return Err(InitiativeError::Precondition(
                    "confirmation belongs to another proposal".into(),
                ));
            }
            return Ok(());
        }
        initiative.completion_confirmation = Some(CompletionConfirmation {
            proposal_id,
            reason: reason.into(),
            confirmed_at: at,
            last_error: None,
            intent_json: None,
        });
        Ok(())
    }

    pub fn set_completion_intent(&mut self, name: &str, json: &str) -> Result<()> {
        let confirmation = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?
            .completion_confirmation
            .as_mut()
            .ok_or_else(|| {
                InitiativeError::Precondition("completion has not been confirmed".into())
            })?;
        if confirmation.intent_json.as_ref().is_some_and(|previous| previous != json) {
            return Err(InitiativeError::Precondition(
                "confirmed intent baseline cannot be replaced".into(),
            ));
        }
        confirmation.intent_json = Some(json.into());
        Ok(())
    }

    pub fn record_completion_error(&mut self, name: &str, message: &str) -> Result<()> {
        let confirmation = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?
            .completion_confirmation
            .as_mut()
            .ok_or_else(|| {
                InitiativeError::Precondition("completion has not been confirmed".into())
            })?;
        confirmation.last_error = Some(message.into());
        Ok(())
    }

    pub fn record_et_close(
        &mut self,
        name: &str,
        intent_ref: &str,
        at: DateTime<Utc>,
    ) -> Result<()> {
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        if initiative.completion_confirmation.is_none()
            || !initiative
                .close_proposals
                .iter()
                .any(|proposal| proposal.intent_ref == intent_ref)
        {
            return Err(InitiativeError::Precondition(
                "Tracker close requires confirmed, reviewed intent".into(),
            ));
        }
        let confirmation = initiative.completion_confirmation.as_mut().ok_or_else(|| {
            InitiativeError::Precondition("completion has not been confirmed".into())
        })?;
        let proposal_id = confirmation.proposal_id.clone();
        if !initiative
            .completion_receipts
            .iter()
            .any(|receipt| receipt.intent_ref == intent_ref && receipt.proposal_id == proposal_id)
        {
            initiative.completion_receipts.push(EtCloseReceipt {
                proposal_id,
                intent_ref: intent_ref.into(),
                completed_at: at,
            });
        }
        confirmation.last_error = None;
        Ok(())
    }

    /// Discard only a proposal which fresh host evidence has proved stale.
    pub fn invalidate_completion(
        &mut self,
        name: &str,
        snapshot: &OutcomeSnapshot,
    ) -> Result<bool> {
        self.validate_outcome_snapshot(name, snapshot)?;
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        if initiative.completion_confirmation.is_some() {
            return Ok(false);
        }
        if initiative
            .completion_proposal
            .as_ref()
            .is_some_and(|proposal| proposal.snapshot != *snapshot)
        {
            initiative.completion_proposal = None;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Stage owned work without reserving execution or consuming a cycle.
    pub fn stage_initiative_campaign(
        &mut self,
        name: &str,
        definition: &StageCampaignDefinition,
        member: Option<&str>,
    ) -> Result<()> {
        let initiative = self.require_initiative(name)?;
        if initiative.status.is_terminal()
            || initiative.authorized_by.as_ref().is_none_or(|owner| owner.trim().is_empty())
        {
            return Err(InitiativeError::Precondition(
                "staging requires an authorized initiative".into(),
            ));
        }
        if definition.name.is_empty()
            || !definition.name.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
            })
            || matches!(definition.name.as_str(), "." | "..")
            || definition.budget.max_cycles == 0
        {
            return Err(InitiativeError::Invalid(
                "staged campaign name and budget are invalid".into(),
            ));
        }
        let fallback = definition
            .agent_provider
            .as_ref()
            .or(initiative.agent_provider.as_ref())
            .map(|provider| {
                provider
                    .parse::<crate::gateway::AgentProvider>()
                    .map_err(InitiativeError::Invalid)
            })
            .transpose()?;
        let effective = definition.providers.inherit(&initiative.providers);
        for stage in [
            crate::gateway::AgentStage::Execution,
            crate::gateway::AgentStage::Review,
        ] {
            if effective
                .resolve(stage, fallback)
                .is_some_and(|provider| !provider.enforces_writable_root())
            {
                return Err(InitiativeError::Invalid(
                    "staged campaign execution and review must use an enforced repository boundary"
                        .into(),
                ));
            }
        }
        if !initiative.projects.contains(&definition.project)
            || self.find(&definition.name).is_some()
            || self.shared_cycles_used(name)? >= initiative.budget.max_cycles
        {
            return Err(InitiativeError::Invalid("staged campaign requires an allowed project, unused name and remaining initiative budget".into()));
        }
        if definition
            .intent_refs
            .iter()
            .any(|reference| !initiative.intent_refs.contains(reference))
        {
            return Err(InitiativeError::Invalid(
                "staged campaign intent is outside this initiative".into(),
            ));
        }
        let updated_member = self.staged_member(name, definition, member)?;
        let mut updated = initiative.clone();
        updated.members.retain(|member| member.name != updated_member.name);
        updated.members.push(updated_member);
        updated.validate()?;
        let mut budget = definition.budget.clone();
        budget.max_cycles = initiative.budget.max_cycles;
        let campaign: Campaign = serde_json::from_value(serde_json::json!({
            "name":definition.name,"project":definition.project,"mission":definition.mission,
            "initiative":name,"intent_refs":definition.intent_refs,"context_paths":definition.context_paths,
            "writable_repositories":definition.writable_repositories,"done_evidence":definition.done_evidence,
            "budget":budget,"providers":definition.providers,
            "agent_provider":definition.agent_provider.as_ref().or(initiative.agent_provider.as_ref()),
            "authorized_by":initiative.authorized_by
        })).map_err(|error| InitiativeError::Invalid(error.to_string()))?;
        campaign
            .validate()
            .map_err(|error| InitiativeError::Invalid(error.to_string()))?;
        self.campaigns.push(campaign);
        *self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))? = updated;
        Ok(())
    }
    fn staged_member(
        &self,
        name: &str,
        definition: &StageCampaignDefinition,
        member: Option<&str>,
    ) -> Result<super::InitiativeMember> {
        let initiative = self.require_initiative(name)?;
        let candidate = member
            .map(|member| {
                initiative.members.iter().find(|item| item.name == member).ok_or_else(|| {
                    InitiativeError::NotFound(format!("candidate '{member}' not found"))
                })
            })
            .transpose()?;
        if let Some(candidate) = candidate {
            if initiative
                .close_proposals
                .iter()
                .any(|proposal| candidate.intent_ref.as_ref() == Some(&proposal.intent_ref))
            {
                return Err(InitiativeError::Precondition(
                    "realised intent awaits owner confirmation, not new work".into(),
                ));
            }
            if candidate.kind != MemberKind::Candidate
                || candidate.name != definition.name
                || candidate
                    .intent_state
                    .as_deref()
                    .is_some_and(|state| !matches!(state, "create" | "update" | "delete"))
                || candidate
                    .intent_ref
                    .as_ref()
                    .is_some_and(|reference| !definition.intent_refs.contains(reference))
                || (initiative.derive_source.is_some()
                    && !candidate.audit_finding_id.as_ref().is_some_and(|id| {
                        self.find_finding(id).is_some_and(|finding| finding.audit.is_some())
                    }))
            {
                return Err(InitiativeError::Precondition(
                    "stage must preserve the current audited candidate's identity and intent"
                        .into(),
                ));
            }
        } else if initiative.members.iter().any(|item| item.name == definition.name) {
            return Err(InitiativeError::AlreadyExists("member already exists".into()));
        }
        let mut staged = candidate.cloned().unwrap_or_else(|| super::InitiativeMember {
            kind: MemberKind::Campaign,
            name: definition.name.clone(),
            after: vec![],
            intent_ref: None,
            description: Some(definition.mission.clone()),
            intent_state: None,
            audit_finding_id: None,
        });
        staged.kind = MemberKind::Campaign;
        for dependency in &definition.after {
            if !staged.after.contains(dependency) {
                staged.after.push(dependency.clone());
            }
        }
        Ok(staged)
    }
}

#[cfg(test)]
mod tests;
