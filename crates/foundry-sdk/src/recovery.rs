//! Operational recovery is evidence for reassessment, never an owner decision.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{campaign::CampaignStore, initiative::InitiativeError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryKind {
    CapabilityUnavailable,
    AccessUnavailable,
    EnvironmentUnavailable,
    MissingEvidence,
    UnmetPrecondition,
    RunnerUnavailable,
}

impl RecoveryKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CapabilityUnavailable => "capability_unavailable",
            Self::AccessUnavailable => "access_unavailable",
            Self::EnvironmentUnavailable => "environment_unavailable",
            Self::MissingEvidence => "missing_evidence",
            Self::UnmetPrecondition => "unmet_precondition",
            Self::RunnerUnavailable => "runner_unavailable",
        }
    }
}
impl std::fmt::Display for RecoveryKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryResolution {
    pub evidence: Vec<String>,
    pub resolved_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryState {
    pub id: String,
    pub kind: RecoveryKind,
    pub detail: String,
    pub recorded_at: DateTime<Utc>,
    #[serde(default)]
    pub resolution: Option<RecoveryResolution>,
}

impl RecoveryState {
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.resolution.is_none()
    }

    fn resolve(
        &mut self,
        id: &str,
        evidence: &[String],
        at: DateTime<Utc>,
    ) -> Result<(), InitiativeError> {
        if self.id != id || !self.is_pending() {
            return Err(InitiativeError::Precondition(
                "recovery identity is stale or already resolved".into(),
            ));
        }
        if evidence.is_empty() || evidence.iter().any(|item| item.trim().is_empty()) {
            return Err(InitiativeError::Invalid("recovery requires non-empty evidence".into()));
        }
        self.resolution = Some(RecoveryResolution {
            evidence: evidence.iter().map(|item| item.trim().to_owned()).collect(),
            resolved_at: at,
        });
        Ok(())
    }
}

fn record(
    slot: &mut Option<RecoveryState>,
    id: &str,
    kind: RecoveryKind,
    detail: &str,
    at: DateTime<Utc>,
) -> Result<(), InitiativeError> {
    if id.trim().is_empty() || detail.trim().is_empty() {
        return Err(InitiativeError::Invalid("recovery requires an identity and detail".into()));
    }
    if let Some(existing) = slot.as_ref().filter(|state| state.is_pending()) {
        if existing.id == id && existing.kind == kind && existing.detail == detail {
            return Ok(());
        }
        let reassessable =
            |kind| matches!(kind, RecoveryKind::MissingEvidence | RecoveryKind::UnmetPrecondition);
        if existing.id == id || !reassessable(existing.kind) || !reassessable(kind) {
            return Err(InitiativeError::Precondition(format!(
                "recovery '{}' is still pending",
                existing.id
            )));
        }
        // Fresh steering verifies the current snapshot before recording this
        // replacement. A changed completion prerequisite supersedes the old
        // wait; neither record grants readiness or resolves an owner question.
    }
    *slot = Some(RecoveryState {
        id: id.into(),
        kind,
        detail: detail.into(),
        recorded_at: at,
        resolution: None,
    });
    Ok(())
}

impl CampaignStore {
    pub fn record_campaign_recovery(
        &mut self,
        name: &str,
        id: &str,
        kind: RecoveryKind,
        detail: &str,
        at: DateTime<Utc>,
    ) -> Result<(), InitiativeError> {
        let campaign = self.find_mut(name).ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        record(&mut campaign.recovery, id, kind, detail, at)
    }
    pub fn record_initiative_recovery(
        &mut self,
        name: &str,
        id: &str,
        kind: RecoveryKind,
        detail: &str,
        at: DateTime<Utc>,
    ) -> Result<(), InitiativeError> {
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        record(&mut initiative.recovery, id, kind, detail, at)
    }
    pub fn recover_campaign(
        &mut self,
        name: &str,
        id: &str,
        evidence: &[String],
        at: DateTime<Utc>,
    ) -> Result<(), InitiativeError> {
        let campaign = self.find_mut(name).ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        if campaign.status.is_terminal()
            || campaign.authorized_by.as_ref().is_none_or(|owner| owner.trim().is_empty())
        {
            return Err(InitiativeError::Precondition(
                "recovery requires a nonterminal authorized campaign".into(),
            ));
        }
        campaign
            .recovery
            .as_mut()
            .ok_or_else(|| InitiativeError::Precondition("campaign has no recovery".into()))?
            .resolve(id, evidence, at)
    }
    pub fn recover_initiative(
        &mut self,
        name: &str,
        id: &str,
        evidence: &[String],
        at: DateTime<Utc>,
    ) -> Result<(), InitiativeError> {
        let initiative = self
            .find_initiative_mut(name)
            .ok_or_else(|| InitiativeError::NotFound(name.into()))?;
        if initiative.status.is_terminal()
            || initiative.authorized_by.as_ref().is_none_or(|owner| owner.trim().is_empty())
        {
            return Err(InitiativeError::Precondition(
                "recovery requires a nonterminal authorized initiative".into(),
            ));
        }
        initiative
            .recovery
            .as_mut()
            .ok_or_else(|| InitiativeError::Precondition("initiative has no recovery".into()))?
            .resolve(id, evidence, at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        campaign::{Campaign, CampaignStatus},
        initiative::{Initiative, InitiativeStatus},
    };

    fn store() -> CampaignStore {
        let mut store = CampaignStore::default();
        let campaign: Campaign = serde_json::from_value(serde_json::json!({"name":"campaign","project":"p","mission":"ship","authorized_by":"owner","status":"active"})).unwrap();
        let initiative: Initiative = serde_json::from_value(serde_json::json!({"name":"initiative","outcome":"ship","projects":["p"],"done_evidence":[],"authorized_by":"owner","status":"active"})).unwrap();
        store.campaigns.push(campaign);
        store.initiatives.push(initiative);
        store
    }

    #[test]
    fn resolution_matches_current_identity_requires_evidence_and_survives_restart() {
        let mut store = store();
        let at = Utc::now();
        store
            .record_campaign_recovery(
                "campaign",
                "recovery-run",
                RecoveryKind::RunnerUnavailable,
                "runner missing",
                at,
            )
            .unwrap();
        assert!(
            store
                .recover_campaign("campaign", "other", &["probe passed".into()], at)
                .is_err()
        );
        assert!(store.recover_campaign("campaign", "recovery-run", &[], at).is_err());
        assert!(store.recover_campaign("campaign", "recovery-run", &[" ".into()], at).is_err());
        store
            .recover_campaign("campaign", "recovery-run", &["probe passed".into()], at)
            .unwrap();
        let restarted: CampaignStore =
            serde_json::from_value(serde_json::to_value(&store).unwrap()).unwrap();
        assert!(!restarted.find("campaign").unwrap().recovery.as_ref().unwrap().is_pending());
        assert!(
            store
                .recover_campaign("campaign", "recovery-run", &["probe passed".into()], at)
                .is_err()
        );
    }

    #[test]
    fn operational_evidence_cannot_change_owner_pause_questions_authority_or_budget() {
        let mut store = store();
        let at = Utc::now();
        store
            .record_campaign_recovery(
                "campaign",
                "c",
                RecoveryKind::AccessUnavailable,
                "credential missing",
                at,
            )
            .unwrap();
        store.find_mut("campaign").unwrap().pause();
        let campaign_before = serde_json::to_value(store.find("campaign").unwrap()).unwrap();
        store
            .recover_campaign("campaign", "c", &["credential probe passed".into()], at)
            .unwrap();
        let mut campaign_after = serde_json::to_value(store.find("campaign").unwrap()).unwrap();
        campaign_after["recovery"] = campaign_before["recovery"].clone();
        assert_eq!(campaign_after, campaign_before);
        assert_eq!(store.find("campaign").unwrap().status, CampaignStatus::Paused);

        store
            .record_required_question("initiative", "Which contract is canonical?", &[])
            .unwrap();
        store
            .record_initiative_recovery(
                "initiative",
                "i",
                RecoveryKind::MissingEvidence,
                "probe missing",
                at,
            )
            .unwrap();
        let before = serde_json::to_value(store.find_initiative("initiative").unwrap()).unwrap();
        store
            .recover_initiative("initiative", "i", &["probe recorded".into()], at)
            .unwrap();
        let mut after = serde_json::to_value(store.find_initiative("initiative").unwrap()).unwrap();
        after["recovery"] = before["recovery"].clone();
        assert_eq!(before, after);
    }

    #[test]
    fn fresh_completion_prerequisite_supersedes_only_an_evidence_or_precondition_wait() {
        let mut store = store();
        let at = Utc::now();
        store
            .record_initiative_recovery(
                "initiative",
                "waiting-worker",
                RecoveryKind::UnmetPrecondition,
                "worker in flight",
                at,
            )
            .unwrap();
        store
            .record_initiative_recovery(
                "initiative",
                "missing-probe",
                RecoveryKind::MissingEvidence,
                "gate 1 has no current evidence",
                at,
            )
            .unwrap();
        let recovery = store.find_initiative("initiative").unwrap().recovery.as_ref().unwrap();
        assert_eq!(recovery.id, "missing-probe");
        assert!(recovery.is_pending());
        assert!(
            store
                .recover_initiative("initiative", "waiting-worker", &["worker settled".into()], at)
                .is_err(),
            "stale evidence must not clear the current prerequisite"
        );
        assert!(
            store
                .record_initiative_recovery(
                    "initiative",
                    "new-capability",
                    RecoveryKind::CapabilityUnavailable,
                    "macOS runner missing",
                    at
                )
                .is_err()
        );

        store
            .record_campaign_recovery(
                "campaign",
                "capability",
                RecoveryKind::CapabilityUnavailable,
                "macOS runner missing",
                at,
            )
            .unwrap();
        assert!(
            store
                .record_campaign_recovery(
                    "campaign",
                    "completion",
                    RecoveryKind::UnmetPrecondition,
                    "probe not recorded",
                    at
                )
                .is_err(),
            "a completion recheck cannot erase a capability requirement"
        );
        assert_eq!(store.find("campaign").unwrap().recovery.as_ref().unwrap().id, "capability");
    }

    #[test]
    fn terminal_or_unauthorized_work_cannot_be_recovered() {
        let mut store = store();
        let at = Utc::now();
        store
            .record_campaign_recovery(
                "campaign",
                "c",
                RecoveryKind::RunnerUnavailable,
                "missing",
                at,
            )
            .unwrap();
        store.find_mut("campaign").unwrap().authorized_by = None;
        assert!(store.recover_campaign("campaign", "c", &["passed".into()], at).is_err());
        store
            .record_initiative_recovery(
                "initiative",
                "i",
                RecoveryKind::MissingEvidence,
                "missing",
                at,
            )
            .unwrap();
        store.find_initiative_mut("initiative").unwrap().status = InitiativeStatus::Completed;
        assert!(store.recover_initiative("initiative", "i", &["passed".into()], at).is_err());
    }
}
