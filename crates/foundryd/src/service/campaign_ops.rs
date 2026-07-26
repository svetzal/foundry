use std::path::{Component, Path};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use tonic::{Request, Response, Status};

use foundry_sdk::campaign::{
    CampaignStore, CampaignStoreGuard, DoneEvidence, OwnerDecision, Transition, TransitionError,
};
use foundry_sdk::error::StoreError;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    CampaignAdvanceRequestedPayload, CampaignCancelledPayload, CampaignTerminalPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::throttle::Throttle;

use crate::proto::{
    AddCampaignRequest, AddCampaignResponse, AdvanceCampaignRequest, AdvanceCampaignResponse,
    Campaign as ProtoCampaign, CampaignDetail, CancelCampaignRequest, CancelCampaignResponse,
    CompleteCampaignRequest, CompleteCampaignResponse, DecideCampaignRequest,
    DecideCampaignResponse, DoneEvidence as ProtoDoneEvidence, GetCampaignRequest,
    GetCampaignResponse, ListCampaignsRequest, ListCampaignsResponse,
    OwnerDecision as ProtoOwnerDecision, PauseCampaignRequest, PauseCampaignResponse,
    ResumeCampaignRequest, ResumeCampaignResponse,
};

fn campaign_to_proto(campaign: &foundry_sdk::campaign::Campaign) -> ProtoCampaign {
    ProtoCampaign {
        name: campaign.name.clone(),
        project: campaign.project.clone(),
        mission: campaign.mission.clone(),
        status: campaign.status.to_string(),
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

/// Map a [`TransitionError`] to its gRPC status.
///
/// Every variant here is a rule violation the caller could have avoided by
/// checking state first, which is exactly what `FAILED_PRECONDITION` means.
fn map_transition_error(error: &TransitionError) -> Status {
    Status::failed_precondition(error.to_string())
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
        status: campaign.status.to_string(),
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

fn invalid_argument(message: impl Into<String>) -> Status {
    Status::invalid_argument(message.into())
}

fn validate_context_paths(
    campaign: &foundry_sdk::campaign::Campaign,
    repo_path: &Path,
) -> Result<(), Status> {
    let repo = repo_path.canonicalize().map_err(|e| {
        tracing::error!(
            error = %e,
            campaign = %campaign.name,
            project = %campaign.project,
            "validate_context_paths: project checkout is unreadable"
        );
        Status::failed_precondition(format!(
            "campaign '{}' project '{}' checkout is unreadable",
            campaign.name, campaign.project
        ))
    })?;
    for context_path in &campaign.context_paths {
        validate_context_path(campaign, &repo, context_path)?;
    }
    // Shared with the CLI's offline path: this is the default admission route,
    // so a budget enforced only in the CLI would never actually run.
    foundry_sdk::campaign::check_inline_context_budget(campaign, &repo)
        .map_err(Status::failed_precondition)
}

fn validate_context_path(
    campaign: &foundry_sdk::campaign::Campaign,
    repo: &Path,
    context_path: &str,
) -> Result<(), Status> {
    let relative = Path::new(context_path);
    if relative.is_absolute() {
        return Err(invalid_argument(format!(
            "campaign '{}' context path must be repository-relative: {context_path}",
            campaign.name
        )));
    }
    if relative.components().any(|component| matches!(component, Component::ParentDir)) {
        return Err(invalid_argument(format!(
            "campaign '{}' context path must not traverse parent directories: {context_path}",
            campaign.name
        )));
    }

    let candidate = repo.join(relative);
    if !candidate.exists() {
        return Err(Status::failed_precondition(format!(
            "campaign '{}' context path missing: {context_path}",
            campaign.name
        )));
    }

    let canonical = candidate.canonicalize().map_err(|e| {
        tracing::error!(
            error = %e,
            campaign = %campaign.name,
            context_path = %context_path,
            "validate_context_path: context path is unreadable"
        );
        Status::failed_precondition(format!(
            "campaign '{}' context path is unreadable: {context_path}",
            campaign.name
        ))
    })?;
    if !canonical.starts_with(repo) {
        return Err(invalid_argument(format!(
            "campaign '{}' context path escapes project checkout: {context_path}",
            campaign.name
        )));
    }
    if !canonical.is_file() {
        return Err(Status::failed_precondition(format!(
            "campaign '{}' context path must be a file: {context_path}",
            campaign.name
        )));
    }
    Ok(())
}

fn validate_campaign_definition(
    campaign: &foundry_sdk::campaign::Campaign,
    registry: &Arc<RwLock<Registry>>,
) -> Result<(), Status> {
    campaign.validate().map_err(|error| invalid_argument(error.to_string()))?;
    let registry = registry.read().map_err(|e| {
        tracing::error!(error = %e, "validate_campaign_definition: registry lock poisoned");
        Status::internal("registry lock poisoned")
    })?;
    let project = registry.find_project(&campaign.project).ok_or_else(|| {
        Status::failed_precondition(format!(
            "campaign '{}' references unknown registered project '{}'",
            campaign.name, campaign.project
        ))
    })?;
    validate_context_paths(campaign, Path::new(&project.path))
}

pub(super) fn add(
    campaigns_path: &Path,
    registry: &Arc<RwLock<Registry>>,
    request: Request<AddCampaignRequest>,
) -> Result<Response<AddCampaignResponse>, Status> {
    let definition_json = request.into_inner().definition_json;
    let campaign: foundry_sdk::campaign::Campaign = serde_json::from_str(&definition_json)
        .map_err(|source| {
            invalid_argument(format!("campaign definition JSON is invalid: {source}"))
        })?;
    validate_campaign_definition(&campaign, registry)?;

    let mut guard = lock_store_exclusive(campaigns_path)?;
    let name = campaign.name.clone();
    guard
        .store
        .add(campaign)
        .map_err(|error| Status::already_exists(error.to_string()))?;
    let detail = campaign_to_detail(
        guard
            .store
            .find(&name)
            .ok_or_else(|| Status::internal("campaign disappeared before save"))?,
    );
    guard.save().map_err(map_save_error)?;
    Ok(Response::new(AddCampaignResponse {
        campaign: Some(detail),
    }))
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
    campaign.pause();
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

    campaign.resume(add_cycles).map_err(|e| map_transition_error(&e))?;
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

    campaign
        .record_owner_decision(decision, Utc::now())
        .map_err(|e| map_transition_error(&e))?;
    let detail = campaign_to_detail(campaign);
    guard.save().map_err(map_save_error)?;
    Ok(Response::new(DecideCampaignResponse {
        campaign: Some(detail),
    }))
}

/// Mark an authorized campaign complete outside the formation loop.
///
/// The reason is retained in the append-only owner record and a terminal event
/// is emitted so existing digest and observability blocks see the completion.
pub(super) fn complete(
    campaigns_path: &Path,
    ctx: &super::RuntimeContext,
    request: Request<CompleteCampaignRequest>,
) -> Result<Response<CompleteCampaignResponse>, Status> {
    let req = request.into_inner();
    let name = req.name;
    let reason = req.reason.trim().to_string();
    if reason.is_empty() {
        return Err(Status::invalid_argument("reason must be non-empty"));
    }

    let (detail, event) = {
        let mut guard = lock_store_exclusive(campaigns_path)?;
        let campaign = guard
            .store
            .find_mut(&name)
            .ok_or_else(|| Status::not_found(format!("campaign '{name}' not found")))?;
        if campaign.complete(&reason, Utc::now()).map_err(|e| map_transition_error(&e))?
            == Transition::AlreadySettled
        {
            return Ok(Response::new(CompleteCampaignResponse {
                campaign: Some(campaign_to_detail(campaign)),
            }));
        }
        let detail = campaign_to_detail(campaign);
        let payload = Event::serialize_payload(&CampaignTerminalPayload {
            campaign: campaign.name.clone(),
            project: campaign.project.clone(),
            reason,
            cycles_completed: campaign.cycles_completed,
            cycles_landed: campaign.cycles_landed,
        })
        .map_err(|e| Status::internal(format!("failed to serialize completion payload: {e}")))?;
        // Mint both ids here: this manual completion is a workflow root
        // dispatched through `spawn_workflow`, with no trigger event to inherit
        // from. `stamp_context` only ever *inherits* the ids, so a bare
        // `Event::new` propagates `None` to `CampaignCompleted` and to every
        // event the terminal-surfacing block emits from it — the same gap the
        // `advance` path above already closes. No parent span: a manual
        // completion opens its own root chain.
        let event = Event::new(
            EventType::CampaignCompleted,
            campaign.project.clone(),
            Throttle::Full,
            payload,
        )
        .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
        .with_span_ids(Some(foundry_sdk::event::mint_span_id()), None);
        guard.save().map_err(map_save_error)?;
        (detail, event)
    };

    super::spawn_workflow(event, ctx);
    Ok(Response::new(CompleteCampaignResponse {
        campaign: Some(detail),
    }))
}

/// Stop a campaign permanently, optionally killing its in-flight cycle first.
///
/// Ordering matters and is deliberate: the abort happens **before** the store
/// lock is taken. `execute_campaign_advance` holds that lock across the whole
/// formation agent call, and `fs2::lock_exclusive` is a blocking syscall — so
/// locking first would park a tokio worker thread for minutes, making `--now`
/// the slowest command in the tool. Aborting first releases the lock (see
/// `WorkflowTracker::abort_campaign`, which awaits the unwind for exactly this
/// reason), leaving the subsequent acquisition uncontended.
///
/// The window between the abort and the lock is safe because the only thing
/// that could have advanced this campaign is the task just aborted, and the
/// state is re-read under the lock regardless.
pub(super) async fn cancel(
    campaigns_path: &Path,
    ctx: &super::RuntimeContext,
    request: Request<CancelCampaignRequest>,
) -> Result<Response<CancelCampaignResponse>, Status> {
    let req = request.into_inner();
    let name = req.name;
    let reason = req.reason.trim().to_string();
    if reason.is_empty() {
        return Err(Status::invalid_argument("reason must be non-empty"));
    }

    // Resolve what we can without contending for the lock, so an obviously
    // doomed call fails fast and never aborts a workflow needlessly.
    {
        let store = load_store(campaigns_path)?;
        let campaign = store
            .find(&name)
            .ok_or_else(|| Status::not_found(format!("campaign '{name}' not found")))?;
        let _ = campaign.check_cancellable().map_err(|e| map_transition_error(&e))?;
    }

    let aborted_event_id = if req.terminate_now {
        ctx.workflow_tracker
            .abort_campaign(&name)
            .await
            .into_iter()
            .next()
            .map(|workflow| workflow.event_id)
    } else {
        None
    };

    let (detail, event) = {
        let mut guard = lock_store_exclusive(campaigns_path)?;
        let campaign = guard
            .store
            .find_mut(&name)
            .ok_or_else(|| Status::not_found(format!("campaign '{name}' not found")))?;

        if campaign.cancel(&reason, Utc::now()).map_err(|e| map_transition_error(&e))?
            == Transition::AlreadySettled
        {
            return Ok(Response::new(CancelCampaignResponse {
                campaign: Some(campaign_to_detail(campaign)),
                event_id: String::new(),
            }));
        }
        let detail = campaign_to_detail(campaign);

        let payload = Event::serialize_payload(&CampaignCancelledPayload {
            terminal: CampaignTerminalPayload {
                campaign: campaign.name.clone(),
                project: campaign.project.clone(),
                reason,
                cycles_completed: campaign.cycles_completed,
                cycles_landed: campaign.cycles_landed,
            },
            terminated_now: req.terminate_now,
            discard_work: req.discard_work,
            aborted_event_id,
        })
        .map_err(|e| Status::internal(format!("failed to serialize cancellation payload: {e}")))?;
        // Mint both ids: like a manual completion, this is a workflow root
        // dispatched through `spawn_workflow` with no trigger to inherit from.
        // No parent span — it opens its own root chain.
        let event = Event::new(
            EventType::CampaignCancelled,
            campaign.project.clone(),
            Throttle::Full,
            payload,
        )
        .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
        .with_span_ids(Some(foundry_sdk::event::mint_span_id()), None);
        guard.save().map_err(map_save_error)?;
        (detail, event)
    };

    let event_id = event.id.clone();
    super::spawn_workflow(event, ctx);
    Ok(Response::new(CancelCampaignResponse {
        campaign: Some(detail),
        event_id,
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

        campaign.check_advanceable().map_err(|e| map_transition_error(&e))?;

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

    // Mint both ids here: this is the root of a campaign run, and
    // `stamp_context` only ever *inherits* them from the trigger.
    //
    // The trace groups the whole campaign run — every later cycle's
    // auto-advance inherits it through the task result that triggered it. The
    // span groups one cycle: `CampaignAdvanceRequested` is a span opener, so
    // each subsequent advance mints a fresh span that its formation, execution,
    // review, and finalize events all inherit.
    //
    // Without them every campaign event carries `None` for both, leaving the
    // longest-running workflow in the system the only one that cannot be
    // reconstructed — and forcing consumers to recover cycle boundaries by
    // sorting on time and guessing.
    let event = Event::new(EventType::CampaignAdvanceRequested, project, Throttle::Full, payload)
        .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
        .with_span_ids(Some(foundry_sdk::event::mint_span_id()), None);
    let event_id = event.id.clone();

    super::spawn_workflow(event, ctx);

    Ok(Response::new(AdvanceCampaignResponse {
        campaign: Some(detail),
        event_id,
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
    use foundry_sdk::campaign::{
        Campaign, CampaignBudget, CampaignStatus, DoneEvidence, OwnerDecision,
    };

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
            objective_history: vec![],
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
            objective_history: vec![],
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
