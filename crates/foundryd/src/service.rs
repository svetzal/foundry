use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;
use tonic::{Request, Response, Status};
use tracing::Instrument;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::{
    InstallConfig, InstallsSkill, ProjectEdits, ProjectSpec, Registry, RegistryMutationError,
    parse_stack,
};
use foundry_core::throttle::Throttle;
use foundry_core::trace::{BlockExecution, ProcessResult};

use crate::engine::Engine;
use crate::proto::{
    EmitRequest, EmitResponse, Project, RegistryAddRequest, RegistryAddResponse,
    RegistryEditRequest, RegistryEditResponse, RegistryRemoveRequest, RegistryRemoveResponse,
    SpanRequest, SpanResponse, StatusRequest, StatusResponse, TraceBlockExecution, TraceEvent,
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
    registry: Arc<RwLock<Registry>>,
    registry_path: PathBuf,
}

impl FoundryService {
    pub fn new(
        engine: Arc<Engine>,
        trace_store: Arc<TraceStore>,
        event_tx: broadcast::Sender<Event>,
        workflow_tracker: Arc<WorkflowTracker>,
        trace_writer: Arc<TraceWriter>,
        registry: Arc<RwLock<Registry>>,
        registry_path: PathBuf,
    ) -> Self {
        Self {
            engine,
            trace_store,
            workflow_tracker,
            event_tx,
            trace_writer,
            registry,
            registry_path,
        }
    }
}

/// Convert a `RegistryMutationError` to a gRPC `Status`.
fn mutation_error_to_status(err: RegistryMutationError) -> Status {
    match err {
        RegistryMutationError::DuplicateName(name) => {
            Status::already_exists(format!("project '{name}' already exists"))
        }
        RegistryMutationError::NotFound(name) => {
            Status::not_found(format!("project '{name}' not found"))
        }
        RegistryMutationError::InvalidStack(s) => {
            Status::invalid_argument(format!("invalid stack '{s}'"))
        }
        RegistryMutationError::ConflictingInstall => {
            Status::invalid_argument("provide at most one of install_command or install_brew")
        }
    }
}

/// Convert a `ProjectEntry` to the proto `Project` message.
fn project_to_proto(entry: &foundry_core::registry::ProjectEntry) -> Project {
    let (install_command, install_brew) = match &entry.install {
        Some(InstallConfig::Command(cmd)) => (cmd.clone(), String::new()),
        Some(InstallConfig::Brew(formula)) => (String::new(), formula.clone()),
        None => (String::new(), String::new()),
    };
    let (installs_skill_bool, installs_skill_command) = match &entry.installs_skill {
        Some(InstallsSkill::Default(true)) => (true, String::new()),
        Some(InstallsSkill::Custom { command }) => (false, command.clone()),
        _ => (false, String::new()),
    };
    let _ = installs_skill_bool; // not in proto yet — silence lint
    let _ = installs_skill_command;
    Project {
        name: entry.name.clone(),
        path: entry.path.clone(),
        stack: entry.stack.to_string(),
        agent: entry.agent.clone(),
        repo: entry.repo.clone(),
        branch: entry.branch.clone(),
        skip: entry.skip.clone().unwrap_or_default(),
        iterate: entry.actions.iterate,
        maintain: entry.actions.maintain,
        push: entry.actions.push,
        audit: entry.actions.audit,
        release: entry.actions.release,
        install_command,
        install_brew,
        notes: entry.notes.clone().unwrap_or_default(),
        timeout_secs: entry.timeout_secs.unwrap_or(0),
    }
}

/// Extract per-project sub-traces from a system-level maintenance `ProcessResult`.
///
/// Groups events and block executions by project name, returning a map of
/// project name → `ProcessResult` for each per-project `ProjectRunStarted`
/// event found in the result.
fn extract_per_project_traces(result: &ProcessResult) -> HashMap<String, ProcessResult> {
    let event_map: HashMap<&str, &Event> =
        result.events.iter().map(|e| (e.id.as_str(), e)).collect();

    // Find per-project root events.
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

/// After a system-level maintenance cycle's `process()` traversal returns,
/// write per-project sub-traces to disk, then emit and process
/// `MaintenanceSummaryRequested` so `GenerateSummary` can read those traces.
///
/// The cycle's `MaintenanceCycleCompleted` is no longer synthesised here — the
/// engine's scatter/gather produces it as a genuine fan-in. This function's
/// remaining job is trace persistence and triggering the summary phase.
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

fn parse_throttle(proto_value: i32) -> Throttle {
    match proto_value {
        1 => Throttle::DryRun,
        _ => Throttle::Full,
    }
}

fn parse_emit_request(req: EmitRequest) -> Result<Event, Status> {
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

async fn run_workflow(
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

/// Convert a domain `Event` to the proto `TraceEvent` message.
fn trace_event_from(e: &Event) -> TraceEvent {
    TraceEvent {
        event_id: e.id.clone(),
        event_type: e.event_type.as_str().to_string(),
        project: e.project.clone(),
        occurred_at: e.occurred_at.to_rfc3339(),
        throttle: match e.throttle {
            Throttle::Full => 0,
            Throttle::DryRun => 1,
        },
        trace_id: e.trace_id.clone().unwrap_or_default(),
        span_id: e.span_id.clone().unwrap_or_default(),
        parent_span_id: e.parent_span_id.clone().unwrap_or_default(),
    }
}

/// Convert a domain `BlockExecution` to the proto `TraceBlockExecution` message.
fn trace_block_from(b: &BlockExecution) -> TraceBlockExecution {
    TraceBlockExecution {
        block_name: b.block_name.clone(),
        trigger_event_id: b.trigger_event_id.clone(),
        success: b.success,
        summary: b.summary.clone(),
        emitted_event_ids: b.emitted_event_ids.clone(),
        duration_ms: b.duration_ms,
        raw_output: b.raw_output.clone().unwrap_or_default(),
        exit_code: b.exit_code.unwrap_or(0),
        trigger_payload_json: b.trigger_payload.to_string(),
        emitted_payload_jsons: b.emitted_payloads.iter().map(ToString::to_string).collect(),
        audit_artifacts: b.audit_artifacts.clone(),
        span_id: b.span_id.clone().unwrap_or_default(),
        parent_span_id: b.parent_span_id.clone().unwrap_or_default(),
    }
}

#[tonic::async_trait]
impl Foundry for FoundryService {
    async fn emit(&self, request: Request<EmitRequest>) -> Result<Response<EmitResponse>, Status> {
        let event = parse_emit_request(request.into_inner())?;
        let event_id = event.id.clone();
        let trace_id = event.trace_id.clone().unwrap_or_default();

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
            trace_id,
            started_at: chrono::Utc::now(),
        });

        let span = tracing::info_span!(
            "process",
            event_id = %event_id,
            event_type = %event.event_type,
            project = %event.project,
        );

        tokio::spawn(
            run_workflow(
                event,
                Arc::clone(&self.engine),
                Arc::clone(&self.trace_store),
                Arc::clone(&self.workflow_tracker),
                Arc::clone(&self.trace_writer),
                self.event_tx.clone(),
                Arc::clone(&self.registry),
            )
            .instrument(span),
        );

        Ok(Response::new(EmitResponse {
            event_id,
            workflow_id: String::new(),
        }))
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
                trace_id: w.trace_id,
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

        Ok(Response::new(Box::pin(stream)))
    }

    async fn registry_add(
        &self,
        request: Request<RegistryAddRequest>,
    ) -> Result<Response<RegistryAddResponse>, Status> {
        let req = request.into_inner();

        let stack = parse_stack(if req.stack.is_empty() {
            "rust"
        } else {
            &req.stack
        })
        .map_err(mutation_error_to_status)?;

        let branch = if req.branch.is_empty() {
            "main".to_string()
        } else {
            req.branch.clone()
        };

        // Validate mutual exclusivity client-side before calling add_project.
        if !req.install_command.is_empty() && !req.install_brew.is_empty() {
            return Err(mutation_error_to_status(RegistryMutationError::ConflictingInstall));
        }

        let spec = ProjectSpec {
            name: req.name.clone(),
            path: req.path.clone(),
            stack,
            agent: req.agent.clone(),
            repo: req.repo.clone(),
            branch,
            iterate: req.iterate,
            maintain: req.maintain,
            push: req.push,
            audit: req.audit,
            release: req.release,
            install_command: if req.install_command.is_empty() {
                None
            } else {
                Some(req.install_command.clone())
            },
            install_brew: if req.install_brew.is_empty() {
                None
            } else {
                Some(req.install_brew.clone())
            },
            notes: if req.notes.is_empty() {
                None
            } else {
                Some(req.notes.clone())
            },
            timeout_secs: if req.timeout_secs == 0 {
                None
            } else {
                Some(req.timeout_secs)
            },
        };

        let entry_proto = {
            let mut reg = self.registry.write().expect("registry lock poisoned");
            let entry = reg.add_project(spec).map_err(mutation_error_to_status)?;
            let proto = project_to_proto(entry);
            reg.save(&self.registry_path)
                .map_err(|e| Status::internal(format!("failed to save registry: {e}")))?;
            proto
        };

        tracing::info!(project = %req.name, "registry_add: project added");

        Ok(Response::new(RegistryAddResponse {
            project: Some(entry_proto),
        }))
    }

    async fn registry_remove(
        &self,
        request: Request<RegistryRemoveRequest>,
    ) -> Result<Response<RegistryRemoveResponse>, Status> {
        let req = request.into_inner();

        {
            let mut reg = self.registry.write().expect("registry lock poisoned");
            reg.remove_project(&req.name).map_err(mutation_error_to_status)?;
            reg.save(&self.registry_path)
                .map_err(|e| Status::internal(format!("failed to save registry: {e}")))?;
        }

        tracing::info!(project = %req.name, "registry_remove: project removed");

        Ok(Response::new(RegistryRemoveResponse {}))
    }

    #[allow(clippy::too_many_lines)]
    async fn registry_edit(
        &self,
        request: Request<RegistryEditRequest>,
    ) -> Result<Response<RegistryEditResponse>, Status> {
        let req = request.into_inner();

        let stack = if req.stack.is_empty() {
            None
        } else {
            Some(parse_stack(&req.stack).map_err(mutation_error_to_status)?)
        };

        // Validate mutual exclusivity before building edits.
        if !req.install_command.is_empty() && !req.install_brew.is_empty() {
            return Err(mutation_error_to_status(RegistryMutationError::ConflictingInstall));
        }

        let skip = if req.clear_skip {
            Some(None)
        } else if req.skip.is_empty() {
            None
        } else {
            Some(Some(req.skip.clone()))
        };

        let edits = ProjectEdits {
            path: if req.path.is_empty() {
                None
            } else {
                Some(req.path.clone())
            },
            stack,
            agent: if req.agent.is_empty() {
                None
            } else {
                Some(req.agent.clone())
            },
            repo: if req.repo.is_empty() {
                None
            } else {
                Some(req.repo.clone())
            },
            branch: if req.branch.is_empty() {
                None
            } else {
                Some(req.branch.clone())
            },
            skip,
            iterate: if req.clear_iterate {
                Some(false)
            } else if req.iterate {
                Some(true)
            } else {
                None
            },
            maintain: if req.clear_maintain {
                Some(false)
            } else if req.maintain {
                Some(true)
            } else {
                None
            },
            push: if req.clear_push {
                Some(false)
            } else if req.push {
                Some(true)
            } else {
                None
            },
            audit: if req.clear_audit {
                Some(false)
            } else if req.audit {
                Some(true)
            } else {
                None
            },
            release: if req.clear_release {
                Some(false)
            } else if req.release {
                Some(true)
            } else {
                None
            },
            install_command: if req.install_command.is_empty() {
                None
            } else {
                Some(req.install_command.clone())
            },
            install_brew: if req.install_brew.is_empty() {
                None
            } else {
                Some(req.install_brew.clone())
            },
            clear_install: req.clear_install,
            // notes: empty string → clear (edit_project treats "" as "unset")
            notes: if req.clear_notes {
                Some(String::new())
            } else if req.notes.is_empty() {
                None
            } else {
                Some(req.notes.clone())
            },
            timeout_secs: if req.timeout_secs == 0 {
                None
            } else {
                Some(req.timeout_secs)
            },
            clear_timeout: req.clear_timeout,
        };

        let entry_proto = {
            let mut reg = self.registry.write().expect("registry lock poisoned");
            let entry = reg.edit_project(&req.name, edits).map_err(mutation_error_to_status)?;
            let proto = project_to_proto(entry);
            reg.save(&self.registry_path)
                .map_err(|e| Status::internal(format!("failed to save registry: {e}")))?;
            proto
        };

        tracing::info!(project = %req.name, "registry_edit: project updated");

        Ok(Response::new(RegistryEditResponse {
            project: Some(entry_proto),
        }))
    }

    async fn trace(
        &self,
        request: Request<TraceRequest>,
    ) -> Result<Response<TraceResponse>, Status> {
        let req = request.into_inner();

        let span = tracing::info_span!("trace", event_id = %req.event_id);
        let _guard = span.enter();

        if let Some(result) = self.trace_store.get(&req.event_id) {
            let events = result.events.iter().map(trace_event_from).collect();
            let block_executions = result.block_executions.iter().map(trace_block_from).collect();
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

    async fn span(&self, request: Request<SpanRequest>) -> Result<Response<SpanResponse>, Status> {
        let req = request.into_inner();

        let span = tracing::info_span!("span", span_id = %req.span_id);
        let _guard = span.enter();

        let response = if let Some(r) = self.trace_store.find_span(&req.span_id) {
            tracing::info!(events = r.events.len(), blocks = r.blocks.len(), "span found");
            SpanResponse {
                found: true,
                events: r.events.iter().map(trace_event_from).collect(),
                block_executions: r.blocks.iter().map(trace_block_from).collect(),
                total_duration_ms: r.total_duration_ms,
            }
        } else {
            tracing::info!("span not found");
            SpanResponse {
                found: false,
                events: vec![],
                block_executions: vec![],
                total_duration_ms: 0,
            }
        };

        Ok(Response::new(response))
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
        let registry = Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let tmp_registry = tempfile::NamedTempFile::new().expect("tempfile");
        let registry_path = tmp_registry.path().to_path_buf();
        let service = FoundryService::new(
            engine,
            trace_store,
            event_tx,
            workflow_tracker,
            trace_writer,
            registry,
            registry_path,
        );
        (service, rx)
    }

    #[tokio::test]
    async fn project_run_broadcasts_completion_event() {
        let (service, mut rx) = test_service();

        let request = Request::new(EmitRequest {
            event_type: "project_run_started".to_string(),
            project: "test-project".to_string(),
            throttle: 2, // dry_run
            payload_json: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: String::new(),
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
                    if event.event_type == EventType::ProjectRunCompleted {
                        saw_completed = true;
                        completed_payload = event.payload.clone();
                        break;
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }

        assert!(saw_root, "root event should be broadcast");
        assert!(saw_completed, "ProjectRunCompleted should be broadcast");
        assert_eq!(completed_payload["root_event_id"], root_event_id);
        assert_eq!(completed_payload["success"], true);
    }

    #[tokio::test]
    async fn system_maintenance_cycle_broadcasts_summary_request_with_root_event_id() {
        let (service, mut rx) = test_service();

        let request = Request::new(EmitRequest {
            event_type: "maintenance_cycle_started".to_string(),
            project: "system".to_string(),
            throttle: 2, // dry_run
            payload_json: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: String::new(),
        });

        let response = service.emit(request).await.expect("emit should succeed");
        let root_event_id = response.into_inner().event_id;

        let mut saw_summary = false;
        let mut summary_payload = serde_json::Value::Null;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let result = tokio::time::timeout_at(deadline, rx.recv()).await;
            match result {
                Ok(Ok(event)) => {
                    if event.event_type == EventType::MaintenanceSummaryRequested
                        && event.project == "system"
                    {
                        saw_summary = true;
                        summary_payload = event.payload.clone();
                        break;
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }

        assert!(
            saw_summary,
            "the service should broadcast MaintenanceSummaryRequested after a system cycle"
        );
        assert_eq!(
            summary_payload["root_event_id"], root_event_id,
            "the summary request must include root_event_id so the CLI can detect run end"
        );
    }

    #[tokio::test]
    async fn emit_mints_span_id_when_request_omits_it() {
        let (service, mut rx) = test_service();

        let request = Request::new(EmitRequest {
            event_type: "project_run_started".to_string(),
            project: "span-mint-project".to_string(),
            throttle: 2, // dry_run
            payload_json: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: String::new(),
        });

        let response = service.emit(request).await.expect("emit should succeed");
        let root_event_id = response.into_inner().event_id;

        // Read the broadcast stream until we see the root event itself, which
        // carries the minted span_id.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut root_span_id: Option<String> = None;
        loop {
            let result = tokio::time::timeout_at(deadline, rx.recv()).await;
            match result {
                Ok(Ok(event)) => {
                    if event.id == root_event_id {
                        root_span_id = event.span_id.clone();
                        break;
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }

        let span_id = root_span_id.expect("root event must carry a span_id");
        assert!(
            !span_id.is_empty(),
            "Emit must mint a non-empty span_id when the request omits one"
        );
        // span_ids are 16 hex chars (see mint_span_id contract).
        assert_eq!(span_id.len(), 16, "minted span_id must be 16 hex chars");
        assert!(span_id.chars().all(|c| c.is_ascii_hexdigit()), "minted span_id must be hex");
    }

    #[tokio::test]
    async fn span_rpc_returns_events_sharing_requested_span_id() {
        let (service, mut rx) = test_service();

        // 1. Emit a workflow root event with no trace/span set — Emit will mint both.
        let request = Request::new(EmitRequest {
            event_type: "project_run_started".to_string(),
            project: "span-rpc-project".to_string(),
            throttle: 2, // dry_run
            payload_json: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: String::new(),
        });

        let response = service.emit(request).await.expect("emit should succeed");
        let root_event_id = response.into_inner().event_id;

        // Wait for ProjectRunCompleted to confirm the background task has
        // finished inserting the trace into the store.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut saw_completed = false;
        loop {
            let result = tokio::time::timeout_at(deadline, rx.recv()).await;
            match result {
                Ok(Ok(event)) => {
                    if event.event_type == EventType::ProjectRunCompleted {
                        saw_completed = true;
                        break;
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert!(saw_completed, "background processing must complete before Trace lookup");

        // 2. Call Trace and confirm span fields are populated end-to-end.
        let trace_resp = service
            .trace(Request::new(TraceRequest {
                event_id: root_event_id.clone(),
            }))
            .await
            .expect("trace should succeed")
            .into_inner();

        assert!(trace_resp.found, "trace must be found for the root event");
        let root_trace_event = trace_resp
            .events
            .iter()
            .find(|e| e.event_id == root_event_id)
            .expect("root event must appear in trace");
        assert!(!root_trace_event.span_id.is_empty(), "root event's span_id must be populated");
        assert!(!root_trace_event.trace_id.is_empty(), "root event's trace_id must be populated");

        // 3. Pick the workflow span_id from the response.
        let workflow_span_id = root_trace_event.span_id.clone();

        // 4. Call Span and confirm found=true plus every returned event shares that span_id.
        let span_resp = service
            .span(Request::new(SpanRequest {
                span_id: workflow_span_id.clone(),
            }))
            .await
            .expect("span should succeed")
            .into_inner();

        assert!(span_resp.found, "span must be found");
        assert!(!span_resp.events.is_empty(), "span lookup must surface at least the root event");
        for e in &span_resp.events {
            assert_eq!(
                e.span_id, workflow_span_id,
                "every event returned by Span must share the requested span_id"
            );
        }
    }

    #[tokio::test]
    async fn span_rpc_returns_not_found_for_unknown_span() {
        let (service, _rx) = test_service();

        let resp = service
            .span(Request::new(SpanRequest {
                span_id: "deadbeefdeadbeef".to_string(),
            }))
            .await
            .expect("span should succeed")
            .into_inner();

        assert!(!resp.found);
        assert!(resp.events.is_empty());
        assert!(resp.block_executions.is_empty());
        assert_eq!(resp.total_duration_ms, 0);
    }

    #[tokio::test]
    async fn non_maintenance_event_does_not_broadcast_completion() {
        let (service, mut rx) = test_service();

        let request = Request::new(EmitRequest {
            event_type: "greeting_requested".to_string(),
            project: "test-project".to_string(),
            throttle: 0,
            payload_json: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: String::new(),
        });

        service.emit(request).await.expect("emit should succeed");

        // Give the background task time to complete.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Drain all events — none should be MaintenanceCycleCompleted or ProjectRunCompleted.
        let mut saw_completed = false;
        while let Ok(event) = rx.try_recv() {
            if event.event_type == EventType::MaintenanceCycleCompleted
                || event.event_type == EventType::ProjectRunCompleted
            {
                saw_completed = true;
            }
        }

        assert!(!saw_completed, "no completion event for non-maintenance runs");
    }

    // -- Registry mutation tests --

    #[tokio::test]
    async fn registry_add_inserts_project_and_saves() {
        let (service, _rx) = test_service();

        let req = Request::new(RegistryAddRequest {
            name: "my-project".to_string(),
            path: "/tmp/my-project".to_string(),
            stack: "rust".to_string(),
            agent: "claude".to_string(),
            repo: "owner/my-project".to_string(),
            branch: "main".to_string(),
            iterate: true,
            maintain: false,
            push: true,
            audit: false,
            release: false,
            install_command: String::new(),
            install_brew: String::new(),
            notes: String::new(),
            timeout_secs: 0,
        });

        let resp = service.registry_add(req).await.expect("add should succeed");
        let project = resp.into_inner().project.expect("project should be returned");
        assert_eq!(project.name, "my-project");
        assert_eq!(project.stack, "rust");
        assert!(project.iterate);

        // In-memory registry should now have the project.
        let reg = service.registry.read().unwrap();
        assert_eq!(reg.projects.len(), 1);
        assert_eq!(reg.projects[0].name, "my-project");
    }

    #[tokio::test]
    async fn registry_add_duplicate_returns_already_exists() {
        let (service, _rx) = test_service();

        let make_req = || {
            Request::new(RegistryAddRequest {
                name: "alpha".to_string(),
                path: "/tmp/alpha".to_string(),
                stack: "rust".to_string(),
                agent: String::new(),
                repo: String::new(),
                branch: "main".to_string(),
                iterate: false,
                maintain: false,
                push: false,
                audit: false,
                release: false,
                install_command: String::new(),
                install_brew: String::new(),
                notes: String::new(),
                timeout_secs: 0,
            })
        };

        service.registry_add(make_req()).await.expect("first add should succeed");
        let err = service.registry_add(make_req()).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
    }

    #[tokio::test]
    async fn registry_remove_deletes_project() {
        let (service, _rx) = test_service();

        // Add first, then remove.
        service
            .registry_add(Request::new(RegistryAddRequest {
                name: "to-remove".to_string(),
                path: "/tmp/tr".to_string(),
                stack: "rust".to_string(),
                agent: String::new(),
                repo: String::new(),
                branch: "main".to_string(),
                iterate: false,
                maintain: false,
                push: false,
                audit: false,
                release: false,
                install_command: String::new(),
                install_brew: String::new(),
                notes: String::new(),
                timeout_secs: 0,
            }))
            .await
            .expect("add should succeed");

        service
            .registry_remove(Request::new(RegistryRemoveRequest {
                name: "to-remove".to_string(),
            }))
            .await
            .expect("remove should succeed");

        let reg = service.registry.read().unwrap();
        assert!(reg.projects.is_empty());
    }

    #[tokio::test]
    async fn registry_remove_not_found_returns_not_found_status() {
        let (service, _rx) = test_service();

        let err = service
            .registry_remove(Request::new(RegistryRemoveRequest {
                name: "nonexistent".to_string(),
            }))
            .await
            .unwrap_err();

        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn registry_edit_updates_project_fields() {
        let (service, _rx) = test_service();

        service
            .registry_add(Request::new(RegistryAddRequest {
                name: "editable".to_string(),
                path: "/tmp/editable".to_string(),
                stack: "rust".to_string(),
                agent: String::new(),
                repo: String::new(),
                branch: "main".to_string(),
                iterate: false,
                maintain: false,
                push: false,
                audit: false,
                release: false,
                install_command: String::new(),
                install_brew: String::new(),
                notes: String::new(),
                timeout_secs: 0,
            }))
            .await
            .expect("add should succeed");

        let resp = service
            .registry_edit(Request::new(RegistryEditRequest {
                name: "editable".to_string(),
                path: String::new(),
                stack: String::new(),
                agent: "gemini".to_string(),
                repo: String::new(),
                branch: String::new(),
                skip: String::new(),
                clear_skip: false,
                iterate: true,
                clear_iterate: false,
                maintain: false,
                clear_maintain: false,
                push: false,
                clear_push: false,
                audit: false,
                clear_audit: false,
                release: false,
                clear_release: false,
                install_command: String::new(),
                install_brew: String::new(),
                clear_install: false,
                notes: String::new(),
                clear_notes: false,
                timeout_secs: 0,
                clear_timeout: false,
            }))
            .await
            .expect("edit should succeed");

        let project = resp.into_inner().project.expect("project should be returned");
        assert_eq!(project.agent, "gemini");
        assert!(project.iterate);
    }

    #[tokio::test]
    async fn registry_edit_not_found_returns_not_found_status() {
        let (service, _rx) = test_service();

        let err = service
            .registry_edit(Request::new(RegistryEditRequest {
                name: "ghost".to_string(),
                path: String::new(),
                stack: String::new(),
                agent: String::new(),
                repo: String::new(),
                branch: String::new(),
                skip: String::new(),
                clear_skip: false,
                iterate: false,
                clear_iterate: false,
                maintain: false,
                clear_maintain: false,
                push: false,
                clear_push: false,
                audit: false,
                clear_audit: false,
                release: false,
                clear_release: false,
                install_command: String::new(),
                install_brew: String::new(),
                clear_install: false,
                notes: String::new(),
                clear_notes: false,
                timeout_secs: 0,
                clear_timeout: false,
            }))
            .await
            .unwrap_err();

        assert_eq!(err.code(), tonic::Code::NotFound);
    }
}
