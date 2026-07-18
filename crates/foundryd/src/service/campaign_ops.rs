use std::path::Path;

use tonic::{Request, Response, Status};

use foundry_sdk::campaign::{CampaignStatus, CampaignStore, DoneEvidence};
use foundry_sdk::error::StoreError;

use crate::proto::{
    Campaign as ProtoCampaign, CampaignDetail as ProtoCampaignDetail,
    DoneEvidenceItem as ProtoDoneEvidenceItem, GetCampaignRequest, GetCampaignResponse,
    ListCampaignsRequest, ListCampaignsResponse,
};

fn campaign_to_proto(campaign: &foundry_sdk::campaign::Campaign) -> ProtoCampaign {
    ProtoCampaign {
        name: campaign.name.clone(),
        project: campaign.project.clone(),
        mission: campaign.mission.clone(),
        status: status_string(campaign.status),
        cycles_completed: campaign.cycles_completed,
        cycles_landed: campaign.cycles_landed,
        max_cycles: campaign.budget.max_cycles,
        authorized_by: campaign.authorized_by.clone().unwrap_or_default(),
        agent_provider: campaign.agent_provider.clone().unwrap_or_default(),
        last_run_event_id: campaign.last_run_event_id.clone().unwrap_or_default(),
    }
}

fn status_string(status: CampaignStatus) -> String {
    match status {
        CampaignStatus::Staged => "staged".to_string(),
        CampaignStatus::Active => "active".to_string(),
        CampaignStatus::Paused => "paused".to_string(),
        CampaignStatus::Escalated => "escalated".to_string(),
        CampaignStatus::Completed => "completed".to_string(),
    }
}

fn campaign_to_detail_proto(campaign: &foundry_sdk::campaign::Campaign) -> ProtoCampaignDetail {
    ProtoCampaignDetail {
        name: campaign.name.clone(),
        project: campaign.project.clone(),
        mission: campaign.mission.clone(),
        intent_refs: campaign.intent_refs.clone(),
        context_paths: campaign.context_paths.clone(),
        done_evidence: campaign
            .done_evidence
            .iter()
            .map(|item| match item {
                DoneEvidence::Gate { command, required } => ProtoDoneEvidenceItem {
                    kind: "gate".to_string(),
                    command: command.clone(),
                    required: *required,
                    statement: String::new(),
                },
                DoneEvidence::Review { statement } => ProtoDoneEvidenceItem {
                    kind: "review".to_string(),
                    command: String::new(),
                    required: false,
                    statement: statement.clone(),
                },
            })
            .collect(),
        escalation: campaign.escalation.clone(),
        authorized_by: campaign.authorized_by.clone().unwrap_or_default(),
        agent_provider: campaign.agent_provider.clone().unwrap_or_default(),
        max_cycles: campaign.budget.max_cycles,
        status: status_string(campaign.status),
        cycles_completed: campaign.cycles_completed,
        cycles_landed: campaign.cycles_landed,
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

pub(super) fn get(
    campaigns_path: &Path,
    request: Request<GetCampaignRequest>,
) -> Result<Response<GetCampaignResponse>, Status> {
    let req = request.into_inner();
    let store = load_store(campaigns_path)?;

    let campaign = store
        .campaigns
        .iter()
        .find(|c| c.name == req.name)
        .ok_or_else(|| Status::not_found(format!("campaign '{}' not found", req.name)))?;

    Ok(Response::new(GetCampaignResponse {
        campaign: Some(campaign_to_detail_proto(campaign)),
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

    #[test]
    fn campaign_to_detail_proto_maps_all_fields() {
        let campaign = Campaign {
            name: "detail-test".to_string(),
            project: "proj-x".to_string(),
            mission: "Prove correctness end to end.".to_string(),
            intent_refs: vec!["ref-a".to_string(), "ref-b".to_string()],
            context_paths: vec!["docs/context.md".to_string()],
            done_evidence: vec![
                DoneEvidence::Gate {
                    command: "cargo test".to_string(),
                    required: true,
                },
                DoneEvidence::Gate {
                    command: "cargo clippy".to_string(),
                    required: false,
                },
                DoneEvidence::Review {
                    statement: "No regressions observed.".to_string(),
                },
            ],
            budget: CampaignBudget { max_cycles: 5 },
            escalation: vec!["owner decision required".to_string()],
            status: CampaignStatus::Paused,
            cycles_completed: 3,
            cycles_landed: 2,
            authorized_by: Some("Alice".to_string()),
            agent_provider: Some("gemini".to_string()),
            last_run_event_id: Some("evt-7".to_string()),
        };

        let proto = campaign_to_detail_proto(&campaign);

        assert_eq!(proto.name, "detail-test");
        assert_eq!(proto.project, "proj-x");
        assert_eq!(proto.mission, "Prove correctness end to end.");
        assert_eq!(proto.intent_refs, vec!["ref-a", "ref-b"]);
        assert_eq!(proto.context_paths, vec!["docs/context.md"]);
        assert_eq!(proto.done_evidence.len(), 3);

        let required_gate = &proto.done_evidence[0];
        assert_eq!(required_gate.kind, "gate");
        assert_eq!(required_gate.command, "cargo test");
        assert!(required_gate.required);

        let optional_gate = &proto.done_evidence[1];
        assert_eq!(optional_gate.kind, "gate");
        assert_eq!(optional_gate.command, "cargo clippy");
        assert!(!optional_gate.required);

        let review = &proto.done_evidence[2];
        assert_eq!(review.kind, "review");
        assert_eq!(review.statement, "No regressions observed.");

        assert_eq!(proto.escalation, vec!["owner decision required"]);
        assert_eq!(proto.authorized_by, "Alice");
        assert_eq!(proto.agent_provider, "gemini");
        assert_eq!(proto.max_cycles, 5);
        assert_eq!(proto.status, "paused");
        assert_eq!(proto.cycles_completed, 3);
        assert_eq!(proto.cycles_landed, 2);
        assert_eq!(proto.last_run_event_id, "evt-7");
    }
}
