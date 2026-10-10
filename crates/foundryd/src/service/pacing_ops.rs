//! The pacing RPCs: read the stage, pause and resume lanes.
//!
//! Reads load the ledger and both pacing files on every call and cache
//! nothing. A pause or resume is a `load` → apply → atomic `save` of the
//! daemon-owned state file, then a `pacing_paused` / `pacing_resumed` event
//! for the project `system`, durable and on Watch; the scheduler wakes on
//! that event and re-evaluates every queued item.

use std::path::Path;

use tonic::{Request, Response, Status};

use foundry_sdk::error::StoreError;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::pacing::{self, LaneSelection, PacingPaths, PauseState, Snapshot, SnapshotItem};
use foundry_sdk::payload::PacingPayload;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{WorkItemStore, WorkLane};

use crate::proto::{
    GetPacingRequest, GetPacingResponse, PacingItem, PacingStatus, PausePacingRequest,
    PausePacingResponse, ResumePacingRequest, ResumePacingResponse,
};

fn map_store_error(what: &str, error: StoreError) -> Status {
    match error {
        StoreError::Parse { source, .. } => {
            Status::failed_precondition(format!("{what} is malformed: {source}"))
        }
        StoreError::Io { source, .. } => {
            Status::internal(format!("{what} is unreadable: {source}"))
        }
        StoreError::NotFound { .. } => {
            Status::internal(format!("{what} load reported NotFound, which it never does"))
        }
    }
}

/// Wire form of one snapshot item.
fn item_to_proto(item: &SnapshotItem) -> PacingItem {
    PacingItem {
        id: item.id.clone(),
        project: item.project.clone(),
        repository: item.repository.clone(),
        kind: item.kind.tag().to_string(),
        lane: item.lane.tag().to_string(),
        state: item.state.tag().to_string(),
        reason: item.reason.clone(),
        since: item.since.map(|at| at.to_rfc3339()).unwrap_or_default(),
    }
}

/// Wire form of the stage.
pub(super) fn snapshot_to_proto(snapshot: &Snapshot) -> PacingStatus {
    PacingStatus {
        max_running: snapshot.max_running as u64,
        running: snapshot.running.len() as u64,
        paused_lanes: snapshot.paused.iter().map(|lane| lane.tag().to_string()).collect(),
        running_items: snapshot.running.iter().map(item_to_proto).collect(),
        waiting_items: snapshot.waiting.iter().map(item_to_proto).collect(),
    }
}

/// The stage as it stands, read off the ledger and the pacing files.
fn snapshot(
    work_items_path: &Path,
    pacing: &PacingPaths,
    ctx: &super::RuntimeContext,
) -> Result<Snapshot, Status> {
    let store = WorkItemStore::load(work_items_path)
        .map_err(|error| map_store_error("work-item ledger", error))?;
    let limits = pacing::Limits::load(&pacing.limits)
        .map_err(|error| map_store_error("pacing limits file", error))?;
    let pauses = PauseState::load(&pacing.state)
        .map_err(|error| map_store_error("pacing state file", error))?;
    let registry = ctx.registry.read().map_err(|_| Status::internal("registry lock poisoned"))?;
    let repository_of = |project: &str| pacing::repository_key(&registry, project);
    Ok(pacing::snapshot(&store.items, &limits, &pauses, &repository_of))
}

pub(super) fn get(
    work_items_path: &Path,
    pacing: &PacingPaths,
    ctx: &super::RuntimeContext,
    _request: Request<GetPacingRequest>,
) -> Result<Response<GetPacingResponse>, Status> {
    let snapshot = snapshot(work_items_path, pacing, ctx)?;
    Ok(Response::new(GetPacingResponse {
        pacing: Some(snapshot_to_proto(&snapshot)),
    }))
}

/// Parse the lanes a request names: each a lane tag or `all`; none means all.
fn parse_lanes(lanes: &[String]) -> Result<Vec<WorkLane>, Status> {
    if lanes.is_empty() {
        return Ok(WorkLane::ALL.to_vec());
    }
    let mut selected = Vec::new();
    for text in lanes {
        let selection = LaneSelection::parse(text.trim()).ok_or_else(|| {
            Status::invalid_argument(format!(
                "unknown lane '{text}'; expected one of interactive, campaign, maintenance, all"
            ))
        })?;
        selected.extend(selection.lanes());
    }
    Ok(WorkLane::ALL.into_iter().filter(|lane| selected.contains(lane)).collect())
}

/// Which way a lane transition goes.
#[derive(Clone, Copy)]
enum Transition {
    Pause,
    Resume,
}

/// Apply `transition` to `lanes` in the state file, then announce it.
async fn transition(
    work_items_path: &Path,
    pacing: &PacingPaths,
    ctx: &super::RuntimeContext,
    lanes: &[String],
    operator_origin: String,
    transition: Transition,
) -> Result<PacingStatus, Status> {
    if operator_origin.trim().is_empty() {
        return Err(Status::invalid_argument("operator origin must be nonblank"));
    }
    let lanes = parse_lanes(lanes)?;
    let state_path = pacing.state.clone();
    let apply_lanes = lanes.clone();
    let paused = tokio::task::spawn_blocking(move || {
        let mut state = PauseState::load(&state_path)
            .map_err(|error| map_store_error("pacing state file", error))?;
        match transition {
            Transition::Pause => {
                state.pause(&apply_lanes);
            }
            Transition::Resume => {
                state.resume(&apply_lanes);
            }
        }
        state.save(&state_path).map_err(|error| {
            Status::internal(format!("failed to persist pacing state: {error}"))
        })?;
        Ok::<_, Status>(state.paused)
    })
    .await
    .map_err(|error| Status::internal(format!("pacing mutation did not finish: {error}")))??;

    let payload = Event::serialize_payload(&PacingPayload {
        lanes,
        paused,
        operator_origin,
    })
    .map_err(|error| Status::internal(format!("cannot serialize pacing event: {error}")))?;
    let event_type = match transition {
        Transition::Pause => EventType::PacingPaused,
        Transition::Resume => EventType::PacingResumed,
    };
    let event = Event::new(event_type, "system".to_string(), Throttle::Full, payload)
        .with_trace_id(Some(foundry_sdk::event::mint_trace_id()));
    ctx.engine.process(event).await;

    let snapshot = snapshot(work_items_path, pacing, ctx)?;
    Ok(snapshot_to_proto(&snapshot))
}

pub(super) async fn pause(
    work_items_path: &Path,
    pacing: &PacingPaths,
    ctx: &super::RuntimeContext,
    request: Request<PausePacingRequest>,
) -> Result<Response<PausePacingResponse>, Status> {
    let request = request.into_inner();
    let status = transition(
        work_items_path,
        pacing,
        ctx,
        &request.lanes,
        request.operator_origin,
        Transition::Pause,
    )
    .await?;
    Ok(Response::new(PausePacingResponse {
        pacing: Some(status),
    }))
}

pub(super) async fn resume(
    work_items_path: &Path,
    pacing: &PacingPaths,
    ctx: &super::RuntimeContext,
    request: Request<ResumePacingRequest>,
) -> Result<Response<ResumePacingResponse>, Status> {
    let request = request.into_inner();
    let status = transition(
        work_items_path,
        pacing,
        ctx,
        &request.lanes,
        request.operator_origin,
        Transition::Resume,
    )
    .await?;
    Ok(Response::new(ResumePacingResponse {
        pacing: Some(status),
    }))
}

#[cfg(test)]
mod tests {
    use super::parse_lanes;
    use foundry_sdk::work_item::WorkLane;

    #[test]
    fn lanes_parse_each_tag_and_all_and_none_means_every_lane() {
        assert_eq!(parse_lanes(&[]).expect("none"), WorkLane::ALL.to_vec());
        assert_eq!(parse_lanes(&["all".to_string()]).expect("all"), WorkLane::ALL.to_vec());
        assert_eq!(
            parse_lanes(&["campaign".to_string(), "interactive".to_string()]).expect("two"),
            vec![WorkLane::Interactive, WorkLane::Campaign],
            "in lane order, whatever the request's order"
        );
        assert_eq!(
            parse_lanes(&["nightly".to_string()]).expect_err("unknown").code(),
            tonic::Code::InvalidArgument
        );
    }
}
