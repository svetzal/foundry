use std::sync::Arc;

use tonic::{Request, Response};

use foundry_sdk::event::Event;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::trace::{BlockExecution, TraceIndex};

use crate::proto::{
    HistoryDay, HistoryRequest, HistoryResponse, HistoryTrace, SpanRequest, SpanResponse,
    TraceBlockExecution, TraceEvent, TraceRequest, TraceResponse,
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

fn history_trace_from(index: &TraceIndex) -> HistoryTrace {
    HistoryTrace {
        event_id: index.event_id.clone(),
        event_type: index.event_type.clone(),
        project: index.project.clone(),
        success: index.success,
        total_duration_ms: index.total_duration_ms,
        trace_id: index.trace_id.clone().unwrap_or_default(),
    }
}

pub(super) fn history_rpc(
    trace_store: &Arc<TraceStore>,
    request: Request<HistoryRequest>,
) -> Response<HistoryResponse> {
    let req = request.into_inner();
    let project_filter = (!req.project.is_empty()).then_some(req.project.as_str());

    let span = tracing::info_span!("history", date = %req.date, project = %req.project);
    let _guard = span.enter();

    let days = if req.date.is_empty() {
        let recent_days = usize::try_from(req.recent_days).unwrap_or(7).max(1);
        trace_store
            .list_recent(recent_days, project_filter)
            .into_iter()
            .map(|(date, traces)| HistoryDay {
                date,
                traces: traces.iter().map(history_trace_from).collect(),
            })
            .collect()
    } else {
        let traces = trace_store.list_date(&req.date, project_filter);
        vec![HistoryDay {
            date: req.date,
            traces: traces.iter().map(history_trace_from).collect(),
        }]
    };

    Response::new(HistoryResponse { days })
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
            trace_id: r.trace_id,
            total_duration_ms: r.total_duration_ms,
        }
    } else {
        tracing::info!("span not found");
        SpanResponse {
            found: false,
            events: vec![],
            block_executions: vec![],
            trace_id: String::new(),
            total_duration_ms: 0,
        }
    };

    Response::new(inner)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use tonic::Request;

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::trace::{BlockExecution, ProcessResult};

    use crate::proto::{HistoryRequest, SpanRequest, TraceRequest};
    use crate::trace_store::TraceStore;

    use super::{history_rpc, span_rpc, trace_block_from, trace_event_from, trace_rpc};

    fn event_with_spans() -> Event {
        let mut e = Event::new(
            EventType::ProjectRunStarted,
            "proj".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        e.trace_id = Some("abcdef0123456789abcdef0123456789".to_string());
        e.span_id = Some("0123456789abcdef".to_string());
        e.parent_span_id = Some("fedcba9876543210".to_string());
        e
    }

    #[test]
    fn trace_event_from_maps_fields_with_span_ids() {
        let event = event_with_spans();
        let proto = trace_event_from(&event);

        assert_eq!(proto.event_id, event.id);
        assert_eq!(proto.event_type, "project_run_started");
        assert_eq!(proto.project, "proj");
        assert_eq!(proto.throttle, 0); // Full → 0
        assert_eq!(proto.trace_id, "abcdef0123456789abcdef0123456789");
        assert_eq!(proto.span_id, "0123456789abcdef");
        assert_eq!(proto.parent_span_id, "fedcba9876543210");
    }

    #[test]
    fn trace_event_from_defaults_absent_span_fields_to_empty_string() {
        let event = Event::new(
            EventType::ProjectRunStarted,
            "proj".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let proto = trace_event_from(&event);
        assert_eq!(proto.trace_id, "");
        assert_eq!(proto.span_id, "");
        assert_eq!(proto.parent_span_id, "");
    }

    #[test]
    fn trace_event_from_dry_run_throttle_maps_to_one() {
        let event = Event::new(
            EventType::ProjectRunStarted,
            "proj".to_string(),
            Throttle::DryRun,
            serde_json::json!({}),
        );
        assert_eq!(trace_event_from(&event).throttle, 1);
    }

    #[test]
    fn trace_block_from_absent_optional_fields_default_correctly() {
        let b = BlockExecution::new("blk", "evt-1", 42, serde_json::json!({"k": "v"}));
        let proto = trace_block_from(&b);

        assert_eq!(proto.block_name, "blk");
        assert_eq!(proto.trigger_event_id, "evt-1");
        assert_eq!(proto.duration_ms, 42);
        assert_eq!(proto.raw_output, ""); // None → ""
        assert_eq!(proto.exit_code, 0); // None → 0
        assert!(proto.emitted_payload_jsons.is_empty());
        assert_eq!(proto.span_id, "");
        assert_eq!(proto.parent_span_id, "");
    }

    #[test]
    fn trace_block_from_serializes_emitted_payloads_as_json_strings() {
        let mut b = BlockExecution::new("blk", "evt", 1, serde_json::json!({}));
        b.emitted_payloads = vec![serde_json::json!({"x": 1}), serde_json::json!({"y": 2})];
        let proto = trace_block_from(&b);

        assert_eq!(proto.emitted_payload_jsons.len(), 2);
        let p0: serde_json::Value = serde_json::from_str(&proto.emitted_payload_jsons[0]).unwrap();
        assert_eq!(p0["x"], 1);
    }

    #[test]
    fn history_rpc_returns_exact_date_entries_in_deterministic_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let writer = Arc::new(foundry_blocks::trace_writer::TraceWriter::new(
            tmp.path().to_str().expect("trace dir must be UTF-8"),
        ));
        let store =
            Arc::new(TraceStore::with_trace_writer(Duration::from_secs(60), Arc::clone(&writer)));

        let mut newer = event_with_spans();
        newer.project = "alpha".to_string();
        newer.occurred_at = chrono::DateTime::parse_from_rfc3339("2026-07-24T12:00:00Z")
            .expect("valid timestamp")
            .with_timezone(&chrono::Utc);
        let newer_id = newer.id.clone();
        writer
            .write(
                &newer_id,
                &ProcessResult {
                    events: vec![newer],
                    block_executions: vec![],
                    total_duration_ms: 11,
                },
            )
            .expect("write newer trace");

        let mut older = event_with_spans();
        older.project = "alpha".to_string();
        older.occurred_at = chrono::DateTime::parse_from_rfc3339("2026-07-24T08:00:00Z")
            .expect("valid timestamp")
            .with_timezone(&chrono::Utc);
        let older_id = older.id.clone();
        writer
            .write(
                &older_id,
                &ProcessResult {
                    events: vec![older],
                    block_executions: vec![],
                    total_duration_ms: 7,
                },
            )
            .expect("write older trace");

        let response = history_rpc(
            &store,
            Request::new(HistoryRequest {
                date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
                project: "alpha".to_string(),
                recent_days: 7,
            }),
        )
        .into_inner();

        assert_eq!(response.days.len(), 1);
        let traces = &response.days[0].traces;
        let event_ids: Vec<_> = traces.iter().map(|trace| trace.event_id.as_str()).collect();
        assert_eq!(event_ids, vec![newer_id.as_str(), older_id.as_str()]);
        assert_eq!(traces[0].project, "alpha");
        assert_eq!(traces[0].total_duration_ms, 11);
        assert_eq!(traces[1].total_duration_ms, 7);
    }

    #[test]
    fn trace_rpc_returns_found_for_known_event_id() {
        let store = Arc::new(TraceStore::new(Duration::from_secs(60)));
        let event = event_with_spans();
        let event_id = event.id.clone();
        store.insert(
            event_id.clone(),
            ProcessResult {
                events: vec![event],
                block_executions: vec![],
                total_duration_ms: 10,
            },
        );

        let resp = trace_rpc(
            &store,
            Request::new(TraceRequest {
                event_id: event_id.clone(),
            }),
        )
        .into_inner();

        assert!(resp.found);
        assert_eq!(resp.total_duration_ms, 10);
        assert_eq!(resp.events.len(), 1);
        assert_eq!(resp.events[0].event_id, event_id);
    }

    #[test]
    fn trace_rpc_returns_not_found_for_unknown_event_id() {
        let store = Arc::new(TraceStore::new(Duration::from_secs(60)));
        let resp = trace_rpc(
            &store,
            Request::new(TraceRequest {
                event_id: "no-such-id".to_string(),
            }),
        )
        .into_inner();

        assert!(!resp.found);
        assert!(resp.events.is_empty());
        assert!(resp.block_executions.is_empty());
        assert_eq!(resp.total_duration_ms, 0);
    }

    #[test]
    fn span_rpc_returns_found_for_indexed_span_id() {
        let store = Arc::new(TraceStore::new(Duration::from_secs(60)));
        let event = event_with_spans();
        let span_id = event.span_id.clone().unwrap();
        let event_id = event.id.clone();
        store.insert(
            event_id,
            ProcessResult {
                events: vec![event],
                block_executions: vec![],
                total_duration_ms: 5,
            },
        );

        let resp = span_rpc(
            &store,
            Request::new(SpanRequest {
                span_id: span_id.clone(),
            }),
        )
        .into_inner();

        assert!(resp.found);
        assert_eq!(resp.events.len(), 1);
        assert_eq!(resp.events[0].span_id, span_id);
    }

    #[test]
    fn span_rpc_returns_not_found_for_unknown_span_id() {
        let store = Arc::new(TraceStore::new(Duration::from_secs(60)));
        let resp = span_rpc(
            &store,
            Request::new(SpanRequest {
                span_id: "deadbeefdeadbeef".to_string(),
            }),
        )
        .into_inner();

        assert!(!resp.found);
        assert!(resp.events.is_empty());
        assert!(resp.block_executions.is_empty());
        assert_eq!(resp.total_duration_ms, 0);
    }
}
