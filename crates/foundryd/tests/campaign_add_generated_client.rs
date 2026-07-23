//! Integration tests for `AddCampaign` through the generated tonic client.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{CampaignStatus, CampaignStore};
use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};
use foundry_sdk::sentinel::SentinelStore;
use foundryd::{
    proto::{AddCampaignRequest, foundry_client::FoundryClient, foundry_server::FoundryServer},
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic::transport::Server;

fn registry_with_project(repo_root: &std::path::Path) -> Registry {
    Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: "daemon-project".to_string(),
            path: repo_root.display().to_string(),
            stack: Stack::Rust,
            agent: "codex".to_string(),
            repo: "daemon/project".to_string(),
            branch: "main".to_string(),
            skip: None,
            actions: ActionFlags::default(),
            install: None,
            installs_skill: None,
            notes: None,
            timeout_secs: None,
            audit_exceptions: vec![],
        }],
    }
}

fn make_service_with_campaigns_path(
    campaigns_path: std::path::PathBuf,
    registry: Registry,
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
    let registry = Arc::new(RwLock::new(registry));
    let tmp_registry = NamedTempFile::new().expect("tempfile for registry");
    let registry_path = tmp_registry.path().to_path_buf();
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

fn make_service(repo_root: &std::path::Path) -> (FoundryService, NamedTempFile, TempDir) {
    let tmp_campaigns = NamedTempFile::new().expect("tempfile for campaigns");
    let (service, tmp_traces) = make_service_with_campaigns_path(
        tmp_campaigns.path().to_path_buf(),
        registry_with_project(repo_root),
    );
    (service, tmp_campaigns, tmp_traces)
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

fn add_definition_json(name: &str) -> String {
    format!(
        "{{\"name\":\"{name}\",\"project\":\"daemon-project\",\"mission\":\"Mission for {name}\",\"done_evidence\":[{{\"kind\":\"review\",\"statement\":\"done\"}}],\"authorized_by\":\"owner\"}}"
    )
}

#[tokio::test]
async fn generated_client_add_persists_definition_and_returns_detail() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let (service, tmp_campaigns, _tmp_traces) = make_service(repo_root.path());
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let response = client
        .add_campaign(AddCampaignRequest {
            definition_json: add_definition_json("alpha"),
        })
        .await
        .expect("add campaign must succeed")
        .into_inner();

    let detail = response.campaign.expect("response campaign detail");
    assert_eq!(detail.name, "alpha");
    assert_eq!(detail.project, "daemon-project");
    assert_eq!(detail.status, "staged");

    let stored = CampaignStore::load(tmp_campaigns.path()).expect("load campaigns");
    let campaign = stored.find("alpha").expect("stored campaign");
    assert_eq!(campaign.status, CampaignStatus::Staged);
    assert_eq!(campaign.project, "daemon-project");
}

#[tokio::test]
async fn generated_client_add_duplicate_returns_already_exists_store_unchanged_and_no_path() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let (service, tmp_campaigns, _tmp_traces) = make_service(repo_root.path());
    CampaignStore::default()
        .save(tmp_campaigns.path())
        .expect("save empty campaigns");
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client
        .add_campaign(AddCampaignRequest {
            definition_json: add_definition_json("alpha"),
        })
        .await
        .expect("initial add succeeds");
    let before = std::fs::read(tmp_campaigns.path()).expect("read campaigns before duplicate");

    let err = client
        .add_campaign(AddCampaignRequest {
            definition_json: add_definition_json("alpha"),
        })
        .await
        .expect_err("duplicate add must fail");

    assert_eq!(err.code(), Code::AlreadyExists);
    assert!(err.message().contains("campaign 'alpha' already exists"));
    assert!(!err.message().contains(&tmp_campaigns.path().display().to_string()));
    assert_eq!(
        std::fs::read(tmp_campaigns.path()).expect("read campaigns after duplicate"),
        before
    );
}

#[tokio::test]
async fn generated_client_add_unknown_project_returns_failed_precondition_and_no_path() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let (service, tmp_campaigns, _tmp_traces) = make_service(repo_root.path());
    let addr = start_server(service).await;
    let before = std::fs::read(tmp_campaigns.path()).expect("read empty campaigns");

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .add_campaign(AddCampaignRequest {
            definition_json: "{\"name\":\"alpha\",\"project\":\"missing-project\",\"mission\":\"Mission\",\"done_evidence\":[{\"kind\":\"review\",\"statement\":\"done\"}]}".to_string(),
        })
        .await
        .expect_err("unknown project must fail");

    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(err.message().contains("references unknown registered project"));
    assert!(!err.message().contains(&tmp_campaigns.path().display().to_string()));
    assert_eq!(
        std::fs::read(tmp_campaigns.path()).expect("read campaigns after failure"),
        before
    );
}

/// The daemon is the DEFAULT admission route — `foundry campaign add` goes
/// through this RPC unless `--offline` is passed. A budget enforced only in the
/// CLI's offline path would therefore never run in practice, which is exactly
/// how a 347,694-byte definition was accepted after the guard was added to the
/// CLI alone.
#[tokio::test]
async fn generated_client_add_rejects_context_over_the_inline_budget() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let huge = "x".repeat(
        usize::try_from(foundry_sdk::campaign::MAX_INLINE_CONTEXT_BYTES + 1).expect("fits usize"),
    );
    std::fs::write(repo_root.path().join("HUGE.md"), &huge).expect("write oversized context");
    let (service, tmp_campaigns, _tmp_traces) = make_service(repo_root.path());
    let addr = start_server(service).await;
    let before = std::fs::read(tmp_campaigns.path()).expect("read empty campaigns");

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .add_campaign(AddCampaignRequest {
            definition_json: "{\"name\":\"alpha\",\"project\":\"daemon-project\",\"mission\":\"Mission\",\"context_paths\":[\"HUGE.md\"],\"done_evidence\":[{\"kind\":\"review\",\"statement\":\"done\"}]}".to_string(),
        })
        .await
        .expect_err("oversized inline context must fail");

    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(err.message().contains("over the"), "must name the budget: {}", err.message());
    assert!(err.message().contains("HUGE.md"), "must name the offender: {}", err.message());
    assert!(
        !err.message().contains(&tmp_campaigns.path().display().to_string()),
        "must not leak the store path"
    );
    assert_eq!(
        std::fs::read(tmp_campaigns.path()).expect("read campaigns after failure"),
        before,
        "a rejected definition must leave the store byte-identical"
    );
}

/// Source context is listed for the agent to read on demand, so it costs
/// nothing in the prompt and must not be charged against the budget.
#[tokio::test]
async fn generated_client_add_accepts_large_source_context() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let huge = "x".repeat(
        usize::try_from(foundry_sdk::campaign::MAX_INLINE_CONTEXT_BYTES * 2).expect("fits usize"),
    );
    std::fs::write(repo_root.path().join("big.rs"), &huge).expect("write oversized source");
    let (service, tmp_campaigns, _tmp_traces) = make_service(repo_root.path());
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client
        .add_campaign(AddCampaignRequest {
            definition_json: "{\"name\":\"alpha\",\"project\":\"daemon-project\",\"mission\":\"Mission\",\"context_paths\":[\"big.rs\"],\"done_evidence\":[{\"kind\":\"review\",\"statement\":\"done\"}]}".to_string(),
        })
        .await
        .expect("source context must not consume the inline budget");

    let store = CampaignStore::load(tmp_campaigns.path()).expect("load campaigns");
    assert_eq!(store.find("alpha").expect("campaign persisted").context_paths.len(), 1);
}

#[tokio::test]
async fn generated_client_add_invalid_json_returns_invalid_argument_store_unchanged() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let (service, tmp_campaigns, _tmp_traces) = make_service(repo_root.path());
    let addr = start_server(service).await;
    let before = std::fs::read(tmp_campaigns.path()).expect("read empty campaigns");

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .add_campaign(AddCampaignRequest {
            definition_json: "{ not valid json }".to_string(),
        })
        .await
        .expect_err("invalid definition json must fail");

    assert_eq!(err.code(), Code::InvalidArgument);
    assert!(err.message().contains("campaign definition JSON is invalid"));
    assert_eq!(
        std::fs::read(tmp_campaigns.path()).expect("read campaigns after invalid json"),
        before
    );
}

#[tokio::test]
async fn generated_client_concurrent_adds_to_distinct_names_have_no_lost_update() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let (service, tmp_campaigns, _tmp_traces) = make_service(repo_root.path());
    let addr = start_server(service).await;

    let mut client_a = FoundryClient::connect(addr.clone()).await.expect("connect client a");
    let mut client_b = FoundryClient::connect(addr).await.expect("connect client b");
    let first = client_a.add_campaign(AddCampaignRequest {
        definition_json: add_definition_json("alpha"),
    });
    let second = client_b.add_campaign(AddCampaignRequest {
        definition_json: add_definition_json("beta"),
    });
    let (left, right) = tokio::join!(first, second);
    left.expect("alpha add succeeds");
    right.expect("beta add succeeds");

    let stored =
        CampaignStore::load(tmp_campaigns.path()).expect("load campaigns after concurrent adds");
    assert!(stored.find("alpha").is_some());
    assert!(stored.find("beta").is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn generated_client_add_persist_failure_returns_internal_and_store_unchanged() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let campaigns_dir = tempfile::tempdir().expect("campaigns tempdir");
    let campaigns_path = campaigns_dir.path().join("campaigns.json");
    CampaignStore::default().save(&campaigns_path).expect("save initial campaigns");
    drop(
        CampaignStore::lock_exclusive(&campaigns_path)
            .expect("create lock file before making directory readonly"),
    );
    let before = std::fs::read(&campaigns_path).expect("read initial campaigns");
    let mut permissions = std::fs::metadata(campaigns_dir.path())
        .expect("stat campaigns dir")
        .permissions();
    permissions.set_mode(0o555);
    std::fs::set_permissions(campaigns_dir.path(), permissions).expect("set readonly dir");

    let (service, _tmp_traces) = make_service_with_campaigns_path(
        campaigns_path.clone(),
        registry_with_project(repo_root.path()),
    );
    let addr = start_server(service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .add_campaign(AddCampaignRequest {
            definition_json: add_definition_json("alpha"),
        })
        .await
        .expect_err("persist failure must fail");

    let mut restore = std::fs::metadata(campaigns_dir.path())
        .expect("stat campaigns dir for restore")
        .permissions();
    restore.set_mode(0o755);
    std::fs::set_permissions(campaigns_dir.path(), restore).expect("restore dir perms");

    assert_eq!(err.code(), Code::Internal);
    assert!(err.message().contains("campaign store save failed"));
    assert!(!err.message().contains(&campaigns_path.display().to_string()));
    assert_eq!(std::fs::read(&campaigns_path).expect("read campaigns after failure"), before);
}
