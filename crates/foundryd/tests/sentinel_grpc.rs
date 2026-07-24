//! Integration tests for the sentinel gRPC handlers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::{EmitSpec, Schedule, SentinelEntry, SentinelStore};
use foundry_sdk::throttle::Throttle;
use foundryd::{
    proto::{
        SentinelDisableRequest, SentinelEnableRequest, SentinelListRequest, SentinelShowRequest,
        foundry_client::FoundryClient, foundry_server::FoundryServer,
    },
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

fn daemon_sentinels() -> SentinelStore {
    SentinelStore {
        version: 1,
        sentinels: vec![
            SentinelEntry {
                name: "nightly-maintenance".to_string(),
                schedule: Schedule::Cron("7 4 * * 1".to_string()),
                emit: EmitSpec {
                    event_type: foundry_sdk::event::EventType::MaintenanceCycleStarted,
                    project: "daemon-system".to_string(),
                    throttle: Throttle::DryRun,
                    payload: serde_json::json!({"scope":"daemon-owned","window":"night"}),
                },
                enabled: false,
            },
            SentinelEntry {
                name: "ops-digest".to_string(),
                schedule: Schedule::Cron("11 */6 * * *".to_string()),
                emit: EmitSpec {
                    event_type: foundry_sdk::event::EventType::OpsDigestStarted,
                    project: "daemon-ops".to_string(),
                    throttle: Throttle::Full,
                    payload: serde_json::json!({"kind":"ops","priority":"normal"}),
                },
                enabled: true,
            },
        ],
    }
}

fn make_service_with_sentinels(
    sentinels_data: SentinelStore,
) -> (FoundryService, NamedTempFile, TempDir) {
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
    let registry = Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![],
    }));
    let tmp_registry = NamedTempFile::new().expect("tempfile for registry");
    let registry_path = tmp_registry.path().to_path_buf();
    let tmp_campaigns = NamedTempFile::new().expect("tempfile for campaigns");
    let campaigns_path = tmp_campaigns.path().to_path_buf();
    let sentinels = Arc::new(RwLock::new(sentinels_data));
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

    (FoundryService::new(ctx, stores), tmp_sentinels, tmp_traces)
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

#[tokio::test]
async fn list_returns_full_daemon_owned_sentinel_fields() {
    let (service, _tmp_sentinels, _tmp_traces) = make_service_with_sentinels(daemon_sentinels());
    let addr = start_server(service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect client");

    let response = client
        .sentinel_list(SentinelListRequest {})
        .await
        .expect("sentinel_list should succeed")
        .into_inner();

    assert_eq!(response.sentinels.len(), 2);
    let sentinel = &response.sentinels[0];
    assert_eq!(sentinel.name, "nightly-maintenance");
    assert_eq!(sentinel.cron, "7 4 * * 1");
    assert_eq!(sentinel.emit_event_type, "maintenance_cycle_started");
    assert_eq!(sentinel.emit_project, "daemon-system");
    assert_eq!(sentinel.emit_throttle, 1);
    assert_eq!(sentinel.emit_payload_json, r#"{"scope":"daemon-owned","window":"night"}"#);
    assert!(!sentinel.enabled);
}

#[tokio::test]
async fn show_returns_full_daemon_owned_sentinel_fields() {
    let (service, _tmp_sentinels, _tmp_traces) = make_service_with_sentinels(daemon_sentinels());
    let addr = start_server(service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect client");

    let response = client
        .sentinel_show(SentinelShowRequest {
            name: "ops-digest".to_string(),
        })
        .await
        .expect("sentinel_show should succeed")
        .into_inner();

    let sentinel = response.sentinel.expect("sentinel echoed");
    assert_eq!(sentinel.name, "ops-digest");
    assert_eq!(sentinel.cron, "11 */6 * * *");
    assert_eq!(sentinel.emit_event_type, "ops_digest_started");
    assert_eq!(sentinel.emit_project, "daemon-ops");
    assert_eq!(sentinel.emit_throttle, 0);
    assert_eq!(sentinel.emit_payload_json, r#"{"kind":"ops","priority":"normal"}"#);
    assert!(sentinel.enabled);
}

#[tokio::test]
async fn enable_and_disable_round_trip_via_generated_client() {
    let (service, tmp_sentinels, _tmp_traces) = make_service_with_sentinels(daemon_sentinels());
    let addr = start_server(service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect client");

    let enabled = client
        .sentinel_enable(SentinelEnableRequest {
            name: "nightly-maintenance".to_string(),
        })
        .await
        .expect("sentinel_enable should succeed")
        .into_inner()
        .sentinel
        .expect("sentinel echoed");
    assert!(enabled.enabled);
    assert_eq!(enabled.name, "nightly-maintenance");
    assert_eq!(enabled.cron, "7 4 * * 1");
    assert_eq!(enabled.emit_event_type, "maintenance_cycle_started");
    assert_eq!(enabled.emit_project, "daemon-system");
    assert_eq!(enabled.emit_throttle, 1);
    assert_eq!(enabled.emit_payload_json, r#"{"scope":"daemon-owned","window":"night"}"#);

    let disabled = client
        .sentinel_disable(SentinelDisableRequest {
            name: "ops-digest".to_string(),
        })
        .await
        .expect("sentinel_disable should succeed")
        .into_inner()
        .sentinel
        .expect("sentinel echoed");
    assert!(!disabled.enabled);
    assert_eq!(disabled.name, "ops-digest");
    assert_eq!(disabled.cron, "11 */6 * * *");
    assert_eq!(disabled.emit_event_type, "ops_digest_started");
    assert_eq!(disabled.emit_project, "daemon-ops");
    assert_eq!(disabled.emit_throttle, 0);
    assert_eq!(disabled.emit_payload_json, r#"{"kind":"ops","priority":"normal"}"#);

    let on_disk = SentinelStore::load(tmp_sentinels.path()).expect("load daemon sentinel store");
    assert!(
        on_disk.find_sentinel("nightly-maintenance").expect("seed exists").enabled,
        "enable must persist to disk"
    );
    assert!(
        !on_disk.find_sentinel("ops-digest").expect("seed exists").enabled,
        "disable must persist to disk"
    );
}
