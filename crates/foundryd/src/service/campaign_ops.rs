use std::path::Path;

use tonic::{Request, Response, Status};

use foundry_sdk::campaign::{CampaignStatus, CampaignStore, DoneEvidence};
use foundry_sdk::error::StoreError;

use crate::proto::{
    Campaign as ProtoCampaign, CampaignDetail, DoneEvidence as ProtoDoneEvidence,
    GetCampaignRequest, GetCampaignResponse, ListCampaignsRequest, ListCampaignsResponse,
};

fn campaign_to_proto(campaign: &foundry_sdk::campaign::Campaign) -> ProtoCampaign {
    ProtoCampaign {
        name: campaign.name.clone(),
        project: campaign.project.clone(),
        mission: campaign.mission.clone(),
        status: status_str(campaign.status),
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

fn status_str(status: CampaignStatus) -> String {
    match status {
        CampaignStatus::Staged => "staged",
        CampaignStatus::Active => "active",
        CampaignStatus::Paused => "paused",
        CampaignStatus::Escalated => "escalated",
        CampaignStatus::Completed => "completed",
    }
    .to_string()
}

fn done_evidence_to_proto(evidence: &DoneEvidence) -> ProtoDoneEvidence {
    match evidence {
        DoneEvidence::Gate { command, required } => ProtoDoneEvidence {
            kind: "gate".to_string(),
            command: command.clone(),
            required: *required,
            statement: String::new(),
        },
        DoneEvidence::Review { statement } => ProtoDoneEvidence {
            kind: "review".to_string(),
            command: String::new(),
            required: false,
            statement: statement.clone(),
        },
    }
}

fn campaign_to_detail(campaign: &foundry_sdk::campaign::Campaign) -> CampaignDetail {
    CampaignDetail {
        name: campaign.name.clone(),
        project: campaign.project.clone(),
        mission: campaign.mission.clone(),
        status: status_str(campaign.status),
        cycles_completed: campaign.cycles_completed,
        cycles_landed: campaign.cycles_landed,
        max_cycles: campaign.budget.max_cycles,
        authorized_by: campaign.authorized_by.clone().unwrap_or_default(),
        agent_provider: campaign.agent_provider.clone().unwrap_or_default(),
        last_run_event_id: campaign.last_run_event_id.clone().unwrap_or_default(),
        intent_refs: campaign.intent_refs.clone(),
        context_paths: campaign.context_paths.clone(),
        done_evidence: campaign.done_evidence.iter().map(done_evidence_to_proto).collect(),
        escalation: campaign.escalation.clone(),
    }
}

pub(super) fn get(
    campaigns_path: &Path,
    request: Request<GetCampaignRequest>,
) -> Result<Response<GetCampaignResponse>, Status> {
    let name = request.into_inner().name;
    let store = load_store(campaigns_path)?;
    match store.find(&name) {
        Some(campaign) => Ok(Response::new(GetCampaignResponse {
            campaign: Some(campaign_to_detail(campaign)),
        })),
        None => Err(Status::not_found(format!("campaign '{name}' not found"))),
    }
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

    // ── GetCampaign unit tests ────────────────────────────────────────────────

    fn write_store_with(campaigns: Vec<Campaign>) -> tempfile::NamedTempFile {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let store = CampaignStore {
            version: 1,
            campaigns,
        };
        store.save(tmp.path()).expect("save store");
        tmp
    }

    #[test]
    fn get_returns_full_detail_including_gate_and_review_evidence() {
        let campaign = Campaign {
            name: "my-campaign".to_string(),
            project: "my-project".to_string(),
            mission: "Prove quality.".to_string(),
            intent_refs: vec!["intent.one".to_string(), "intent.two".to_string()],
            context_paths: vec!["docs/context.md".to_string()],
            done_evidence: vec![
                DoneEvidence::Gate {
                    command: "cargo test --workspace".to_string(),
                    required: true,
                },
                DoneEvidence::Review {
                    statement: "All reviewers approved.".to_string(),
                },
            ],
            budget: CampaignBudget { max_cycles: 5 },
            escalation: vec!["Ping the team.".to_string()],
            status: CampaignStatus::Active,
            cycles_completed: 3,
            cycles_landed: 2,
            authorized_by: Some("alice".to_string()),
            agent_provider: Some("claude".to_string()),
            last_run_event_id: Some("evt-99".to_string()),
        };
        let tmp = write_store_with(vec![campaign]);

        let request = Request::new(GetCampaignRequest {
            name: "my-campaign".to_string(),
        });
        let response = get(tmp.path(), request).expect("get should succeed");
        let detail = response.into_inner().campaign.expect("campaign present");

        assert_eq!(detail.name, "my-campaign");
        assert_eq!(detail.project, "my-project");
        assert_eq!(detail.mission, "Prove quality.");
        assert_eq!(detail.status, "active");
        assert_eq!(detail.cycles_completed, 3);
        assert_eq!(detail.cycles_landed, 2);
        assert_eq!(detail.max_cycles, 5);
        assert_eq!(detail.authorized_by, "alice");
        assert_eq!(detail.agent_provider, "claude");
        assert_eq!(detail.last_run_event_id, "evt-99");
        assert_eq!(detail.intent_refs, vec!["intent.one", "intent.two"]);
        assert_eq!(detail.context_paths, vec!["docs/context.md"]);
        assert_eq!(detail.escalation, vec!["Ping the team."]);

        // Verify Gate evidence: kind, command, required flag
        assert_eq!(detail.done_evidence.len(), 2);
        let gate = &detail.done_evidence[0];
        assert_eq!(gate.kind, "gate");
        assert_eq!(gate.command, "cargo test --workspace");
        assert!(gate.required, "gate.required must be true");
        assert_eq!(gate.statement, "");

        // Verify Review evidence: kind, statement
        let review = &detail.done_evidence[1];
        assert_eq!(review.kind, "review");
        assert_eq!(review.statement, "All reviewers approved.");
        assert_eq!(review.command, "");
        assert!(!review.required);
    }

    #[test]
    fn get_returns_not_found_for_absent_name_in_non_empty_store() {
        // The store has one campaign — but not the requested name.
        let tmp = write_store_with(vec![sample_campaign("other", "proj")]);
        let request = Request::new(GetCampaignRequest {
            name: "does-not-exist".to_string(),
        });
        let err = get(tmp.path(), request).expect_err("should be not found");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[test]
    fn get_returns_failed_precondition_on_malformed_store() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), b"not valid json at all }{").expect("write");
        let request = Request::new(GetCampaignRequest {
            name: "anything".to_string(),
        });
        let err = get(tmp.path(), request).expect_err("should fail");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[test]
    fn get_returns_internal_on_unreadable_store() {
        // Use a directory path — CampaignStore::load on a directory produces an Io error.
        let tmp = tempfile::tempdir().expect("tempdir");
        let request = Request::new(GetCampaignRequest {
            name: "anything".to_string(),
        });
        let err = get(tmp.path(), request).expect_err("should fail");
        assert_eq!(err.code(), tonic::Code::Internal);
    }
}
