use std::path::Path;

use tonic::{Request, Response, Status};

use foundry_sdk::campaign::{CampaignStatus, CampaignStore};
use foundry_sdk::error::StoreError;

use crate::proto::{Campaign as ProtoCampaign, ListCampaignsRequest, ListCampaignsResponse};

fn campaign_to_proto(campaign: &foundry_sdk::campaign::Campaign) -> ProtoCampaign {
    ProtoCampaign {
        name: campaign.name.clone(),
        project: campaign.project.clone(),
        mission: campaign.mission.clone(),
        status: match campaign.status {
            CampaignStatus::Staged => "staged".to_string(),
            CampaignStatus::Active => "active".to_string(),
            CampaignStatus::Paused => "paused".to_string(),
            CampaignStatus::Escalated => "escalated".to_string(),
            CampaignStatus::Completed => "completed".to_string(),
        },
        cycles_completed: campaign.cycles_completed,
        cycles_landed: campaign.cycles_landed,
        max_cycles: campaign.budget.max_cycles,
        authorized_by: campaign.authorized_by.clone().unwrap_or_default(),
        agent_provider: campaign.agent_provider.clone().unwrap_or_default(),
        last_run_event_id: campaign.last_run_event_id.clone().unwrap_or_default(),
    }
}

fn load_store(path: &Path) -> Result<CampaignStore, Status> {
    CampaignStore::load(path).map_err(|error| match error {
        StoreError::Parse { source, .. } => {
            Status::failed_precondition(format!("campaign store is malformed: {source}"))
        }
        StoreError::Io { source, .. } => {
            Status::internal(format!("campaign store is unreadable: {source}"))
        }
        StoreError::NotFound { .. } => unreachable!("campaign store treats missing files as empty"),
    })
}

pub(super) fn list(
    campaigns_path: &Path,
    request: Request<ListCampaignsRequest>,
) -> Result<Response<ListCampaignsResponse>, Status> {
    let req = request.into_inner();
    let mut campaigns = load_store(campaigns_path)?.campaigns;

    if !req.project.is_empty() {
        campaigns.retain(|campaign| campaign.project == req.project);
    }

    campaigns.sort_by(|left, right| left.name.cmp(&right.name));

    Ok(Response::new(ListCampaignsResponse {
        campaigns: campaigns.iter().map(campaign_to_proto).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_sdk::campaign::{Campaign, CampaignBudget, DoneEvidence};

    fn sample_campaign(name: &str, project: &str) -> Campaign {
        Campaign {
            name: name.to_string(),
            project: project.to_string(),
            mission: format!("Mission for {name}"),
            intent_refs: vec![],
            context_paths: vec![],
            done_evidence: vec![DoneEvidence::Review {
                statement: "Reviewed.".to_string(),
            }],
            budget: CampaignBudget { max_cycles: 7 },
            escalation: vec![],
            status: CampaignStatus::Active,
            cycles_completed: 2,
            cycles_landed: 1,
            authorized_by: Some("Owner".to_string()),
            agent_provider: Some("codex".to_string()),
            last_run_event_id: Some("run-42".to_string()),
        }
    }

    #[test]
    fn campaign_to_proto_maps_summary_fields() {
        let campaign = sample_campaign("alpha", "proj");
        let proto = campaign_to_proto(&campaign);

        assert_eq!(proto.name, "alpha");
        assert_eq!(proto.project, "proj");
        assert_eq!(proto.mission, "Mission for alpha");
        assert_eq!(proto.status, "active");
        assert_eq!(proto.cycles_completed, 2);
        assert_eq!(proto.cycles_landed, 1);
        assert_eq!(proto.max_cycles, 7);
        assert_eq!(proto.authorized_by, "Owner");
        assert_eq!(proto.agent_provider, "codex");
        assert_eq!(proto.last_run_event_id, "run-42");
    }
}
