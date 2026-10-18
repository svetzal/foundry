//! Persist operational recheck evidence without creating owner authority.
use crate::proto;
use chrono::Utc;
use foundry_sdk::{
    campaign::{CampaignStatus, CampaignStore},
    event::{Event, EventType},
    payload::{CampaignAdvanceRequestedPayload, InitiativeAdvanceRequestedPayload},
    recovery::RecoveryState,
    throttle::Throttle,
    work_source::WorkSource,
};
use std::path::Path;
use tonic::Status;

pub(super) fn to_proto(state: &RecoveryState) -> proto::RecoveryState {
    proto::RecoveryState {
        id: state.id.clone(),
        kind: state.kind.to_string(),
        detail: state.detail.clone(),
        recorded_at: state.recorded_at.to_rfc3339(),
        resolution: state.resolution.as_ref().map(|resolution| proto::RecoveryResolution {
            evidence: resolution.evidence.clone(),
            resolved_at: resolution.resolved_at.to_rfc3339(),
        }),
    }
}

pub(super) struct Recheck {
    pub response: proto::RecoverResponse,
    pub fact: Event,
    pub advance: Option<Event>,
}

pub(super) fn campaign(path: &Path, request: &proto::RecoverRequest) -> Result<Recheck, Status> {
    let mut guard =
        CampaignStore::lock_exclusive(path).map_err(|error| Status::internal(error.to_string()))?;
    guard
        .store
        .recover_campaign(&request.name, &request.recovery_id, &request.evidence, Utc::now())
        .map_err(super::initiative_ops::error)?;
    let campaign = guard
        .store
        .find(&request.name)
        .ok_or_else(|| Status::not_found(&request.name))?;
    let recovery = campaign
        .recovery
        .as_ref()
        .ok_or_else(|| Status::internal("recovery state missing after recheck"))?;
    let source = campaign.initiative.as_ref().map_or_else(
        || WorkSource::campaign(&campaign.name, campaign.cycles_completed.saturating_add(1)),
        |name| WorkSource::initiative(name, campaign.cycles_completed.saturating_add(1)),
    );
    let waiting = if campaign.status == CampaignStatus::Active {
        guard
            .store
            .enforce_can_form(&request.name, &[])
            .err()
            .map(|error| error.to_string())
    } else {
        Some(format!("campaign is {}; its owner controls are unchanged", campaign.status))
    };
    let advance = if waiting.is_none() {
        let payload = Event::serialize_payload(&CampaignAdvanceRequestedPayload {
            campaign: request.name.clone(),
            run_event_id: None,
            run_result: None,
            operator_origin: Some(format!("recovery recheck {}", request.recovery_id)),
            providers: foundry_sdk::gateway::StageProviders::default(),
        })
        .map_err(|error| Status::internal(error.to_string()))?;
        Some(
            Event::new(
                EventType::CampaignAdvanceRequested,
                campaign.project.clone(),
                Throttle::Full,
                payload,
            )
            .with_source(Some(source.clone()))
            .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
            .with_span_ids(Some(foundry_sdk::event::mint_span_id()), None),
        )
    } else {
        None
    };
    let fact = Event::new(
        EventType::CampaignRecoveryRechecked,
        campaign.project.clone(),
        Throttle::Full,
        serde_json::json!({"campaign":request.name,"recovery":recovery}),
    )
    .with_source(Some(source));
    let response = response(recovery, advance.as_ref(), waiting);
    guard.save().map_err(|error| Status::internal(error.to_string()))?;
    Ok(Recheck {
        response,
        fact,
        advance,
    })
}

pub(super) fn initiative(path: &Path, request: &proto::RecoverRequest) -> Result<Recheck, Status> {
    let mut guard =
        CampaignStore::lock_exclusive(path).map_err(|error| Status::internal(error.to_string()))?;
    guard
        .store
        .recover_initiative(&request.name, &request.recovery_id, &request.evidence, Utc::now())
        .map_err(super::initiative_ops::error)?;
    let initiative = guard
        .store
        .find_initiative(&request.name)
        .ok_or_else(|| Status::not_found(&request.name))?;
    let recovery = initiative
        .recovery
        .as_ref()
        .ok_or_else(|| Status::internal("recovery state missing after recheck"))?;
    let source = Some(WorkSource::initiative(&request.name, 0));
    let payload = Event::serialize_payload(&InitiativeAdvanceRequestedPayload {
        initiative: request.name.clone(),
        member: None,
        operator_origin: Some(format!("recovery recheck {}", request.recovery_id)),
        trigger_id: None,
        follow_up_of: None,
    })
    .map_err(|error| Status::internal(error.to_string()))?;
    let advance = Some(
        Event::new(EventType::InitiativeAdvanceRequested, "system".into(), Throttle::Full, payload)
            .with_source(source.clone()),
    );
    let fact = Event::new(
        EventType::InitiativeRecoveryRechecked,
        "system".into(),
        Throttle::Full,
        serde_json::json!({"initiative":request.name,"recovery":recovery}),
    )
    .with_source(source);
    let response = response(recovery, advance.as_ref(), None);
    guard.save().map_err(|error| Status::internal(error.to_string()))?;
    Ok(Recheck {
        response,
        fact,
        advance,
    })
}

fn response(
    recovery: &RecoveryState,
    event: Option<&Event>,
    waiting: Option<String>,
) -> proto::RecoverResponse {
    proto::RecoverResponse {
        recovery: Some(to_proto(recovery)),
        event_id: event.map_or_else(String::new, |event| event.id.clone()),
        waiting_reason: waiting.unwrap_or_default(),
    }
}
