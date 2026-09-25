use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use tokio::sync::broadcast;
use tonic::{Request, Response, Status};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{MajorUpgradeStatus, MajorUpgradesPlannedPayload};
use foundry_sdk::registry::Registry;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::trace::ProcessResult;

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
        foundry_sdk::event::mint_trace_id()
    } else {
        req.trace_id
    };
    let request_span_id = if req.span_id.is_empty() {
        Some(foundry_sdk::event::mint_span_id())
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

/// The major-upgrade tasks a nightly summary phase planned, as root
/// `ExecutionRequested` events — one task per major, in plan order.
///
/// Empty unless the plan enabled dispatch (nightly, full throttle). Each event
/// is a fresh workflow root with its own trace, exactly as `foundry task`
/// would emit it.
pub(super) fn planned_major_dispatches(summary: &ProcessResult) -> Vec<Event> {
    summary
        .parsed_events_of::<MajorUpgradesPlannedPayload>(EventType::MajorUpgradesPlanned)
        .filter(|plan| plan.dispatch_enabled && !plan.review)
        .flat_map(|plan| plan.upgrades)
        .filter(|m| m.status == MajorUpgradeStatus::Dispatch)
        .map(|m| {
            Event::new(
                EventType::ExecutionRequested,
                m.project.clone(),
                Throttle::Full,
                serde_json::json!({
                    "project": m.project,
                    "workflow": "task",
                    "prompt": m.objective,
                }),
            )
            .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
            .with_span_ids(Some(foundry_sdk::event::mint_span_id()), None)
        })
        .collect()
}

/// A boxed `run_workflow`, so a workflow can start further workflows (the
/// majors lane) without an infinitely recursive future type.
fn run_workflow_boxed(
    event: Event,
    ctx: super::RuntimeContext,
) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(run_workflow(
        event,
        ctx.engine,
        ctx.trace_store,
        ctx.workflow_tracker,
        ctx.trace_writer,
        ctx.event_tx,
        ctx.registry,
    ))
}

/// Run the planned major-upgrade tasks one after another, each as its own
/// tracked workflow. Sequential on purpose: each task builds, tests and may
/// land on trunk, and two at once on one project would race to land.
fn dispatch_major_upgrades(dispatches: Vec<Event>, ctx: super::RuntimeContext) {
    if dispatches.is_empty() {
        return;
    }
    tracing::info!(count = dispatches.len(), "dispatching major-upgrade tasks");
    tokio::spawn(async move {
        for event in dispatches {
            tracing::info!(project = %event.project, event_id = %event.id, "starting major-upgrade task");
            super::track_workflow(&event, &ctx.workflow_tracker);
            run_workflow_boxed(event, ctx.clone()).await;
        }
    });
}

/// After a system-level maintenance cycle completes, write per-project sub-traces
/// to disk and emit `MaintenanceSummaryRequested` for the summary phase. When
/// the summary phase plans major-upgrade tasks for dispatch, start them.
async fn finalise_system_maintenance(
    result: &ProcessResult,
    ctx: &super::RuntimeContext,
    throttle: Throttle,
    root_event_id: &str,
) {
    let engine = &ctx.engine;
    let trace_writer = &ctx.trace_writer;
    let registry = &ctx.registry;
    let event_tx = &ctx.event_tx;
    // Extract skipped projects before any .await — RwLock guards must not cross await points.
    // A poisoned registry lock must not abort the maintenance summary: log it
    // and degrade to an empty skipped-project list rather than taking down
    // the daemon (foundryd is long-lived state — see AGENTS.md Failure Policy).
    let skipped_projects: Vec<String> = match foundry_sdk::error::read_lock(registry, "registry") {
        Ok(reg) => reg
            .projects
            .iter()
            .filter(|p| p.skip.is_some())
            .map(|p| p.name.clone())
            .collect(),
        Err(e) => {
            tracing::error!(error = %e, "registry lock poisoned; maintenance summary will report no skipped projects");
            Vec::new()
        }
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

    // Mint both ids here: this is a fresh workflow root. `finalise` opens a new
    // `engine.process` chain for the summary phase, and `stamp_context` only
    // ever *inherits* the ids from the trigger — a bare `Event::new` carries
    // `None`, so the whole triage + summary fan-out (`MaintenanceTriageCompleted`,
    // `MaintenanceTriageDigestWritten`, the generated summary) inherits `None`
    // and lands in the event log untraceable. The trace groups this summary
    // phase; the span groups it as a root (`MaintenanceSummaryRequested` is a
    // span opener, so each sink inherits this root span). No parent span — the
    // maintenance cycle that produced `result` was a separate root chain.
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
    )
    .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
    .with_span_ids(Some(foundry_sdk::event::mint_span_id()), None);

    let summary_result = engine.process(summary_event.clone()).await;

    if let Err(e) = trace_writer.write(&summary_event.id, &summary_result) {
        tracing::warn!(error = %e, "failed to write summary trace");
    }

    dispatch_major_upgrades(planned_major_dispatches(&summary_result), ctx.clone());

    // Best-effort: a send error means no Watch subscribers are attached,
    // which is the normal steady state; summary emission must not depend on
    // a listener.
    if let Err(e) = event_tx.send(summary_event) {
        tracing::debug!(error = %e, "no Watch subscribers for MaintenanceSummaryRequested");
    }
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

    let ctx = super::RuntimeContext {
        engine: Arc::clone(&engine),
        trace_store: Arc::clone(&trace_store),
        workflow_tracker: Arc::clone(&tracker),
        trace_writer: Arc::clone(&trace_writer),
        event_tx: event_tx.clone(),
        registry: Arc::clone(&registry),
    };
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
        finalise_system_maintenance(&result, &ctx, root_throttle, &event_id).await;
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
        // Best-effort: a send error means no Watch subscribers are attached,
        // which is the normal steady state; completion emission must not
        // depend on a listener.
        if let Err(e) = event_tx.send(completed) {
            tracing::debug!(error = %e, event_id = %event_id, "no Watch subscribers for ProjectRunCompleted");
        }
    }

    trace_store.insert(event_id, result);
}

pub(super) fn emit_rpc(
    ctx: &super::RuntimeContext,
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

    super::spawn_workflow(event, ctx);

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

#[cfg(test)]
mod tests {
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::trace::{BlockExecution, ProcessResult};

    use crate::proto::EmitRequest;

    use super::{
        extract_per_project_traces, parse_emit_request, parse_throttle, planned_major_dispatches,
    };

    fn basic_emit_request() -> EmitRequest {
        EmitRequest {
            event_type: "project_run_started".to_string(),
            project: "my-project".to_string(),
            throttle: 0,
            payload_json: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: String::new(),
        }
    }

    #[test]
    fn parse_throttle_one_maps_to_dry_run() {
        assert_eq!(parse_throttle(1), Throttle::DryRun);
    }

    #[test]
    fn parse_throttle_any_other_value_maps_to_full() {
        assert_eq!(parse_throttle(0), Throttle::Full);
        assert_eq!(parse_throttle(2), Throttle::Full);
        assert_eq!(parse_throttle(-1), Throttle::Full);
    }

    #[test]
    fn parse_emit_request_valid_parses_event_type_and_project() {
        let event = parse_emit_request(basic_emit_request()).expect("should parse");
        assert_eq!(event.event_type, EventType::ProjectRunStarted);
        assert_eq!(event.project, "my-project");
        assert_eq!(event.throttle, Throttle::Full);
    }

    #[test]
    fn parse_emit_request_invalid_payload_json_returns_invalid_argument() {
        let mut req = basic_emit_request();
        req.payload_json = "not valid json {{{".to_string();
        let err = parse_emit_request(req).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn parse_emit_request_empty_trace_id_mints_32_char_hex() {
        let event = parse_emit_request(basic_emit_request()).unwrap();
        let trace_id = event.trace_id.expect("trace_id must be set");
        assert_eq!(trace_id.len(), 32);
        assert!(trace_id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_emit_request_supplied_trace_id_is_preserved() {
        let mut req = basic_emit_request();
        req.trace_id = "abcdef0123456789abcdef0123456789".to_string();
        let event = parse_emit_request(req).unwrap();
        assert_eq!(event.trace_id.as_deref(), Some("abcdef0123456789abcdef0123456789"));
    }

    #[test]
    fn parse_emit_request_empty_span_id_mints_16_char_hex() {
        let event = parse_emit_request(basic_emit_request()).unwrap();
        let span_id = event.span_id.expect("span_id must be set");
        assert_eq!(span_id.len(), 16);
        assert!(span_id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_emit_request_empty_parent_span_id_stays_none() {
        let event = parse_emit_request(basic_emit_request()).unwrap();
        assert!(event.parent_span_id.is_none());
    }

    #[test]
    fn parse_emit_request_supplied_parent_span_id_is_preserved() {
        let mut req = basic_emit_request();
        req.parent_span_id = "fedcba9876543210".to_string();
        let event = parse_emit_request(req).unwrap();
        assert_eq!(event.parent_span_id.as_deref(), Some("fedcba9876543210"));
    }

    fn event(event_type: EventType, project: &str) -> Event {
        Event::new(event_type, project.to_string(), Throttle::Full, serde_json::json!({}))
    }

    #[test]
    fn extract_per_project_traces_partitions_events_by_project() {
        let sys_root = event(EventType::MaintenanceCycleStarted, "system");
        let alpha_start = event(EventType::ProjectRunStarted, "proj-a");
        let alpha_end = event(EventType::ProjectRunCompleted, "proj-a");
        let beta_start = event(EventType::ProjectRunStarted, "proj-b");

        let block_a = {
            let mut b = BlockExecution::new("blk-a", &alpha_start.id, 100, serde_json::json!({}));
            b.success = true;
            b
        };
        let block_sys = BlockExecution::new("blk-sys", &sys_root.id, 50, serde_json::json!({}));

        let result = ProcessResult {
            events: vec![sys_root, alpha_start, alpha_end, beta_start],
            block_executions: vec![block_a, block_sys],
            total_duration_ms: 250,
        };

        let traces = extract_per_project_traces(&result);

        assert_eq!(traces.len(), 2, "one trace per non-system project");

        let a = traces.get("proj-a").expect("proj-a must be present");
        assert_eq!(a.events.len(), 2);
        assert_eq!(a.block_executions.len(), 1);
        assert_eq!(a.total_duration_ms, 100);

        let b = traces.get("proj-b").expect("proj-b must be present");
        assert_eq!(b.events.len(), 1);
        assert!(b.block_executions.is_empty());
        assert_eq!(b.total_duration_ms, 0);
    }

    #[test]
    fn extract_per_project_traces_excludes_system_project() {
        let result = ProcessResult {
            events: vec![event(EventType::MaintenanceCycleStarted, "system")],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        assert!(extract_per_project_traces(&result).is_empty());
    }

    fn plan_event(dispatch_enabled: bool, review: bool) -> Event {
        let upgrade = |package: &str, status: &str| {
            serde_json::json!({
                "project": "alpha", "ecosystem": "npm", "manifest": ".", "package": package,
                "from": "1.0.0", "to": "2.0.0",
                "objective": format!("Upgrade {package} from 1.0.0 to 2.0.0 in alpha: adapt call sites, keep all gates green."),
                "command": "foundry task alpha '...'", "status": status,
            })
        };
        Event::new(
            EventType::MajorUpgradesPlanned,
            "system".to_string(),
            Throttle::Full,
            serde_json::json!({
                "upgrades": [upgrade("x", "dispatch"), upgrade("y", "overflow"), upgrade("z", "dispatch")],
                "per_project_cap": 2, "per_night_cap": 6,
                "dispatch_enabled": dispatch_enabled, "review": review,
            }),
        )
    }

    fn summary_with(events: Vec<Event>) -> ProcessResult {
        ProcessResult {
            events,
            block_executions: vec![],
            total_duration_ms: 0,
        }
    }

    #[test]
    fn planned_dispatches_become_separate_task_roots_in_plan_order() {
        let dispatches = planned_major_dispatches(&summary_with(vec![plan_event(true, false)]));

        assert_eq!(dispatches.len(), 2, "only dispatch entries run");
        assert!(dispatches.iter().all(|e| e.event_type == EventType::ExecutionRequested));
        assert_eq!(dispatches[0].payload["workflow"], "task");
        assert!(dispatches[0].payload["prompt"].as_str().unwrap().starts_with("Upgrade x from"));
        assert!(dispatches[1].payload["prompt"].as_str().unwrap().starts_with("Upgrade z from"));
        assert_ne!(dispatches[0].trace_id, dispatches[1].trace_id, "each task is its own trace");
        assert!(dispatches[0].parent_span_id.is_none(), "each task is a root");
    }

    #[test]
    fn nothing_is_dispatched_under_dry_run_or_for_a_review() {
        assert!(planned_major_dispatches(&summary_with(vec![plan_event(false, false)])).is_empty());
        assert!(planned_major_dispatches(&summary_with(vec![plan_event(true, true)])).is_empty());
        assert!(planned_major_dispatches(&summary_with(vec![])).is_empty());
    }
}
