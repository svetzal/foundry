//! Integration tests for `foundry campaign add` probe output and
//! `foundry campaign edit` via the gRPC online path. The daemon runs done
//! gates through a fake shell, so no process starts.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_cli::campaign_commands::{add_and_render, edit_and_render};
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{CampaignStatus, CampaignStore};
use foundry_sdk::gateway::fakes::FakeShellGateway;
use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};
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

struct Daemon {
    addr: String,
    campaigns: NamedTempFile,
    _traces: TempDir,
}

async fn start(repo_root: &std::path::Path, shell: Arc<FakeShellGateway>) -> Daemon {
    let campaigns = NamedTempFile::new().expect("tempfile for campaigns");
    let (event_tx, _rx) = broadcast::channel(64);
    let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));
    let traces = tempfile::tempdir().expect("tempdir for traces");
    let trace_writer =
        Arc::new(TraceWriter::new(traces.path().to_str().expect("trace dir must be UTF-8")));
    let trace_store = Arc::new(TraceStore::with_trace_writer(
        Duration::from_secs(60),
        Arc::clone(&trace_writer),
    ));
    let registry = Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: "test-project".to_string(),
            path: repo_root.display().to_string(),
            stack: Stack::Rust,
            agent: "codex".to_string(),
            repo: "test/project".to_string(),
            branch: "main".to_string(),
            skip: None,
            actions: ActionFlags::default(),
            install: None,
            installs_skill: None,
            notes: None,
            timeout_secs: None,
            audit_exceptions: vec![],
            update_policy: None,
        }],
    };
    let ctx = RuntimeContext {
        engine,
        trace_store,
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer,
        event_tx,
        registry: Arc::new(RwLock::new(registry)),
    };
    let stores = StoreConfig {
        work_items_path: std::path::PathBuf::new(),
        events_dir: std::path::PathBuf::new(),
        campaigns_path: campaigns.path().to_path_buf(),
        registry_path: NamedTempFile::new().expect("registry").path().to_path_buf(),
        sentinels: Arc::new(RwLock::new(SentinelStore::default_seed())),
        sentinels_path: NamedTempFile::new().expect("sentinels").path().to_path_buf(),
        scheduler_reload: Arc::new(Notify::new()),
    };
    let service = FoundryService::new(ctx, stores).with_shell(shell);

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
    Daemon {
        addr: format!("http://127.0.0.1:{port}"),
        campaigns,
        _traces: traces,
    }
}

/// The author sees the lexical gate's false positive in the add output, then
/// corrects the gate with edit and sees the new probe, all before any cycle.
#[tokio::test]
async fn add_prints_trunk_gate_probes_and_edit_prints_the_new_ones() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let work = tempfile::tempdir().expect("work tempdir");
    let shell = FakeShellGateway::sequence(vec![
        foundry_blocks::shell::CommandResult {
            stdout: "README.md:12: no real network in tests".to_string(),
            stderr: String::new(),
            exit_code: 1,
            success: false,
        },
        foundry_blocks::shell::CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        },
    ]);
    let daemon = start(repo_root.path(), shell.clone()).await;

    let definition = work.path().join("campaign.json");
    std::fs::write(
        &definition,
        serde_json::json!({
            "name": "no-network",
            "project": "test-project",
            "mission": "Stub the network in tests",
            "authorized_by": "owner",
            "done_evidence": [{"kind": "gate", "command": "! rg -n network .", "required": true}]
        })
        .to_string(),
    )
    .unwrap();
    let unused_store = work.path().join("client-campaigns.json");
    let unused_registry = work.path().join("client-registry.json");

    let added = add_and_render(&unused_store, &unused_registry, &daemon.addr, false, &definition)
        .await
        .expect("add succeeds even though the gate fails");
    assert!(added.contains("Status:     staged"), "{added}");
    assert!(added.contains("Done gates on trunk"), "{added}");
    assert!(
        added.contains("  campaign_done_1 [required] FAIL (exit 1): ! rg -n network ."),
        "{added}"
    );
    assert!(added.contains("      README.md:12: no real network in tests"), "{added}");

    let replacement = work.path().join("gates.json");
    std::fs::write(
        &replacement,
        r#"[{"kind":"gate","command":"! rg -n 'TcpListener::bind' src","required":true}]"#,
    )
    .unwrap();
    let edited = edit_and_render(
        &unused_store,
        &daemon.addr,
        false,
        "no-network",
        &replacement,
        "the gate matched README text",
    )
    .await
    .expect("edit succeeds");
    assert!(
        edited.contains("Done evidence amended: the gate matched README text"),
        "{edited}"
    );
    assert!(
        edited.contains("    previous:    gate [required]: ! rg -n network ."),
        "{edited}"
    );
    assert!(
        edited.contains(
            "  campaign_done_1 [required] PASS (exit 0): ! rg -n 'TcpListener::bind' src"
        ),
        "{edited}"
    );

    assert!(!unused_store.exists(), "the online path never writes a client-side store");
    let stored = CampaignStore::load(daemon.campaigns.path()).unwrap();
    assert_eq!(stored.find("no-network").unwrap().status, CampaignStatus::Staged);
    assert_eq!(shell.invocations().len(), 2);
}

/// A refused edit surfaces the daemon's reason.
#[tokio::test]
async fn edit_of_an_unknown_campaign_reports_not_found() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let work = tempfile::tempdir().expect("work tempdir");
    let daemon = start(repo_root.path(), FakeShellGateway::success()).await;
    let replacement = work.path().join("gates.json");
    std::fs::write(&replacement, r#"[{"kind":"review","statement":"done"}]"#).unwrap();

    let error = edit_and_render(
        &work.path().join("client-campaigns.json"),
        &daemon.addr,
        false,
        "missing",
        &replacement,
        "why",
    )
    .await
    .expect_err("unknown campaign");
    assert!(error.to_string().contains("campaign 'missing' not found"), "{error}");
}
