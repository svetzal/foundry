use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::broadcast;
use tonic::{Request, Response, Status};
use tracing::Instrument;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::throttle::Throttle;
use foundry_core::trace::ProcessResult;

use crate::engine::Engine;
use crate::proto::{
    EmitRequest, EmitResponse, StatusRequest, StatusResponse, TraceBlockExecution, TraceEvent,
    TraceRequest, TraceResponse, WatchRequest, WatchResponse, WorkflowStatus,
    foundry_server::Foundry,
};
use crate::trace_store::TraceStore;
use crate::trace_writer::TraceWriter;
use crate::workflow_tracker::{ActiveWorkflow, WorkflowGuard, WorkflowTracker};

pub struct FoundryService {
    engine: Arc<Engine>,
    trace_store: Arc<TraceStore>,
    workflow_tracker: Arc<WorkflowTracker>,
    /// Sender held so new receivers can be created for each Watch subscriber.
    event_tx: broadcast::Sender<Event>,
    trace_writer: Arc<TraceWriter>,
    registry: Arc<Registry>,
}

impl FoundryService {
    pub fn new(
        engine: Arc<Engine>,
        trace_store: Arc<TraceStore>,
        event_tx: broadcast::Sender<Event>,
        workflow_tracker: Arc<WorkflowTracker>,
        trace_writer: Arc<TraceWriter>,
        registry: Arc<Registry>,
    ) -> Self {
        Self {
            engine,
            trace_store,
            workflow_tracker,
            event_tx,
            trace_writer,
            registry,
        }
    }
}

/// Extract per-project sub-traces from a system-level maintenance `ProcessResult`.
///
/// Groups events and block executions by project name, returning a map of
/// project name → `ProcessResult` for each per-project `MaintenanceRunStarted`
/// event found in the result.
fn extract_per_project_traces(result: &ProcessResult) -> HashMap<String, ProcessResult> {
    let event_map: HashMap<&str, &Event> =
        result.events.iter().map(|e| (e.id.as_str(), e)).collect();

    // Find per-project root events.
    let project_roots: Vec<&Event> = result
        .events
        .iter()
        .filter(|e| e.event_type == EventType::MaintenanceRunStarted && e.project != "system")
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

/// After a system-level maintenance run completes, write per-project sub-traces
/// to disk, then synthesise and process `MaintenanceRunCompleted` so that
/// `GenerateSummary` can read the traces.
async fn finalise_system_maintenance(
    result: &ProcessResult,
    engine: &Engine,
    trace_writer: &TraceWriter,
    registry: &Registry,
    throttle: Throttle,
    event_tx: &broadcast::Sender<Event>,
    root_event_id: &str,
) {
    let per_project = extract_per_project_traces(result);
    let mut project_trace_ids: HashMap<String, String> = HashMap::new();

    for (project_name, sub_result) in &per_project {
        if let Some(root_evt) = sub_result
            .events
            .iter()
            .find(|e| e.event_type == EventType::MaintenanceRunStarted)
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

    let skipped_projects: Vec<String> = registry
        .projects
        .iter()
        .filter(|p| p.skip.is_some())
        .map(|p| p.name.clone())
        .collect();

    let completed_event = Event::new(
        EventType::MaintenanceRunCompleted,
        "system".to_string(),
        throttle,
        serde_json::json!({
            "project_trace_ids": project_trace_ids,
            "skipped_projects": skipped_projects,
            "total_duration_ms": result.total_duration_ms,
            "root_event_id": root_event_id,
        }),
    );

    let summary_result = engine.process(completed_event.clone()).await;

    if let Err(e) = trace_writer.write(&completed_event.id, &summary_result) {
        tracing::warn!(error = %e, "failed to write summary trace");
    }

    let _ = event_tx.send(completed_event);
}

fn parse_throttle(proto_value: i32) -> Throttle {
    match proto_value {
        1 => Throttle::AuditOnly,
        2 => Throttle::DryRun,
        _ => Throttle::Full,
    }
}

#[tonic::async_trait]
impl Foundry for FoundryService {
    async fn emit(&self, request: Request<EmitRequest>) -> Result<Response<EmitResponse>, Status> {
        let req = request.into_inner();

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
        let event =
            Event::new(event_type, req.project, throttle, payload).with_trace_id(Some(trace_id));
        let event_id = event.id.clone();

        tracing::info!(
            event_id = %event_id,
            event_type = %event.event_type,
            project = %event.project,
            throttle = %event.throttle,
            "event accepted, spawning background processing"
        );

        // Register as active before spawning so status is immediately visible.
        self.workflow_tracker.insert(ActiveWorkflow {
            event_id: event_id.clone(),
            event_type: event.event_type.to_string(),
            project: event.project.clone(),
            started_at: chrono::Utc::now(),
        });

        let engine = Arc::clone(&self.engine);
        let trace_store = Arc::clone(&self.trace_store);
        let tracker = Arc::clone(&self.workflow_tracker);
        let trace_writer = Arc::clone(&self.trace_writer);
        let event_tx = self.event_tx.clone();
        let root_event_type = event.event_type.clone();
        let root_project = event.project.clone();
        let root_throttle = event.throttle;
        let registry = Arc::clone(&self.registry);

        let span = tracing::info_span!(
            "process",
            event_id = %event_id,
            event_type = %event.event_type,
            project = %event.project,
        );

        let bg_event_id = event_id.clone();
        tokio::spawn(
            async move {
                // Guard ensures removal from tracker even on panic.
                let _guard = WorkflowGuard::new(tracker, bg_event_id.clone());

                let result = engine.process(event).await;

                tracing::info!(
                    total_events = result.events.len(),
                    blocks_executed = result.block_executions.len(),
                    "event chain complete"
                );

                // Persist trace to disk before inserting into the in-memory store.
                if let Err(e) = trace_writer.write(&bg_event_id, &result) {
                    tracing::warn!(error = %e, event_id = %bg_event_id, "failed to write trace to disk");
                }

                if root_event_type == EventType::MaintenanceRunStarted
                    && root_project == "system"
                {
                    finalise_system_maintenance(
                        &result,
                        &engine,
                        &trace_writer,
                        &registry,
                        root_throttle,
                        &event_tx,
                        &bg_event_id,
                    )
                    .await;
                } else if root_event_type == EventType::MaintenanceRunStarted {
                    // Per-project maintenance run (not system-level): broadcast
                    // completion so Watch clients see it.
                    let success = result.is_success();
                    let completed = Event::new(
                        EventType::MaintenanceRunCompleted,
                        root_project,
                        root_throttle,
                        serde_json::json!({
                            "success": success,
                            "root_event_id": bg_event_id,
                        }),
                    );
                    let _ = event_tx.send(completed);
                }

                trace_store.insert(bg_event_id, result);
            }
            .instrument(span),
        );

        let response = EmitResponse {
            event_id,
            workflow_id: String::new(),
        };

        Ok(Response::new(response))
    }

    async fn status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let req = request.into_inner();
        let filter_id = req.workflow_id;

        let active = self.workflow_tracker.list();

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
            })
            .collect();

        Ok(Response::new(StatusResponse { workflows }))
    }

    type WatchStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<WatchResponse, Status>> + Send>>;

    async fn watch(
        &self,
        request: Request<WatchRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let span = tracing::info_span!("watch");
        let _guard = span.enter();

        let project_filter = request.into_inner().project;
        let mut rx = self.event_tx.subscribe();

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

        Ok(Response::new(Box::pin(stream)))
    }

    async fn trace(
        &self,
        request: Request<TraceRequest>,
    ) -> Result<Response<TraceResponse>, Status> {
        let req = request.into_inner();

        let span = tracing::info_span!("trace", event_id = %req.event_id);
        let _guard = span.enter();

        if let Some(result) = self.trace_store.get(&req.event_id) {
            let events = result
                .events
                .iter()
                .map(|e| TraceEvent {
                    event_id: e.id.clone(),
                    event_type: e.event_type.as_str().to_string(),
                    project: e.project.clone(),
                    occurred_at: e.occurred_at.to_rfc3339(),
                    throttle: match e.throttle {
                        Throttle::Full => 0,
                        Throttle::AuditOnly => 1,
                        Throttle::DryRun => 2,
                    },
                    trace_id: e.trace_id.clone().unwrap_or_default(),
                })
                .collect();

            let block_executions = result
                .block_executions
                .iter()
                .map(|b| TraceBlockExecution {
                    block_name: b.block_name.clone(),
                    trigger_event_id: b.trigger_event_id.clone(),
                    success: b.success,
                    summary: b.summary.clone(),
                    emitted_event_ids: b.emitted_event_ids.clone(),
                    duration_ms: b.duration_ms,
                    raw_output: b.raw_output.clone().unwrap_or_default(),
                    exit_code: b.exit_code.unwrap_or(0),
                    trigger_payload_json: b.trigger_payload.to_string(),
                    emitted_payload_jsons: b
                        .emitted_payloads
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                    audit_artifacts: b.audit_artifacts.clone(),
                })
                .collect();

            let total_duration_ms = result.total_duration_ms;

            tracing::info!("trace found");
            Ok(Response::new(TraceResponse {
                found: true,
                events,
                block_executions,
                total_duration_ms,
            }))
        } else {
            tracing::info!("trace not found");
            Ok(Response::new(TraceResponse {
                found: false,
                events: vec![],
                block_executions: vec![],
                total_duration_ms: 0,
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Build a minimal `FoundryService` for testing, returning the service and
    /// a broadcast receiver to observe emitted events.
    fn test_service() -> (FoundryService, broadcast::Receiver<Event>) {
        let (event_tx, rx) = broadcast::channel(64);
        let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));
        let trace_store = Arc::new(TraceStore::new(Duration::from_secs(60)));
        let workflow_tracker = Arc::new(WorkflowTracker::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let trace_writer = Arc::new(TraceWriter::new(tmp.path().to_str().unwrap()));
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![],
        });
        let service = FoundryService::new(
            engine,
            trace_store,
            event_tx,
            workflow_tracker,
            trace_writer,
            registry,
        );
        (service, rx)
    }

    #[tokio::test]
    async fn maintenance_run_broadcasts_completion_event() {
        let (service, mut rx) = test_service();

        let request = Request::new(EmitRequest {
            event_type: "maintenance_run_started".to_string(),
            project: "test-project".to_string(),
            throttle: 2, // dry_run
            payload_json: String::new(),
            trace_id: String::new(),
        });

        let response = service.emit(request).await.expect("emit should succeed");
        let root_event_id = response.into_inner().event_id;

        // Collect events from the broadcast channel until we see the completion
        // event or time out.
        let mut saw_root = false;
        let mut saw_completed = false;
        let mut completed_payload = serde_json::Value::Null;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let result = tokio::time::timeout_at(deadline, rx.recv()).await;
            match result {
                Ok(Ok(event)) => {
                    if event.id == root_event_id {
                        saw_root = true;
                    }
                    if event.event_type == EventType::MaintenanceRunCompleted {
                        saw_completed = true;
                        completed_payload = event.payload.clone();
                        break;
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }

        assert!(saw_root, "root event should be broadcast");
        assert!(saw_completed, "MaintenanceRunCompleted should be broadcast");
        assert_eq!(completed_payload["root_event_id"], root_event_id);
        assert_eq!(completed_payload["success"], true);
    }

    #[tokio::test]
    async fn system_maintenance_run_broadcasts_completion_with_root_event_id() {
        let (service, mut rx) = test_service();

        let request = Request::new(EmitRequest {
            event_type: "maintenance_run_started".to_string(),
            project: "system".to_string(),
            throttle: 2, // dry_run
            payload_json: String::new(),
            trace_id: String::new(),
        });

        let response = service.emit(request).await.expect("emit should succeed");
        let root_event_id = response.into_inner().event_id;

        let mut saw_completed = false;
        let mut completed_payload = serde_json::Value::Null;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let result = tokio::time::timeout_at(deadline, rx.recv()).await;
            match result {
                Ok(Ok(event)) => {
                    if event.event_type == EventType::MaintenanceRunCompleted
                        && event.project == "system"
                    {
                        saw_completed = true;
                        completed_payload = event.payload.clone();
                        break;
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }

        assert!(saw_completed, "system-level MaintenanceRunCompleted should be broadcast");
        assert_eq!(
            completed_payload["root_event_id"], root_event_id,
            "system completion must include root_event_id so CLI can detect run end"
        );
    }

    #[tokio::test]
    async fn non_maintenance_event_does_not_broadcast_completion() {
        let (service, mut rx) = test_service();

        let request = Request::new(EmitRequest {
            event_type: "greet_requested".to_string(),
            project: "test-project".to_string(),
            throttle: 0,
            payload_json: String::new(),
            trace_id: String::new(),
        });

        service.emit(request).await.expect("emit should succeed");

        // Give the background task time to complete.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Drain all events — none should be MaintenanceRunCompleted.
        let mut saw_completed = false;
        while let Ok(event) = rx.try_recv() {
            if event.event_type == EventType::MaintenanceRunCompleted {
                saw_completed = true;
            }
        }

        assert!(!saw_completed, "no completion event for non-maintenance runs");
    }
}
