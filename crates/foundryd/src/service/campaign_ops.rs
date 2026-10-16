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
use foundry_sdk::work_item::{WorkItem, WorkItemState, WorkItemStore};
use foundry_sdk::work_source::WorkSourceKind;

use crate::workflow_tracker::ActiveWorkflow;

use crate::proto::{
    AddCampaignRequest, AddCampaignResponse, AdvanceCampaignRequest, AdvanceCampaignResponse,
    Campaign as ProtoCampaign, CampaignDetail, CampaignEscalation as ProtoCampaignEscalation,
    CampaignWorkInFlight, CancelCampaignRequest, CancelCampaignResponse, CompleteCampaignRequest,
    CompleteCampaignResponse, DecideCampaignRequest, DecideCampaignResponse,
    DoneEvidence as ProtoDoneEvidence, GetCampaignRequest, GetCampaignResponse,
    ListCampaignsRequest, ListCampaignsResponse, OwnerDecision as ProtoOwnerDecision,
    PauseCampaignRequest, PauseCampaignResponse, ResumeCampaignRequest, ResumeCampaignResponse,
};

pub(super) fn campaign_to_proto(campaign: &foundry_sdk::campaign::Campaign) -> ProtoCampaign {
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
        open_escalation: campaign.open_escalation.as_ref().map(escalation_to_proto),
        initiative: campaign.initiative.clone(),
        providers: Some(super::initiative_ops::providers_to_proto(campaign.providers)),
    }
}

fn escalation_to_proto(
    escalation: &foundry_sdk::campaign::CampaignEscalation,
) -> ProtoCampaignEscalation {
    ProtoCampaignEscalation {
        reason: escalation.reason.clone(),
        escalated_at: escalation.escalated_at.to_rfc3339(),
        cycles_completed: escalation.cycles_completed,
        cycles_landed: escalation.cycles_landed,
        question: escalation.question.clone(),
        choices: escalation.choices.clone(),
        from_event_log: escalation.from_event_log,
    }
}

/// Recover, from the event log, the reason of any escalated campaign in
/// `campaigns` whose escalation predates the campaign record.
///
/// Read-only: the recovered records live only in the caller's copy, never in
/// the store.
fn resolve_escalations(
    campaigns: &mut [foundry_sdk::campaign::Campaign],
    events_dir: &Path,
) -> Result<(), Status> {
    foundry_sdk::campaign::escalation::resolve_open_escalations(campaigns, events_dir)
        .map_err(|error| Status::internal(error.to_string()))
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

/// Run a campaign store operation, including its blocking flock, off the runtime.
/// A waiting command reads the latest state after acquisition, just as offline
/// callers do. Once started, it runs to completion even if the RPC is dropped.
pub(super) async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, Status> + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| Status::internal(format!("campaign store operation failed: {error}")))?
}

// Production callers must run inside `blocking`; synchronous helpers remain
// directly usable by unit tests without changing the store's offline contract.
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
        open_escalation: campaign.open_escalation.as_ref().map(escalation_to_proto),
        initiative: campaign.initiative.clone(),
        providers: Some(super::initiative_ops::providers_to_proto(campaign.providers)),
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

/// Read one campaign's full detail by exact name.
///
/// An escalated campaign whose escalation predates the campaign record carries
/// the reason recovered from the event log under `events_dir`.
pub(super) fn get(
    campaigns_path: &Path,
    events_dir: &Path,
    request: Request<GetCampaignRequest>,
) -> Result<Response<GetCampaignResponse>, Status> {
    let name = request.into_inner().name;
    let store = load_store(campaigns_path)?;
    let Some(campaign) = store.find(&name) else {
        return Err(Status::not_found(format!("campaign '{name}' not found")));
    };
    let mut one = [campaign.clone()];
    resolve_escalations(&mut one, events_dir)?;
    Ok(Response::new(GetCampaignResponse {
        campaign: Some(campaign_to_detail(&one[0])),
    }))
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

/// Record a binding owner decision on a campaign.
///
/// An active or paused campaign records it and keeps its status; an escalated
/// one records it and returns to active, adding `add_cycles` to its budget in
/// the same transition. A refusal (`FAILED_PRECONDITION`) saves nothing; the
/// rules live in [`foundry_sdk::campaign::Campaign::record_owner_decision`].
/// No event is emitted: the response and the store carry the resulting status.
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
        .record_owner_decision(decision, Utc::now(), req.add_cycles)
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
        let initiative_name = campaign.initiative.clone();
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
        if let Some(initiative) = initiative_name.filter(|name| {
            guard.store.find_initiative(name).is_some_and(|initiative| {
                initiative
                    .authorized_by
                    .as_deref()
                    .is_some_and(|owner| !owner.trim().is_empty())
                    && !matches!(
                        initiative.status,
                        foundry_sdk::initiative::InitiativeStatus::Completed
                            | foundry_sdk::initiative::InitiativeStatus::Cancelled
                    )
            })
        }) {
            guard
                .store
                .record_milestone(
                    &initiative,
                    &name,
                    "completed",
                    vec![
                        event.id.clone(),
                        event.payload_str("reason").unwrap_or_default().to_owned(),
                    ],
                )
                .map_err(super::initiative_ops::error)?;
        }
        guard.save().map_err(map_save_error)?;
        (detail, event)
    };

    super::spawn_workflow(event, ctx);
    Ok(Response::new(CompleteCampaignResponse {
        campaign: Some(detail),
    }))
}

fn select_aborted_owner(
    workflows: &[crate::workflow_tracker::ActiveWorkflow],
    store: &foundry_sdk::work_item::WorkItemStore,
) -> Option<crate::workflow_tracker::ActiveWorkflow> {
    workflows
        .iter()
        .find(|workflow| {
            store.items.iter().any(|item| {
                item.occupies_slot()
                    && item.project == workflow.project
                    && item.trace_id.as_deref() == Some(workflow.trace_id.as_str())
                    && item.started_root.as_ref().is_some_and(|root| root.id == workflow.event_id)
            })
        })
        .or_else(|| workflows.first())
        .cloned()
}

fn select_aborted_owner_in_file(
    path: &Path,
    workflows: &[crate::workflow_tracker::ActiveWorkflow],
) -> Option<crate::workflow_tracker::ActiveWorkflow> {
    if workflows.is_empty() {
        return None;
    }
    match foundry_sdk::work_item::WorkItemStore::load(path) {
        Ok(store) => select_aborted_owner(workflows, &store),
        Err(error) => {
            tracing::warn!(%error, "could not identify cancelled execution owner");
            workflows.first().cloned()
        }
    }
}

/// Stop a campaign permanently, optionally killing its in-flight cycle first.
///
/// Abort the target workflow before waiting for the store lock, so `--now`
/// never waits on a critical section of the task it is about to kill. Others
/// may hold the lock briefly; acquisition and persistence therefore run on the
/// blocking pool.
///
/// The window between the abort and the lock is safe because the only thing
/// that could have advanced this campaign is the task just aborted, and the
/// state is re-read under the lock regardless.
pub(super) async fn cancel(
    campaigns_path: &Path,
    work_items_path: &Path,
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

    // Both ids come off the same aborted workflow: the event id is the only
    // handle onto its partial events, and the trace is what correlates the
    // killed cycle with its `running` entry in the work-item ledger.
    let aborted = if req.terminate_now {
        ctx.workflow_tracker.abort_campaign(&name).await
    } else {
        Vec::new()
    };

    let campaigns_path = campaigns_path.to_path_buf();
    let work_items_path = work_items_path.to_path_buf();
    let spawn_ctx = ctx.clone();
    let (response, waiting_cycles) = blocking(move || {
        // Both submission coordinators and dispatched roots are tracked. The
        // ledger owner must identify disposal, regardless of HashMap order.
        let aborted = select_aborted_owner_in_file(&work_items_path, &aborted);
        let aborted_event_id = aborted.as_ref().map(|workflow| workflow.event_id.clone());
        let aborted_trace_id = aborted.as_ref().map(|workflow| workflow.trace_id.clone());
        let (detail, event, waiting_cycles) = {
            let mut guard = lock_store_exclusive(&campaigns_path)?;
            let campaign = guard
                .store
                .find_mut(&name)
                .ok_or_else(|| Status::not_found(format!("campaign '{name}' not found")))?;

            if campaign.cancel(&reason, Utc::now()).map_err(|e| map_transition_error(&e))?
                == Transition::AlreadySettled
            {
                return Ok((
                    Response::new(CancelCampaignResponse {
                        campaign: Some(campaign_to_detail(campaign)),
                        event_id: String::new(),
                    }),
                    Vec::new(),
                ));
            }
            let detail = campaign_to_detail(campaign);

            // A cycle the scheduler has not started yet must not start after
            // the campaign is gone. The ledger is settled here, beside the
            // campaign, so the two never disagree about whether a cycle is
            // still coming.
            let waiting_cycles = match super::work_item_ops::cancel_waiting_campaign_cycles(
                &work_items_path,
                &campaign.name,
                &reason,
            ) {
                Ok(cancelled) => cancelled,
                Err(error) => {
                    // Best-effort: the ledger is bookkeeping beside the
                    // cancellation, never a precondition for it; a queued
                    // cycle left behind is visible in `foundry queue`.
                    tracing::warn!(%error, campaign = %campaign.name, "could not cancel the campaign's waiting cycles");
                    Vec::new()
                }
            };

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
                aborted_trace_id,
            })
            .map_err(|e| {
                Status::internal(format!("failed to serialize cancellation payload: {e}"))
            })?;
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
            // A withdrawn cycle or formation never runs, so its completion never
            // releases the initiative reservation it holds. Release it in the
            // same store save as the cancellation.
            let withdrawn: Vec<String> = waiting_cycles.iter().map(|item| item.id.clone()).collect();
            guard.store.release_reservations_held_by(&withdrawn);
            guard.save().map_err(map_save_error)?;
            (detail, event, waiting_cycles)
        };

        let event_id = event.id.clone();
        super::spawn_workflow(event, &spawn_ctx);
        Ok((
            Response::new(CancelCampaignResponse {
                campaign: Some(detail),
                event_id,
            }),
            waiting_cycles,
        ))
    })
    .await?;
    super::work_item_ops::announce_cancelled(ctx, &waiting_cycles).await;
    Ok(response)
}

/// The label an in-flight advance carries on the wire.
const IN_FLIGHT_ADVANCE: &str = "advance";
/// The label an in-flight cycle carries on the wire.
const IN_FLIGHT_CYCLE: &str = "cycle";

fn in_flight_kind(root_type: &EventType) -> &'static str {
    if *root_type == EventType::CampaignAdvanceRequested {
        IN_FLIGHT_ADVANCE
    } else {
        IN_FLIGHT_CYCLE
    }
}

/// Whether `item` is unsettled work for `campaign`: one of its cycles, or an
/// advance of it, still submitted, queued, held or running (or still holding
/// its execution slot through cleanup).
fn is_campaign_work_in_flight(item: &WorkItem, campaign: &str) -> bool {
    let unsettled = item.state.is_waiting() || item.is_running() || item.occupies_slot();
    if !unsettled {
        return false;
    }
    // Cycles carry the campaign as their typed source; an automatic advance
    // inherits it from the cycle that asked for it.
    let dispatched_by_campaign = item
        .source
        .as_ref()
        .is_some_and(|source| source.matches(WorkSourceKind::Campaign, campaign));
    // A manual advance records no source, so its root names the campaign.
    let advances_campaign = item
        .pending_root
        .as_ref()
        .or(item.started_root.as_ref())
        .is_some_and(|root| root.payload_str("campaign") == Some(campaign));
    dispatched_by_campaign || advances_campaign
}

/// The advance or cycle of `campaign` already in flight, if any.
///
/// The ledger is the durable record and is consulted first; a running item is
/// preferred over a waiting one. The tracker covers the moment between an
/// accepted advance RPC and its ledger admission, which happens later inside
/// the spawned workflow, and a cycle's workflow while it submits the
/// automatic advance that follows it.
pub(super) fn find_campaign_work_in_flight(
    campaign: &str,
    ledger: &WorkItemStore,
    tracked: &[ActiveWorkflow],
) -> Option<CampaignWorkInFlight> {
    let in_flight: Vec<&WorkItem> = ledger
        .items
        .iter()
        .filter(|item| is_campaign_work_in_flight(item, campaign))
        .collect();
    let item = in_flight
        .iter()
        .find(|item| item.is_running())
        .or_else(|| in_flight.first())
        .copied();
    if let Some(item) = item {
        let root = item.pending_root.as_ref().or(item.started_root.as_ref());
        return Some(CampaignWorkInFlight {
            kind: root
                .map_or(IN_FLIGHT_CYCLE, |root| in_flight_kind(&root.event_type))
                .to_string(),
            work_item_id: item.id.clone(),
            state: item.state.tag().to_string(),
            event_id: root.map(|root| root.id.clone()).unwrap_or_default(),
        });
    }
    let advance = EventType::CampaignAdvanceRequested.to_string();
    let cycle = EventType::ExecutionRequested.to_string();
    tracked
        .iter()
        .filter(|workflow| workflow.campaign.as_deref() == Some(campaign))
        .find(|workflow| workflow.event_type == advance || workflow.event_type == cycle)
        .map(|workflow| CampaignWorkInFlight {
            kind: if workflow.event_type == advance {
                IN_FLIGHT_ADVANCE
            } else {
                IN_FLIGHT_CYCLE
            }
            .to_string(),
            work_item_id: String::new(),
            state: WorkItemState::Submitted.tag().to_string(),
            event_id: workflow.event_id.clone(),
        })
}

/// Dispatch one manual advance iteration for an active (or staged) campaign.
///
/// Validates campaign existence and status under the exclusive store lock,
/// then refuses to duplicate work: when the campaign already has an advance or
/// a cycle submitted, queued, held or running, nothing is dispatched and the
/// response names that work in `in_flight`. Otherwise it emits
/// `CampaignAdvanceRequested` through the engine. Returns
/// `FAILED_PRECONDITION` for `paused`, `escalated`, `completed` or
/// `cancelled` campaigns.
///
/// The check and the dispatch happen under the same store lock, and
/// `spawn_workflow` records the new workflow in the tracker before it
/// returns. So of two simultaneous advances for one campaign, the second to
/// take the lock always sees the first: exactly one workflow is dispatched.
/// The ledger alone could not close that window, because admission to it
/// happens later, inside the spawned workflow. The formation that workflow
/// runs takes the same lock on the blocking pool and waits only until this
/// function returns.
pub(super) fn advance(
    campaigns_path: &Path,
    work_items_path: &Path,
    ctx: &super::RuntimeContext,
    request: Request<AdvanceCampaignRequest>,
) -> Result<Response<AdvanceCampaignResponse>, Status> {
    let req = request.into_inner();
    let name = req.name;
    // Opaque: an empty field is simply "no operator context". It is never
    // parsed, and never a reason to refuse, delay or alter the dispatch.
    let operator_origin = Some(req.operator_origin).filter(|text| !text.is_empty());

    let guard = lock_store_exclusive(campaigns_path)?;
    let campaign = guard
        .store
        .find(&name)
        .ok_or_else(|| Status::not_found(format!("campaign '{name}' not found")))?;
    campaign.check_advanceable().map_err(|e| map_transition_error(&e))?;
    if let Some(initiative) = campaign
        .initiative
        .as_deref()
        .and_then(|name| guard.store.find_initiative(name))
        && (initiative.reservations.iter().any(|reservation| reservation.member == campaign.name)
            || initiative.pending_actions.iter().any(|action| matches!(&action.decision, foundry_sdk::initiative::SteeringDecision::Advance { member, .. } if member == &campaign.name))) {
            return Err(Status::failed_precondition("the initiative already reserved this member; inspect initiative show before advancing it"));
        }
    let detail = campaign_to_detail(campaign);
    let project = campaign.project.clone();

    // Lock order: campaign store, then ledger gate, as everywhere else.
    let ledger = {
        let _gate = foundry_sdk::work_item::ledger_write_gate()
            .lock()
            .map_err(|_| Status::internal("work-item ledger write gate poisoned"))?;
        super::work_item_ops::load_store(work_items_path)?
    };
    if let Some(in_flight) =
        find_campaign_work_in_flight(&name, &ledger, &ctx.workflow_tracker.list())
    {
        tracing::info!(
            campaign = %name,
            kind = %in_flight.kind,
            work_item_id = %in_flight.work_item_id,
            event_id = %in_flight.event_id,
            state = %in_flight.state,
            "campaign advance requested while its work is in flight; nothing dispatched"
        );
        return Ok(Response::new(AdvanceCampaignResponse {
            campaign: Some(detail),
            event_id: String::new(),
            in_flight: Some(in_flight),
        }));
    }

    let tracked = ctx.workflow_tracker.list();
    let active_members = guard
        .store
        .campaigns
        .iter()
        .filter(|member| {
            // A completed or cancelled member's waiting work never starts, so
            // only its still-running work holds an active-member slot.
            find_campaign_work_in_flight(&member.name, &ledger, &tracked).is_some_and(|work| {
                !member.status.is_terminal()
                    || !WorkItemState::from_tag(&work.state).is_some_and(WorkItemState::is_waiting)
            })
        })
        .map(|member| member.name.clone())
        .collect::<Vec<_>>();
    guard
        .store
        .enforce_can_form(&name, &active_members)
        .map_err(super::initiative_ops::error)?;

    // Emit CampaignAdvanceRequested through the existing engine path.
    let payload = Event::serialize_payload(&CampaignAdvanceRequestedPayload {
        campaign: name,
        run_event_id: None,
        run_result: None,
        operator_origin,
        providers: foundry_sdk::gateway::StageProviders::default(),
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
    let source = campaign.initiative.as_ref().map(|initiative| {
        foundry_sdk::work_source::WorkSource::initiative(
            initiative,
            campaign.cycles_completed.saturating_add(1),
        )
    });
    let event = Event::new(EventType::CampaignAdvanceRequested, project, Throttle::Full, payload)
        .with_source(source)
        .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
        .with_span_ids(Some(foundry_sdk::event::mint_span_id()), None);
    let event_id = event.id.clone();

    super::spawn_workflow(event, ctx);
    // Released only now, after the tracker records the workflow.
    drop(guard);

    Ok(Response::new(AdvanceCampaignResponse {
        campaign: Some(detail),
        event_id,
        in_flight: None,
    }))
}

/// List the campaign inventory, optionally for one project, sorted by name.
///
/// Escalated campaigns carry their reason, recovered from the event log under
/// `events_dir` when the escalation predates the campaign record.
pub(super) fn list(
    campaigns_path: &Path,
    events_dir: &Path,
    request: Request<ListCampaignsRequest>,
) -> Result<Response<ListCampaignsResponse>, Status> {
    let req = request.into_inner();
    let mut campaigns = load_store(campaigns_path)?.campaigns;

    if !req.project.is_empty() {
        campaigns.retain(|campaign| campaign.project == req.project);
    }
    resolve_escalations(&mut campaigns, events_dir)?;

    campaigns.sort_by(|left, right| left.name.cmp(&right.name));

    Ok(Response::new(ListCampaignsResponse {
        campaigns: campaigns.iter().map(campaign_to_proto).collect(),
    }))
}

#[cfg(test)]
mod in_flight_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use chrono::Utc;
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::work_item::{WorkItem, WorkItemKind, WorkItemSpec, WorkItemStore, WorkLane};
    use foundry_sdk::work_source::WorkSource;

    use super::find_campaign_work_in_flight;
    use crate::workflow_tracker::ActiveWorkflow;

    fn root(event_type: EventType, campaign: &str, cycle: Option<u64>) -> Event {
        Event::new(
            event_type,
            "p".to_string(),
            Throttle::Full,
            serde_json::json!({"campaign": campaign}),
        )
        .with_source(cycle.map(|n| WorkSource::campaign(campaign, n)))
    }

    fn queued(root: Event) -> WorkItem {
        let source = root.source.clone();
        WorkItem::queued(
            WorkItemSpec {
                project: "p".to_string(),
                objective: "work".to_string(),
                kind: WorkItemKind::Workflow,
                lane: WorkLane::Campaign,
                origin: "test".to_string(),
                trace_id: None,
            },
            root,
            Utc::now(),
        )
        .with_source(source)
    }

    fn started(root: Event) -> WorkItem {
        let mut item = queued(root.clone());
        item.start(Utc::now());
        item.started_root = Some(root);
        item
    }

    fn ledger(items: Vec<WorkItem>) -> WorkItemStore {
        WorkItemStore {
            items,
            ..WorkItemStore::default()
        }
    }

    fn tracked(event_type: &EventType, campaign: Option<&str>) -> ActiveWorkflow {
        ActiveWorkflow {
            event_id: "evt_tracked".to_string(),
            event_type: event_type.to_string(),
            project: "p".to_string(),
            trace_id: "t".to_string(),
            started_at: Utc::now(),
            campaign: campaign.map(ToString::to_string),
            initiative: None,
        }
    }

    #[test]
    fn nothing_in_flight_is_none() {
        assert!(find_campaign_work_in_flight("c", &ledger(vec![]), &[]).is_none());
    }

    #[test]
    fn a_running_item_is_named_before_a_waiting_one() {
        let waiting = queued(root(EventType::CampaignAdvanceRequested, "c", None));
        let running = started(root(EventType::ExecutionRequested, "c", Some(2)));
        let found = find_campaign_work_in_flight("c", &ledger(vec![waiting, running.clone()]), &[])
            .unwrap();
        assert_eq!(found.work_item_id, running.id);
        assert_eq!(found.kind, "cycle");
        assert_eq!(found.state, "running");
    }

    #[test]
    fn an_automatic_advance_carrying_the_campaign_source_is_an_advance() {
        let auto = queued(root(EventType::CampaignAdvanceRequested, "c", Some(3)));
        let found = find_campaign_work_in_flight("c", &ledger(vec![auto.clone()]), &[]).unwrap();
        assert_eq!(found.kind, "advance");
        assert_eq!(found.work_item_id, auto.id);
    }

    #[test]
    fn a_settled_cycle_still_holding_its_slot_is_in_flight() {
        let mut cleaning_up = started(root(EventType::ExecutionRequested, "c", Some(1)));
        cleaning_up.settle_landed("landed", Utc::now());
        assert!(!cleaning_up.execution_completed);
        let found =
            find_campaign_work_in_flight("c", &ledger(vec![cleaning_up.clone()]), &[]).unwrap();
        assert_eq!(found.work_item_id, cleaning_up.id);

        cleaning_up.execution_completed = true;
        assert!(find_campaign_work_in_flight("c", &ledger(vec![cleaning_up]), &[]).is_none());
    }

    #[test]
    fn an_accepted_advance_not_yet_in_the_ledger_is_found_in_the_tracker() {
        let found = find_campaign_work_in_flight(
            "c",
            &ledger(vec![]),
            &[tracked(&EventType::CampaignAdvanceRequested, Some("c"))],
        )
        .unwrap();
        assert_eq!(found.kind, "advance");
        assert_eq!(found.state, "submitted");
        assert!(
            found.work_item_id.is_empty(),
            "expected `found.work_item_id` to be empty: {:?}",
            found.work_item_id
        );
        assert_eq!(found.event_id, "evt_tracked");
    }

    #[test]
    fn other_campaigns_and_terminal_roots_in_the_tracker_do_not_count() {
        let tracker = [
            tracked(&EventType::CampaignAdvanceRequested, Some("other")),
            tracked(&EventType::CampaignCancelled, Some("c")),
            tracked(&EventType::ExecutionRequested, None),
        ];
        assert!(find_campaign_work_in_flight("c", &ledger(vec![]), &tracker).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_sdk::campaign::{
        Campaign, CampaignBudget, CampaignStatus, DoneEvidence, OwnerDecision,
    };

    #[test]
    fn cancellation_selects_the_started_owner_instead_of_its_submission_coordinator() {
        use foundry_sdk::work_item::{
            WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
        };
        let coordinator = crate::workflow_tracker::ActiveWorkflow {
            event_id: "evt_submission".into(),
            event_type: "execution_requested".into(),
            project: "proj".into(),
            trace_id: "a".repeat(32),
            started_at: Utc::now(),
            campaign: Some("campaign".into()),
            initiative: None,
        };
        let mut started = coordinator.clone();
        started.event_id = "evt_started".into();
        let mut item = WorkItem::dispatched(
            WorkItemSpec {
                project: "proj".into(),
                objective: "work".into(),
                kind: WorkItemKind::CampaignCycle,
                lane: WorkLane::Campaign,
                origin: "campaign".into(),
                trace_id: Some(started.trace_id.clone()),
            },
            Utc::now(),
        );
        let mut root = Event::new(
            EventType::ExecutionRequested,
            "proj".into(),
            Throttle::Full,
            serde_json::json!({}),
        );
        root.id = started.event_id.clone();
        item.started_root = Some(root);
        let mut store = WorkItemStore::default();
        for state in [
            WorkItemState::Running,
            WorkItemState::Preserved,
            WorkItemState::Cancelled,
        ] {
            item.state = state;
            store.upsert(item.clone());
            let selected =
                select_aborted_owner(&[coordinator.clone(), started.clone()], &store).unwrap();
            assert_eq!(selected.event_id, started.event_id);
        }
    }

    fn sample_campaign(name: &str, project: &str) -> Campaign {
        Campaign {
            initiative: None,
            providers: foundry_sdk::gateway::StageProviders::default(),
            name: name.to_string(),
            project: project.to_string(),
            mission: format!("Mission for {name}"),
            intent_refs: vec![],
            context_paths: vec![],
            done_evidence: vec![DoneEvidence::Review {
                statement: "Reviewed.".to_string(),
            }],
            budget: CampaignBudget {
                max_cycles: 7,
                ..Default::default()
            },
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
            open_escalation: None,
            writable_repositories: vec![],
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
            findings: vec![],
            initiatives: vec![],
            version: 1,
            campaigns,
        };
        store.save(tmp.path()).expect("save store");
        tmp
    }

    #[test]
    fn get_returns_full_detail_including_gate_and_review_evidence() {
        let campaign = Campaign {
            initiative: None,
            providers: foundry_sdk::gateway::StageProviders::default(),
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
            budget: CampaignBudget {
                max_cycles: 5,
                ..Default::default()
            },
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
            open_escalation: None,
            writable_repositories: vec![],
        };
        let tmp = write_store_with(vec![campaign]);

        let request = Request::new(GetCampaignRequest {
            name: "my-campaign".to_string(),
        });
        let response =
            get(tmp.path(), &tmp.path().join("no-events"), request).expect("get should succeed");
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
        let err = get(tmp.path(), &tmp.path().join("no-events"), request)
            .expect_err("should be not found");
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[test]
    fn get_returns_failed_precondition_on_malformed_store() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), b"not valid json at all }{").expect("write");
        let request = Request::new(GetCampaignRequest {
            name: "anything".to_string(),
        });
        let err = get(tmp.path(), &tmp.path().join("no-events"), request).expect_err("should fail");
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[test]
    fn get_returns_internal_on_unreadable_store() {
        // Use a directory path — CampaignStore::load on a directory produces an Io error.
        let tmp = tempfile::tempdir().expect("tempdir");
        let request = Request::new(GetCampaignRequest {
            name: "anything".to_string(),
        });
        let err = get(tmp.path(), &tmp.path().join("no-events"), request).expect_err("should fail");
        assert_eq!(err.code(), tonic::Code::Internal);
    }

    // ── escalation reason on GetCampaign / ListCampaigns ──────────────────────

    fn escalated(name: &str) -> Campaign {
        let mut campaign = sample_campaign(name, "proj");
        campaign.status = CampaignStatus::Escalated;
        campaign
    }

    /// Write one `campaign_escalated` event through the daemon's own writer.
    fn log_escalation(events_dir: &Path, campaign: &str, reason: &str) {
        let event = Event::new(
            EventType::CampaignEscalated,
            "proj".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignTerminalPayload {
                campaign: campaign.to_string(),
                project: "proj".to_string(),
                reason: reason.to_string(),
                cycles_completed: 6,
                cycles_landed: 6,
            })
            .expect("payload serializes"),
        );
        foundry_engine::event_writer::EventWriter::new(events_dir)
            .write(&event)
            .expect("write the event");
    }

    #[test]
    fn get_carries_the_recorded_escalation_reason_and_question() {
        let mut campaign = escalated("asks");
        campaign.open_escalation = Some(foundry_sdk::campaign::CampaignEscalation {
            reason: "boundaries differ; options: A | B".to_string(),
            escalated_at: chrono::DateTime::parse_from_rfc3339("2026-10-11T03:00:00Z")
                .expect("timestamp")
                .with_timezone(&Utc),
            cycles_completed: 2,
            cycles_landed: 1,
            question: Some("boundaries differ".to_string()),
            choices: vec!["A".to_string(), "B".to_string()],
            from_event_log: false,
        });
        let tmp = write_store_with(vec![campaign]);
        let events = tempfile::tempdir().expect("events dir");
        // A newer logged reason must not override the campaign's own record.
        log_escalation(events.path(), "asks", "a different logged reason");

        let detail = get(
            tmp.path(),
            events.path(),
            Request::new(GetCampaignRequest {
                name: "asks".to_string(),
            }),
        )
        .expect("get succeeds")
        .into_inner()
        .campaign
        .expect("campaign present");

        let open = detail.open_escalation.expect("the reason reaches the response");
        assert_eq!(open.reason, "boundaries differ; options: A | B");
        assert_eq!(open.escalated_at, "2026-10-11T03:00:00+00:00");
        assert_eq!(open.question.as_deref(), Some("boundaries differ"));
        assert_eq!(open.choices, ["A", "B"]);
        assert_eq!((open.cycles_completed, open.cycles_landed), (2, 1));
        assert!(!open.from_event_log);
    }

    /// The four production campaigns escalated before the record existed: the
    /// daemon recovers their reason from the event log without writing it back.
    #[test]
    fn get_and_list_recover_a_pre_record_escalation_from_the_event_log() {
        let tmp = write_store_with(vec![escalated("legacy"), sample_campaign("busy", "proj")]);
        let before = std::fs::read(tmp.path()).expect("read store");
        let events = tempfile::tempdir().expect("events dir");
        log_escalation(events.path(), "legacy", "campaign cycle budget exhausted (6)");
        log_escalation(events.path(), "busy", "an escalation it has since moved past");

        let detail = get(
            tmp.path(),
            events.path(),
            Request::new(GetCampaignRequest {
                name: "legacy".to_string(),
            }),
        )
        .expect("get succeeds")
        .into_inner()
        .campaign
        .expect("campaign present");
        let open = detail.open_escalation.expect("recovered from the log");
        assert_eq!(open.reason, "campaign cycle budget exhausted (6)");
        assert!(open.from_event_log);

        let listed = list(
            tmp.path(),
            events.path(),
            Request::new(ListCampaignsRequest {
                project: String::new(),
            }),
        )
        .expect("list succeeds")
        .into_inner()
        .campaigns;
        let legacy = listed.iter().find(|c| c.name == "legacy").expect("listed");
        assert_eq!(
            legacy.open_escalation.as_ref().map(|e| e.reason.as_str()),
            Some("campaign cycle budget exhausted (6)")
        );
        let busy = listed.iter().find(|c| c.name == "busy").expect("listed");
        assert!(busy.open_escalation.is_none(), "only an escalated campaign carries a reason");

        assert_eq!(std::fs::read(tmp.path()).expect("read store"), before, "a read writes nothing");
    }
}
