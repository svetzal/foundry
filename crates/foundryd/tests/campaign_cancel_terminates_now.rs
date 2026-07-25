//! Integration tests for the `--now` half of `foundry campaign cancel`.
//!
//! These are the load-bearing proofs for the cancellation design. Everything
//! else about the feature is a status transition and an event; this file is the
//! evidence that aborting the workflow task actually does the two things the
//! design claims it does.
//!
//! Evidence gates verified by this file:
//!
//! 1. **The agent process really dies.** A block spawns a real subprocess
//!    through the same `ProcessShellGateway` the agent runner uses, and after
//!    `CancelCampaign { terminate_now: true }` that process is gone. The chain
//!    under test is: abort the task → drop its future → drop the
//!    `tokio::process::Child` it owns → `kill_on_drop` sends the kill.
//!
//! 2. **The aborted task's store lock is released before `cancel` needs it.**
//!    `execute_campaign_advance` holds the campaign store's advisory file lock
//!    across the whole formation agent call, and `cancel` takes that same lock
//!    immediately after aborting. `WorkflowTracker::abort_campaign` awaits the
//!    aborted task's join handle for exactly this reason. Remove that await and
//!    this test hangs rather than failing — which is the regression it exists
//!    to catch.
//!
//! Deliberately **not** asserted: that grandchildren die. `kill_on_drop` kills
//! only the direct child, so tool subprocesses spawned by a real `claude` or
//! `codex` agent are reparented and survive. Fixing that needs process groups
//! and `killpg`, which the workspace's `unsafe_code = "deny"` rules out. The
//! limitation is documented in the CLI help and the campaigns guide.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{
    Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundryd::{
    proto::{
        CancelCampaignRequest, EmitRequest, foundry_client::FoundryClient,
        foundry_server::FoundryServer,
    },
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::TempDir;
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

const CAMPAIGN: &str = "long-runner";

// ── Blocks under test ────────────────────────────────────────────────────────

/// Spawns a real, long-lived subprocess and records its pid.
///
/// Goes through `ProcessShellGateway` — the same gateway the agent runner uses,
/// and therefore the same `kill_on_drop` behaviour — so this exercises the real
/// ownership chain rather than a stand-in for it.
struct SpawnLongProcess {
    pidfile: PathBuf,
}

impl TaskBlock for SpawnLongProcess {
    fn name(&self) -> &'static str {
        "Spawn Long Process"
    }
    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }
    fn sinks_on(&self) -> &[EventType] {
        &[EventType::GreetingRequested]
    }

    fn execute(
        &self,
        _trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let script = format!("echo $$ > {}; sleep 300", self.pidfile.display());
        Box::pin(async move {
            let shell = foundry_blocks::gateway::ProcessShellGateway;
            foundry_blocks::gateway::ShellGateway::run(
                &shell,
                std::path::Path::new("/tmp"),
                "sh",
                &["-c", &script],
                None,
                None,
            )
            .await?;
            Ok(TaskBlockResult::success("slept", vec![]))
        })
    }
}

/// Takes the campaign store's exclusive advisory lock and holds it.
///
/// This is what `execute_campaign_advance` does for the duration of a formation
/// agent call, reproduced here so the cancellation path meets the same
/// contention it meets in production.
struct HoldStoreLock {
    store_path: PathBuf,
    holding: Arc<Notify>,
}

impl TaskBlock for HoldStoreLock {
    fn name(&self) -> &'static str {
        "Hold Store Lock"
    }
    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }
    fn sinks_on(&self) -> &[EventType] {
        &[EventType::GreetingRequested]
    }

    fn execute(
        &self,
        _trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let path = self.store_path.clone();
        let holding = Arc::clone(&self.holding);
        Box::pin(async move {
            let _guard = CampaignStore::lock_exclusive(&path)?;
            holding.notify_waiters();
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(TaskBlockResult::success("released", vec![]))
        })
    }
}

// ── Test harness ─────────────────────────────────────────────────────────────

struct Harness {
    service: FoundryService,
    /// Held so the temporary directory outlives the test.
    _tmp: TempDir,
}

fn make_harness(block: Box<dyn TaskBlock>, store_path: &std::path::Path) -> Harness {
    let (event_tx, _rx) = broadcast::channel(64);
    let mut engine = Engine::new().with_event_broadcaster(event_tx.clone());
    engine.register(block);
    let engine = Arc::new(engine);

    let tmp = tempfile::tempdir().expect("tempdir");
    let trace_writer =
        Arc::new(TraceWriter::new(tmp.path().to_str().expect("trace dir must be UTF-8")));
    let trace_store = Arc::new(TraceStore::with_trace_writer(
        Duration::from_secs(60),
        Arc::clone(&trace_writer),
    ));
    let registry = Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![],
    }));

    let ctx = RuntimeContext {
        engine,
        trace_store,
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer,
        event_tx,
        registry,
    };
    let stores = StoreConfig {
        campaigns_path: store_path.to_path_buf(),
        registry_path: tmp.path().join("registry.json"),
        sentinels: Arc::new(RwLock::new(SentinelStore::default_seed())),
        sentinels_path: tmp.path().join("sentinels.json"),
        scheduler_reload: Arc::new(Notify::new()),
    };

    Harness {
        service: FoundryService::new(ctx, stores),
        _tmp: tmp,
    }
}

fn seed_active_campaign(path: &std::path::Path) {
    CampaignStore {
        version: 1,
        campaigns: vec![Campaign {
            name: CAMPAIGN.to_string(),
            project: "test-project".to_string(),
            mission: "Run for a long time".to_string(),
            intent_refs: vec![],
            context_paths: vec![],
            done_evidence: vec![DoneEvidence::Review {
                statement: "done".to_string(),
            }],
            budget: CampaignBudget { max_cycles: 10 },
            escalation: vec![],
            status: CampaignStatus::Active,
            cycles_completed: 1,
            cycles_landed: 0,
            authorized_by: Some("owner".to_string()),
            agent_provider: None,
            last_run_event_id: None,
            owner_decisions: vec![],
            pending_run_result: None,
            objective_history: vec![],
        }],
    }
    .save(path)
    .expect("seed campaign store");
}

/// A root event naming the campaign, emitted through the public `Emit` RPC so
/// the workflow is spawned exactly the way a real one is — which is what makes
/// `ActiveWorkflow.campaign` get populated and `abort_campaign` able to find it.
fn campaign_root() -> EmitRequest {
    EmitRequest {
        event_type: EventType::GreetingRequested.to_string(),
        project: "test-project".to_string(),
        throttle: 0,
        payload_json: serde_json::json!({ "campaign": CAMPAIGN }).to_string(),
        trace_id: String::new(),
        span_id: String::new(),
        parent_span_id: String::new(),
    }
}

async fn start_server(service: FoundryService) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        Server::builder()
            .add_service(FoundryServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("gRPC server error");
    });
    tokio::task::yield_now().await;
    format!("http://127.0.0.1:{port}")
}

fn cancel_now(name: &str) -> CancelCampaignRequest {
    CancelCampaignRequest {
        name: name.to_string(),
        reason: "stopping immediately".to_string(),
        terminate_now: true,
        discard_work: false,
    }
}

/// Whether the process is alive, via the `kill` binary.
///
/// `unsafe_code = "deny"` rules out `libc::kill`, and `libc` is not a
/// dependency, so shelling out is the available route.
fn is_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .output()
        .is_ok_and(|out| out.status.success())
}

async fn wait_for<F: Fn() -> bool>(label: &str, timeout: Duration, condition: F) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!("timed out waiting for {label}");
    false
}

// ── Proof 1: the agent process dies ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn cancel_now_kills_the_running_agent_process() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store_path = tmp.path().join("campaigns.json");
    let pidfile = tmp.path().join("agent.pid");
    seed_active_campaign(&store_path);

    let harness = make_harness(
        Box::new(SpawnLongProcess {
            pidfile: pidfile.clone(),
        }),
        &store_path,
    );
    let addr = start_server(harness.service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client.emit(campaign_root()).await.expect("emit the campaign root");

    assert!(
        wait_for("the subprocess to start", Duration::from_secs(10), || pidfile.exists()).await,
        "the block never spawned its subprocess"
    );
    let pid = std::fs::read_to_string(&pidfile).expect("read pidfile").trim().to_string();
    assert!(is_alive(&pid), "precondition: the subprocess must be running, pid {pid}");

    client
        .cancel_campaign(cancel_now(CAMPAIGN))
        .await
        .expect("cancel --now must succeed");

    assert!(
        wait_for("the subprocess to die", Duration::from_secs(10), || !is_alive(&pid)).await,
        "pid {pid} survived cancellation; aborting the task did not drop the Child, so \
         kill_on_drop never fired"
    );

    let store = CampaignStore::load(&store_path).expect("load store");
    assert_eq!(store.find(CAMPAIGN).expect("campaign").status, CampaignStatus::Cancelled);
}

// ── Proof 2: the aborted task's store lock is released ───────────────────────

/// The deadlock guard.
///
/// If `abort_campaign` stopped awaiting the aborted task's join handle, the
/// task's `CampaignStoreGuard` would not reliably have dropped by the time
/// `cancel` tries to take the same lock — and `cancel` would block on a lock
/// nobody is releasing. This test would then hang, not fail, so the outer
/// timeout is the real assertion.
#[tokio::test(flavor = "multi_thread")]
async fn cancel_now_reclaims_the_store_lock_held_by_the_aborted_task() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let store_path = tmp.path().join("campaigns.json");
    seed_active_campaign(&store_path);

    let holding = Arc::new(Notify::new());
    let harness = make_harness(
        Box::new(HoldStoreLock {
            store_path: store_path.clone(),
            holding: Arc::clone(&holding),
        }),
        &store_path,
    );
    let addr = start_server(harness.service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect");

    let waiter = holding.notified();
    client.emit(campaign_root()).await.expect("emit the campaign root");
    tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .expect("the block never acquired the store lock");

    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client.cancel_campaign(cancel_now(CAMPAIGN)),
    )
    .await
    .expect(
        "cancel blocked on the store lock held by the aborted task — abort_campaign returned \
         before that task had unwound and released it",
    )
    .expect("cancel --now must succeed");

    assert_eq!(response.into_inner().campaign.expect("detail").status, "cancelled");
    let store = CampaignStore::load(&store_path).expect("load store");
    assert_eq!(store.find(CAMPAIGN).expect("campaign").status, CampaignStatus::Cancelled);
}
