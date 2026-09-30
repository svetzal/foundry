//! Reads and owner controls of the daemon-owned work-item ledger, and of one item's
//! `work_item_*` events in the durable event log.
//!
//! Every operation here loads from disk on every call and caches nothing: the
//! ledger is authoritative on disk, and a reader that cached it would report
//! work as running after another process settled it. Reads do not take the
//! write gate; mutations serialize their load→modify→save sequences.

use std::path::{Path, PathBuf};

use tonic::{Request, Response, Status};

use foundry_sdk::error::StoreError;
use foundry_sdk::work_item::{WorkItem, WorkItemState, WorkItemStore};
use foundry_sdk::work_item_events::{WorkItemEventRecord, read_work_item_events};

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

fn load_store(path: &Path) -> Result<WorkItemStore, Status> {
    WorkItemStore::load(path).map_err(map_store_error)
}

/// Which ordering group an item's state puts it in.
///
/// The groups are the reading order an operator wants: what is under way, what
/// is waiting, what still needs a person, and finally what is done with.
fn order_group(state: WorkItemState) -> u8 {
    match state {
        WorkItemState::Running => 0,
        WorkItemState::Submitted | WorkItemState::Queued => 1,
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
        WorkItemState::Submitted | WorkItemState::Queued => item.submitted_at.timestamp_micros(),
        WorkItemState::Preserved
        | WorkItemState::NeedsDecision
        | WorkItemState::Failed
        | WorkItemState::Landed
        | WorkItemState::Cancelled => -item.settled_at.map_or(0, |at| at.timestamp_micros()),
    };
    (group, stamp, item.id.as_str())
}

/// Every item the request selects, in the RPC's deterministic order.
///
/// Pure over the loaded store, so the ordering and filtering rules are
/// testable without a service or a filesystem.
fn selected(store: &WorkItemStore, project: &str, state: Option<WorkItemState>) -> Vec<WorkItem> {
    let mut items: Vec<WorkItem> = store
        .items
        .iter()
        .filter(|item| project.is_empty() || item.project == project)
        .filter(|item| state.is_none_or(|wanted| item.state == wanted))
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

pub(super) fn list(
    work_items_path: &Path,
    request: Request<ListWorkItemsRequest>,
) -> Result<Response<ListWorkItemsResponse>, Status> {
    let request = request.into_inner();
    let state = parse_state_filter(&request.state)?;
    let store = load_store(work_items_path)?;
    let items = selected(&store, &request.project, state).iter().map(item_to_proto).collect();
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
        let allowed = if close {
            item.state.is_open()
        } else {
            matches!(item.state, WorkItemState::Submitted | WorkItemState::Queued)
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
        item.settle_cancelled(&reason, item.disposition.clone(), chrono::Utc::now());
        let item = item.clone();
        store.save(&path).map_err(|error| {
            Status::internal(format!("failed to persist work-item state: {error}"))
        })?;
        Ok(item)
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

/// Admit a continuation atomically before allowing execution to start.
#[tracing::instrument(skip_all, fields(item_id = %id))]
pub(super) async fn resume_item(
    path: &Path,
    ctx: &super::RuntimeContext,
    id: String,
    operator_origin: String,
) -> Result<ProtoWorkItem, Status> {
    if id.trim().is_empty() || operator_origin.trim().is_empty() {
        return Err(Status::invalid_argument("id and operator origin must be nonblank"));
    }
    let path = path.to_path_buf();
    let registry = std::sync::Arc::clone(&ctx.registry);
    let (item, base) = tokio::task::spawn_blocking(move || {
        let _guard = foundry_sdk::work_item::ledger_write_gate()
            .lock()
            .map_err(|_| Status::internal("work-item ledger write gate poisoned"))?;
        let mut store = load_store(&path)?;
        let parent = store
            .find(&id)
            .ok_or_else(|| Status::not_found(format!("work item '{id}' not found")))?;
        if parent.state != WorkItemState::Preserved {
            return Err(Status::failed_precondition("only preserved work can be resumed"));
        }
        if parent.objective.trim().is_empty() {
            return Err(Status::failed_precondition("preserved work has no objective"));
        }
        let entry = registry
            .read()
            .map_err(|_| Status::internal("registry lock poisoned"))?
            .find_project(&parent.project)
            .cloned()
            .ok_or_else(|| Status::failed_precondition("project is no longer registered"))?;
        let base = parent
            .disposition
            .as_ref()
            .and_then(|d| d.preservation_ref.as_ref())
            .filter(|reference| !reference.trim().is_empty())
            .ok_or_else(|| Status::failed_precondition("preserved work has no preservation ref"))?
            .clone();
        validate_preservation(&entry.path, &base)?;
        let mut child = WorkItem::dispatched(
            foundry_sdk::work_item::WorkItemSpec {
                project: parent.project.clone(),
                objective: parent.objective.clone(),
                kind: foundry_sdk::work_item::WorkItemKind::Task,
                lane: foundry_sdk::work_item::WorkLane::Interactive,
                origin: parent.origin.clone(),
                trace_id: Some(foundry_sdk::event::mint_trace_id()),
            },
            chrono::Utc::now(),
        );
        child.resumes = Some(parent.id.clone());
        child.operator_action = Some(foundry_sdk::work_item::WorkItemOperatorAction {
            command: "resume".to_string(),
            origin: operator_origin,
            previous_state: parent.state,
            previous_reason: parent.reason.clone(),
            previous_settled_at: parent.settled_at,
        });
        store.upsert(child.clone());
        store.save(&path).map_err(|error| {
            Status::internal(format!("failed to persist work-item state: {error}"))
        })?;
        Ok((child, base))
    })
    .await
    .map_err(|error| Status::internal(format!("resume admission did not finish: {error}")))??;
    for event_type in [
        foundry_sdk::event::EventType::WorkItemSubmitted,
        foundry_sdk::event::EventType::WorkItemStarted,
    ] {
        let mut snapshot = item.clone();
        if event_type == foundry_sdk::event::EventType::WorkItemSubmitted {
            snapshot.state = WorkItemState::Submitted;
            snapshot.reason = "submitted".to_string();
        }
        let payload = foundry_sdk::event::Event::serialize_payload(
            &foundry_sdk::payload::WorkItemEventPayload::from_item(&snapshot),
        )
        .map_err(|error| Status::internal(format!("cannot serialize resume: {error}")))?;
        ctx.engine
            .process(
                foundry_sdk::event::Event::new(
                    event_type,
                    item.project.clone(),
                    foundry_sdk::throttle::Throttle::Full,
                    payload,
                )
                .with_trace_id(item.trace_id.clone()),
            )
            .await;
    }
    let event = foundry_sdk::event::Event::new(
        foundry_sdk::event::EventType::ExecutionRequested,
        item.project.clone(),
        foundry_sdk::throttle::Throttle::Full,
        serde_json::json!({"project": item.project, "prompt": item.objective,
            "workflow": "task", "base_ref": base, "admitted_work_item_id": item.id}),
    )
    .with_trace_id(item.trace_id.clone());
    super::spawn_workflow(event, ctx);
    Ok(item_to_proto(&item))
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

    use super::{order_group, parse_state_filter, selected};
    use foundry_sdk::work_item::{
        WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
    };

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
    fn an_item_with_no_timestamp_still_sorts_deterministically() {
        // A hand-written record can carry no started_at even while running.
        let store = WorkItemStore {
            version: 1,
            items: vec![
                item("wi_b", "alpha", WorkItemState::Running),
                item("wi_a", "alpha", WorkItemState::Running),
            ],
        };
        let ordered = selected(&store, "", None);
        let ids: Vec<&str> = ordered.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["wi_a", "wi_b"], "equal keys break by id ascending");
    }
}
