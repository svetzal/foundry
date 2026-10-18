//! Computed admission diagnostics. Completion is a campaign outcome, not deployment proof.
use serde::{Deserialize, Serialize};

use super::{CampaignStore, Initiative, InitiativeError, MemberKind, Result};
use crate::campaign::CampaignStatus;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadinessBlockerKind {
    Unauthorized,
    InitiativeTerminal,
    CompletionPending,
    CampaignPaused,
    CampaignTerminal,
    RecoveryPending,
    IntentInactive,
    OwnerQuestion,
    Held,
    DependencyUnmet,
    WorkInFlight,
    BudgetExhausted,
    RepositoryBusy,
    ProjectUnavailable,
    MemberLimit,
}
impl ReadinessBlockerKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::InitiativeTerminal => "initiative_terminal",
            Self::CompletionPending => "completion_pending",
            Self::CampaignPaused => "campaign_paused",
            Self::CampaignTerminal => "campaign_terminal",
            Self::RecoveryPending => "recovery_pending",
            Self::IntentInactive => "intent_inactive",
            Self::OwnerQuestion => "owner_question",
            Self::Held => "held",
            Self::DependencyUnmet => "dependency_unmet",
            Self::WorkInFlight => "work_in_flight",
            Self::BudgetExhausted => "budget_exhausted",
            Self::RepositoryBusy => "repository_busy",
            Self::ProjectUnavailable => "project_unavailable",
            Self::MemberLimit => "member_limit",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadinessBlocker {
    pub kind: ReadinessBlockerKind,
    pub detail: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyReadiness {
    pub prerequisite: String,
    pub member: String,
    pub milestone: String,
    pub satisfied: bool,
    pub evidence: Vec<String>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemberReadiness {
    pub member: String,
    pub ready: bool,
    pub blockers: Vec<ReadinessBlocker>,
    pub dependencies: Vec<DependencyReadiness>,
}
impl MemberReadiness {
    pub fn block(&mut self, kind: ReadinessBlockerKind, detail: String) {
        self.blockers.push(ReadinessBlocker { kind, detail });
        self.ready = false;
    }
}

pub(super) fn parse_prerequisite(prerequisite: &str) -> Result<(&str, &str)> {
    let (member, milestone) = prerequisite.split_once(':').unwrap_or((prerequisite, "completed"));
    if member.is_empty()
        || member.trim() != member
        || milestone.is_empty()
        || milestone.trim() != milestone
        || milestone.contains(':')
    {
        return Err(InitiativeError::Invalid(format!("malformed prerequisite '{prerequisite}'")));
    }
    Ok((member, milestone))
}

impl CampaignStore {
    pub fn dependency_readiness(
        &self,
        name: &str,
        prerequisite: &str,
    ) -> Result<DependencyReadiness> {
        let initiative = self.require_initiative(name)?;
        let (member, milestone) = parse_prerequisite(prerequisite)?;
        let definition = initiative
            .members
            .iter()
            .find(|definition| definition.name == member)
            .ok_or_else(|| {
                InitiativeError::Invalid(format!("unknown prerequisite '{prerequisite}'"))
            })?;
        let mut evidence: Vec<String> = initiative
            .milestones
            .iter()
            .filter(|proof| {
                proof.member == member
                    && proof.name == milestone
                    && !(milestone == "completed" && definition.kind != MemberKind::Task)
            })
            .flat_map(|proof| {
                proof.evidence.iter().filter(|proof| !proof.trim().is_empty()).cloned()
            })
            .collect();
        if let Some(campaign) = self.find(member) {
            if milestone == "completed" && campaign.status == CampaignStatus::Completed {
                evidence.push(format!("campaign:{member}:completed"));
            } else if milestone == "landed" && campaign.cycles_landed > 0 {
                evidence
                    .push(format!("campaign:{member}:landed_cycles:{}", campaign.cycles_landed));
            }
        }
        if milestone == "completed"
            && definition.kind == MemberKind::Candidate
            && let Some(finding) =
                definition.audit_finding_id.as_deref().and_then(|id| self.find_finding(id))
            && finding
                .audit
                .as_ref()
                .is_some_and(|audit| audit.classification == super::AuditClassification::Realised)
        {
            evidence.extend(finding.refs.iter().filter(|proof| !proof.trim().is_empty()).cloned());
        }
        Ok(DependencyReadiness {
            prerequisite: prerequisite.into(),
            member: member.into(),
            milestone: milestone.into(),
            satisfied: !evidence.is_empty(),
            evidence,
        })
    }

    pub fn member_readiness(
        &self,
        name: &str,
        member: &str,
        active_members: &[String],
        busy_projects: &[String],
    ) -> Result<MemberReadiness> {
        let initiative = self.initiative_view(name)?;
        let definition = initiative
            .members
            .iter()
            .find(|definition| definition.name == member)
            .ok_or_else(|| InitiativeError::NotFound(format!("member '{member}' not found")))?;
        let mut prerequisites = definition.after.clone();
        for prerequisite in initiative
            .holds
            .iter()
            .filter(|hold| hold.member == member)
            .flat_map(|hold| &hold.until)
        {
            if !prerequisites.contains(prerequisite) {
                prerequisites.push(prerequisite.clone());
            }
        }
        let dependencies = prerequisites
            .iter()
            .map(|prerequisite| self.dependency_readiness(name, prerequisite))
            .collect::<Result<Vec<_>>>()?;
        let mut readiness = MemberReadiness {
            member: member.into(),
            ready: true,
            blockers: vec![],
            dependencies,
        };
        self.readiness_controls(&initiative, definition, &mut readiness);
        self.readiness_capacity(
            &initiative,
            member,
            active_members,
            busy_projects,
            &mut readiness,
        )?;
        Ok(readiness)
    }

    fn readiness_controls(
        &self,
        initiative: &Initiative,
        definition: &super::InitiativeMember,
        readiness: &mut MemberReadiness,
    ) {
        use ReadinessBlockerKind as Kind;
        let name = &initiative.name;
        let member = &definition.name;
        if initiative.status.is_terminal() {
            readiness.block(
                Kind::InitiativeTerminal,
                "initiative is terminal or has not been authorized".into(),
            );
        }
        if initiative.authorized_by.as_ref().is_none_or(|owner| owner.trim().is_empty()) {
            readiness.block(
                Kind::Unauthorized,
                "initiative is terminal or has not been authorized".into(),
            );
        }
        if initiative.completion_proposal.is_some() {
            readiness
                .block(Kind::CompletionPending, "done proposal awaits owner confirmation".into());
        }
        if let Some(campaign) = self.find(member) {
            if campaign.status == CampaignStatus::Paused {
                readiness.block(Kind::CampaignPaused, "campaign is paused".into());
            }
            if campaign.status.is_terminal() {
                readiness.block(Kind::CampaignTerminal, format!("campaign is {}", campaign.status));
            }
            if let Some(recovery) =
                campaign.recovery.as_ref().filter(|recovery| recovery.is_pending())
            {
                readiness.block(
                    Kind::RecoveryPending,
                    format!(
                        "member '{member}' waits on recovery '{}': {}",
                        recovery.id, recovery.detail
                    ),
                );
            }
        }
        if definition
            .intent_state
            .as_deref()
            .is_some_and(|state| matches!(state, "discarded" | "plan" | "revoked"))
        {
            readiness.block(Kind::IntentInactive, "member intent was discarded or revoked".into());
        }
        if initiative
            .open_questions
            .iter()
            .chain(&initiative.deferred_questions)
            .any(|question| {
                question.members.is_empty() || question.members.iter().any(|value| value == member)
            })
        {
            readiness.block(
                Kind::OwnerQuestion,
                format!("member '{member}' waits on an initiative owner question"),
            );
        }
        for hold in initiative.holds.iter().filter(|hold| hold.member == *member) {
            if hold.until.is_empty()
                || hold.until.iter().any(|prerequisite| {
                    self.dependency_readiness(name, prerequisite)
                        .map_or(true, |readiness| !readiness.satisfied)
                })
            {
                readiness.block(Kind::Held, format!("member '{member}' is held: {}", hold.reason));
            }
        }
        let waiting: Vec<_> = readiness
            .dependencies
            .iter()
            .filter(|dependency| !dependency.satisfied)
            .map(|dependency| dependency.prerequisite.clone())
            .collect();
        for prerequisite in waiting {
            readiness.block(
                Kind::DependencyUnmet,
                format!("member '{member}' waits on '{prerequisite}'"),
            );
        }
    }

    fn readiness_capacity(
        &self,
        initiative: &Initiative,
        member: &str,
        active_members: &[String],
        busy_projects: &[String],
        readiness: &mut MemberReadiness,
    ) -> Result<()> {
        use ReadinessBlockerKind as Kind;
        let occupied = self.occupied_member_slots(&initiative.name, active_members)?;
        if occupied.contains(member) {
            readiness.block(Kind::WorkInFlight, "has work in flight".into());
        }
        if !occupied.contains(member) && occupied.len() >= initiative.budget.max_active_members {
            readiness.block(
                Kind::MemberLimit,
                format!("initiative '{}' has reached its active member limit", initiative.name),
            );
        }
        if self.shared_cycles_used(&initiative.name)? >= initiative.budget.max_cycles {
            readiness.block(
                Kind::BudgetExhausted,
                format!("initiative '{}' exhausted its cycle budget", initiative.name),
            );
        }
        let free = |project: &str| !busy_projects.iter().any(|busy| busy == project);
        let available = self.find(member).map_or_else(
            || initiative.projects.iter().any(|project| free(project)),
            |campaign| free(&campaign.project),
        );
        if !available {
            readiness.block(
                Kind::RepositoryBusy,
                "its repository has an execution or cleanup owner".into(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
