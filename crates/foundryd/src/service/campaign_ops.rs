use std::path::Path;

use chrono::Utc;
use tonic::{Request, Response, Status};

use foundry_sdk::campaign::{
    CampaignStatus, CampaignStore, CampaignStoreGuard, DoneEvidence, OwnerDecision,
};
use foundry_sdk::error::StoreError;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::CampaignAdvanceRequestedPayload;
use foundry_sdk::throttle::Throttle;

use crate::proto::{
    AdvanceCampaignRequest, AdvanceCampaignResponse, Campaign as ProtoCampaign, CampaignDetail,
    DecideCampaignRequest, DecideCampaignResponse, DoneEvidence as ProtoDoneEvidence,
    GetCampaignRequest, GetCampaignResponse, ListCampaignsRequest, ListCampaignsResponse,
    OwnerDecision as ProtoOwnerDecision, PauseCampaignRequest, PauseCampaignResponse,
    ResumeCampaignRequest, ResumeCampaignResponse,
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

fn map_store_error(error: StoreError) -> Status {
    match error {
        StoreError::Parse { source, .. } => {
            Status::failed_precondition(format!("campaign store is malformed: {source}"))
        }
        StoreError::Io { source, .. } => {
            Status::internal(format!("campaign store is unreadable: {source}"))
        }
        StoreError::NotFound { .. } => unreachable!("campaign store treats missing files as empty"),
    }
}

fn map_save_error(error: StoreError) -> Status {
    match error {
        StoreError::Parse { source, .. } => {
            Status::internal(format!("campaign store serialization failed: {source}"))
        }
        StoreError::Io { source, .. } => {
            Status::internal(format!("campaign store save failed: {source}"))
        }
        StoreError::NotFound { .. } => unreachable!("save never returns NotFound"),
    }
}

fn load_store(path: &Path) -> Result<CampaignStore, Status> {
    CampaignStore::load(path).map_err(map_store_error)
}

fn lock_store_exclusive(path: &Path) -> Result<CampaignStoreGuard, Status> {
    CampaignStore::lock_exclusive(path).map_err(map_store_error)
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
        DoneEvidence::Gate {
            command,
            required,
            artifacts,
        } => ProtoDoneEvidence {
            kind: "gate".to_string(),
            command: command.clone(),
            required: *required,
            statement: String::new(),
            artifacts: artifacts.clone(),
        },
        DoneEvidence::Review { statement } => ProtoDoneEvidence {
            kind: "review".to_string(),
            command: String::new(),
            required: false,
            statement: statement.clone(),
            artifacts: vec![],
        },
    }
}

fn owner_decision_to_proto(decision: &OwnerDecision) -> ProtoOwnerDecision {
    ProtoOwnerDecision {
        decision: decision.decision.clone(),
        authorized_by: decision.authorized_by.clone(),
        decided_at: decision.decided_at.to_rfc3339(),
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
        owner_decisions: campaign.owner_decisions.iter().map(owner_decision_to_proto).collect(),
    }
}

fn invalid_state(name: &str, detail: &str) -> Status {
    Status::failed_precondition(format!("campaign '{name}' {detail}"))
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

/// Pause a campaign, preserving any stored `pending_run_result`.
///
/// Status is unconditionally set to `Paused`; `pending_run_result` is never
/// cleared or overwritten by this operation.
pub(super) fn pause(
    campaigns_path: &Path,
    request: Request<PauseCampaignRequest>,
) -> Result<Response<PauseCampaignResponse>, Status> {
    let name = request.into_inner().name;
    let mut guard = lock_store_exclusive(campaigns_path)?;
    let campaign = guard
        .store
        .find_mut(&name)
        .ok_or_else(|| Status::not_found(format!("campaign '{name}' not found")))?;
    campaign.status = CampaignStatus::Paused;
    // pending_run_result is intentionally left untouched.
    let detail = campaign_to_detail(campaign);
    guard.save().map_err(map_save_error)?;
    Ok(Response::new(PauseCampaignResponse {
        campaign: Some(detail),
    }))
}

/// Resume a paused or escalated campaign, optionally extending its cycle budget.
///
/// Requires `authorized_by` to be set (`FAILED_PRECONDITION` otherwise).
/// Valid only when the campaign status is `Paused` or `Escalated`
/// (`FAILED_PRECONDITION` for all other statuses).
///
/// When `add_cycles == 0` and `cycles_completed >= budget.max_cycles` the
/// budget is exhausted; the caller must pass a positive `add_cycles` to
/// explicitly authorize more work (`FAILED_PRECONDITION` otherwise).
///
/// `add_cycles` is applied to `budget.max_cycles` via checked addition;
/// overflow returns `FAILED_PRECONDITION`.
///
/// `pending_run_result` is intentionally left untouched — it remains pending
/// for the next manual advance to consume.
pub(super) fn resume(
    campaigns_path: &Path,
    request: Request<ResumeCampaignRequest>,
) -> Result<Response<ResumeCampaignResponse>, Status> {
    let req = request.into_inner();
    let name = req.name;
    let add_cycles = req.add_cycles;

    let mut guard = lock_store_exclusive(campaigns_path)?;
    let campaign = guard
        .store
        .find_mut(&name)
        .ok_or_else(|| Status::not_found(format!("campaign '{name}' not found")))?;

    if campaign.authorized_by.is_none() {
        return Err(Status::failed_precondition(format!(
            "campaign '{name}' has not been authorized; resume requires an authorized_by owner"
        )));
    }
    if campaign.status != CampaignStatus::Paused && campaign.status != CampaignStatus::Escalated {
        return Err(Status::failed_precondition(format!(
            "campaign '{name}' is '{}'; resume requires Paused or Escalated status",
            campaign.status
        )));
    }
    if add_cycles == 0 && campaign.cycles_completed >= campaign.budget.max_cycles {
        return Err(Status::failed_precondition(format!(
            "campaign '{name}' exhausted its cycle budget; pass add_cycles > 0 to authorize more work"
        )));
    }
    if add_cycles > 0 {
        campaign.budget.max_cycles =
            campaign.budget.max_cycles.checked_add(add_cycles).ok_or_else(|| {
                Status::failed_precondition(format!(
                    "add_cycles ({add_cycles}) would overflow max_cycles for campaign '{name}'"
                ))
            })?;
    }
    campaign.status = CampaignStatus::Active;
    // pending_run_result is intentionally left untouched.
    let detail = campaign_to_detail(campaign);
    guard.save().map_err(map_save_error)?;
    Ok(Response::new(ResumeCampaignResponse {
        campaign: Some(detail),
    }))
}

/// Record an owner decision on an escalated campaign and return it to active state.
pub(super) fn decide(
    campaigns_path: &Path,
    request: Request<DecideCampaignRequest>,
) -> Result<Response<DecideCampaignResponse>, Status> {
    let req = request.into_inner();
    let name = req.name;
    let decision = req.decision.trim().to_string();
    if decision.is_empty() {
        return Err(Status::invalid_argument("decision must be non-empty"));
    }

    let mut guard = lock_store_exclusive(campaigns_path)?;
    let campaign = guard
        .store
        .find_mut(&name)
        .ok_or_else(|| Status::not_found(format!("campaign '{name}' not found")))?;

    let authorized_by = campaign.authorized_by.clone().ok_or_else(|| {
        invalid_state(&name, "has not been authorized; decide requires authorized_by")
    })?;
    if campaign.status != CampaignStatus::Escalated {
        return Err(invalid_state(
            &name,
            &format!("is '{}'; decide requires Escalated status", campaign.status),
        ));
    }

    campaign.owner_decisions.push(OwnerDecision {
        decision,
        authorized_by,
        decided_at: Utc::now(),
    });
    campaign.status = CampaignStatus::Active;
    let detail = campaign_to_detail(campaign);
    guard.save().map_err(map_save_error)?;
    Ok(Response::new(DecideCampaignResponse {
        campaign: Some(detail),
    }))
}

/// Dispatch one manual advance iteration for an active (or staged) campaign.
///
/// Validates campaign existence and status under the exclusive lock, releases
/// the lock, then emits `CampaignAdvanceRequested` through the engine.
/// Returns `FAILED_PRECONDITION` for `paused`, `escalated`, or `completed`
/// campaigns.
pub(super) fn advance(
    campaigns_path: &Path,
    ctx: &super::RuntimeContext,
    request: Request<AdvanceCampaignRequest>,
) -> Result<Response<AdvanceCampaignResponse>, Status> {
    let name = request.into_inner().name;

    // Validate under the exclusive lock; release before emitting to avoid a
    // deadlock with the AdvanceCampaign block which also acquires the lock.
    let (detail, project) = {
        let guard = lock_store_exclusive(campaigns_path)?;
        let campaign = guard
            .store
            .find(&name)
            .ok_or_else(|| Status::not_found(format!("campaign '{name}' not found")))?;

        match campaign.status {
            CampaignStatus::Active | CampaignStatus::Staged => {}
            status => {
                return Err(Status::failed_precondition(format!(
                    "campaign '{name}' is '{status}'; advance requires Active or Staged status"
                )));
            }
        }

        (campaign_to_detail(campaign), campaign.project.clone())
        // guard drops here, releasing the exclusive lock before the event is emitted.
    };

    // Emit CampaignAdvanceRequested through the existing engine path.
    let payload = Event::serialize_payload(&CampaignAdvanceRequestedPayload {
        campaign: name,
        run_event_id: None,
        run_result: None,
    })
    .map_err(|e| Status::internal(format!("failed to serialize advance payload: {e}")))?;

    let event = Event::new(EventType::CampaignAdvanceRequested, project, Throttle::Full, payload);

    super::spawn_workflow(event, ctx);

    Ok(Response::new(AdvanceCampaignResponse {
        campaign: Some(detail),
    }))
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
    use foundry_sdk::campaign::{Campaign, CampaignBudget, DoneEvidence, OwnerDecision};

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
            owner_decisions: vec![],
            pending_run_result: None,
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
                    artifacts: vec!["tests/campaign_detail.rs".to_string()],
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
            owner_decisions: vec![OwnerDecision {
                decision: "Proceed with the migration.".to_string(),
                authorized_by: "alice".to_string(),
                decided_at: chrono::DateTime::parse_from_rfc3339("2026-07-18T10:00:00Z")
                    .expect("parse RFC3339 timestamp")
                    .with_timezone(&Utc),
            }],
            pending_run_result: None,
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
        assert_eq!(detail.owner_decisions.len(), 1);
        assert_eq!(detail.owner_decisions[0].decision, "Proceed with the migration.");
        assert_eq!(detail.owner_decisions[0].authorized_by, "alice");
        assert_eq!(detail.owner_decisions[0].decided_at, "2026-07-18T10:00:00+00:00");

        // Verify Gate evidence: kind, command, required flag
        assert_eq!(detail.done_evidence.len(), 2);
        let gate = &detail.done_evidence[0];
        assert_eq!(gate.kind, "gate");
        assert_eq!(gate.command, "cargo test --workspace");
        assert!(gate.required, "gate.required must be true");
        assert_eq!(gate.statement, "");
        assert_eq!(gate.artifacts, vec!["tests/campaign_detail.rs"]);

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
