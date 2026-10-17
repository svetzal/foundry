//! Real CLI, daemon service and admission scheduler completion regressions.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::blocks::{RouteProjectWorkflow, ValidateProject};
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_engine::event_writer::EventWriter;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::pacing::PacingPaths;
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::task_block::{BlockFuture, BlockKind, TaskBlock, TaskBlockResult};
use foundryd::pacing::PacingScheduler;
use foundryd::proto::foundry_server::FoundryServer;
use foundryd::service::{FoundryService, RuntimeContext, StoreConfig};
use foundryd::trace_store::TraceStore;
use foundryd::work_admission::DurableWorkAdmission;
use foundryd::workflow_tracker::WorkflowTracker;
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

/// Upper bound for work these tests expect to finish: it only turns a hang
/// into a failure. It is generous because the CLI runs as a subprocess, which
/// is slow under coverage instrumentation (tarpaulin's ptrace engine) on a
/// loaded host.
const LIVENESS_BOUND: Duration = Duration::from_secs(60);

struct Summary {
    started: Arc<Notify>,
    release: Arc<Notify>,
}
impl TaskBlock for Summary {
    fn name(&self) -> &'static str {
        "Controlled summary observer"
    }
    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }
    fn sinks_on(&self) -> &[EventType] {
        &[EventType::MaintenanceSummaryRequested]
    }
    fn execute(&self, _: &Event) -> BlockFuture<'_> {
        Box::pin(async move {
            self.started.notify_one();
            self.release.notified().await;
            Ok(TaskBlockResult::success("summary completed", vec![]))
        })
    }
}

struct Daemon {
    addr: String,
    state: tempfile::TempDir,
    started: Arc<Notify>,
    release: Arc<Notify>,
    server: tokio::task::JoinHandle<()>,
    scheduler: tokio::task::JoinHandle<()>,
}
impl Drop for Daemon {
    fn drop(&mut self) {
        self.server.abort();
        self.scheduler.abort();
    }
}

fn git(path: &Path, args: &[&str]) {
    // Isolate the fixture from inherited git configuration, such as an agent
    // session's `remote.origin.pushurl` override, so its local push succeeds.
    let mut command = std::process::Command::new("git");
    command
        .current_dir(path)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS");
    for index in 0..8 {
        command.env_remove(format!("GIT_CONFIG_KEY_{index}"));
        command.env_remove(format!("GIT_CONFIG_VALUE_{index}"));
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn daemon() -> Daemon {
    let state = tempfile::tempdir().unwrap();
    let checkout = state.path().join("project");
    let remote = state.path().join("remote.git");
    std::fs::create_dir(&checkout).unwrap();
    std::fs::create_dir(&remote).unwrap();
    git(&remote, &["init", "--bare"]);
    git(&checkout, &["init", "-b", "main"]);
    git(&checkout, &["config", "user.name", "Test"]);
    git(&checkout, &["config", "user.email", "test@example.invalid"]);
    git(&checkout, &["commit", "--allow-empty", "-m", "initial"]);
    git(&checkout, &["remote", "add", "origin", remote.to_str().unwrap()]);
    git(&checkout, &["push", "-u", "origin", "main"]);
    let registry = Arc::new(RwLock::new(serde_json::from_value::<Registry>(serde_json::json!({
        "version": 2, "projects": [{"name": "sample", "path": checkout, "stack": "rust", "agent": "codex", "repo": "", "branch": "main", "actions": {}}]
    })).unwrap()));
    let ledger = state.path().join("work-items.json");
    let pacing = PacingPaths {
        limits: state.path().join("pacing.json"),
        state: state.path().join("pacing-state.json"),
    };
    let (event_tx, _) = broadcast::channel(256);
    let writer = Arc::new(EventWriter::new(state.path().join("events")));
    let trace_writer = Arc::new(TraceWriter::new(state.path().join("traces").to_str().unwrap()));
    let policy = Arc::new(DurableWorkAdmission::new(
        ledger.clone(),
        registry.clone(),
        pacing.clone(),
        event_tx.clone(),
        writer.clone(),
        trace_writer.clone(),
    ));
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut engine = Engine::new()
        .with_event_writer(writer)
        .with_event_broadcaster(event_tx.clone())
        .with_work_admission(policy);
    engine.register(Box::new(ValidateProject::new(registry.clone())));
    engine.register(Box::new(RouteProjectWorkflow));
    engine.register(Box::new(Summary {
        started: started.clone(),
        release: release.clone(),
    }));
    let wake = engine.admission_wake().unwrap();
    let ctx = RuntimeContext {
        engine: Arc::new(engine),
        trace_store: Arc::new(TraceStore::with_trace_writer(
            Duration::from_secs(60),
            trace_writer.clone(),
        )),
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer,
        event_tx,
        registry,
    };
    let scheduler = tokio::spawn(
        PacingScheduler::new(ctx.clone(), ledger.clone(), pacing.clone(), wake.clone()).run(),
    );
    let service = FoundryService::new(
        ctx,
        StoreConfig {
            work_items_path: ledger,
            events_dir: state.path().join("events"),
            campaigns_path: state.path().join("campaigns.json"),
            registry_path: state.path().join("registry.json"),
            sentinels: Arc::new(RwLock::new(SentinelStore {
                version: 1,
                sentinels: vec![],
            })),
            sentinels_path: state.path().join("sentinels.json"),
            scheduler_reload: Arc::new(Notify::new()),
        },
    )
    .with_pacing(pacing, wake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(FoundryServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    Daemon {
        addr,
        state,
        started,
        release,
        server,
        scheduler,
    }
}

fn run(daemon: &Daemon, project: bool) -> tokio::process::Child {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_foundry"));
    command.arg("--addr").arg(&daemon.addr).arg("run");
    if project {
        command.args(["--project", "sample"]);
    }
    command
        .env("HOME", daemon.state.path())
        .env("XDG_CONFIG_HOME", daemon.state.path().join("config"));
    command
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap()
}

fn events(daemon: &Daemon) -> Vec<Event> {
    foundry_blocks::blocks::triage_core::read_events_jsonl(
        &daemon.state.path().join("events"),
        chrono::Utc::now() - chrono::Duration::days(1),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_without_actions_completes_the_cli_and_records_its_correlated_terminal() {
    let daemon = daemon().await;
    let output = tokio::time::timeout(LIVENESS_BOUND, run(&daemon, true).wait_with_output())
        .await
        .expect("project CLI must complete")
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let logged = events(&daemon);
    let root = logged
        .iter()
        .find(|event| {
            event.event_type == EventType::ProjectRunStarted
                && event.payload.get("admitted_work_item_id").is_none()
        })
        .unwrap();
    let terminals: Vec<_> = logged
        .iter()
        .filter(|event| {
            event.event_type == EventType::ProjectRunCompleted
                && event.payload_str("root_event_id") == Some(&root.id)
        })
        .collect();
    assert_eq!(terminals.len(), 1);
    assert_eq!(terminals[0].trace_id, root.trace_id);
    assert_eq!(terminals[0].source, root.source);
    assert_eq!(terminals[0].causation_id.as_deref(), Some(root.id.as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn system_cli_waits_for_the_final_notification_on_its_separate_summary_trace() {
    let daemon = daemon().await;
    let child = run(&daemon, false);
    let mut output = tokio::spawn(child.wait_with_output());
    tokio::time::timeout(LIVENESS_BOUND, daemon.started.notified())
        .await
        .expect("summary started");
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut output).await.is_err(),
        "CLI must wait for summary completion"
    );
    daemon.release.notify_one();
    let output = tokio::time::timeout(LIVENESS_BOUND, output)
        .await
        .expect("system CLI must complete")
        .unwrap()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let logged = events(&daemon);
    let root = logged
        .iter()
        .find(|event| event.event_type == EventType::MaintenanceCycleStarted)
        .unwrap();
    let terminal = logged
        .iter()
        .find(|event| {
            event.event_type == EventType::MaintenanceSummaryRequested
                && event.payload_bool("service_completed") == Some(true)
        })
        .unwrap();
    assert_eq!(terminal.payload_str("root_event_id"), Some(root.id.as_str()));
    assert_ne!(terminal.trace_id, root.trace_id);
    assert_eq!(terminal.source, root.source);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_queued_project_has_one_failed_terminal_and_a_failing_cli() {
    use foundry_sdk::work_item::WorkItemStore;
    use foundryd::proto::{
        CancelWorkItemRequest, PausePacingRequest, foundry_client::FoundryClient,
    };
    let daemon = daemon().await;
    let mut client = FoundryClient::connect(daemon.addr.clone()).await.unwrap();
    client
        .pause_pacing(PausePacingRequest {
            lanes: vec![],
            operator_origin: "test".into(),
        })
        .await
        .unwrap();
    let child = run(&daemon, true);
    let ledger = daemon.state.path().join("work-items.json");
    let item = tokio::time::timeout(LIVENESS_BOUND, async {
        loop {
            if let Some(item) = WorkItemStore::load(&ledger).unwrap().items.into_iter().next() {
                break item;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("project admitted");
    let root_id = item.pending_root.as_ref().unwrap().id.clone();
    client
        .cancel_work_item(CancelWorkItemRequest {
            id: item.id,
            operator_origin: "test".into(),
        })
        .await
        .unwrap();
    let output = tokio::time::timeout(LIVENESS_BOUND, child.wait_with_output())
        .await
        .expect("cancelled CLI stops")
        .unwrap();
    assert!(!output.status.success());
    let traces = TraceWriter::new(daemon.state.path().join("traces").to_str().unwrap());
    tokio::time::timeout(LIVENESS_BOUND, async {
        while traces.read(&root_id).is_none() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("service completed cancelled run");
    let logged = events(&daemon);
    let terminals: Vec<_> = logged
        .iter()
        .filter(|event| event.event_type == EventType::ProjectRunCompleted)
        .collect();
    assert_eq!(terminals.len(), 1, "service must not duplicate the typed cancellation");
    assert_eq!(terminals[0].payload_bool("success"), Some(false));
}
