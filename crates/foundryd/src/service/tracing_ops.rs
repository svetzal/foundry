use std::sync::Arc;

use tonic::{Request, Response};

use foundry_core::event::Event;
use foundry_core::throttle::Throttle;
use foundry_core::trace::BlockExecution;

use crate::proto::{
    SpanRequest, SpanResponse, TraceBlockExecution, TraceEvent, TraceRequest, TraceResponse,
};
use crate::trace_store::TraceStore;

pub(super) fn trace_event_from(e: &Event) -> TraceEvent {
    TraceEvent {
        event_id: e.id.clone(),
        event_type: e.event_type.as_str(),
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

pub(super) fn trace_block_from(b: &BlockExecution) -> TraceBlockExecution {
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

pub(super) fn trace_rpc(
    trace_store: &Arc<TraceStore>,
    request: Request<TraceRequest>,
) -> Response<TraceResponse> {
    let req = request.into_inner();

    let span = tracing::info_span!("trace", event_id = %req.event_id);
    let _guard = span.enter();

    if let Some(result) = trace_store.get(&req.event_id) {
        let events = result.events.iter().map(trace_event_from).collect();
        let block_executions = result.block_executions.iter().map(trace_block_from).collect();
        let total_duration_ms = result.total_duration_ms;

        tracing::info!("trace found");
        Response::new(TraceResponse {
            found: true,
            events,
            block_executions,
            total_duration_ms,
        })
    } else {
        tracing::info!("trace not found");
        Response::new(TraceResponse {
            found: false,
            events: vec![],
            block_executions: vec![],
            total_duration_ms: 0,
        })
    }
}

pub(super) fn span_rpc(
    trace_store: &Arc<TraceStore>,
    request: Request<SpanRequest>,
) -> Response<SpanResponse> {
    let req = request.into_inner();

    let span = tracing::info_span!("span", span_id = %req.span_id);
    let _guard = span.enter();

    let inner = if let Some(r) = trace_store.find_span(&req.span_id) {
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

    Response::new(inner)
}
