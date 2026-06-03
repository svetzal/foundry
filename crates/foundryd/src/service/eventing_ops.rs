use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;
use tonic::{Request, Response, Status};

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::throttle::Throttle;
use foundry_core::trace::ProcessResult;

use crate::proto::{
    EmitRequest, EmitResponse, StatusRequest, StatusResponse, WatchRequest, WatchResponse,
    WorkflowStatus,
};
use crate::trace_store::TraceStore;
use crate::workflow_tracker::{WorkflowGuard, WorkflowTracker};
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;

pub(super) fn parse_throttle(proto_value: i32) -> Throttle {
    match proto_value {
        1 => Throttle::DryRun,
        _ => Throttle::Full,
    }
}

pub(super) fn parse_emit_request(req: EmitRequest) -> Result<Event, Status> {
    let event_type: EventType =
        req.event_type.parse().map_err(|e| Status::invalid_argument(format!("{e}")))?;

    let throttle = parse_throttle(req.throttle);

    let payload: serde_json::Value = if req.payload_json.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(&req.payload_json)
            .map_err(|e| Status::invalid_argument(format!("invalid payload JSON: {e}")))?
    };

    let trace_id = if req.trace_id.is_empty() {
        foundry_core::event::mint_trace_id()
    } else {
        req.trace_id
    };
    let request_span_id = if req.span_id.is_empty() {
        Some(foundry_core::event::mint_span_id())
    } else {
        Some(req.span_id)
    };
    let request_parent_span_id = if req.parent_span_id.is_empty() {
        None
    } else {
        Some(req.parent_span_id)
    };
    Ok(Event::new(event_type, req.project, throttle, payload)
        .with_trace_id(Some(trace_id))
        .with_span_ids(request_span_id, request_parent_span_id))
}

/// Extract per-project sub-traces from a system-level maintenance `ProcessResult`.
fn extract_per_project_traces(result: &ProcessResult) -> HashMap<String, ProcessResult> {
    let event_map: HashMap<&str, &Event> =
        result.events.iter().map(|e| (e.id.as_str(), e)).collect();

    let project_roots: Vec<&Event> = result
        .events
        .iter()
        .filter(|e| e.event_type == EventType::ProjectRunStarted && e.project != "system")
        .collect();

    let mut traces = HashMap::new();

    for root in project_roots {
        let project = &root.project;

        let events: Vec<Event> =
            result.events.iter().filter(|e| e.project == *project).cloned().collect();

        let block_executions: Vec<_> = result
            .block_executions
            .iter()
            .filter(|b| {
                event_map
                    .get(b.trigger_event_id.as_str())
                    .is_some_and(|e| e.project == *project)
            })
            .cloned()
            .collect();

        let total_duration_ms: u64 = block_executions.iter().map(|b| b.duration_ms).sum();

        traces.insert(
            project.clone(),
            ProcessResult {
                events,
                block_executions,
                total_duration_ms,
            },
        );
    }

    traces
}

/// After a system-level maintenance cycle completes, write per-project sub-traces
/// to disk and emit `MaintenanceSummaryRequested` for the summary phase.
async fn finalise_system_maintenance(
    result: &ProcessResult,
    engine: &Engine,
    trace_writer: &TraceWriter,
    registry: &Arc<RwLock<Registry>>,
    throttle: Throttle,
    event_tx: &broadcast::Sender<Event>,
    root_event_id: &str,
) {
    // Extract skipped projects before any .await — RwLock guards must not cross await points.
    let skipped_projects: Vec<String> = {
        let reg = registry.read().expect("registry lock poisoned");
        reg.projects
            .iter()
            .filter(|p| p.skip.is_some())
            .map(|p| p.name.clone())
            .collect()
    };

    let per_project = extract_per_project_traces(result);
    let mut project_trace_ids: HashMap<String, String> = HashMap::new();

    for (project_name, sub_result) in &per_project {
        if let Some(root_evt) =
            sub_result.events.iter().find(|e| e.event_type == EventType::ProjectRunStarted)
        {
            let sub_id = root_evt.id.clone();
            if let Err(e) = trace_writer.write(&sub_id, sub_result) {
                tracing::warn!(
                    error = %e,
                    project = %project_name,
                    "failed to write per-project trace"
                );
            }
            project_trace_ids.insert(project_name.clone(), sub_id);
        }
    }

    let summary_event = Event::new(
        EventType::MaintenanceSummaryRequested,
        "system".to_string(),
        throttle,
        serde_json::json!({
            "project_trace_ids": project_trace_ids,
            "skipped_projects": skipped_projects,
            "total_duration_ms": result.total_duration_ms,
            "root_event_id": root_event_id,
        }),
    );

    let summary_result = engine.process(summary_event.clone()).await;

    if let Err(e) = trace_writer.write(&summary_event.id, &summary_result) {
        tracing::warn!(error = %e, "failed to write summary trace");
    }

    let _ = event_tx.send(summary_event);
}

pub(super) async fn run_workflow(
    event: Event,
    engine: Arc<Engine>,
    trace_store: Arc<TraceStore>,
    tracker: Arc<WorkflowTracker>,
    trace_writer: Arc<TraceWriter>,
    event_tx: broadcast::Sender<Event>,
    registry: Arc<RwLock<Registry>>,
) {
    let event_id = event.id.clone();
    let root_event_type = event.event_type.clone();
    let root_project = event.project.clone();
    let root_throttle = event.throttle;

    let _guard = WorkflowGuard::new(tracker, event_id.clone());

    let result = engine.process(event).await;

    tracing::info!(
        total_events = result.events.len(),
        blocks_executed = result.block_executions.len(),
        "event chain complete"
    );

    if let Err(e) = trace_writer.write(&event_id, &result) {
        tracing::warn!(error = %e, event_id = %event_id, "failed to write trace to disk");
    }

    if root_event_type == EventType::MaintenanceCycleStarted && root_project == "system" {
        finalise_system_maintenance(
            &result,
            &engine,
            &trace_writer,
            &registry,
            root_throttle,
            &event_tx,
            &event_id,
        )
        .await;
    } else if root_event_type == EventType::ProjectRunStarted {
        let success = result.is_success();
        let completed = Event::new(
            EventType::ProjectRunCompleted,
            root_project,
            root_throttle,
            serde_json::json!({
                "success": success,
                "root_event_id": event_id,
            }),
        );
        let _ = event_tx.send(completed);
    }

    trace_store.insert(event_id, result);
}

pub(super) fn emit_rpc(
    engine: &Arc<Engine>,
    trace_store: &Arc<TraceStore>,
    workflow_tracker: &Arc<WorkflowTracker>,
    trace_writer: &Arc<TraceWriter>,
    event_tx: &broadcast::Sender<Event>,
    registry: &Arc<RwLock<Registry>>,
    request: Request<EmitRequest>,
) -> Result<Response<EmitResponse>, Status> {
    let event = parse_emit_request(request.into_inner())?;
    let event_id = event.id.clone();

    tracing::info!(
        event_id = %event_id,
        event_type = %event.event_type,
        project = %event.project,
        throttle = %event.throttle,
        "event accepted, spawning background processing"
    );

    super::spawn_workflow(
        event,
        Arc::clone(engine),
        Arc::clone(trace_store),
        Arc::clone(workflow_tracker),
        Arc::clone(trace_writer),
        event_tx.clone(),
        Arc::clone(registry),
    );

    Ok(Response::new(EmitResponse {
        event_id,
        workflow_id: String::new(),
    }))
}

pub(super) fn status_rpc(
    workflow_tracker: &Arc<WorkflowTracker>,
    request: Request<StatusRequest>,
) -> Response<StatusResponse> {
    let req = request.into_inner();
    let filter_id = req.workflow_id;

    let active = workflow_tracker.list();

    let workflows = active
        .into_iter()
        .filter(|w| filter_id.is_empty() || w.event_id == filter_id)
        .map(|w| WorkflowStatus {
            workflow_id: w.event_id,
            workflow_type: w.event_type,
            project: w.project,
            state: "running".to_string(),
            started_at: w.started_at.to_rfc3339(),
            completed_at: String::new(),
            task_blocks: vec![],
            trace_id: w.trace_id,
        })
        .collect();

    Response::new(StatusResponse { workflows })
}

type WatchStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<WatchResponse, Status>> + Send>>;

pub(super) fn watch_rpc(
    event_tx: &broadcast::Sender<Event>,
    request: Request<WatchRequest>,
) -> Response<WatchStream> {
    let span = tracing::info_span!("watch");
    let _guard = span.enter();

    let project_filter = request.into_inner().project;
    let mut rx = event_tx.subscribe();

    tracing::info!(project = %project_filter, "watch stream started");

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if project_filter.is_empty() || event.project == project_filter {
                        yield Ok(WatchResponse {
                            event_id: event.id.clone(),
                            event_type: event.event_type.to_string(),
                            project: event.project.clone(),
                            payload_json: event.payload.to_string(),
                            trace_id: event.trace_id.clone().unwrap_or_default(),
                            span_id: event.span_id.clone().unwrap_or_default(),
                            parent_span_id: event.parent_span_id.clone().unwrap_or_default(),
                        });
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(missed = n, "watch subscriber lagged, skipping missed events");
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    };

    Response::new(Box::pin(stream))
}
