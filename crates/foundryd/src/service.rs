use std::sync::Arc;

use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use foundry_core::event::{Event, EventType};
use foundry_core::throttle::Throttle;

use crate::engine::Engine;
use crate::proto::{
    EmitRequest, EmitResponse, StatusRequest, StatusResponse, TraceBlockExecution, TraceEvent,
    TraceRequest, TraceResponse, WorkflowStatus, foundry_server::Foundry,
};
use crate::trace_store::TraceStore;

pub struct FoundryService {
    engine: Arc<Engine>,
    trace_store: Arc<TraceStore>,
}

impl FoundryService {
    pub fn new(engine: Arc<Engine>, trace_store: Arc<TraceStore>) -> Self {
        Self {
            engine,
            trace_store,
        }
    }
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

        let event = Event::new(event_type, req.project, throttle, payload);
        let event_id = event.id.clone();

        let span = tracing::info_span!(
            "emit",
            event_id = %event_id,
            event_type = %event.event_type,
            project = %event.project,
            throttle = %event.throttle,
        );
        let _guard = span.enter();

        tracing::info!("processing event");

        let result = self.engine.process(event).await;

        tracing::info!(
            total_events = result.events.len(),
            blocks_executed = result.block_executions.len(),
            "event chain complete"
        );

        self.trace_store.insert(event_id.clone(), result);

        let response = EmitResponse {
            event_id,
            workflow_id: String::new(),
        };

        Ok(Response::new(response))
    }

    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        let span = tracing::info_span!("status");
        let _guard = span.enter();

        tracing::info!("status request");
        let response = StatusResponse { workflows: vec![] };
        Ok(Response::new(response))
    }

    type WatchStream = ReceiverStream<Result<WorkflowStatus, Status>>;

    async fn watch(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        let span = tracing::info_span!("watch");
        let _guard = span.enter();

        tracing::info!("watch stream started");
        let (_tx, rx) = tokio::sync::mpsc::channel(16);
        Ok(Response::new(ReceiverStream::new(rx)))
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
