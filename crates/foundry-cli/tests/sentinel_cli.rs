//! Integration tests for `foundry sentinel` CLI behavior.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::{EmitSpec, Schedule, SentinelEntry, SentinelStore};
use foundry_sdk::throttle::Throttle;
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

const DUMMY_ADDR: &str = "http://127.0.0.1:9";

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
    sentinels_path: std::path::PathBuf,
) -> (FoundryService, TempDir) {
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
    sentinels_path: &std::path::Path,
    addr: &str,
    args: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_foundry"))
        .arg("--addr")
        .arg(addr)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("FOUNDRY_SENTINELS_PATH", sentinels_path)
        .output()
        .expect("run foundry binary")
}

fn stdout_string(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout must be valid UTF-8")
}

fn stderr_string(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr must be valid UTF-8")
}

fn assert_command_succeeded(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout_string(output),
        stderr_string(output)
    );
}

fn seed_client_sentinel_trap(home: &std::path::Path) -> (std::path::PathBuf, Vec<u8>) {
    let sentinels_path = home.join(".foundry/sentinels.json");
    std::fs::create_dir_all(
        sentinels_path.parent().expect("client sentinel path must have a parent"),
    )
    .expect("create client sentinel parent");
    let bytes = br"not valid json and must stay untouched".to_vec();
    std::fs::write(&sentinels_path, &bytes).expect("seed client sentinel trap bytes");
    (sentinels_path, bytes)
}

fn missing_client_sentinels_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".foundry/sentinels.json")
}

fn assert_client_file_absent(path: &std::path::Path, context: &str) {
    assert!(
        !path.exists(),
        "{context}: expected client sentinels file to remain absent at {}",
        path.display()
    );
}

fn assert_daemon_sentinel_matches(path: &std::path::Path, expected: &SentinelEntry) {
    let daemon_store = SentinelStore::load(path).expect("load daemon sentinel store");
    let actual = daemon_store
        .find_sentinel(&expected.name)
        .expect("daemon sentinel store should contain expected entry");
    let Schedule::Cron(actual_cron) = &actual.schedule;
    let Schedule::Cron(expected_cron) = &expected.schedule;
    assert_eq!(actual.name, expected.name);
    assert_eq!(actual_cron, expected_cron);
    assert_eq!(actual.emit.event_type, expected.emit.event_type);
    assert_eq!(actual.emit.project, expected.emit.project);
    assert_eq!(actual.emit.throttle, expected.emit.throttle);
    assert_eq!(actual.emit.payload, expected.emit.payload);
    assert_eq!(actual.enabled, expected.enabled);
}

fn assert_offline_sentinel_file_matches(path: &std::path::Path, expected: &SentinelEntry) {
    let store = SentinelStore::load(path).expect("load offline sentinel store");
    let actual = store
        .find_sentinel(&expected.name)
        .expect("offline sentinel store should contain expected entry");
    let Schedule::Cron(actual_cron) = &actual.schedule;
    let Schedule::Cron(expected_cron) = &expected.schedule;
    assert_eq!(actual.name, expected.name);
    assert_eq!(actual_cron, expected_cron);
    assert_eq!(actual.emit.event_type, expected.emit.event_type);
    assert_eq!(actual.emit.project, expected.emit.project);
    assert_eq!(actual.emit.throttle, expected.emit.throttle);
    assert_eq!(actual.emit.payload, expected.emit.payload);
    assert_eq!(actual.enabled, expected.enabled);
}

fn assert_online_unreachable_keeps_client_sentinels_absent(
    client_home: &std::path::Path,
    args: &[&str],
    expected_stderr: &str,
    context: &str,
) {
    let client_sentinels = missing_client_sentinels_path(client_home);
    assert_client_file_absent(&client_sentinels, context);

    let output = run_foundry(client_home, &client_sentinels, DUMMY_ADDR, args);

    assert!(!output.status.success(), "{context}: command should fail");
    assert_eq!(stderr_string(&output).trim(), expected_stderr);
    assert_client_file_absent(&client_sentinels, context);
}

fn assert_online_unreachable_keeps_client_sentinels_byte_identical(
    client_home: &std::path::Path,
    args: &[&str],
    expected_stderr: &str,
    context: &str,
) {
    let (client_sentinels, before) = seed_client_sentinel_trap(client_home);

    let output = run_foundry(client_home, &client_sentinels, DUMMY_ADDR, args);

    assert!(!output.status.success(), "{context}: command should fail");
    assert_eq!(stderr_string(&output).trim(), expected_stderr);
    assert_eq!(
        std::fs::read(&client_sentinels)
            .expect("read trap sentinel file after failed online command"),
        before,
        "{context}: command must not read or mutate the client-side sentinel file"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn online_list_reads_daemon_owned_sentinels_and_leaves_client_file_absent() {
    let daemon_tempdir = tempfile::tempdir().expect("tempdir");
    let daemon_sentinels_path = daemon_tempdir.path().join("daemon-sentinels.json");
    daemon_sentinels().save(&daemon_sentinels_path).expect("seed daemon sentinels");
    let (service, _traces) =
        make_service_with_sentinels(daemon_sentinels(), daemon_sentinels_path.clone());
    let addr = start_server(service).await;

    let client_home = tempfile::tempdir().expect("client home");
    let client_sentinels = missing_client_sentinels_path(client_home.path());
    assert_client_file_absent(&client_sentinels, "online list precondition");

    let output = run_foundry(client_home.path(), &client_sentinels, &addr, &["sentinel", "list"]);

    assert_command_succeeded(&output);
    let stdout = stdout_string(&output);
    assert!(stdout.contains("nightly-maintenance"));
    assert!(stdout.contains("cron: 7 4 * * 1"));
    assert!(stdout.contains("maintenance_cycle_started"));
    assert!(stdout.contains("daemon-system"));
    assert!(stdout.contains("no"));
    assert!(stdout.contains("ops-digest"));
    assert!(stdout.contains("cron: 11 */6 * * *"));
    assert!(stdout.contains("daemon-ops"));
    assert!(stdout.contains("yes"));
    assert_client_file_absent(&client_sentinels, "online list");
}

#[tokio::test(flavor = "multi_thread")]
async fn online_show_reads_daemon_owned_sentinel_and_leaves_client_trap_untouched() {
    let daemon_tempdir = tempfile::tempdir().expect("tempdir");
    let daemon_sentinels_path = daemon_tempdir.path().join("daemon-sentinels.json");
    daemon_sentinels().save(&daemon_sentinels_path).expect("seed daemon sentinels");
    let (service, _traces) =
        make_service_with_sentinels(daemon_sentinels(), daemon_sentinels_path.clone());
    let addr = start_server(service).await;

    let client_home = tempfile::tempdir().expect("client home");
    let (client_sentinels, before) = seed_client_sentinel_trap(client_home.path());

    let output = run_foundry(
        client_home.path(),
        &client_sentinels,
        &addr,
        &["sentinel", "show", "nightly-maintenance"],
    );

    assert_command_succeeded(&output);
    let stdout = stdout_string(&output);
    assert!(stdout.contains("Name:      nightly-maintenance"));
    assert!(stdout.contains("Schedule:  cron: 7 4 * * 1"));
    assert!(stdout.contains("Enabled:   no"));
    assert!(stdout.contains("Emits:     maintenance_cycle_started"));
    assert!(stdout.contains("Project:   daemon-system"));
    assert!(stdout.contains("Throttle:  dry_run"));
    assert!(stdout.contains(r#"Payload:   {"scope":"daemon-owned","window":"night"}"#));
    assert_eq!(
        std::fs::read(&client_sentinels).expect("read trap sentinel file after online show"),
        before
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn online_enable_mutates_daemon_store_and_leaves_client_trap_untouched() {
    let daemon_tempdir = tempfile::tempdir().expect("tempdir");
    let daemon_sentinels_path = daemon_tempdir.path().join("daemon-sentinels.json");
    daemon_sentinels().save(&daemon_sentinels_path).expect("seed daemon sentinels");
    let (service, _traces) =
        make_service_with_sentinels(daemon_sentinels(), daemon_sentinels_path.clone());
    let addr = start_server(service).await;

    let client_home = tempfile::tempdir().expect("client home");
    let (client_sentinels, before) = seed_client_sentinel_trap(client_home.path());

    let output = run_foundry(
        client_home.path(),
        &client_sentinels,
        &addr,
        &["sentinel", "enable", "nightly-maintenance"],
    );

    assert_command_succeeded(&output);
    assert_eq!(stdout_string(&output).trim(), "Enabled sentinel 'nightly-maintenance'.");
    assert_eq!(
        std::fs::read(&client_sentinels).expect("read trap sentinel file after online enable"),
        before
    );

    let mut expected = daemon_sentinels()
        .find_sentinel("nightly-maintenance")
        .expect("seed sentinel exists")
        .clone();
    expected.enabled = true;
    assert_daemon_sentinel_matches(&daemon_sentinels_path, &expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn online_disable_mutates_daemon_store_and_leaves_client_file_absent() {
    let daemon_tempdir = tempfile::tempdir().expect("tempdir");
    let daemon_sentinels_path = daemon_tempdir.path().join("daemon-sentinels.json");
    daemon_sentinels().save(&daemon_sentinels_path).expect("seed daemon sentinels");
    let (service, _traces) =
        make_service_with_sentinels(daemon_sentinels(), daemon_sentinels_path.clone());
    let addr = start_server(service).await;

    let client_home = tempfile::tempdir().expect("client home");
    let client_sentinels = missing_client_sentinels_path(client_home.path());

    let output = run_foundry(
        client_home.path(),
        &client_sentinels,
        &addr,
        &["sentinel", "disable", "ops-digest"],
    );

    assert_command_succeeded(&output);
    assert_eq!(stdout_string(&output).trim(), "Disabled sentinel 'ops-digest'.");
    assert_client_file_absent(&client_sentinels, "online disable");

    let mut expected = daemon_sentinels()
        .find_sentinel("ops-digest")
        .expect("seed sentinel exists")
        .clone();
    expected.enabled = false;
    assert_daemon_sentinel_matches(&daemon_sentinels_path, &expected);
}

#[test]
fn online_list_unreachable_keeps_client_file_absent() {
    let client_home = tempfile::tempdir().expect("client home");
    assert_online_unreachable_keeps_client_sentinels_absent(
        client_home.path(),
        &["sentinel", "list"],
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry sentinel list --offline`",
        "sentinel list unreachable",
    );
}

#[test]
fn online_show_unreachable_keeps_client_trap_byte_identical() {
    let client_home = tempfile::tempdir().expect("client home");
    assert_online_unreachable_keeps_client_sentinels_byte_identical(
        client_home.path(),
        &["sentinel", "show", "nightly-maintenance"],
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry sentinel show nightly-maintenance --offline`",
        "sentinel show unreachable",
    );
}

#[test]
fn online_enable_unreachable_keeps_client_file_absent() {
    let client_home = tempfile::tempdir().expect("client home");
    assert_online_unreachable_keeps_client_sentinels_absent(
        client_home.path(),
        &["sentinel", "enable", "nightly-maintenance"],
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry sentinel enable nightly-maintenance --offline`",
        "sentinel enable unreachable",
    );
}

#[test]
fn online_disable_unreachable_keeps_client_trap_byte_identical() {
    let client_home = tempfile::tempdir().expect("client home");
    assert_online_unreachable_keeps_client_sentinels_byte_identical(
        client_home.path(),
        &["sentinel", "disable", "nightly-maintenance"],
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry sentinel disable nightly-maintenance --offline`",
        "sentinel disable unreachable",
    );
}

#[test]
fn offline_list_reads_direct_file_state() {
    let client_home = tempfile::tempdir().expect("client home");
    let sentinels_path = client_home.path().join(".foundry/offline-sentinels.json");
    daemon_sentinels().save(&sentinels_path).expect("seed offline sentinel store");

    let output = run_foundry(
        client_home.path(),
        &sentinels_path,
        DUMMY_ADDR,
        &["sentinel", "list", "--offline"],
    );

    assert_command_succeeded(&output);
    let stdout = stdout_string(&output);
    assert!(stdout.contains("nightly-maintenance"));
    assert!(stdout.contains("cron: 7 4 * * 1"));
    assert!(stdout.contains("maintenance_cycle_started"));
    assert!(stdout.contains("daemon-system"));
    assert!(stdout.contains("ops-digest"));
    assert!(stdout.contains("cron: 11 */6 * * *"));
    assert!(stdout.contains("ops_digest_started"));
    assert!(stdout.contains("daemon-ops"));
}

#[test]
fn offline_show_reads_direct_file_state() {
    let client_home = tempfile::tempdir().expect("client home");
    let sentinels_path = client_home.path().join(".foundry/offline-sentinels.json");
    daemon_sentinels().save(&sentinels_path).expect("seed offline sentinel store");

    let output = run_foundry(
        client_home.path(),
        &sentinels_path,
        DUMMY_ADDR,
        &["sentinel", "show", "--offline", "nightly-maintenance"],
    );

    assert_command_succeeded(&output);
    let stdout = stdout_string(&output);
    assert!(stdout.contains("Name:      nightly-maintenance"));
    assert!(stdout.contains("Schedule:  cron: 7 4 * * 1"));
    assert!(stdout.contains("Enabled:   no"));
    assert!(stdout.contains("Emits:     maintenance_cycle_started"));
    assert!(stdout.contains("Project:   daemon-system"));
    assert!(stdout.contains("Throttle:  dry_run"));
    assert!(stdout.contains(r#"Payload:   {"scope":"daemon-owned","window":"night"}"#));
}

#[test]
fn offline_enable_mutates_direct_file_state() {
    let client_home = tempfile::tempdir().expect("client home");
    let sentinels_path = client_home.path().join(".foundry/offline-sentinels.json");
    daemon_sentinels().save(&sentinels_path).expect("seed offline sentinel store");

    let output = run_foundry(
        client_home.path(),
        &sentinels_path,
        DUMMY_ADDR,
        &["sentinel", "enable", "--offline", "nightly-maintenance"],
    );

    assert_command_succeeded(&output);
    assert_eq!(stdout_string(&output).trim(), "Enabled sentinel 'nightly-maintenance'.");

    let mut expected = daemon_sentinels()
        .find_sentinel("nightly-maintenance")
        .expect("seed sentinel exists")
        .clone();
    expected.enabled = true;
    assert_offline_sentinel_file_matches(&sentinels_path, &expected);
}

#[test]
fn offline_disable_mutates_direct_file_state() {
    let client_home = tempfile::tempdir().expect("client home");
    let sentinels_path = client_home.path().join(".foundry/offline-sentinels.json");
    daemon_sentinels().save(&sentinels_path).expect("seed offline sentinel store");

    let output = run_foundry(
        client_home.path(),
        &sentinels_path,
        DUMMY_ADDR,
        &["sentinel", "disable", "--offline", "ops-digest"],
    );

    assert_command_succeeded(&output);
    assert_eq!(stdout_string(&output).trim(), "Disabled sentinel 'ops-digest'.");

    let mut expected = daemon_sentinels()
        .find_sentinel("ops-digest")
        .expect("seed sentinel exists")
        .clone();
    expected.enabled = false;
    assert_offline_sentinel_file_matches(&sentinels_path, &expected);
}
