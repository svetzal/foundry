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

fn load_store(path: &Path) -> Result<WorkItemStore, Status> {
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

/// Keep owner attribution and automated upgrade identity explicit at admission.
pub(super) enum ResumeSource {
    Owner(String),
    Nightly {
        project: String,
        package: String,
        target: String,
    },
}

/// Admit a continuation atomically before allowing execution to start.
#[tracing::instrument(skip_all, fields(item_id = %id))]
pub(super) async fn resume_item(
    path: &Path,
    ctx: &super::RuntimeContext,
    id: String,
    operator_origin: String,
) -> Result<ProtoWorkItem, Status> {
    let (item, event) = admit_resume(path, ctx, id, ResumeSource::Owner(operator_origin)).await?;
    super::spawn_workflow(event, ctx);
    Ok(item_to_proto(&item))
}

/// Shared durable admission; scheduling remains the caller's responsibility.
/// Scheduling and submission identity are explicit for owner and nightly work.
pub(super) async fn admit_resume(
    path: &Path,
    ctx: &super::RuntimeContext,
    id: String,
    source: ResumeSource,
) -> Result<(WorkItem, foundry_sdk::event::Event), Status> {
    if id.trim().is_empty()
        || matches!(&source, ResumeSource::Owner(origin) if origin.trim().is_empty())
    {
        return Err(Status::invalid_argument("id and operator origin must be nonblank"));
    }
    let path = path.to_path_buf();
    let admission_path = path.clone();
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
        if let ResumeSource::Nightly {
            project,
            package,
            target,
        } = &source
            && (parent.project != *project
                || foundry_blocks::dependency_updates::majors::parse_objective(&parent.objective)
                    != Some((package.clone(), target.clone(), project.clone())))
        {
            return Err(Status::failed_precondition(
                "preserved upgrade identity changed since planning",
            ));
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
        let child = resume_child(
            parent,
            match source {
                ResumeSource::Owner(origin) => Some(origin),
                ResumeSource::Nightly { .. } => None,
            },
        );
        // Fail closed: until both lifecycle appends and the final ledger save
        // succeed, this child holds no claim that execution has started.
        let mut rejected = child.clone();
        rejected.state = WorkItemState::Failed;
        rejected.reason = "resume admission incomplete; execution not dispatched".to_string();
        rejected.started_at = None;
        rejected.settled_at = Some(chrono::Utc::now());
        store.upsert(rejected);
        store.save(&path).map_err(|error| {
            Status::internal(format!("failed to persist work-item state: {error}"))
        })?;
        Ok((child, base))
    })
    .await
    .map_err(|error| Status::internal(format!("resume admission did not finish: {error}")))??;
    persist_resume_lifecycle(&ctx.engine, &item).await?;
    let admitted = item.clone();
    tokio::task::spawn_blocking(move || {
        let _guard = foundry_sdk::work_item::ledger_write_gate()
            .lock()
            .map_err(|_| Status::internal("work-item ledger write gate poisoned"))?;
        let mut store = load_store(&admission_path)?;
        let current = store
            .find(&admitted.id)
            .ok_or_else(|| Status::internal("resume admission record disappeared"))?;
        if current.state != WorkItemState::Failed {
            return Err(Status::internal("resume admission record changed before dispatch"));
        }
        store.upsert(admitted);
        store.save(&admission_path).map_err(|error| {
            Status::internal(format!("failed to persist work-item state: {error}"))
        })
    })
    .await
    .map_err(|error| Status::internal(format!("resume admission did not finish: {error}")))??;
    let event = continuation_root(&item, &base);
    Ok((item, event))
}

/// The task-workflow root that runs an admitted continuation `item` from the
/// preservation ref `base`.
///
/// It names the admitted item so `RecordWorkItem` does not mint a second
/// identity on the same trace, and it carries the item's typed source so the
/// chain below it says what dispatched the work.
fn continuation_root(item: &WorkItem, base: &str) -> foundry_sdk::event::Event {
    foundry_sdk::event::Event::new(
        foundry_sdk::event::EventType::ExecutionRequested,
        item.project.clone(),
        foundry_sdk::throttle::Throttle::Full,
        serde_json::json!({"project": item.project, "prompt": item.objective,
            "workflow": "task", "base_ref": base, "admitted_work_item_id": item.id}),
    )
    .with_trace_id(item.trace_id.clone())
    .with_source(item.source.clone())
}

/// Submission identity reflects who is continuing the obligation, while the
/// original record and its evidence remain untouched.
///
/// The child's typed source is the parent item, for an owner resume and a
/// nightly continuation alike: it is work created from another item, and
/// `resumes` already carries the same link. Who asked is recorded in the
/// child's operator action (owner) or its lane and origin (nightly).
fn resume_child(parent: &WorkItem, operator_origin: Option<String>) -> WorkItem {
    let nightly = operator_origin.is_none();
    let mut child = WorkItem::dispatched(
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
    child.resumes = Some(parent.id.clone());
    child.operator_action =
        operator_origin.map(|origin| foundry_sdk::work_item::WorkItemOperatorAction {
            command: "resume".to_string(),
            origin,
            previous_state: parent.state,
            previous_reason: parent.reason.clone(),
            previous_settled_at: parent.settled_at,
        });
    child
}

/// Lifecycle roots are admission prerequisites; their downstream processing
/// retains the engine's existing policy.
async fn persist_resume_lifecycle(
    engine: &foundry_engine::engine::Engine,
    item: &WorkItem,
) -> Result<(), Status> {
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
        engine
            .process_required(
                foundry_sdk::event::Event::new(
                    event_type,
                    item.project.clone(),
                    foundry_sdk::throttle::Throttle::Full,
                    payload,
                )
                .with_trace_id(item.trace_id.clone()),
            )
            .await
            .map_err(|error| {
                Status::internal(format!("failed to persist resume lifecycle: {error}"))
            })?;
    }
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

        let owner = super::resume_child(&parent, Some("host desk".to_string()));
        assert_eq!(owner.source, Some(WorkSource::work_item("wi_parent")));
        assert_eq!(owner.resumes.as_deref(), Some("wi_parent"));
        assert_eq!(owner.operator_action.as_ref().map(|a| a.command.as_str()), Some("resume"));

        let nightly = super::resume_child(&parent, None);
        assert_eq!(nightly.source, Some(WorkSource::work_item("wi_parent")));
        assert_eq!(nightly.resumes.as_deref(), Some("wi_parent"));
        assert_eq!(nightly.kind, WorkItemKind::MajorUpgrade);

        assert_eq!(parent.source, Some(WorkSource::operator("workbench")), "parent untouched");
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
        assert!(status.message().contains("campaign, sentinel, operator, work_item"));

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
