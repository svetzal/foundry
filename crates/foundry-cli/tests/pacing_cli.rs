//! Integration tests for `foundry pacing` and the pacing owner controls on
//! `foundry queue` (`hold`, `release`).
//!
//! These stand up a real `FoundryService` over a temporary ledger and
//! temporary pacing files, so the online path is exercised end to end through
//! `GetPacing`, `PausePacing`, `ResumePacing`, `HoldWorkItem` and
//! `ReleaseWorkItem`, and run the real `foundry` binary against it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::Utc;
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_engine::event_writer::EventWriter;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::pacing::{PacingPaths, PauseState};
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{
    WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
};
use foundryd::{
    proto::foundry_server::FoundryServer,
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::TempDir;
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

/// A port nothing listens on, so the online path must fail rather than connect.
const DUMMY_ADDR: &str = "http://127.0.0.1:9";

fn root(project: &str) -> Event {
    Event::new(
        EventType::ExecutionRequested,
        project.to_string(),
        Throttle::Full,
        serde_json::json!({"project": project, "workflow": "task", "prompt": "x"}),
    )
}

fn queued(id: &str, project: &str, reason: &str) -> WorkItem {
    let mut item = WorkItem::queued(
        WorkItemSpec {
            project: project.to_string(),
            objective: format!("objective for {id}"),
            kind: WorkItemKind::Task,
            lane: WorkLane::Interactive,
            origin: "foundry task".to_string(),
            trace_id: Some("a".repeat(32)),
        },
        root(project),
        Utc::now(),
    );
    item.id = id.to_string();
    item.reason = reason.to_string();
    item
}

fn running(id: &str, project: &str) -> WorkItem {
    let mut item = queued(id, project, "running");
    item.start(Utc::now());
    item
}

/// A daemon over `ledger`, keeping its state under `state_dir`.
struct Daemon {
    addr: String,
    ledger: PathBuf,
    pacing: PacingPaths,
    _state: TempDir,
}

async fn daemon_over(items: Vec<WorkItem>) -> Daemon {
    let state = tempfile::tempdir().expect("tempdir for daemon state");
    let ledger = state.path().join("work-items.json");
    WorkItemStore { version: 1, items }.save(&ledger).expect("seed ledger");
    let pacing = PacingPaths {
        limits: state.path().join("pacing.json"),
        state: state.path().join("pacing-state.json"),
    };

    let (event_tx, _rx) = broadcast::channel(64);
    let engine = Arc::new(
        Engine::new()
            .with_event_broadcaster(event_tx.clone())
            .with_event_writer(Arc::new(EventWriter::new(state.path().join("events")))),
    );
    let trace_writer = Arc::new(TraceWriter::new(
        state.path().join("traces").to_str().expect("trace dir must be UTF-8"),
    ));
    let ctx = RuntimeContext {
        engine,
        trace_store: Arc::new(TraceStore::with_trace_writer(
            Duration::from_secs(60),
            Arc::clone(&trace_writer),
        )),
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer,
        event_tx,
        registry: Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        })),
    };
    let stores = StoreConfig {
        work_items_path: ledger.clone(),
        events_dir: state.path().join("events"),
        campaigns_path: PathBuf::new(),
        registry_path: PathBuf::new(),
        sentinels: Arc::new(RwLock::new(SentinelStore {
            version: 1,
            sentinels: vec![],
        })),
        sentinels_path: PathBuf::new(),
        scheduler_reload: Arc::new(Notify::new()),
    };
    let service =
        FoundryService::new(ctx, stores).with_pacing(pacing.clone(), Arc::new(Notify::new()));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        Server::builder()
            .add_service(FoundryServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("gRPC server error");
    });
    tokio::task::yield_now().await;

    Daemon {
        addr: format!("http://127.0.0.1:{port}"),
        ledger,
        pacing,
        _state: state,
    }
}

fn run_foundry(home: &Path, daemon: &Daemon, addr: &str, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_foundry"))
        .arg("--addr")
        .arg(addr)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("FOUNDRY_WORK_ITEMS_PATH", &daemon.ledger)
        .env("FOUNDRY_PACING_PATH", &daemon.pacing.limits)
        .env("FOUNDRY_PACING_STATE_PATH", &daemon.pacing.state)
        .env("FOUNDRY_REGISTRY_PATH", home.join("absent-registry.json"))
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

fn item_state(ledger: &Path, id: &str) -> WorkItemState {
    WorkItemStore::load(ledger).unwrap().find(id).unwrap().state
}

// ── pacing show ───────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn pacing_show_prints_limits_running_waiting_and_paused_lanes_online_and_offline() {
    let daemon = daemon_over(vec![
        running("wi_run", "alpha"),
        queued("wi_wait", "alpha", "repository busy: wi_run"),
    ])
    .await;
    std::fs::write(&daemon.pacing.limits, r#"{"max_running": 3}"#).unwrap();
    let mut pauses = PauseState::default();
    pauses.pause(&[WorkLane::Campaign]);
    pauses.save(&daemon.pacing.state).unwrap();
    let home = tempfile::tempdir().expect("client home");

    for args in [vec!["pacing", "show"], vec!["pacing", "show", "--offline"]] {
        let output = run_foundry(home.path(), &daemon, &daemon.addr, &args);
        assert_command_succeeded(&output);
        let out = stdout_string(&output);
        assert!(out.contains("Limits: 1 of 3 running on this host"), "{args:?}:\n{out}");
        assert!(out.contains("Paused lanes: campaign"), "{args:?}:\n{out}");
        assert!(out.contains("wi_run") && out.contains("wi_wait"), "{args:?}:\n{out}");
        assert!(out.contains("repository busy: wi_run"), "{args:?}:\n{out}");
    }

    let output = run_foundry(home.path(), &daemon, &daemon.addr, &["pacing", "show", "--json"]);
    assert_command_succeeded(&output);
    let parsed: serde_json::Value = serde_json::from_str(&stdout_string(&output)).unwrap();
    assert_eq!(parsed["max_running"], 3);
    assert_eq!(parsed["running"], 1);
    assert_eq!(parsed["paused_lanes"], serde_json::json!(["campaign"]));
    assert_eq!(parsed["running_items"][0]["id"], "wi_run");
    assert_eq!(parsed["waiting_items"][0]["reason"], "repository busy: wi_run");
}

#[test]
fn pacing_show_with_no_daemon_fails_and_names_the_offline_form() {
    let state = tempfile::tempdir().unwrap();
    let daemon = Daemon {
        addr: DUMMY_ADDR.to_string(),
        ledger: state.path().join("work-items.json"),
        pacing: PacingPaths {
            limits: state.path().join("pacing.json"),
            state: state.path().join("pacing-state.json"),
        },
        _state: state,
    };
    let home = tempfile::tempdir().unwrap();
    let output = run_foundry(home.path(), &daemon, DUMMY_ADDR, &["pacing", "show"]);
    assert!(!output.status.success());
    assert!(stderr_string(&output).contains("foundry pacing show --offline"));
    for mutation in [
        vec!["pacing", "pause", "--offline"],
        vec!["pacing", "resume", "--offline"],
        vec!["pacing", "drain", "--offline"],
        vec!["queue", "hold", "wi_x", "--offline"],
        vec!["queue", "release", "wi_x", "--offline"],
    ] {
        let output = run_foundry(home.path(), &daemon, DUMMY_ADDR, &mutation);
        assert!(!output.status.success(), "{mutation:?} must refuse --offline");
        assert!(stderr_string(&output).contains("--offline is not supported"), "{mutation:?}");
    }
}

// ── pause and resume ──────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn pacing_pause_and_resume_change_the_persisted_lanes() {
    let daemon = daemon_over(vec![]).await;
    let home = tempfile::tempdir().expect("client home");

    let output =
        run_foundry(home.path(), &daemon, &daemon.addr, &["pacing", "pause", "--lane", "campaign"]);
    assert_command_succeeded(&output);
    assert_eq!(stdout_string(&output), "paused; paused lanes: campaign\n");
    assert_eq!(PauseState::load(&daemon.pacing.state).unwrap().paused, vec![WorkLane::Campaign]);

    let output = run_foundry(home.path(), &daemon, &daemon.addr, &["pacing", "pause"]);
    assert_command_succeeded(&output);
    assert_eq!(
        stdout_string(&output),
        "paused; paused lanes: interactive, campaign, maintenance\n"
    );

    let output = run_foundry(
        home.path(),
        &daemon,
        &daemon.addr,
        &["pacing", "resume", "--lane", "interactive"],
    );
    assert_command_succeeded(&output);
    assert_eq!(stdout_string(&output), "resumed; paused lanes: campaign, maintenance\n");

    let output = run_foundry(home.path(), &daemon, &daemon.addr, &["pacing", "resume"]);
    assert_command_succeeded(&output);
    assert_eq!(stdout_string(&output), "resumed; paused lanes: none\n");
    assert!(PauseState::load(&daemon.pacing.state).unwrap().paused.is_empty());

    let output =
        run_foundry(home.path(), &daemon, &daemon.addr, &["pacing", "pause", "--lane", "nightly"]);
    assert!(!output.status.success());
    assert!(stderr_string(&output).contains("--lane takes"));
}

// ── drain ─────────────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn pacing_drain_pauses_every_lane_and_exits_zero_when_nothing_runs() {
    let daemon = daemon_over(vec![queued("wi_wait", "alpha", "ready")]).await;
    let home = tempfile::tempdir().expect("client home");

    let output = run_foundry(home.path(), &daemon, &daemon.addr, &["pacing", "drain"]);
    assert_command_succeeded(&output);
    let out = stdout_string(&output);
    assert!(out.contains("paused every lane; paused lanes: interactive, campaign, maintenance"));
    assert!(out.contains("idle: nothing is running"), "got:\n{out}");
    assert_eq!(
        PauseState::load(&daemon.pacing.state).unwrap().paused,
        WorkLane::ALL.to_vec(),
        "every lane stays paused: the quiet point holds until a resume"
    );
    assert_eq!(
        item_state(&daemon.ledger, "wi_wait"),
        WorkItemState::Queued,
        "waiting work is untouched"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pacing_drain_with_a_timeout_exits_non_zero_naming_what_still_runs() {
    let daemon = daemon_over(vec![running("wi_run", "alpha")]).await;
    let home = tempfile::tempdir().expect("client home");

    let output =
        run_foundry(home.path(), &daemon, &daemon.addr, &["pacing", "drain", "--timeout", "1s"]);
    assert!(!output.status.success(), "a running item past the timeout is a non-zero exit");
    let out = stdout_string(&output);
    assert!(out.contains("waiting for 1 running item(s) to settle"), "got:\n{out}");
    assert!(out.contains("still running after the timeout:"), "got:\n{out}");
    assert!(out.contains("wi_run  alpha  task"), "the item is named:\n{out}");
    assert_eq!(item_state(&daemon.ledger, "wi_run"), WorkItemState::Running, "untouched");
}

#[tokio::test(flavor = "multi_thread")]
async fn pacing_drain_prints_each_item_as_it_settles_then_exits_zero() {
    let daemon = daemon_over(vec![running("wi_run", "alpha")]).await;
    let home = tempfile::tempdir().expect("client home");
    let ledger = daemon.ledger.clone();
    let addr = daemon.addr.clone();

    // Settle the running item through the daemon while drain waits: an owner
    // close is the one settlement an operator can make from outside a run.
    let home_path = home.path().to_path_buf();
    let settle = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        let mut store = WorkItemStore::load(&ledger).unwrap();
        store.find_mut("wi_run").unwrap().settle_failed("stopped by hand", Utc::now());
        store.save(&ledger).unwrap();
        Command::new(env!("CARGO_BIN_EXE_foundry"))
            .args([
                "--addr", &addr, "queue", "close", "wi_run", "--reason", "drained",
            ])
            .env("HOME", &home_path)
            .env("XDG_CONFIG_HOME", home_path.join(".config"))
            .output()
            .expect("run foundry binary")
    });

    let output =
        run_foundry(home.path(), &daemon, &daemon.addr, &["pacing", "drain", "--timeout", "30s"]);
    let closed = settle.join().unwrap();
    assert_command_succeeded(&closed);
    assert_command_succeeded(&output);
    let out = stdout_string(&output);
    assert!(out.contains("settled wi_run: cancelled — drained"), "got:\n{out}");
    assert!(out.contains("idle: nothing is running"), "got:\n{out}");
}

// ── queue hold and release ────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn queue_hold_and_release_move_an_item_out_of_and_back_into_the_queue() {
    let daemon = daemon_over(vec![queued("wi_q", "alpha", "ready")]).await;
    let home = tempfile::tempdir().expect("client home");

    let output = run_foundry(home.path(), &daemon, &daemon.addr, &["queue", "hold", "wi_q"]);
    assert_command_succeeded(&output);
    assert_eq!(stdout_string(&output), "wi_q: held — held by operator\n");
    assert_eq!(item_state(&daemon.ledger, "wi_q"), WorkItemState::Held);

    let output = run_foundry(home.path(), &daemon, &daemon.addr, &["queue"]);
    assert_command_succeeded(&output);
    let out = stdout_string(&output);
    assert!(out.contains("held by operator"), "the row carries the reason:\n{out}");

    let output = run_foundry(home.path(), &daemon, &daemon.addr, &["queue", "release", "wi_q"]);
    assert_command_succeeded(&output);
    assert_eq!(stdout_string(&output), "wi_q: queued — ready\n");
    assert_eq!(item_state(&daemon.ledger, "wi_q"), WorkItemState::Queued);

    let output = run_foundry(home.path(), &daemon, &daemon.addr, &["queue", "release", "wi_q"]);
    assert!(!output.status.success(), "a queued item is not released");
    assert!(
        stderr_string(&output).contains("cannot be released from queued"),
        "got: {}",
        stderr_string(&output)
    );

    let output = run_foundry(home.path(), &daemon, &daemon.addr, &["queue", "show", "wi_q"]);
    assert_command_succeeded(&output);
    let out = stdout_string(&output);
    assert!(out.contains("work_item_held"), "the hold is in the item's events:\n{out}");
    assert!(out.contains("work_item_released"), "so is the release:\n{out}");
}
