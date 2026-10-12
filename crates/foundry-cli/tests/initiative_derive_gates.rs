//! A real CLI carries declared integration gates through the typed daemon RPC.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::CampaignStore;
use foundry_sdk::initiative::DoneEvidence;
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundryd::proto::foundry_server::FoundryServer;
use foundryd::service::{FoundryService, RuntimeContext, StoreConfig};
use foundryd::trace_store::TraceStore;
use foundryd::workflow_tracker::WorkflowTracker;
use serde_json::json;
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;

fn git(root: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success()
    );
}

fn intent_repository(root: &Path) {
    std::fs::create_dir(root).unwrap();
    git(root, &["init", "--quiet", "--initial-branch=main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    std::fs::create_dir(root.join(".et")).unwrap();
    std::fs::write(root.join(".et/projection.json"), json!({"goals":[{"id":"outcome","description":"A usable outcome","success_criteria":"The owner can use the contract","state":"create"}],"interactions":[{"id":"contract","description":"The contract works","state":"create","goals":["outcome"]}],"journeys":[]}).to_string()).unwrap();
    git(root, &["add", ".et/projection.json"]);
    git(root, &["commit", "--quiet", "-m", "Seed synthetic intent"]);
}

async fn daemon(home: &Path, source: &Path) -> (String, tokio::task::JoinHandle<()>) {
    let (event_tx, _receiver) = broadcast::channel(64);
    let trace_writer = Arc::new(TraceWriter::new(home.join("traces").to_str().unwrap()));
    let registry: Registry = serde_json::from_value(json!({"version":2,"projects":[{"name":"sample","path":source,"stack":"rust","agent":"codex","repo":"local/sample","branch":"main"}]})).unwrap();
    let ctx = RuntimeContext {
        engine: Arc::new(Engine::new()),
        trace_store: Arc::new(TraceStore::with_trace_writer(
            Duration::from_secs(60),
            trace_writer.clone(),
        )),
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer,
        event_tx,
        registry: Arc::new(RwLock::new(registry)),
    };
    let service = FoundryService::new(
        ctx,
        StoreConfig {
            campaigns_path: home.join("campaigns.json"),
            work_items_path: home.join("work-items.json"),
            events_dir: home.join("events"),
            registry_path: home.join("registry.json"),
            sentinels: Arc::new(RwLock::new(SentinelStore::default_seed())),
            sentinels_path: home.join("sentinels.json"),
            scheduler_reload: Arc::new(Notify::new()),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(FoundryServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    (address, server)
}

#[tokio::test]
async fn cli_derives_with_gate_file_preserving_tracker_reviews_and_declared_host_artifacts() {
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join("source");
    intent_repository(&source);
    let file = home.path().join("gates.json");
    std::fs::write(&file, r#"[{"kind":"gate","command":"python3 sample/integrate.py","host":"build-host","required":true,"artifacts":["sample/contract.json"]}]"#).unwrap();
    let (address, server) = daemon(home.path(), &source).await;
    let output = derive_cli(&address, &source, &file).await;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Derived initiative 'outcome'"));
    let store = CampaignStore::load(&home.path().join("campaigns.json")).unwrap();
    let initiative = store.find_initiative("outcome").unwrap();
    assert!(initiative.done_evidence.iter().any(|evidence| matches!(evidence, DoneEvidence::Review { statement } if statement == "The owner can use the contract")));
    assert!(initiative.done_evidence.iter().any(|evidence| matches!(evidence, DoneEvidence::Review { statement } if statement == "The contract works")));
    let gate = initiative
        .done_evidence
        .iter()
        .find(|evidence| matches!(evidence, DoneEvidence::Gate { .. }))
        .unwrap();
    assert!(
        matches!(gate, DoneEvidence::Gate { command, host, required:true, artifacts } if command == "python3 sample/integrate.py" && host.as_deref() == Some("build-host") && artifacts == &["sample/contract.json"])
    );
    assert!(!initiative.steering_enabled);
    let before = std::fs::read(home.path().join("campaigns.json")).unwrap();
    std::fs::write(&file, r#"[{"kind":"review","statement":"Bypass Tracker"}]"#).unwrap();
    let rejected = derive_cli(&address, &source, &file).await;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("only gate declarations"));
    assert_eq!(std::fs::read(home.path().join("campaigns.json")).unwrap(), before);
    server.abort();
}

async fn derive_cli(address: &str, source: &Path, file: &Path) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_foundry"))
        .args([
            "--addr",
            address,
            "initiative",
            "derive",
            "--from",
            "et:goal:outcome",
            "--projects",
            "sample",
            "--authorized-by",
            "Test",
            "--integration-gates",
            file.to_str().unwrap(),
        ])
        .current_dir(source)
        .output()
        .await
        .unwrap()
}
