//! Integration tests for daemon-authoritative observability CLI commands.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
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
    proto::foundry_server::FoundryServer,
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::{ActiveWorkflow, WorkflowTracker},
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

const DUMMY_ADDR: &str = "http://127.0.0.1:9";

fn parse_utc(timestamp: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(timestamp)
        .expect("valid timestamp")
        .with_timezone(&Utc)
}

fn make_event(
    event_id: &str,
    event_type: EventType,
    project: &str,
    occurred_at: &str,
    trace_id: &str,
    span_id: &str,
) -> Event {
    let mut event =
        Event::new(event_type, project.to_string(), Throttle::Full, serde_json::json!({}));
    let occurred_at = parse_utc(occurred_at);
    event.id = event_id.to_string();
    event.occurred_at = occurred_at;
    event.recorded_at = occurred_at;
    event.trace_id = Some(trace_id.to_string());
    event.span_id = Some(span_id.to_string());
    event
}

fn make_block(
    block_name: &str,
    trigger_event_id: &str,
    emitted_event_ids: &[&str],
    success: bool,
    duration_ms: u64,
    summary: &str,
    span_id: &str,
    parent_span_id: &str,
) -> BlockExecution {
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
        emitted_payloads: vec![serde_json::json!({"ok": success})],
        audit_artifacts: vec![],
        span_id: Some(span_id.to_string()),
        parent_span_id: Some(parent_span_id.to_string()),
    }
}

fn alpha_trace() -> ProcessResult {
    let root = make_event(
        "evt_alpha_root",
        EventType::ProjectRunStarted,
        "alpha",
        "2026-07-24T12:00:00Z",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "1111111111111111",
    );
    let completed = make_event(
        "evt_alpha_completed",
        EventType::ProjectRunCompleted,
        "alpha",
        "2026-07-24T12:00:05Z",
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "1111111111111111",
    );
    ProcessResult {
        events: vec![root, completed],
        block_executions: vec![make_block(
            "RunAlpha",
            "evt_alpha_root",
            &["evt_alpha_completed"],
            true,
            53,
            "completed alpha",
            "2222222222222222",
            "1111111111111111",
        )],
        total_duration_ms: 53,
    }
}

fn alpha_failed_trace() -> ProcessResult {
    let root = make_event(
        "evt_alpha_failed",
        EventType::ProjectRunStarted,
        "alpha",
        "2026-07-24T09:00:00Z",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        "3333333333333333",
    );
    ProcessResult {
        events: vec![root],
        block_executions: vec![make_block(
            "RunAlphaFailed",
            "evt_alpha_failed",
            &[],
            false,
            19,
            "failed alpha",
            "4444444444444444",
            "3333333333333333",
        )],
        total_duration_ms: 19,
    }
}

fn beta_trace() -> ProcessResult {
    let root = make_event(
        "evt_beta_root",
        EventType::ProjectRunStarted,
        "beta",
        "2026-07-24T10:30:00Z",
        "cccccccccccccccccccccccccccccccc",
        "5555555555555555",
    );
    ProcessResult {
        events: vec![root],
        block_executions: vec![make_block(
            "RunBeta",
            "evt_beta_root",
            &[],
            true,
            31,
            "completed beta",
            "6666666666666666",
            "5555555555555555",
        )],
        total_duration_ms: 31,
    }
}

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
    trace_writer.write("evt_alpha_root", &alpha_trace()).expect("write alpha trace");
    trace_writer
        .write("evt_alpha_failed", &alpha_failed_trace())
        .expect("write alpha failed trace");
    trace_writer.write("evt_beta_root", &beta_trace()).expect("write beta trace");
    trace_store.insert("evt_alpha_root".to_string(), alpha_trace());
    trace_store.insert("evt_alpha_failed".to_string(), alpha_failed_trace());
    trace_store.insert("evt_beta_root".to_string(), beta_trace());

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

    let registry = Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![],
    }));
    let tmp_registry = NamedTempFile::new().expect("tempfile for registry");
    let tmp_campaigns = NamedTempFile::new().expect("tempfile for campaigns");
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
        campaigns_path: tmp_campaigns.path().to_path_buf(),
        registry_path: tmp_registry.path().to_path_buf(),
        sentinels,
        sentinels_path: tmp_sentinels.path().to_path_buf(),
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
    traces_dir: &std::path::Path,
    addr: &str,
    args: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_foundry"))
        .arg("--addr")
        .arg(addr)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("FOUNDRY_TRACES_DIR", traces_dir)
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

fn missing_client_traces_dir(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".foundry/client-traces")
}

fn seed_client_traces_trap(
    home: &std::path::Path,
) -> (std::path::PathBuf, std::path::PathBuf, Vec<u8>) {
    let traces_dir = home.join(".foundry/client-traces");
    let date_dir = traces_dir.join("2026-07-24");
    std::fs::create_dir_all(&date_dir).expect("create trap traces dir");
    let trap_file = date_dir.join("trap.json");
    let bytes = br"trap bytes that must stay untouched".to_vec();
    std::fs::write(&trap_file, &bytes).expect("seed trap trace file");
    (traces_dir, trap_file, bytes)
}

fn assert_client_traces_absent(path: &std::path::Path, context: &str) {
    assert!(
        !path.exists(),
        "{context}: expected client traces path to remain absent at {}",
        path.display()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn online_history_reads_daemon_owned_filtered_history_and_leaves_client_traces_absent() {
    let (service, _daemon_traces) = make_service();
    let addr = start_server(service).await;
    let client_home = tempfile::tempdir().expect("client home");
    let client_traces = missing_client_traces_dir(client_home.path());
    assert_client_traces_absent(&client_traces, "history precondition");

    let today = Utc::now().format("%Y-%m-%d").to_string();
    let output = run_foundry(
        client_home.path(),
        &client_traces,
        &addr,
        &["history", &today, "--project", "alpha"],
    );

    assert_command_succeeded(&output);
    let stdout = stdout_string(&output);
    assert!(stdout.contains(&today));
    assert!(stdout.contains("evt_alpha_root"));
    assert!(stdout.contains("evt_alpha_failed"));
    assert!(stdout.contains("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
    assert!(stdout.contains("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"));
    assert!(stdout.contains("project_run_started"));
    assert!(stdout.contains("alpha"));
    assert!(stdout.contains("53ms"));
    assert!(stdout.contains("19ms"));
    assert!(!stdout.contains("evt_beta_root"));
    assert!(stdout.find("evt_alpha_root").unwrap() < stdout.find("evt_alpha_failed").unwrap());
    assert_client_traces_absent(&client_traces, "online history must not create client traces");
}

#[tokio::test(flavor = "multi_thread")]
async fn online_trace_reads_daemon_owned_trace_and_leaves_client_trap_untouched() {
    let (service, _daemon_traces) = make_service();
    let addr = start_server(service).await;
    let client_home = tempfile::tempdir().expect("client home");
    let (client_traces, trap_file, before) = seed_client_traces_trap(client_home.path());

    let output =
        run_foundry(client_home.path(), &client_traces, &addr, &["trace", "evt_alpha_root"]);

    assert_command_succeeded(&output);
    let stdout = stdout_string(&output);
    assert!(stdout.contains("[span] project_run_started  span=11111111…  parent=∅"));
    assert!(stdout.contains("project_run_started  project=alpha"));
    assert!(stdout.contains("project_run_completed  project=alpha"));
    assert!(stdout.contains("[block: RunAlpha]  block_span=22222222…  duration=53ms"));
    assert!(stdout.contains("Total: 53ms (blocks: 53ms)"));
    assert_eq!(
        std::fs::read(&trap_file).expect("read trap after trace"),
        before,
        "online trace must not touch client trap traces"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn online_status_span_filter_uses_daemon_span_lookup_and_leaves_client_trap_untouched() {
    let (service, _daemon_traces) = make_service();
    let addr = start_server(service).await;
    let client_home = tempfile::tempdir().expect("client home");
    let (client_traces, trap_file, before) = seed_client_traces_trap(client_home.path());

    let output = run_foundry(
        client_home.path(),
        &client_traces,
        &addr,
        &["status", "--span", "1111111111111111"],
    );

    assert_command_succeeded(&output);
    let stdout = stdout_string(&output);
    assert!(stdout.contains("wf_alpha [project_run_started] alpha"));
    assert!(!stdout.contains("wf_beta"));
    assert!(!stdout.contains("No active workflows"));
    assert_eq!(
        std::fs::read(&trap_file).expect("read trap after status"),
        before,
        "online status --span must not touch client trap traces"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_online_observability_commands_leave_client_traces_unchanged() {
    let client_home = tempfile::tempdir().expect("client home");
    let (client_traces, trap_file, before) = seed_client_traces_trap(client_home.path());
    let today = Utc::now().format("%Y-%m-%d").to_string();

    let history = run_foundry(
        client_home.path(),
        &client_traces,
        DUMMY_ADDR,
        &["history", &today, "--project", "alpha"],
    );
    assert!(!history.status.success(), "history should fail when daemon is unreachable");
    assert_eq!(
        stderr_string(&history).trim(),
        format!(
            "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry --offline history {today} --project alpha`"
        )
    );

    let status = run_foundry(client_home.path(), &client_traces, DUMMY_ADDR, &["status"]);
    assert!(!status.status.success(), "status should fail when daemon is unreachable");
    assert_eq!(
        stderr_string(&status).trim(),
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon"
    );

    let trace =
        run_foundry(client_home.path(), &client_traces, DUMMY_ADDR, &["trace", "evt_alpha_root"]);
    assert!(!trace.status.success(), "trace should fail when daemon is unreachable");
    assert_eq!(
        stderr_string(&trace).trim(),
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon"
    );

    assert_eq!(
        std::fs::read(&trap_file).expect("read trap after unreachable commands"),
        before,
        "unreachable online observability commands must not mutate client trap traces"
    );
}
