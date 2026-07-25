//! Generated-client boundary tests for daemon-owned observability RPCs.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::trace::{BlockExecution, ProcessResult};
use foundryd::{
    proto::{
        HistoryRequest, SpanRequest, StatusRequest, TraceRequest, foundry_client::FoundryClient,
        foundry_server::FoundryServer,
    },
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::{ActiveWorkflow, WorkflowTracker},
};
use prost::Message;
use prost_types::{DescriptorProto, FileDescriptorSet};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

fn parse_utc(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .expect("valid timestamp")
        .with_timezone(&Utc)
}

struct EventFixture<'a> {
    id: &'a str,
    event_type: EventType,
    project: &'a str,
    occurred_at: &'a str,
    trace_id: &'a str,
    span_id: &'a str,
    parent_span_id: Option<&'a str>,
}

fn make_event(fixture: EventFixture<'_>) -> Event {
    let mut event = Event::new(
        fixture.event_type,
        fixture.project.to_string(),
        Throttle::Full,
        serde_json::json!({}),
    );
    let occurred_at = parse_utc(fixture.occurred_at);
    event.id = fixture.id.to_string();
    event.occurred_at = occurred_at;
    event.recorded_at = occurred_at;
    event.trace_id = Some(fixture.trace_id.to_string());
    event.span_id = Some(fixture.span_id.to_string());
    event.parent_span_id = fixture.parent_span_id.map(str::to_string);
    event
}

struct SpanFixture<'a> {
    id: &'a str,
    parent_id: &'a str,
}

struct BlockFixture<'a> {
    block_name: &'a str,
    trigger_event_id: &'a str,
    emitted_event_ids: &'a [&'a str],
    success: bool,
    duration_ms: u64,
    summary: &'a str,
    span: SpanFixture<'a>,
}

fn make_block(fixture: BlockFixture<'_>) -> BlockExecution {
    let BlockFixture {
        block_name,
        trigger_event_id,
        emitted_event_ids,
        success,
        duration_ms,
        summary,
        span,
    } = fixture;

    let SpanFixture { id, parent_id } = span;

    BlockExecution {
        block_name: block_name.to_string(),
        trigger_event_id: trigger_event_id.to_string(),
        success,
        summary: summary.to_string(),
        emitted_event_ids: emitted_event_ids.iter().map(|id| (*id).to_string()).collect(),
        duration_ms,
        raw_output: Some("daemon block output".to_string()),
        exit_code: Some(0),
        trigger_payload: serde_json::json!({"from":"test"}),
        emitted_payloads: vec![serde_json::json!({"success": success})],
        audit_artifacts: vec!["/tmp/audit.txt".to_string()],
        span_id: Some(id.to_string()),
        parent_span_id: Some(parent_id.to_string()),
    }
}

fn alpha_trace() -> ProcessResult {
    let trace_id = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    let workflow_span = "1111111111111111";
    let block_span = "2222222222222222";
    let root = make_event(EventFixture {
        id: "evt_alpha_root",
        event_type: EventType::ProjectRunStarted,
        project: "alpha",
        occurred_at: "2026-07-24T12:00:00Z",
        trace_id,
        span_id: workflow_span,
        parent_span_id: None,
    });
    let completed = make_event(EventFixture {
        id: "evt_alpha_completed",
        event_type: EventType::ProjectRunCompleted,
        project: "alpha",
        occurred_at: "2026-07-24T12:00:05Z",
        trace_id,
        span_id: workflow_span,
        parent_span_id: None,
    });
    let block = make_block(BlockFixture {
        block_name: "RunAlpha",
        trigger_event_id: "evt_alpha_root",
        emitted_event_ids: &["evt_alpha_completed"],
        success: true,
        duration_ms: 53,
        summary: "completed alpha",
        span: SpanFixture {
            id: block_span,
            parent_id: workflow_span,
        },
    });
    ProcessResult {
        events: vec![root, completed],
        block_executions: vec![block],
        total_duration_ms: 53,
    }
}

fn alpha_failed_trace() -> ProcessResult {
    let root = make_event(EventFixture {
        id: "evt_alpha_failed",
        event_type: EventType::ProjectRunStarted,
        project: "alpha",
        occurred_at: "2026-07-24T09:00:00Z",
        trace_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        span_id: "3333333333333333",
        parent_span_id: None,
    });
    ProcessResult {
        events: vec![root],
        block_executions: vec![make_block(BlockFixture {
            block_name: "RunAlphaFailed",
            trigger_event_id: "evt_alpha_failed",
            emitted_event_ids: &[],
            success: false,
            duration_ms: 19,
            summary: "failed alpha",
            span: SpanFixture {
                id: "4444444444444444",
                parent_id: "3333333333333333",
            },
        })],
        total_duration_ms: 19,
    }
}

fn beta_trace() -> ProcessResult {
    let root = make_event(EventFixture {
        id: "evt_beta_root",
        event_type: EventType::ProjectRunStarted,
        project: "beta",
        occurred_at: "2026-07-24T10:30:00Z",
        trace_id: "cccccccccccccccccccccccccccccccc",
        span_id: "5555555555555555",
        parent_span_id: None,
    });
    ProcessResult {
        events: vec![root],
        block_executions: vec![make_block(BlockFixture {
            block_name: "RunBeta",
            trigger_event_id: "evt_beta_root",
            emitted_event_ids: &[],
            success: true,
            duration_ms: 31,
            summary: "completed beta",
            span: SpanFixture {
                id: "6666666666666666",
                parent_id: "5555555555555555",
            },
        })],
        total_duration_ms: 31,
    }
}

async fn make_service() -> (String, TempDir) {
    let (event_tx, _rx) = broadcast::channel(64);
    let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));
    let tmp_traces = tempfile::tempdir().expect("tempdir for traces");
    let trace_writer =
        Arc::new(TraceWriter::new(tmp_traces.path().to_str().expect("trace dir must be UTF-8")));
    let trace_store = Arc::new(TraceStore::with_trace_writer(
        Duration::from_secs(60),
        Arc::clone(&trace_writer),
    ));
    let workflow_tracker = Arc::new(WorkflowTracker::new());
    workflow_tracker.insert(ActiveWorkflow {
        event_id: "wf_alpha".to_string(),
        event_type: "project_run_started".to_string(),
        project: "alpha".to_string(),
        trace_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        started_at: parse_utc("2026-07-24T12:00:00Z"),
    });
    workflow_tracker.insert(ActiveWorkflow {
        event_id: "wf_beta".to_string(),
        event_type: "project_run_started".to_string(),
        project: "beta".to_string(),
        trace_id: "cccccccccccccccccccccccccccccccc".to_string(),
        started_at: parse_utc("2026-07-24T10:30:00Z"),
    });

    let alpha = alpha_trace();
    let alpha_failed = alpha_failed_trace();
    let beta = beta_trace();
    trace_writer.write("evt_alpha_root", &alpha).expect("write alpha trace");
    trace_writer
        .write("evt_alpha_failed", &alpha_failed)
        .expect("write alpha failed trace");
    trace_writer.write("evt_beta_root", &beta).expect("write beta trace");
    trace_store.insert("evt_alpha_root".to_string(), alpha);
    trace_store.insert("evt_alpha_failed".to_string(), alpha_failed);
    trace_store.insert("evt_beta_root".to_string(), beta);

    let registry = Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![],
    }));
    let tmp_registry = NamedTempFile::new().expect("tempfile for registry");
    let campaigns_path = NamedTempFile::new().expect("tempfile for campaigns").into_temp_path();
    let sentinels = Arc::new(RwLock::new(SentinelStore::default_seed()));
    let tmp_sentinels = NamedTempFile::new().expect("tempfile for sentinels");
    let scheduler_reload = Arc::new(Notify::new());

    let ctx = RuntimeContext {
        engine,
        trace_store,
        workflow_tracker,
        trace_writer,
        event_tx,
        registry,
    };
    let stores = StoreConfig {
        campaigns_path: campaigns_path.to_path_buf(),
        registry_path: tmp_registry.path().to_path_buf(),
        sentinels,
        sentinels_path: tmp_sentinels.path().to_path_buf(),
        scheduler_reload,
    };
    let service = FoundryService::new(ctx, stores);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    let incoming = TcpListenerStream::new(listener);

    tokio::spawn(async move {
        Server::builder()
            .add_service(FoundryServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .expect("gRPC server error");
    });
    tokio::task::yield_now().await;
    let addr = format!("http://127.0.0.1:{port}");

    (addr, tmp_traces)
}

fn span_response_descriptor() -> DescriptorProto {
    let descriptor_bytes = include_bytes!(concat!(env!("OUT_DIR"), "/foundry_descriptor.bin"));
    let descriptor_set =
        FileDescriptorSet::decode(&descriptor_bytes[..]).expect("decode generated descriptor set");

    descriptor_set
        .file
        .into_iter()
        .find(|file| file.package.as_deref() == Some("foundry"))
        .and_then(|file| {
            file.message_type
                .into_iter()
                .find(|message| message.name.as_deref() == Some("SpanResponse"))
        })
        .expect("generated descriptor must contain foundry.SpanResponse")
}

#[test]
fn generated_proto_keeps_span_response_wire_numbers_compatible() {
    let descriptor = span_response_descriptor();

    let total_duration = descriptor
        .field
        .iter()
        .find(|field| field.name.as_deref() == Some("total_duration_ms"))
        .expect("SpanResponse.total_duration_ms field must exist");
    assert_eq!(
        total_duration.number,
        Some(4),
        "SpanResponse.total_duration_ms must stay on wire field 4"
    );

    let trace_id = descriptor
        .field
        .iter()
        .find(|field| field.name.as_deref() == Some("trace_id"))
        .expect("SpanResponse.trace_id field must exist");
    assert_eq!(
        trace_id.number,
        Some(5),
        "SpanResponse.trace_id must use a newly appended wire field number"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_client_status_returns_exact_active_workflows() {
    let (addr, _tmp_traces) = make_service().await;
    let mut client = FoundryClient::connect(addr).await.expect("connect client");

    let response = client
        .status(StatusRequest {
            workflow_id: String::new(),
        })
        .await
        .expect("status RPC should succeed")
        .into_inner();

    assert_eq!(response.workflows.len(), 2);
    let alpha = response
        .workflows
        .iter()
        .find(|workflow| workflow.workflow_id == "wf_alpha")
        .expect("alpha workflow must be present");
    assert_eq!(alpha.workflow_type, "project_run_started");
    assert_eq!(alpha.project, "alpha");
    assert_eq!(alpha.state, "running");
    assert_eq!(alpha.trace_id, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

    let beta = response
        .workflows
        .iter()
        .find(|workflow| workflow.workflow_id == "wf_beta")
        .expect("beta workflow must be present");
    assert_eq!(beta.project, "beta");
    assert_eq!(beta.trace_id, "cccccccccccccccccccccccccccccccc");
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_client_history_filters_project_and_preserves_order() {
    let (addr, _tmp_traces) = make_service().await;
    let mut client = FoundryClient::connect(addr).await.expect("connect client");

    let response = client
        .history(HistoryRequest {
            date: Utc::now().format("%Y-%m-%d").to_string(),
            project: "alpha".to_string(),
            recent_days: 7,
        })
        .await
        .expect("history RPC should succeed")
        .into_inner();

    assert_eq!(response.days.len(), 1);
    let traces = &response.days[0].traces;
    assert_eq!(traces.len(), 2);
    assert_eq!(traces[0].event_id, "evt_alpha_root");
    assert_eq!(traces[0].event_type, "project_run_started");
    assert_eq!(traces[0].project, "alpha");
    assert!(traces[0].success);
    assert_eq!(traces[0].total_duration_ms, 53);
    assert_eq!(traces[0].trace_id, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert_eq!(traces[1].event_id, "evt_alpha_failed");
    assert!(!traces[1].success);
    assert_eq!(traces[1].total_duration_ms, 19);
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_client_trace_returns_exact_daemon_owned_record() {
    let (addr, _tmp_traces) = make_service().await;
    let mut client = FoundryClient::connect(addr).await.expect("connect client");

    let response = client
        .trace(TraceRequest {
            event_id: "evt_alpha_root".to_string(),
        })
        .await
        .expect("trace RPC should succeed")
        .into_inner();

    assert!(response.found);
    assert_eq!(response.total_duration_ms, 53);
    assert_eq!(response.events.len(), 2);
    assert_eq!(response.events[0].event_id, "evt_alpha_root");
    assert_eq!(response.events[0].event_type, "project_run_started");
    assert_eq!(response.events[0].project, "alpha");
    assert_eq!(response.events[0].trace_id, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert_eq!(response.events[1].event_id, "evt_alpha_completed");
    assert_eq!(response.block_executions.len(), 1);
    assert_eq!(response.block_executions[0].block_name, "RunAlpha");
    assert!(response.block_executions[0].success);
    assert_eq!(response.block_executions[0].duration_ms, 53);
    assert_eq!(response.block_executions[0].trigger_event_id, "evt_alpha_root");
    assert_eq!(response.block_executions[0].span_id, "2222222222222222");
    assert_eq!(response.block_executions[0].parent_span_id, "1111111111111111");
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_client_span_returns_exact_workflow_span_members() {
    let (addr, _tmp_traces) = make_service().await;
    let mut client = FoundryClient::connect(addr).await.expect("connect client");

    let response = client
        .span(SpanRequest {
            span_id: "1111111111111111".to_string(),
        })
        .await
        .expect("span RPC should succeed")
        .into_inner();

    assert!(response.found);
    assert_eq!(response.trace_id, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert_eq!(response.total_duration_ms, 53);
    let event_ids: Vec<_> = response.events.iter().map(|event| event.event_id.as_str()).collect();
    assert_eq!(event_ids, vec!["evt_alpha_root", "evt_alpha_completed"]);
    assert!(
        response
            .events
            .iter()
            .all(|event| event.trace_id == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
    );
    assert_eq!(response.block_executions.len(), 1);
    assert_eq!(response.block_executions[0].block_name, "RunAlpha");
    assert_eq!(response.block_executions[0].parent_span_id, "1111111111111111");
    assert_eq!(response.block_executions[0].duration_ms, 53);
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_client_span_returns_block_only_span_with_owning_trace() {
    let (addr, _tmp_traces) = make_service().await;
    let mut client = FoundryClient::connect(addr).await.expect("connect client");

    let response = client
        .span(SpanRequest {
            span_id: "2222222222222222".to_string(),
        })
        .await
        .expect("span RPC should succeed")
        .into_inner();

    assert!(response.found);
    assert_eq!(response.trace_id, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    assert!(response.events.is_empty());
    assert_eq!(response.block_executions.len(), 1);
    assert_eq!(response.block_executions[0].block_name, "RunAlpha");
    assert_eq!(response.block_executions[0].span_id, "2222222222222222");
    assert_eq!(response.block_executions[0].parent_span_id, "1111111111111111");
    assert_eq!(response.total_duration_ms, 53);
}

#[tokio::test(flavor = "multi_thread")]
async fn generated_client_unknown_span_keeps_not_found_shape() {
    let (addr, _tmp_traces) = make_service().await;
    let mut client = FoundryClient::connect(addr).await.expect("connect client");

    let response = client
        .span(SpanRequest {
            span_id: "ffffffffffffffff".to_string(),
        })
        .await
        .expect("span RPC should succeed")
        .into_inner();

    assert!(!response.found);
    assert!(response.trace_id.is_empty());
    assert!(response.events.is_empty());
    assert!(response.block_executions.is_empty());
    assert_eq!(response.total_duration_ms, 0);
}
