//! Integration tests for CLI daemon-address precedence.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundryd::{
    proto::foundry_server::FoundryServer,
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

fn make_service() -> (FoundryService, TempDir) {
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

    (FoundryService::new(ctx, stores), tmp_traces)
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

fn run_foundry(
    home: &std::path::Path,
    env_addr: Option<&str>,
    args: &[&str],
) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_foundry"));
    command
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"));
    if let Some(addr) = env_addr {
        command.env("FOUNDRY_DAEMON_ADDR", addr);
    }
    command.output().expect("run foundry binary")
}

fn stdout_string(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout must be UTF-8")
}

fn stderr_string(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr must be UTF-8")
}

#[tokio::test(flavor = "multi_thread")]
async fn status_uses_configured_daemon_addr_when_flag_absent() {
    let (service, _tmp_traces) = make_service();
    let addr = start_server(service).await;
    let home = tempfile::tempdir().expect("tempdir for cli home");

    let output = run_foundry(home.path(), Some(&addr), &["status"]);
    assert!(
        output.status.success(),
        "status should succeed via configured daemon addr\nstdout: {}\nstderr: {}",
        stdout_string(&output),
        stderr_string(&output)
    );
    assert_eq!(stdout_string(&output), "No active workflows.\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn explicit_addr_overrides_configured_daemon_addr() {
    let (service, _tmp_traces) = make_service();
    let addr = start_server(service).await;
    let home = tempfile::tempdir().expect("tempdir for cli home");

    let output = run_foundry(home.path(), Some("http://127.0.0.1:9"), &["--addr", &addr, "status"]);
    assert!(
        output.status.success(),
        "--addr should override configured daemon addr\nstdout: {}\nstderr: {}",
        stdout_string(&output),
        stderr_string(&output)
    );
    assert_eq!(stdout_string(&output), "No active workflows.\n");
}
