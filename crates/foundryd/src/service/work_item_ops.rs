//! Reads and owner controls of the daemon-owned work-item ledger, and of one item's
//! `work_item_*` events in the durable event log.
//!
//! Every operation here loads from disk on every call and caches nothing: the
//! ledger is authoritative on disk, and a reader that cached it would report
//! work as running after another process settled it. Reads do not take the
//! write gate; mutations serialize their load→modify→save sequences.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use tonic::{Request, Response, Status};

use foundry_sdk::error::StoreError;
use foundry_sdk::work_item::{WorkItem, WorkItemKind, WorkItemState, WorkItemStore};
use foundry_sdk::work_item_events::{WorkItemEventRecord, read_work_item_events};
use foundry_sdk::work_source::{WorkSource, WorkSourceKind};

use crate::proto::{
    GetWorkItemRequest, GetWorkItemResponse, ListWorkItemEventsRequest, ListWorkItemEventsResponse,
    ListWorkItemsRequest, ListWorkItemsResponse, WorkItem as ProtoWorkItem,
    WorkItemEvent as ProtoWorkItemEvent,
};

/// Map a ledger load failure to its gRPC status, matching the campaign-store
/// conventions: malformed content is the caller's precondition to fix, an I/O
/// fault is the daemon's problem.
fn map_store_error(error: StoreError) -> Status {
    match error {
        StoreError::Parse { source, .. } => {
            Status::failed_precondition(format!("work-item ledger is malformed: {source}"))
        }
        StoreError::Io { source, .. } => {
            Status::internal(format!("work-item ledger is unreadable: {source}"))
        }
        StoreError::NotFound { .. } => {
            Status::internal("work-item ledger load reported NotFound, which it never does")
        }
    }
}

pub(super) fn load_store(path: &Path) -> Result<WorkItemStore, Status> {
    WorkItemStore::load(path).map_err(map_store_error)
}

/// Parse a wire `WorkSource` into the typed source.
///
/// The kind is a closed enum, so an unknown tag is the caller's mistake, as is
/// a blank reference: a source that names nothing is not a source.
pub(super) fn source_from_proto(source: crate::proto::WorkSource) -> Result<WorkSource, Status> {
    let kind = WorkSourceKind::from_tag(&source.kind).ok_or_else(|| {
        Status::invalid_argument(format!(
            "unknown work-source kind '{}'; expected one of {}",
            source.kind,
            known_source_kinds()
        ))
    })?;
    if source.r#ref.trim().is_empty() {
        return Err(Status::invalid_argument("work-source ref must be nonblank"));
    }
    Ok(WorkSource {
        kind,
        reference: source.r#ref,
        cycle: source.cycle,
    })
}

/// Wire form of a typed source.
pub(super) fn source_to_proto(source: &WorkSource) -> crate::proto::WorkSource {
    crate::proto::WorkSource {
        kind: source.kind.tag().to_string(),
        r#ref: source.reference.clone(),
        cycle: source.cycle,
    }
}

/// The source kinds a caller may name, for error messages.
fn known_source_kinds() -> String {
    WorkSourceKind::ALL.iter().map(|kind| kind.tag()).collect::<Vec<_>>().join(", ")
}

/// Which ordering group an item's state puts it in.
///
/// The groups are the reading order an operator wants: what is under way, what
/// is waiting, what still needs a person, and finally what is done with.
fn order_group(state: WorkItemState) -> u8 {
    match state {
        WorkItemState::Running => 0,
        WorkItemState::Submitted | WorkItemState::Queued | WorkItemState::Held => 1,
        WorkItemState::Preserved | WorkItemState::NeedsDecision | WorkItemState::Failed => 2,
        WorkItemState::Landed | WorkItemState::Cancelled => 3,
    }
}

/// The sort key for one item, as a tuple ordered exactly as the RPC contract
/// describes.
///
/// Each group sorts on the timestamp that means something for it, and the
/// descending groups are expressed by negating the timestamp rather than by
/// reversing a comparator, so one key function covers every group. A missing
/// timestamp sorts as `0`, which keeps the ordering total even for a record
/// written by hand without one.
fn sort_key(item: &WorkItem) -> (u8, i64, &str) {
    let group = order_group(item.state);
    let stamp = match item.state {
        WorkItemState::Running => item.started_at.map_or(0, |at| at.timestamp_micros()),
        WorkItemState::Submitted | WorkItemState::Queued | WorkItemState::Held => {
            item.submitted_at.timestamp_micros()
        }
        WorkItemState::Preserved
        | WorkItemState::NeedsDecision
        | WorkItemState::Failed
        | WorkItemState::Landed
        | WorkItemState::Cancelled => -item.settled_at.map_or(0, |at| at.timestamp_micros()),
    };
    (group, stamp, item.id.as_str())
}

/// An exact source to select on: the kind and its reference.
type SourceFilter = (WorkSourceKind, String);

/// Every item the request selects, in the RPC's deterministic order.
///
/// Pure over the loaded store, so the ordering and filtering rules are
/// testable without a service or a filesystem. An item that records no
/// source never matches a source filter, and is listed like any other
/// without one.
fn selected(
    store: &WorkItemStore,
    project: &str,
    state: Option<WorkItemState>,
    source: Option<&SourceFilter>,
) -> Vec<WorkItem> {
    let mut items: Vec<WorkItem> = store
        .items
        .iter()
        .filter(|item| project.is_empty() || item.project == project)
        .filter(|item| state.is_none_or(|wanted| item.state == wanted))
        .filter(|item| {
            source.is_none_or(|(kind, reference)| {
                item.source.as_ref().is_some_and(|recorded| recorded.matches(*kind, reference))
                    || (*kind == WorkSourceKind::Campaign
                        && item
                            .pending_root
                            .as_ref()
                            .or(item.started_root.as_ref())
                            .is_some_and(|root| root.payload_str("campaign") == Some(reference)))
            })
        })
        .cloned()
        .collect();
    items.sort_by(|left, right| sort_key(left).cmp(&sort_key(right)));
    items
}

/// Wire form of one ledger record.
fn item_to_proto(item: &WorkItem) -> ProtoWorkItem {
    let disposition = item.disposition.as_ref();
    ProtoWorkItem {
        id: item.id.clone(),
        resumes: item.resumes.clone(),
        project: item.project.clone(),
        objective: item.objective.clone(),
        kind: item.kind.tag().to_string(),
        lane: item.lane.tag().to_string(),
        origin: item.origin.clone(),
        submitted_at: item.submitted_at.to_rfc3339(),
        started_at: item.started_at.map(|at| at.to_rfc3339()),
        settled_at: item.settled_at.map(|at| at.to_rfc3339()),
        state: item.state.tag().to_string(),
        reason: item.reason.clone(),
        trace_id: item.trace_id.clone(),
        verdict: disposition.and_then(|d| d.verdict.clone()),
        landed_commit: disposition.and_then(|d| d.landed_commit.clone()),
        preservation_ref: disposition.and_then(|d| d.preservation_ref.clone()),
        worktree: disposition.and_then(|d| d.worktree.clone()),
        worktree_removed: disposition.and_then(|d| d.worktree_removed),
        operator_action: item.operator_action.as_ref().map(|action| {
            crate::proto::WorkItemOperatorAction {
                command: action.command.clone(),
                origin: action.origin.clone(),
                previous_state: action.previous_state.tag().to_string(),
                previous_reason: action.previous_reason.clone(),
                previous_settled_at: action.previous_settled_at.map(|at| at.to_rfc3339()),
            }
        }),
        source: item.source.as_ref().map(source_to_proto),
        depends_on: item.depends_on.clone(),
        not_before: item.not_before.map(|at| at.to_rfc3339()),
        continues: item.continues.clone(),
        follow_up: item.follow_up.as_ref().map(follow_up_to_proto),
    }
}

/// Wire form of a follow-up's obligation.
fn follow_up_to_proto(
    follow_up: &foundry_sdk::work_item::FollowUp,
) -> crate::proto::WorkItemFollowUp {
    let gaps = match &follow_up.obligation {
        foundry_sdk::work_item::FollowUpObligation::Remainder { gaps } => gaps.clone(),
    };
    crate::proto::WorkItemFollowUp {
        of: follow_up.of.clone(),
        commit: follow_up.commit.clone(),
        obligation: follow_up.obligation.tag().to_string(),
        gaps,
    }
}

/// Parse the request's state filter. An empty tag means "every state"; an
/// unknown one is the caller's mistake.
fn parse_state_filter(tag: &str) -> Result<Option<WorkItemState>, Status> {
    if tag.is_empty() {
        return Ok(None);
    }
    WorkItemState::from_tag(tag).map(Some).ok_or_else(|| {
        Status::invalid_argument(format!(
            "unknown work-item state '{tag}'; expected one of submitted, queued, running, landed, preserved, needs_decision, failed, cancelled"
        ))
    })
}

/// Parse the request's source filter. Both fields empty means "every source";
/// an unknown kind, or a kind that names no reference, is the caller's
/// mistake.
fn parse_source_filter(kind: &str, reference: &str) -> Result<Option<SourceFilter>, Status> {
    if kind.is_empty() && reference.is_empty() {
        return Ok(None);
    }
    let kind = WorkSourceKind::from_tag(kind).ok_or_else(|| {
        Status::invalid_argument(format!(
            "unknown work-source kind '{kind}'; expected one of {}",
            known_source_kinds()
        ))
    })?;
    if reference.trim().is_empty() {
        return Err(Status::invalid_argument(
            "a work-source filter names a ref beside its kind, as <kind>:<ref>",
        ));
    }
    Ok(Some((kind, reference.to_string())))
}

pub(super) fn list(
    work_items_path: &Path,
    request: Request<ListWorkItemsRequest>,
) -> Result<Response<ListWorkItemsResponse>, Status> {
    let request = request.into_inner();
    let state = parse_state_filter(&request.state)?;
    let source = parse_source_filter(&request.source_kind, &request.source_ref)?;
    let store = load_store(work_items_path)?;
    let items = selected(&store, &request.project, state, source.as_ref())
        .iter()
        .map(item_to_proto)
        .collect();
    Ok(Response::new(ListWorkItemsResponse { items }))
}

pub(super) fn get(
    work_items_path: &Path,
    request: Request<GetWorkItemRequest>,
) -> Result<Response<GetWorkItemResponse>, Status> {
    let id = request.into_inner().id;
    let store = load_store(work_items_path)?;
    match store.find(&id) {
        Some(item) => Ok(Response::new(GetWorkItemResponse {
            item: Some(item_to_proto(item)),
        })),
        None => Err(Status::not_found(format!("work item '{id}' not found"))),
    }
}

/// Owner cancellation: disk work stays off the async worker and shares every
/// ledger writer's load→modify→atomic-save gate. Events follow a successful save.
#[tracing::instrument(skip_all, fields(item_id = %id, close))]
pub(super) async fn cancel_item(
    path: &Path,
    campaigns_path: &Path,
    ctx: &super::RuntimeContext,
    id: String,
    reason: String,
    operator_origin: String,
    close: bool,
) -> Result<ProtoWorkItem, Status> {
    if id.trim().is_empty() || reason.trim().is_empty() || operator_origin.trim().is_empty() {
        return Err(Status::invalid_argument("id, reason and operator origin must be nonblank"));
    }
    let path = path.to_path_buf();
    let campaigns_path = campaigns_path.to_path_buf();
    let item = tokio::task::spawn_blocking(move || {
        settle_owner_stop(&path, &campaigns_path, &id, &reason, operator_origin, close)
    })
    .await
    .map_err(|error| Status::internal(format!("work-item mutation did not finish: {error}")))??;
    let payload = foundry_sdk::event::Event::serialize_payload(
        &foundry_sdk::payload::WorkItemEventPayload::from_item(&item),
    )
    .map_err(|error| Status::internal(format!("cannot serialize cancellation: {error}")))?;
    let event = foundry_sdk::event::Event::new(
        foundry_sdk::event::EventType::WorkItemCancelled,
        item.project.clone(),
        foundry_sdk::throttle::Throttle::Full,
        payload,
    )
    .with_trace_id(item.trace_id.clone());
    ctx.engine.process(event).await;
    Ok(item_to_proto(&item))
}

/// Settle one item `cancelled` for an owner `cancel` (a waiting item) or
/// `close` (an open item), and return it.
///
/// A cancelled waiting item never runs, so its completion never releases the
/// initiative admission reservation it holds. Cancel releases it here, beside
/// the ledger change. Only a steering child (a root carrying
/// `steering_action_id`) can hold a reservation, so only its cancel touches
/// the campaign store: the store is locked first (the daemon's
/// campaign-store-before-ledger order), the ledger is saved, then the store.
/// When the store save fails the reservation stays and the error names it.
/// That leaves the member over-counted, never under-counted, until the next
/// steer or a restart recovers it from the ledger.
fn settle_owner_stop(
    path: &Path,
    campaigns_path: &Path,
    id: &str,
    reason: &str,
    operator_origin: String,
    close: bool,
) -> Result<WorkItem, Status> {
    // A root's identity never changes after admission, so this unguarded read
    // decides the lock before the guarded read below.
    let steering_child = !close
        && load_store(path)?.find(id).is_some_and(|item| {
            item.pending_root
                .iter()
                .chain(item.started_root.iter())
                .any(|root| root.payload_str("steering_action_id").is_some())
        });
    let mut campaigns = if steering_child {
        Some(foundry_sdk::campaign::CampaignStore::lock_exclusive(campaigns_path).map_err(
            |error| Status::internal(format!("cannot lock the campaign store: {error}")),
        )?)
    } else {
        None
    };
    let _guard = foundry_sdk::work_item::ledger_write_gate()
        .lock()
        .map_err(|_| Status::internal("work-item ledger write gate poisoned"))?;
    let mut store = load_store(path)?;
    let item = store
        .items
        .iter_mut()
        .find(|item| item.id == id)
        .ok_or_else(|| Status::not_found(format!("work item '{id}' not found")))?;
    if close && item.occupies_slot() {
        return Err(Status::failed_precondition(format!(
            "work item '{id}' cannot be closed while its workflow still owns execution capacity"
        )));
    }
    let allowed = if close {
        item.state.is_open()
    } else {
        item.state.is_waiting()
    };
    if !allowed {
        return Err(Status::failed_precondition(format!(
            "work item '{id}' cannot be {} from {}",
            if close { "closed" } else { "cancelled" },
            item.state.tag()
        )));
    }
    item.operator_action = Some(foundry_sdk::work_item::WorkItemOperatorAction {
        command: if close { "close" } else { "cancel" }.to_string(),
        origin: operator_origin,
        previous_state: item.state,
        previous_reason: item.reason.clone(),
        previous_settled_at: item.settled_at,
    });
    item.settle_cancelled(reason, item.disposition.clone(), chrono::Utc::now());
    let item = item.clone();
    store
        .save(path)
        .map_err(|error| Status::internal(format!("failed to persist work-item state: {error}")))?;
    if let Some(guard) = campaigns.as_mut()
        && guard.store.release_reservations_held_by(std::slice::from_ref(&item.id))
    {
        guard.save().map_err(|error| {
            Status::internal(format!(
                "work item '{id}' was cancelled, but its initiative admission reservation could not be released: {error}"
            ))
        })?;
    }
    Ok(item)
}

/// The `work_item_*` event that records one owner transition of `item`, on
/// the item's own trace.
fn lifecycle_event(
    event_type: foundry_sdk::event::EventType,
    item: &WorkItem,
) -> Result<foundry_sdk::event::Event, Status> {
    let payload = foundry_sdk::event::Event::serialize_payload(
        &foundry_sdk::payload::WorkItemEventPayload::from_item(item),
    )
    .map_err(|error| Status::internal(format!("cannot serialize work item event: {error}")))?;
    Ok(foundry_sdk::event::Event::new(
        event_type,
        item.project.clone(),
        foundry_sdk::throttle::Throttle::Full,
        payload,
    )
    .with_trace_id(item.trace_id.clone()))
}

/// Owner hold: a `queued` item leaves the scheduler's hands until released.
/// Same gate, persistence and event discipline as [`cancel_item`].
#[tracing::instrument(skip_all, fields(item_id = %id))]
pub(super) async fn hold_item(
    path: &Path,
    ctx: &super::RuntimeContext,
    id: String,
    operator_origin: String,
) -> Result<ProtoWorkItem, Status> {
    if id.trim().is_empty() || operator_origin.trim().is_empty() {
        return Err(Status::invalid_argument("id and operator origin must be nonblank"));
    }
    let path = path.to_path_buf();
    let item = tokio::task::spawn_blocking(move || {
        let _guard = foundry_sdk::work_item::ledger_write_gate()
            .lock()
            .map_err(|_| Status::internal("work-item ledger write gate poisoned"))?;
        let mut store = load_store(&path)?;
        let item = store
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| Status::not_found(format!("work item '{id}' not found")))?;
        if !item.is_queued() {
            return Err(Status::failed_precondition(format!(
                "work item '{id}' cannot be held from {}; only a queued item can",
                item.state.tag()
            )));
        }
        item.operator_action = Some(foundry_sdk::work_item::WorkItemOperatorAction {
            command: "hold".to_string(),
            origin: operator_origin,
            previous_state: item.state,
            previous_reason: item.reason.clone(),
            previous_settled_at: item.settled_at,
        });
        item.hold();
        let item = item.clone();
        store.save(&path).map_err(|error| {
            Status::internal(format!("failed to persist work-item state: {error}"))
        })?;
        Ok(item)
    })
    .await
    .map_err(|error| Status::internal(format!("work-item mutation did not finish: {error}")))??;
    let event = lifecycle_event(foundry_sdk::event::EventType::WorkItemHeld, &item)?;
    ctx.engine.process(event).await;
    Ok(item_to_proto(&item))
}

/// Whether an owner may release `item` back to the queue: it is held, or the
/// scheduler moved it to `needs_decision` before it ever started (a
/// dependency settled some way other than `landed`) and it still holds the
/// root the scheduler would start it from.
///
/// A follow-up is `needs_decision` and never started too, but it holds no
/// root: released, the scheduler could only fail it. It is resumed instead.
fn releasable(item: &WorkItem) -> bool {
    item.state == WorkItemState::Held
        || (item.state == WorkItemState::NeedsDecision
            && item.started_at.is_none()
            && item.pending_root.is_some())
}

/// Refuse to release `item` unless [`releasable`], naming the remedy for a
/// follow-up.
fn require_releasable(item: &WorkItem) -> Result<(), Status> {
    let id = &item.id;
    if item.kind == WorkItemKind::FollowUp {
        return Err(Status::failed_precondition(format!(
            "work item '{id}' is a follow-up, which is not released; \
             `foundry queue resume {id}` admits a task that continues it"
        )));
    }
    if !releasable(item) {
        return Err(Status::failed_precondition(format!(
            "work item '{id}' cannot be released from {}; only a held item, or one \
             waiting on a decision about a dependency, can",
            item.state.tag()
        )));
    }
    Ok(())
}

/// Owner release: a held item, or one waiting on a decision about a
/// dependency, returns to `queued`. A released dependent runs regardless of
/// the dependencies that can never land, so those are dropped; the reason it
/// then waits with is the scheduler's own verdict over the ledger as saved.
#[tracing::instrument(skip_all, fields(item_id = %id))]
pub(super) async fn release_item(
    path: &Path,
    ctx: &super::RuntimeContext,
    pacing: &foundry_sdk::pacing::PacingPaths,
    id: String,
    operator_origin: String,
) -> Result<ProtoWorkItem, Status> {
    if id.trim().is_empty() || operator_origin.trim().is_empty() {
        return Err(Status::invalid_argument("id and operator origin must be nonblank"));
    }
    let path = path.to_path_buf();
    let pacing = pacing.clone();
    let registry = std::sync::Arc::clone(&ctx.registry);
    let item = tokio::task::spawn_blocking(move || {
        let _guard = foundry_sdk::work_item::ledger_write_gate()
            .lock()
            .map_err(|_| Status::internal("work-item ledger write gate poisoned"))?;
        let mut store = load_store(&path)?;
        let landed_or_pending: Vec<String> = store
            .items
            .iter()
            .filter(|candidate| {
                candidate.state == WorkItemState::Landed
                    || candidate.state.is_waiting()
                    || candidate.is_running()
            })
            .map(|candidate| candidate.id.clone())
            .collect();
        let item = store
            .items
            .iter_mut()
            .find(|item| item.id == id)
            .ok_or_else(|| Status::not_found(format!("work item '{id}' not found")))?;
        require_releasable(item)?;
        item.operator_action = Some(foundry_sdk::work_item::WorkItemOperatorAction {
            command: "release".to_string(),
            origin: operator_origin,
            previous_state: item.state,
            previous_reason: item.reason.clone(),
            previous_settled_at: item.settled_at,
        });
        item.depends_on.retain(|dependency| landed_or_pending.contains(dependency));
        item.release();
        let released = item.id.clone();
        let reason = foundry_blocks::blocks::queued_reason(&store, &released, &registry, &pacing);
        let item = store.find_mut(&released).ok_or_else(|| {
            Status::internal("released work item disappeared from the loaded ledger")
        })?;
        item.reason = reason;
        let item = item.clone();
        store.save(&path).map_err(|error| {
            Status::internal(format!("failed to persist work-item state: {error}"))
        })?;
        Ok::<_, Status>(item)
    })
    .await
    .map_err(|error| Status::internal(format!("work-item mutation did not finish: {error}")))??;
    let event = lifecycle_event(foundry_sdk::event::EventType::WorkItemReleased, &item)?;
    ctx.engine.process(event).await;
    Ok(item_to_proto(&item))
}

/// Cancel every waiting cycle of `campaign`, with `reason`, and return the
/// cancelled items. Used when the campaign itself is cancelled: a cycle the
/// scheduler has not started must not start afterwards.
pub(super) fn cancel_waiting_campaign_cycles(
    path: &Path,
    campaign: &str,
    reason: &str,
) -> Result<Vec<WorkItem>, Status> {
    let _guard = foundry_sdk::work_item::ledger_write_gate()
        .lock()
        .map_err(|_| Status::internal("work-item ledger write gate poisoned"))?;
    let mut store = load_store(path)?;
    let now = chrono::Utc::now();
    let mut cancelled = Vec::new();
    for item in store.items.iter_mut().filter(|item| {
        item.state.is_waiting()
            && (item
                .source
                .as_ref()
                .is_some_and(|source| source.matches(WorkSourceKind::Campaign, campaign))
                || item
                    .pending_root
                    .as_ref()
                    .or(item.started_root.as_ref())
                    .is_some_and(|root| root.payload_str("campaign") == Some(campaign)))
    }) {
        item.settle_cancelled(reason, None, now);
        item.pending_root = None;
        cancelled.push(item.clone());
    }
    if cancelled.is_empty() {
        return Ok(cancelled);
    }
    store
        .save(path)
        .map_err(|error| Status::internal(format!("failed to persist work-item state: {error}")))?;
    Ok(cancelled)
}

/// Stop queued work belonging to a terminal initiative, retaining running work.
/// The single ledger save covers every member and any direct initiative task.
pub(super) fn cancel_waiting_initiative(
    path: &Path,
    initiative: &str,
    members: &[String],
    reason: &str,
) -> Result<Vec<WorkItem>, Status> {
    let _guard = foundry_sdk::work_item::ledger_write_gate()
        .lock()
        .map_err(|_| Status::internal("work-item ledger write gate poisoned"))?;
    let mut store = load_store(path)?;
    let now = chrono::Utc::now();
    let mut cancelled = Vec::new();
    for item in &mut store.items {
        let source_matches = item.source.as_ref().is_some_and(|source| {
            source.matches(WorkSourceKind::Initiative, initiative)
                || (source.kind == WorkSourceKind::Campaign && members.contains(&source.reference))
        });
        let member_matches = item
            .pending_root
            .as_ref()
            .or(item.started_root.as_ref())
            .and_then(|root| root.payload_str("campaign"))
            .is_some_and(|member| members.iter().any(|name| name == member));
        if item.state.is_waiting() && (source_matches || member_matches) {
            item.settle_cancelled(reason, None, now);
            item.pending_root = None;
            cancelled.push(item.clone());
        }
    }
    if !cancelled.is_empty() {
        store.save(path).map_err(|error| {
            Status::internal(format!("failed to persist work-item state: {error}"))
        })?;
    }
    Ok(cancelled)
}

/// Record `work_item_cancelled` for each of `items`, on each item's own trace.
pub(super) async fn announce_cancelled(ctx: &super::RuntimeContext, items: &[WorkItem]) {
    for item in items {
        match lifecycle_event(foundry_sdk::event::EventType::WorkItemCancelled, item) {
            Ok(event) => {
                ctx.engine.process(event).await;
            }
            Err(error) => {
                // Best-effort: the ledger already holds the cancellation; an
                // unserializable announcement must not fail the campaign's.
                tracing::warn!(item_id = %item.id, %error, "could not announce a cancelled cycle");
            }
        }
    }
}

/// Wire form of one `work_item_*` event.
fn event_to_proto(record: &WorkItemEventRecord) -> ProtoWorkItemEvent {
    ProtoWorkItemEvent {
        id: record.event_id.clone(),
        event_type: record.event_type.as_str(),
        occurred_at: record.occurred_at.to_rfc3339(),
        state: record.payload.state.tag().to_string(),
        reason: record.payload.reason.clone(),
        trace_id: record.trace_id.clone(),
    }
}

pub(super) async fn list_events(
    work_items_path: &Path,
    events_dir: &Path,
    request: Request<ListWorkItemEventsRequest>,
) -> Result<Response<ListWorkItemEventsResponse>, Status> {
    let id = request.into_inner().id;
    let store = load_store(work_items_path)?;
    if store.find(&id).is_none() {
        return Err(Status::not_found(format!("work item '{id}' not found")));
    }

    // The log holds every event Foundry ever wrote, so reading it is kept off
    // the async worker threads.
    let events_dir: PathBuf = events_dir.to_path_buf();
    let records = tokio::task::spawn_blocking(move || read_work_item_events(&events_dir, &id))
        .await
        .map_err(|err| Status::internal(format!("work-item event read did not finish: {err}")))?
        .map_err(|err| Status::internal(format!("event log is unreadable: {err}")))?;

    let events = records.iter().map(event_to_proto).collect();
    Ok(Response::new(ListWorkItemEventsResponse { events }))
}

/// Keep owner attribution and automated upgrade identity explicit at admission.
pub(super) enum ResumeSource {
    Owner(String),
    Nightly {
        project: String,
        package: String,
        target: String,
    },
}

/// What an owner asked the scheduler to respect for a new item: the items it
/// waits on and the earliest time it may start.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ScheduleHints {
    pub(super) depends_on: Vec<String>,
    pub(super) not_before: Option<chrono::DateTime<chrono::Utc>>,
}

impl ScheduleHints {
    /// Parse the wire form: ids as given, `not_before` as RFC 3339 or empty.
    pub(super) fn from_wire(depends_on: Vec<String>, not_before: &str) -> Result<Self, Status> {
        let not_before = if not_before.trim().is_empty() {
            None
        } else {
            Some(parse_not_before(not_before)?)
        };
        if depends_on.iter().any(|id| id.trim().is_empty()) {
            return Err(Status::invalid_argument("a dependency id must be nonblank"));
        }
        Ok(Self {
            depends_on,
            not_before,
        })
    }
}

/// Parse an RFC 3339 `not_before`, naming the input on failure.
pub(super) fn parse_not_before(text: &str) -> Result<chrono::DateTime<chrono::Utc>, Status> {
    chrono::DateTime::parse_from_rfc3339(text.trim())
        .map(|at| at.with_timezone(&chrono::Utc))
        .map_err(|error| {
            Status::invalid_argument(format!(
                "not_before '{text}' is not an RFC 3339 time: {error}"
            ))
        })
}

/// Every id in `depends_on` must name a ledger item: a dependency that does
/// not exist could never land, and the submitter is there to be told now.
pub(super) fn require_known_dependencies(
    store: &WorkItemStore,
    depends_on: &[String],
) -> Result<(), Status> {
    if let Some(unknown) = depends_on.iter().find(|id| store.find(id).is_none()) {
        return Err(Status::invalid_argument(format!(
            "work item '{unknown}' named by --after is not in the ledger"
        )));
    }
    Ok(())
}

/// Every id in `continues` must name an open item of `project`.
///
/// A follow-up records lineage to work a person still owes, so the submitter
/// is told now about an id that does not exist (`INVALID_ARGUMENT`, as for
/// `--after`), belongs to another project, or is no longer open
/// (`FAILED_PRECONDITION`).
pub(super) fn require_open_continuations(
    store: &WorkItemStore,
    project: &str,
    continues: &[String],
) -> Result<(), Status> {
    for id in continues {
        if id.trim().is_empty() {
            return Err(Status::invalid_argument("--continues needs a work item id"));
        }
        let item = store.find(id).ok_or_else(|| {
            Status::invalid_argument(format!(
                "work item '{id}' named by --continues is not in the ledger"
            ))
        })?;
        if item.project != project {
            return Err(Status::failed_precondition(format!(
                "work item '{id}' named by --continues belongs to project '{}', not '{project}'",
                item.project
            )));
        }
        if !item.state.is_open() {
            return Err(Status::failed_precondition(format!(
                "work item '{id}' named by --continues is {}, not open \
                 (preserved, needs_decision or failed)",
                item.state.tag()
            )));
        }
    }
    Ok(())
}

/// Admit a continuation to the queue; the pacing scheduler starts it.
///
/// `id` is preserved work (resumed from its preservation ref) or an open
/// follow-up (continued from current trunk); see [`admit_resume_with_campaigns`].
#[tracing::instrument(skip_all, fields(item_id = %id))]
pub(crate) async fn resume_item(
    path: &Path,
    campaigns_path: &Path,
    ctx: &super::RuntimeContext,
    pacing: &foundry_sdk::pacing::PacingPaths,
    id: String,
    operator_origin: String,
    hints: ScheduleHints,
) -> Result<ProtoWorkItem, Status> {
    let item = admit_resume_with_campaigns(
        path,
        campaigns_path,
        ctx,
        pacing,
        id,
        ResumeSource::Owner(operator_origin),
        hints,
    )
    .await?;
    Ok(item_to_proto(&item))
}

/// Shared durable admission for owner and nightly continuations: the child
/// is saved `queued`, holding the continuation root the scheduler will start
/// it from, with the reason it waits already computed.
pub(super) async fn admit_resume(
    path: &Path,
    ctx: &super::RuntimeContext,
    pacing: &foundry_sdk::pacing::PacingPaths,
    id: String,
    source: ResumeSource,
    hints: ScheduleHints,
) -> Result<WorkItem, Status> {
    admit_resume_with_campaigns(
        path,
        &foundry_sdk::paths::campaigns_path(),
        ctx,
        pacing,
        id,
        source,
        hints,
    )
    .await
}

/// Durable admission of the task that continues the open item `id`: a
/// preserved item (resumed from its preservation ref) or an open follow-up
/// (continued from current trunk). See [`continuation_child`].
pub(super) async fn admit_resume_with_campaigns(
    path: &Path,
    campaigns_path: &Path,
    ctx: &super::RuntimeContext,
    pacing: &foundry_sdk::pacing::PacingPaths,
    id: String,
    source: ResumeSource,
    hints: ScheduleHints,
) -> Result<WorkItem, Status> {
    if id.trim().is_empty()
        || matches!(&source, ResumeSource::Owner(origin) if origin.trim().is_empty())
    {
        return Err(Status::invalid_argument("id and operator origin must be nonblank"));
    }
    // Atomic store reads do not take the ledger gate. Keep the campaign snapshot
    // before admission, preserving the campaign-store then ledger lock order.
    let campaigns_path = campaigns_path.to_path_buf();
    let campaigns = tokio::task::spawn_blocking(move || {
        foundry_sdk::campaign::CampaignStore::load(&campaigns_path)
    })
    .await
    .map_err(|error| Status::internal(error.to_string()))?
    .map_err(|error| Status::internal(error.to_string()))?;
    let path = path.to_path_buf();
    let admission_path = path.clone();
    let registry = std::sync::Arc::clone(&ctx.registry);
    let reason_registry = std::sync::Arc::clone(&ctx.registry);
    let pacing = pacing.clone();
    let item = tokio::task::spawn_blocking(move || {
        let _guard = foundry_sdk::work_item::ledger_write_gate()
            .lock()
            .map_err(|_| Status::internal("work-item ledger write gate poisoned"))?;
        let mut store = load_store(&path)?;
        require_known_dependencies(&store, &hints.depends_on)?;
        let parent = store
            .find(&id)
            .ok_or_else(|| Status::not_found(format!("work item '{id}' not found")))?;
        let mut child = continuation_child(parent, source, &registry)?;
        attach_initiative_context(parent, &campaigns, &mut child)?;
        child.depends_on.clone_from(&hints.depends_on);
        child.not_before = hints.not_before;
        store.upsert(staged_rejection(&child));
        child.reason =
            foundry_blocks::blocks::queued_reason(&store, &child.id, &reason_registry, &pacing);
        store.save(&path).map_err(|error| {
            Status::internal(format!("failed to persist work-item state: {error}"))
        })?;
        Ok::<_, Status>(child)
    })
    .await
    .map_err(|error| Status::internal(format!("resume admission did not finish: {error}")))??;
    persist_resume_lifecycle(&ctx.engine, &item).await?;
    let admitted = item.clone();
    tokio::task::spawn_blocking(move || publish_resume_admission(&admission_path, admitted))
        .await
        .map_err(|error| Status::internal(format!("resume admission did not finish: {error}")))??;
    Ok(item)
}

/// Whether `parent` is a follow-up an owner may continue: one still open in
/// `needs_decision` or `failed`.
fn is_resumable_follow_up(parent: &WorkItem) -> bool {
    parent.kind == WorkItemKind::FollowUp
        && matches!(parent.state, WorkItemState::NeedsDecision | WorkItemState::Failed)
}

/// The queued task that continues `parent`, or why it cannot be continued.
///
/// Preserved work resumes from its preservation ref: the child records
/// `resumes`, and the ref must still be usable. An open follow-up has no ref:
/// its child starts from current trunk and records `continues`, so its
/// landing settles the follow-up through lineage. Only an owner continues a
/// follow-up; the nightly majors lane resumes preserved upgrades alone.
fn continuation_child(
    parent: &WorkItem,
    source: ResumeSource,
    registry: &std::sync::Arc<std::sync::RwLock<foundry_sdk::registry::Registry>>,
) -> Result<WorkItem, Status> {
    let follow_up = is_resumable_follow_up(parent);
    if parent.state != WorkItemState::Preserved && !follow_up {
        return Err(Status::failed_precondition(
            "only preserved work or an open follow-up can be resumed",
        ));
    }
    if parent.objective.trim().is_empty() {
        return Err(Status::failed_precondition(if follow_up {
            "the follow-up has no objective"
        } else {
            "preserved work has no objective"
        }));
    }
    if let ResumeSource::Nightly {
        project,
        package,
        target,
    } = &source
    {
        if follow_up {
            return Err(Status::failed_precondition("only an owner can resume a follow-up"));
        }
        if parent.project != *project
            || foundry_blocks::dependency_updates::majors::parse_objective(&parent.objective)
                != Some((package.clone(), target.clone(), project.clone()))
        {
            return Err(Status::failed_precondition(
                "preserved upgrade identity changed since planning",
            ));
        }
    }
    let entry = registry
        .read()
        .map_err(|_| Status::internal("registry lock poisoned"))?
        .find_project(&parent.project)
        .cloned()
        .ok_or_else(|| Status::failed_precondition("project is no longer registered"))?;
    let operator_origin = match source {
        ResumeSource::Owner(origin) => Some(origin),
        ResumeSource::Nightly { .. } => None,
    };
    if follow_up {
        return Ok(follow_up_child(parent, operator_origin));
    }
    let base = parent
        .disposition
        .as_ref()
        .and_then(|d| d.preservation_ref.as_ref())
        .filter(|reference| !reference.trim().is_empty())
        .ok_or_else(|| Status::failed_precondition("preserved work has no preservation ref"))?
        .clone();
    validate_preservation(&entry.path, &base)?;
    Ok(resume_child(parent, &base, operator_origin))
}

/// Claim the queued continuation only after its lifecycle events are durable.
fn publish_resume_admission(path: &Path, admitted: WorkItem) -> Result<(), Status> {
    let _guard = foundry_sdk::work_item::ledger_write_gate()
        .lock()
        .map_err(|_| Status::internal("work-item ledger write gate poisoned"))?;
    let mut store = load_store(path)?;
    let current = store
        .find(&admitted.id)
        .ok_or_else(|| Status::internal("resume admission record disappeared"))?;
    if current.state != WorkItemState::Failed {
        return Err(Status::internal("resume admission record changed before dispatch"));
    }
    store.upsert(admitted);
    store
        .save(path)
        .map_err(|error| Status::internal(format!("failed to persist work-item state: {error}")))
}

fn attach_initiative_context(
    parent: &WorkItem,
    campaigns: &foundry_sdk::campaign::CampaignStore,
    child: &mut WorkItem,
) -> Result<(), Status> {
    let parent_root = parent.started_root.as_ref().or(parent.pending_root.as_ref());
    let member = parent_root.and_then(|root| root.payload_str("campaign"));
    let Some(member) = member else {
        return Ok(());
    };
    let context = campaigns
        .formation_context(member, 8192)
        .map_err(super::initiative_ops::error)?;
    let Some(context) = context else {
        return Ok(());
    };
    let initiative = campaigns
        .find_initiative(&context.initiative)
        .ok_or_else(|| Status::failed_precondition("initiative disappeared"))?;
    if initiative.status.is_terminal() {
        return Err(Status::failed_precondition("a terminal initiative cannot resume work"));
    }
    let cycle = parent.campaign_cycle().ok_or_else(|| {
        Status::failed_precondition("initiative work has no campaign cycle identity")
    })?;
    child.source = Some(WorkSource::initiative(context.initiative.clone(), cycle));
    let findings = context.render_findings();
    if !findings.is_empty() {
        write!(child.objective, "\n\nFindings from the initiative:\n{findings}")
            .map_err(|error| Status::internal(error.to_string()))?;
    }
    let root = child
        .pending_root
        .as_mut()
        .ok_or_else(|| Status::internal("resume has no continuation root"))?;
    if let Some(original) = parent_root {
        for key in [
            "campaign",
            "campaign_cycle",
            "agent_provider",
            "writable_repositories",
        ] {
            if let Some(value) = original.payload.get(key) {
                root.payload[key].clone_from(value);
            }
        }
    }
    root.payload["initiative"] = serde_json::json!(context.initiative);
    root.payload["initiative_brief"] = serde_json::to_value(&context.brief)
        .map_err(|error| Status::internal(error.to_string()))?;
    let providers = campaigns
        .find(member)
        .map_or(context.providers, |campaign| campaign.providers.inherit(&context.providers));
    root.payload["providers"] =
        serde_json::to_value(providers).map_err(|error| Status::internal(error.to_string()))?;
    root.payload["initiative_findings"] = serde_json::json!(findings);
    root.payload["finding_ids"] = serde_json::json!(resumed_finding_ids(parent_root, &context)?);
    root.payload["prompt"] = serde_json::json!(child.objective);
    root.source.clone_from(&child.source);
    Ok(())
}

/// Keep the original formation's seen IDs when a newer bounded packet replaces its text.
fn resumed_finding_ids(
    parent_root: Option<&foundry_sdk::event::Event>,
    context: &foundry_sdk::initiative::InitiativeContext,
) -> Result<Vec<String>, Status> {
    let mut ids: Vec<String> = parent_root
        .and_then(|root| root.payload.get("finding_ids"))
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()
        .map_err(|error| {
            Status::internal(format!("cannot read original formation finding IDs: {error}"))
        })?
        .unwrap_or_default();
    for id in context.finding_ids() {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// The record a resume admission saves first, failing closed: until the
/// lifecycle append and the final ledger save succeed, the child holds no
/// claim to a place in the queue.
fn staged_rejection(child: &WorkItem) -> WorkItem {
    let mut rejected = child.clone();
    rejected.state = WorkItemState::Failed;
    rejected.reason = "resume admission incomplete; execution not dispatched".to_string();
    rejected.started_at = None;
    rejected.settled_at = Some(chrono::Utc::now());
    rejected.pending_root = None;
    rejected
}

/// The task-workflow root a continuation `item` is started from: its
/// objective, run from the preservation ref `base` when there is one and from
/// current trunk otherwise, naming the items it `continues`, and carrying the
/// item's typed source so the chain below it says what dispatched the work.
///
/// It is held on the queued item; the scheduler's started root names the
/// item, so the admission block records nothing for it.
fn continuation_root(item: &WorkItem, base: Option<&str>) -> foundry_sdk::event::Event {
    let mut payload = serde_json::json!({"project": item.project, "prompt": item.objective,
        "workflow": "task"});
    if let Some(base) = base {
        payload["base_ref"] = serde_json::json!(base);
    }
    if !item.continues.is_empty() {
        payload["continues"] = serde_json::json!(item.continues);
    }
    foundry_sdk::event::Event::new(
        foundry_sdk::event::EventType::ExecutionRequested,
        item.project.clone(),
        foundry_sdk::throttle::Throttle::Full,
        payload,
    )
    .with_trace_id(item.trace_id.clone())
    .with_source(item.source.clone())
}

/// The owner's `resume` action on a child continuing `parent`.
fn resume_action(
    parent: &WorkItem,
    origin: String,
) -> foundry_sdk::work_item::WorkItemOperatorAction {
    foundry_sdk::work_item::WorkItemOperatorAction {
        command: "resume".to_string(),
        origin,
        previous_state: parent.state,
        previous_reason: parent.reason.clone(),
        previous_settled_at: parent.settled_at,
    }
}

/// The task that discharges the open follow-up `parent`: its objective, in the
/// interactive lane, started from current trunk, continuing the follow-up so
/// that its landing settles it. The follow-up itself is left as it is.
fn follow_up_child(parent: &WorkItem, operator_origin: Option<String>) -> WorkItem {
    let mut child = WorkItem::submitted(
        foundry_sdk::work_item::WorkItemSpec {
            project: parent.project.clone(),
            objective: parent.objective.clone(),
            kind: WorkItemKind::Task,
            lane: foundry_sdk::work_item::WorkLane::Interactive,
            origin: parent.origin.clone(),
            trace_id: Some(foundry_sdk::event::mint_trace_id()),
        },
        chrono::Utc::now(),
    )
    .with_source(Some(WorkSource::work_item(parent.id.clone())));
    child.queue();
    child.continues = vec![parent.id.clone()];
    child.pending_root = Some(continuation_root(&child, None));
    child.operator_action = operator_origin.map(|origin| resume_action(parent, origin));
    child
}

/// Submission identity reflects who is continuing the obligation, while the
/// original record and its evidence remain untouched.
///
/// The child's typed source is the parent item, for an owner resume and a
/// nightly continuation alike: it is work created from another item, and
/// `resumes` already carries the same link. Who asked is recorded in the
/// child's operator action (owner) or its lane and origin (nightly). The
/// child is `queued`, holding the root that starts it from `base`.
fn resume_child(parent: &WorkItem, base: &str, operator_origin: Option<String>) -> WorkItem {
    let nightly = operator_origin.is_none();
    let mut child = WorkItem::submitted(
        foundry_sdk::work_item::WorkItemSpec {
            project: parent.project.clone(),
            objective: parent.objective.clone(),
            kind: if nightly {
                foundry_sdk::work_item::WorkItemKind::MajorUpgrade
            } else {
                foundry_sdk::work_item::WorkItemKind::Task
            },
            lane: if nightly {
                foundry_sdk::work_item::WorkLane::Maintenance
            } else {
                foundry_sdk::work_item::WorkLane::Interactive
            },
            origin: if nightly {
                "nightly majors lane".to_string()
            } else {
                parent.origin.clone()
            },
            trace_id: Some(foundry_sdk::event::mint_trace_id()),
        },
        chrono::Utc::now(),
    )
    .with_source(Some(WorkSource::work_item(parent.id.clone())));
    child.queue();
    child.pending_root = Some(continuation_root(&child, Some(base)));
    child.resumes = Some(parent.id.clone());
    child.operator_action = operator_origin.map(|origin| resume_action(parent, origin));
    child
}

/// The `work_item_submitted` root is an admission prerequisite; its
/// downstream processing retains the engine's existing policy. The start is
/// the scheduler's to announce, when it happens.
async fn persist_resume_lifecycle(
    engine: &foundry_engine::engine::Engine,
    item: &WorkItem,
) -> Result<(), Status> {
    let event = lifecycle_event(foundry_sdk::event::EventType::WorkItemSubmitted, item)?;
    engine.process_required(event).await.map_err(|error| {
        Status::internal(format!("failed to persist resume lifecycle: {error}"))
    })?;
    Ok(())
}

fn validate_preservation(repo: &str, base: &str) -> Result<(), Status> {
    // Read-only validation mirrors continuation's local/ref/bundle inputs.
    // Fetching and worktree creation remain the existing task runner's job.
    let mut git = std::process::Command::new("git");
    git.current_dir(repo);
    if let Some(bundle) = base.strip_prefix("bundle:") {
        git.args(["bundle", "verify", bundle]);
    } else {
        git.args([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{base}^{{commit}}"),
        ]);
    }
    let local = git
        .output()
        .map_err(|error| Status::internal(format!("cannot inspect preservation: {error}")))?;
    if local.status.success()
        && let Some(bundle) = base.strip_prefix("bundle:")
    {
        let heads = std::process::Command::new("git")
            .current_dir(repo)
            .args(["bundle", "list-heads", bundle])
            .output()
            .map_err(|error| {
                Status::internal(format!("cannot inspect preservation bundle: {error}"))
            })?;
        if !heads.status.success()
            || !String::from_utf8_lossy(&heads.stdout)
                .lines()
                .filter_map(|line| line.split_whitespace().nth(1))
                .any(|name| name.starts_with("refs/heads/"))
        {
            return Err(Status::failed_precondition("preservation bundle carries no branch ref"));
        }
    }
    if !local.status.success() {
        if base.starts_with("bundle:") || base.starts_with('-') {
            return Err(Status::failed_precondition("unusable preservation evidence"));
        }
        let remote = std::process::Command::new("git")
            .current_dir(repo)
            .args(["ls-remote", "--exit-code", "origin", base])
            .output()
            .map_err(|error| Status::internal(format!("cannot inspect preserved ref: {error}")))?;
        if !remote.status.success() || remote.stdout.is_empty() {
            return Err(Status::failed_precondition("unusable preservation evidence"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};

    use super::{order_group, parse_source_filter, parse_state_filter, selected};
    use foundry_sdk::work_item::{
        WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
    };
    use foundry_sdk::work_source::{WorkSource, WorkSourceKind};

    #[test]
    fn terminal_initiative_cancels_waiting_members_and_direct_tasks_but_keeps_running_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("work-items.json");
        let mut direct = item("wi_direct", "sample", WorkItemState::Queued);
        direct.source = Some(WorkSource::initiative("shared", 1));
        let mut legacy = item("wi_legacy", "sample", WorkItemState::Held);
        legacy.source = Some(WorkSource::campaign("member", 1));
        let mut rooted = item("wi_rooted", "sample", WorkItemState::Queued);
        rooted.pending_root = Some(foundry_sdk::event::Event::new(
            foundry_sdk::event::EventType::ExecutionRequested,
            "sample".into(),
            foundry_sdk::throttle::Throttle::Full,
            serde_json::json!({"campaign":"member"}),
        ));
        let mut running = item("wi_running", "sample", WorkItemState::Running);
        running.source = Some(WorkSource::initiative("shared", 1));
        let other = item("wi_other", "sample", WorkItemState::Queued);
        let store = WorkItemStore {
            version: 1,
            items: vec![direct, legacy, rooted, running, other],
        };
        store.save(&path).unwrap();
        let cancelled = super::cancel_waiting_initiative(
            &path,
            "shared",
            &["member".into()],
            "Outcome stopped",
        )
        .unwrap();
        assert_eq!(cancelled.len(), 3);
        let current = WorkItemStore::load(&path).unwrap();
        for id in ["wi_direct", "wi_legacy", "wi_rooted"] {
            assert_eq!(current.find(id).unwrap().state, WorkItemState::Cancelled);
            assert!(current.find(id).unwrap().pending_root.is_none());
        }
        assert_eq!(current.find("wi_running").unwrap().state, WorkItemState::Running);
        assert_eq!(current.find("wi_other").unwrap().state, WorkItemState::Queued);
        let before = std::fs::read(&path).unwrap();
        assert!(
            super::cancel_waiting_initiative(&path, "shared", &["member".into()], "Retry")
                .unwrap()
                .is_empty(),
            "no waiting initiative item is cancelled"
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn resumed_initiative_work_receives_current_diagnosis_brief_and_member_providers() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("campaigns.json");
        let mut guard = foundry_sdk::campaign::CampaignStore::lock_exclusive(&path).unwrap();
        let campaign = serde_json::from_value(serde_json::json!({"name":"member","project":"sample","mission":"The member mission",
            "authorized_by":"Stacey","providers":{"execution":"claude"},"done_evidence":[{"kind":"review","statement":"It works"}]})).unwrap();
        guard.store.add(campaign).unwrap();
        let initiative = serde_json::from_value(serde_json::json!({"name":"shared","outcome":"It works together","projects":["sample"],
            "members":[{"kind":"campaign","name":"member"}],"authorized_by":"Stacey",
            "providers":{"execution":"codex","review":"claude"},"done_evidence":[{"kind":"review","statement":"It works"}]})).unwrap();
        guard.store.add_initiative(initiative, &["sample".into()]).unwrap();
        let finding = foundry_sdk::initiative::Finding::new(
            "shared",
            foundry_sdk::initiative::FindingSource {
                kind: "campaign_cycle".into(),
                id: "wi_parent".into(),
            },
            vec!["member".into()],
            foundry_sdk::initiative::FindingKind::Regression,
            &format!("The argument was lost. {}", "x".repeat(6900)),
            vec![],
        );
        let finding_id = finding.id.clone();
        guard.store.add_finding(finding).unwrap();
        let mut older = foundry_sdk::initiative::Finding::new(
            "shared",
            foundry_sdk::initiative::FindingSource {
                kind: "owner".into(),
                id: "Stacey".into(),
            },
            vec!["member".into()],
            foundry_sdk::initiative::FindingKind::Evidence,
            &"Original formation evidence. ".repeat(40),
            vec![],
        );
        older.recorded_at -= chrono::Duration::seconds(1);
        let older_id = older.id.clone();
        guard.store.add_finding(older).unwrap();
        assert_eq!(
            guard.store.formation_context("member", 8192).unwrap().unwrap().finding_ids(),
            vec![finding_id.clone()]
        );
        let brief = guard
            .set_initiative_brief("shared", home.path(), "Use the corrected argument")
            .unwrap();
        let mut parent = item("wi_parent", "sample", WorkItemState::Preserved);
        parent.source = Some(WorkSource::initiative("shared", 1));
        parent.started_root = Some(foundry_sdk::event::Event::new(
            foundry_sdk::event::EventType::ExecutionRequested,
            "sample".into(),
            foundry_sdk::throttle::Throttle::Full,
            serde_json::json!({"campaign":"member","campaign_cycle":1,"agent_provider":"codex", "finding_ids":[older_id]}),
        ));
        let mut child = super::resume_child(&parent, "preserved-work", Some("owner host".into()));
        super::attach_initiative_context(&parent, &guard.store, &mut child).unwrap();
        let root = child.pending_root.as_ref().unwrap();
        assert_eq!(root.payload["campaign"], "member");
        assert_eq!(root.payload["campaign_cycle"], 1);
        assert_eq!(root.payload["finding_ids"], serde_json::json!([older_id, finding_id]));
        assert_eq!(root.payload["initiative_brief"]["hash"], brief.hash);
        assert_eq!(root.payload["providers"]["execution"], "claude");
        assert!(root.payload["prompt"].as_str().unwrap().contains("The argument was lost"));
        assert_eq!(child.source, Some(WorkSource::initiative("shared", 1)));
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("in-range timestamp")
    }

    fn item(id: &str, project: &str, state: WorkItemState) -> WorkItem {
        let mut item = WorkItem::submitted(
            WorkItemSpec {
                project: project.to_string(),
                objective: "o".to_string(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "test".to_string(),
                trace_id: None,
            },
            at(0),
        );
        item.id = id.to_string();
        item.state = state;
        item
    }

    #[test]
    fn every_state_lands_in_exactly_one_ordering_group() {
        assert_eq!(order_group(WorkItemState::Running), 0);
        assert_eq!(order_group(WorkItemState::Submitted), 1);
        assert_eq!(order_group(WorkItemState::Queued), 1);
        assert_eq!(order_group(WorkItemState::Preserved), 2);
        assert_eq!(order_group(WorkItemState::NeedsDecision), 2);
        assert_eq!(order_group(WorkItemState::Failed), 2);
        assert_eq!(order_group(WorkItemState::Landed), 3);
        assert_eq!(order_group(WorkItemState::Cancelled), 3);
    }

    #[test]
    fn an_unknown_state_filter_is_rejected_and_an_empty_one_means_every_state() {
        assert!(parse_state_filter("").expect("empty is every state").is_none());
        assert_eq!(
            parse_state_filter("preserved").expect("known tag"),
            Some(WorkItemState::Preserved)
        );
        let status = parse_state_filter("in_progress").expect_err("unknown tag");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn a_resume_child_records_its_parent_as_the_source_and_keeps_the_resumes_link() {
        use foundry_sdk::work_source::WorkSource;
        let mut parent = item("wi_parent", "alpha", WorkItemState::Preserved);
        parent.source = Some(WorkSource::operator("workbench"));

        let owner = super::resume_child(&parent, "preserved-work", Some("host desk".to_string()));
        assert_eq!(owner.source, Some(WorkSource::work_item("wi_parent")));
        assert_eq!(owner.resumes.as_deref(), Some("wi_parent"));
        assert_eq!(owner.operator_action.as_ref().map(|a| a.command.as_str()), Some("resume"));
        assert_eq!(owner.state, WorkItemState::Queued, "the scheduler starts it");
        assert_eq!(owner.started_at, None);
        let root = owner.pending_root.as_ref().expect("the child holds its continuation root");
        assert_eq!(root.payload["base_ref"], "preserved-work");
        assert_eq!(root.payload["prompt"], parent.objective);
        assert_eq!(root.trace_id, owner.trace_id);
        assert_eq!(root.source, owner.source);
        assert!(root.payload.get("admitted_work_item_id").is_none(), "the started root names it");

        let nightly = super::resume_child(&parent, "preserved-work", None);
        assert_eq!(nightly.source, Some(WorkSource::work_item("wi_parent")));
        assert_eq!(nightly.resumes.as_deref(), Some("wi_parent"));
        assert_eq!(nightly.kind, WorkItemKind::MajorUpgrade);

        assert_eq!(parent.source, Some(WorkSource::operator("workbench")), "parent untouched");
    }

    #[test]
    fn schedule_hints_parse_rfc3339_and_reject_blank_ids_and_bad_times() {
        let hints =
            super::ScheduleHints::from_wire(vec!["wi_a".to_string()], "2026-10-10T14:00:00+00:00")
                .expect("valid");
        assert_eq!(hints.depends_on, vec!["wi_a".to_string()]);
        assert_eq!(
            hints.not_before.map(|at| at.to_rfc3339()),
            Some("2026-10-10T14:00:00+00:00".into())
        );
        assert_eq!(
            super::ScheduleHints::from_wire(vec![], "").expect("empty"),
            super::ScheduleHints::default()
        );
        assert_eq!(
            super::ScheduleHints::from_wire(vec![], "tomorrow")
                .expect_err("bad time")
                .code(),
            tonic::Code::InvalidArgument
        );
        assert_eq!(
            super::ScheduleHints::from_wire(vec![" ".to_string()], "")
                .expect_err("blank id")
                .code(),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn a_dependency_missing_from_the_ledger_is_refused_by_name() {
        let store = WorkItemStore {
            version: 1,
            items: vec![item("wi_a", "alpha", WorkItemState::Landed)],
        };
        super::require_known_dependencies(&store, &["wi_a".to_string()]).expect("known");
        let status =
            super::require_known_dependencies(&store, &["wi_a".to_string(), "wi_zzz".to_string()])
                .expect_err("unknown");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("wi_zzz"), "{}", status.message());
    }

    #[test]
    fn only_held_or_dependency_blocked_items_are_releasable() {
        let mut held = item("wi_h", "alpha", WorkItemState::Held);
        held.started_at = None;
        assert!(super::releasable(&held));
        let mut blocked = item("wi_d", "alpha", WorkItemState::NeedsDecision);
        blocked.started_at = None;
        blocked.pending_root = Some(task_root("alpha"));
        assert!(super::releasable(&blocked), "a dependency decision never started the item");
        blocked.pending_root = None;
        assert!(!super::releasable(&blocked), "without a root the scheduler could only fail it");
        let mut reviewed = item("wi_r", "alpha", WorkItemState::NeedsDecision);
        reviewed.started_at = Some(at(5));
        assert!(!super::releasable(&reviewed), "a verdict's decision is not the scheduler's");
        assert!(!super::releasable(&item("wi_q", "alpha", WorkItemState::Queued)));
    }

    fn task_root(project: &str) -> foundry_sdk::event::Event {
        foundry_sdk::event::Event::new(
            foundry_sdk::event::EventType::ExecutionRequested,
            project.into(),
            foundry_sdk::throttle::Throttle::Full,
            serde_json::json!({"project": project, "workflow": "task", "prompt": "o"}),
        )
    }

    /// An open follow-up of the landed `wi_parent`, holding two gaps.
    fn follow_up(state: WorkItemState) -> WorkItem {
        let mut parent = item("wi_parent", "alpha", WorkItemState::Landed);
        parent.objective = "Add a --quiet flag.".into();
        let mut follow_up = foundry_sdk::work_item::FollowUp {
            of: "wi_parent".into(),
            commit: Some("landedcommit".into()),
            obligation: foundry_sdk::work_item::FollowUpObligation::Remainder {
                gaps: vec!["Document it.".into(), "Test it.".into()],
            },
        }
        .into_item(&parent, at(10));
        follow_up.id = "wi_follow".into();
        follow_up.state = state;
        follow_up
    }

    fn registry_with(
        project: &str,
    ) -> std::sync::Arc<std::sync::RwLock<foundry_sdk::registry::Registry>> {
        let entry: foundry_sdk::registry::ProjectEntry =
            serde_json::from_value(serde_json::json!({
            "name": project, "path": "/nonexistent/checkout", "stack": "rust",
            "agent": "rust-craftsperson", "repo": "", "branch": "main",
            "skip": null, "notes": null, "actions": {}}))
            .unwrap();
        std::sync::Arc::new(std::sync::RwLock::new(foundry_sdk::registry::Registry {
            version: 2,
            projects: vec![entry],
        }))
    }

    #[test]
    fn a_follow_up_is_never_released_however_it_waits() {
        for state in [WorkItemState::NeedsDecision, WorkItemState::Failed] {
            let mut open = follow_up(state);
            assert!(!super::releasable(&open), "{state:?}: it holds no root");
            open.pending_root = Some(task_root("alpha"));
            let status = super::require_releasable(&open).expect_err("a follow-up is resumed");
            assert_eq!(status.code(), tonic::Code::FailedPrecondition);
            assert!(status.message().contains("foundry queue resume wi_follow"), "{status:?}");
        }
    }

    #[test]
    fn an_open_follow_up_is_continued_from_trunk_by_a_task_that_continues_it() {
        let open = follow_up(WorkItemState::NeedsDecision);

        let child = super::continuation_child(
            &open,
            super::ResumeSource::Owner("desk (picking it up)".into()),
            &registry_with("alpha"),
        )
        .expect("an open follow-up can be resumed");

        assert_eq!(child.kind, WorkItemKind::Task);
        assert_eq!(child.lane, WorkLane::Interactive);
        assert_eq!(child.project, "alpha");
        assert_eq!(child.objective, open.objective);
        assert_eq!(child.origin, "follow-up of wi_parent");
        assert_eq!(child.source, Some(WorkSource::work_item("wi_follow")));
        assert_eq!(child.continues, vec!["wi_follow".to_string()]);
        assert_eq!(child.resumes, None, "no exact preserved ref is continued");
        assert_eq!(child.state, WorkItemState::Queued);
        assert_eq!(child.started_at, None);
        assert_ne!(child.trace_id, open.trace_id, "a run of its own");
        let action = child.operator_action.as_ref().expect("the owner's resume");
        assert_eq!(action.command, "resume");
        assert_eq!(action.origin, "desk (picking it up)");
        assert_eq!(action.previous_state, WorkItemState::NeedsDecision);
        assert_eq!(action.previous_reason, open.reason);
        let root = child.pending_root.as_ref().expect("the child holds its root");
        assert!(root.payload.get("base_ref").is_none(), "started from current trunk");
        assert_eq!(root.payload["continues"], serde_json::json!(["wi_follow"]));
        assert_eq!(root.payload["prompt"], open.objective);
        assert_eq!(root.trace_id, child.trace_id);
        assert_eq!(root.source, child.source);
        let typed: foundry_sdk::payload::ExecutionRequestedPayload =
            root.parse_payload().expect("a task dispatch root");
        assert_eq!(typed.continues, vec!["wi_follow".to_string()]);

        let failed = follow_up(WorkItemState::Failed);
        assert!(
            super::continuation_child(
                &failed,
                super::ResumeSource::Owner("desk".into()),
                &registry_with("alpha")
            )
            .is_ok(),
            "a failed follow-up is open too"
        );
    }

    #[test]
    fn a_settled_follow_up_a_nightly_resume_or_an_unregistered_project_is_refused() {
        let owner = || super::ResumeSource::Owner("desk".into());
        for state in [WorkItemState::Landed, WorkItemState::Cancelled] {
            let status =
                super::continuation_child(&follow_up(state), owner(), &registry_with("alpha"))
                    .expect_err("settled");
            assert_eq!(status.code(), tonic::Code::FailedPrecondition);
            assert_eq!(status.message(), "only preserved work or an open follow-up can be resumed");
        }
        let nightly = super::ResumeSource::Nightly {
            project: "alpha".into(),
            package: "serde".into(),
            target: "2".into(),
        };
        let status = super::continuation_child(
            &follow_up(WorkItemState::NeedsDecision),
            nightly,
            &registry_with("alpha"),
        )
        .expect_err("owners only");
        assert_eq!(status.message(), "only an owner can resume a follow-up");
        let status = super::continuation_child(
            &follow_up(WorkItemState::NeedsDecision),
            owner(),
            &registry_with("beta"),
        )
        .expect_err("unregistered");
        assert_eq!(status.message(), "project is no longer registered");
        let mut task = item("wi_task", "alpha", WorkItemState::NeedsDecision);
        task.objective = "x".into();
        assert_eq!(
            super::continuation_child(&task, owner(), &registry_with("alpha"))
                .expect_err("not a follow-up")
                .message(),
            "only preserved work or an open follow-up can be resumed"
        );
    }

    #[test]
    fn a_wire_source_round_trips_and_an_unknown_kind_or_blank_ref_is_invalid() {
        use foundry_sdk::work_source::WorkSource;
        let campaign = WorkSource::campaign("tidy-cli", 3);
        let wire = super::source_to_proto(&campaign);
        assert_eq!(wire.kind, "campaign");
        assert_eq!(wire.r#ref, "tidy-cli");
        assert_eq!(wire.cycle, Some(3));
        assert_eq!(super::source_from_proto(wire).expect("known kind"), campaign);

        let unknown = crate::proto::WorkSource {
            kind: "dashboard".to_string(),
            r#ref: "x".to_string(),
            cycle: None,
        };
        let status = super::source_from_proto(unknown).expect_err("unknown kind");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("campaign, initiative, sentinel, operator, work_item"));

        let blank = crate::proto::WorkSource {
            kind: "sentinel".to_string(),
            r#ref: String::new(),
            cycle: None,
        };
        assert_eq!(
            super::source_from_proto(blank).expect_err("blank ref").code(),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn an_item_with_no_timestamp_still_sorts_deterministically() {
        // A hand-written record can carry no started_at even while running.
        let store = WorkItemStore {
            version: 1,
            items: vec![
                item("wi_b", "alpha", WorkItemState::Running),
                item("wi_a", "alpha", WorkItemState::Running),
            ],
        };
        let ordered = selected(&store, "", None, None);
        let ids: Vec<&str> = ordered.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["wi_a", "wi_b"], "equal keys break by id ascending");
    }

    #[test]
    fn a_source_filter_needs_a_known_kind_and_a_ref_and_both_empty_means_every_source() {
        assert!(parse_source_filter("", "").expect("empty is every source").is_none());
        assert_eq!(
            parse_source_filter("campaign", "tidy-cli").expect("known kind"),
            Some((WorkSourceKind::Campaign, "tidy-cli".to_string()))
        );
        for (kind, reference) in [
            ("dashboard", "x"),
            ("", "x"),
            ("campaign", ""),
            ("campaign", " "),
        ] {
            let status = parse_source_filter(kind, reference).expect_err("rejected");
            assert_eq!(status.code(), tonic::Code::InvalidArgument, "{kind}:{reference}");
        }
    }

    #[test]
    fn a_source_filter_selects_exactly_that_campaigns_cycles_in_the_documented_order() {
        let mut cycle_one = item("wi_c1", "alpha", WorkItemState::Landed);
        cycle_one.source = Some(WorkSource::campaign("tidy-cli", 1));
        cycle_one.settled_at = Some(at(10));
        let mut cycle_two = item("wi_c2", "alpha", WorkItemState::Running);
        cycle_two.source = Some(WorkSource::campaign("tidy-cli", 2));
        let mut other_campaign = item("wi_other", "alpha", WorkItemState::Running);
        other_campaign.source = Some(WorkSource::campaign("tidy", 1));
        let mut sentinel = item("wi_nightly", "alpha", WorkItemState::Running);
        sentinel.source = Some(WorkSource::sentinel("nightly-maintenance"));
        let unsourced = item("wi_old", "alpha", WorkItemState::Running);
        let store = WorkItemStore {
            version: 1,
            items: vec![cycle_one, cycle_two, other_campaign, sentinel, unsourced],
        };

        let filter = (WorkSourceKind::Campaign, "tidy-cli".to_string());
        let cycles = selected(&store, "", None, Some(&filter));
        let ids: Vec<&str> = cycles.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["wi_c2", "wi_c1"], "running first, then settled; nothing else");

        let everything = selected(&store, "", None, None);
        assert_eq!(everything.len(), 5, "without a filter an unsourced item is listed too");
    }
}

#[cfg(test)]
mod owner_stop_tests {
    use chrono::Utc;
    use foundry_sdk::campaign::CampaignStore;
    use foundry_sdk::initiative::{MemberReservation, ReservationKind};
    use foundry_sdk::work_item::{
        WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
    };

    use super::settle_owner_stop;

    /// A ledger with one queued formation item, and an initiative whose
    /// member reserved its slot under that item's id.
    fn seeded(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf, String) {
        let ledger = dir.join("work-items.json");
        let campaigns = dir.join("campaigns.json");
        let item = WorkItem::queued(
            WorkItemSpec {
                project: "sample".into(),
                objective: "Form the next cycle".into(),
                kind: WorkItemKind::Workflow,
                lane: WorkLane::Campaign,
                origin: "initiative".into(),
                trace_id: None,
            },
            foundry_sdk::event::Event::new(
                foundry_sdk::event::EventType::CampaignAdvanceRequested,
                "sample".into(),
                foundry_sdk::throttle::Throttle::Full,
                serde_json::json!({"campaign":"one","initiative":"shared",
                    "steering_action_id":"evt_steer"}),
            ),
            Utc::now(),
        );
        let id = item.id.clone();
        WorkItemStore {
            items: vec![item],
            ..WorkItemStore::default()
        }
        .save(&ledger)
        .unwrap();
        let mut store = CampaignStore::default();
        store
            .add(
                serde_json::from_value(serde_json::json!({"name":"one","project":"sample",
                "mission":"Ship","authorized_by":"Stacey",
                "done_evidence":[{"kind":"review","statement":"works"}]}))
                .unwrap(),
            )
            .unwrap();
        store
            .add_initiative(
                serde_json::from_value(serde_json::json!({"name":"shared","outcome":"Works",
                "projects":["sample"],"authorized_by":"Stacey",
                "members":[{"kind":"campaign","name":"one"}],
                "done_evidence":[{"kind":"review","statement":"works"}]}))
                .unwrap(),
                &["sample".into()],
            )
            .unwrap();
        store
            .find_initiative_mut("shared")
            .unwrap()
            .reservations
            .push(MemberReservation {
                member: "one".into(),
                operation_id: id.clone(),
                kind: ReservationKind::Formation,
            });
        store.save(&campaigns).unwrap();
        (ledger, campaigns, id)
    }

    #[test]
    fn cancelling_a_queued_formation_releases_its_member_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, campaigns, id) = seeded(dir.path());

        let item = settle_owner_stop(
            &ledger,
            &campaigns,
            &id,
            "cancelled by operator",
            "cli".into(),
            false,
        )
        .unwrap();

        assert_eq!(item.state, WorkItemState::Cancelled);
        let store = CampaignStore::load(&campaigns).unwrap();
        let reservations = &store.find_initiative("shared").unwrap().reservations;
        assert!(reservations.is_empty(), "{reservations:?}");
        assert!(store.occupied_member_slots("shared", &[]).unwrap().is_empty());
    }

    #[test]
    fn a_failed_store_save_keeps_the_reservation_and_reports_it() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, campaigns, id) = seeded(dir.path());
        let before = std::fs::read(&campaigns).unwrap();
        std::fs::create_dir(campaigns.with_extension("json.tmp")).unwrap();

        let error = settle_owner_stop(
            &ledger,
            &campaigns,
            &id,
            "cancelled by operator",
            "cli".into(),
            false,
        )
        .unwrap_err();

        assert_eq!(error.code(), tonic::Code::Internal);
        assert!(error.message().contains("reservation could not be released"), "{error:?}");
        assert_eq!(std::fs::read(&campaigns).unwrap(), before, "the store is unchanged");
        let ledger = WorkItemStore::load(&ledger).unwrap();
        assert_eq!(ledger.items[0].state, WorkItemState::Cancelled);
    }

    #[test]
    fn cancelling_work_that_no_steer_admitted_needs_no_campaign_store() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("work-items.json");
        let mut item = WorkItem::submitted(
            WorkItemSpec {
                project: "sample".into(),
                objective: "Owner task".into(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "cli".into(),
                trace_id: None,
            },
            Utc::now(),
        );
        item.queue();
        let id = item.id.clone();
        WorkItemStore {
            items: vec![item],
            ..WorkItemStore::default()
        }
        .save(&ledger)
        .unwrap();

        let item = settle_owner_stop(
            &ledger,
            std::path::Path::new(""),
            &id,
            "cancelled by operator",
            "cli".into(),
            false,
        )
        .unwrap();

        assert_eq!(item.state, WorkItemState::Cancelled);
    }

    #[test]
    fn closing_an_open_follow_up_settles_it_cancelled_and_keeps_its_obligation() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("work-items.json");
        let mut parent = WorkItem::dispatched(
            WorkItemSpec {
                project: "sample".into(),
                objective: "Owner task".into(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "cli".into(),
                trace_id: None,
            },
            Utc::now(),
        );
        parent.settle_landed("landed", Utc::now());
        let follow_up = foundry_sdk::work_item::FollowUp {
            of: parent.id.clone(),
            commit: None,
            obligation: foundry_sdk::work_item::FollowUpObligation::Remainder {
                gaps: vec!["Document it.".into()],
            },
        }
        .into_item(&parent, Utc::now());
        let id = follow_up.id.clone();
        WorkItemStore {
            items: vec![parent, follow_up.clone()],
            ..WorkItemStore::default()
        }
        .save(&ledger)
        .unwrap();

        let closed = settle_owner_stop(
            &ledger,
            std::path::Path::new(""),
            &id,
            "not needed",
            "cli".into(),
            true,
        )
        .unwrap();

        assert_eq!(closed.state, WorkItemState::Cancelled);
        assert_eq!(closed.reason, "not needed");
        assert_eq!(closed.follow_up, follow_up.follow_up, "the obligation stays on record");
        let action = closed.operator_action.as_ref().unwrap();
        assert_eq!(
            (action.command.as_str(), action.previous_state),
            ("close", WorkItemState::NeedsDecision)
        );
        assert_eq!(WorkItemStore::load(&ledger).unwrap().find(&id), Some(&closed));
    }

    #[test]
    fn closing_an_open_item_leaves_the_campaign_store_alone() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, campaigns, id) = seeded(dir.path());
        let mut store = WorkItemStore::load(&ledger).unwrap();
        store.items[0].settle_failed("workflow failed", Utc::now());
        store.save(&ledger).unwrap();
        let before = std::fs::read(&campaigns).unwrap();

        settle_owner_stop(&ledger, &campaigns, &id, "discharged", "cli".into(), true).unwrap();

        assert_eq!(std::fs::read(&campaigns).unwrap(), before);
    }
}
