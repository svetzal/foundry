//! Integration test for the `MaintenanceSummaryRequested` root through the
//! generated tonic client.
//!
//! A real [`FoundryService`] is bound to a temporary TCP port. A
//! `MaintenanceCycleStarted` event for the `system` project is dispatched
//! through the generated `FoundryClient`'s `Emit` RPC, driving the true engine
//! path: `run_workflow` recognises the completed system cycle and calls
//! `finalise_system_maintenance`, which builds and dispatches the
//! `MaintenanceSummaryRequested` root. The test observes that root on the
//! broadcast channel — the real emission path, not a mock.
//!
//! Evidence gate verified by this file:
//!
//! - The `MaintenanceSummaryRequested` root minted by `finalise` carries a
//!   32-char-hex `trace_id`, a 16-char-hex `span_id`, and no `parent_span_id`.
//!
//! Live gap this guards against (confirmed in `~/.foundry/events/2026-07.jsonl`,
//! most recently at 2026-07-24T09:22): `maintenance_triage_completed`,
//! `maintenance_summary_requested`, and `maintenance_triage_digest_written` were
//! emitted with a `causation_id` but no `trace_id`/`span_id`, because the
//! summary root was built with a bare `Event::new` and `stamp_context` only
//! inherits.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundryd::{
    proto::{EmitRequest, foundry_client::FoundryClient, foundry_server::FoundryServer},
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

// ── Test harness ─────────────────────────────────────────────────────────────

/// Construct a `FoundryService` backed by temporary files and return a
/// broadcast receiver so the test can observe emitted events. The registry is
/// empty, so the system cycle fans out to zero projects and finalisation runs
/// immediately.
fn make_service() -> (FoundryService, broadcast::Receiver<Event>, TempDir) {
    let (event_tx, rx) = broadcast::channel(64);
    let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));

    let tmp_traces = tempfile::tempdir().expect("tempdir for traces");
    let trace_writer =
        Arc::new(TraceWriter::new(tmp_traces.path().to_str().expect("trace dir must be UTF-8")));
    let trace_store = Arc::new(TraceStore::with_trace_writer(
        Duration::from_secs(60),
        Arc::clone(&trace_writer),
    ));
    let workflow_tracker = Arc::new(WorkflowTracker::new());
    let registry = Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![],
    }));

    // The system-cycle path reads only the in-memory registry and sentinels;
    // it never touches the campaign or registry files, so these temp handles
    // may drop when this function returns (as the campaign harness also does
    // for the stores its path under test does not read).
    let tmp_registry = NamedTempFile::new().expect("tempfile for registry");
    let registry_path = tmp_registry.path().to_path_buf();
    let tmp_campaigns = NamedTempFile::new().expect("tempfile for campaigns");
    let campaigns_path = tmp_campaigns.path().to_path_buf();
    let sentinels = Arc::new(RwLock::new(SentinelStore::default_seed()));
    let tmp_sentinels = NamedTempFile::new().expect("tempfile for sentinels");
    let sentinels_path = tmp_sentinels.path().to_path_buf();
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
        campaigns_path,
        registry_path,
        sentinels,
        sentinels_path,
        scheduler_reload,
    };
    let service = FoundryService::new(ctx, stores);

    (service, rx, tmp_traces)
}

async fn start_server(service: FoundryService) -> String {
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

    format!("http://127.0.0.1:{port}")
}

// ── Proof: the summary root minted by `finalise` carries a trace ──────────────

/// A completed `system` maintenance cycle must dispatch a
/// `MaintenanceSummaryRequested` root that mints a trace and a root span.
///
/// Without it, the triage + summary fan-out (`MaintenanceTriageCompleted`,
/// `MaintenanceTriageDigestWritten`, the generated summary) inherits `None` for
/// both ids and lands in the event log untraceable — the exact production
/// symptom this test locks out.
#[tokio::test]
async fn system_cycle_dispatches_a_traceable_summary_root_event() {
    let (service, mut rx, _tmp_traces) = make_service();
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client
        .emit(EmitRequest {
            event_type: "maintenance_cycle_started".to_string(),
            project: "system".to_string(),
            throttle: 0,
            payload_json: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: String::new(),
        })
        .await
        .expect("emit of MaintenanceCycleStarted must be accepted");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut root = None;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) if event.event_type == EventType::MaintenanceSummaryRequested => {
                root = Some(event);
                break;
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    let root = root
        .expect("MaintenanceSummaryRequested must be broadcast once the system cycle finalises");

    let trace_id = root
        .trace_id
        .expect("summary root must mint a trace id; without one the triage fan-out is untraceable");
    assert_eq!(
        trace_id.len(),
        32,
        "trace id must be the standard 32-char hex form, got {trace_id:?}"
    );
    assert!(
        trace_id.chars().all(|c| c.is_ascii_hexdigit()),
        "trace id must be lowercase hex, got {trace_id:?}"
    );

    let span_id = root
        .span_id
        .expect("summary root must mint a span id; MaintenanceSummaryRequested opens the span");
    assert_eq!(
        span_id.len(),
        16,
        "span id must be the standard 16-char hex form, got {span_id:?}"
    );
    assert!(
        span_id.chars().all(|c| c.is_ascii_hexdigit()),
        "span id must be lowercase hex, got {span_id:?}"
    );

    assert!(
        root.parent_span_id.is_none(),
        "the summary root opens its own chain; no parent span"
    );
}
