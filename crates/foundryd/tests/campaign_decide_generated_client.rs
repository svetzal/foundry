//! Integration tests for `DecideCampaign` through the generated tonic client.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{
    Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
};
use foundry_sdk::payload::{LoopContext, TaskRunCompletedPayload, TaskVerdict};
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundryd::{
    proto::{DecideCampaignRequest, foundry_client::FoundryClient, foundry_server::FoundryServer},
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic::transport::Server;

fn make_service() -> (FoundryService, NamedTempFile, TempDir) {
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
    let service = FoundryService::new(ctx, stores);

    (service, tmp_campaigns, tmp_traces)
}

fn make_service_with_campaigns_path(
    campaigns_path: std::path::PathBuf,
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
    let service = FoundryService::new(ctx, stores);

    (service, tmp_traces)
}

fn escalated_campaign(name: &str) -> Campaign {
    Campaign {
        name: name.to_string(),
        project: "test-project".to_string(),
        mission: "Test mission".to_string(),
        intent_refs: vec![],
        context_paths: vec![],
        done_evidence: vec![DoneEvidence::Review {
            statement: "done".to_string(),
        }],
        budget: CampaignBudget { max_cycles: 10 },
        escalation: vec!["owner decision required".to_string()],
        status: CampaignStatus::Escalated,
        cycles_completed: 3,
        cycles_landed: 2,
        authorized_by: Some("owner".to_string()),
        agent_provider: None,
        last_run_event_id: Some("run-3".to_string()),
        owner_decisions: vec![],
        pending_run_result: Some(TaskRunCompletedPayload {
            project: "test-project".to_string(),
            success: false,
            landed: false,
            summary: "waiting on owner policy".to_string(),
            preservation_ref: Some("foundry-task/preserved-ref".to_string()),
            verdict: TaskVerdict::BlockedOnDecision {
                finding: "boundary choice required".to_string(),
                options: vec!["grpc".to_string(), "json".to_string()],
            },
            context: LoopContext {
                campaign: Some(name.to_string()),
                ..LoopContext::default()
            },
        }),
    }
}

fn seed_campaign(tmp: &NamedTempFile, campaign: Campaign) {
    CampaignStore {
        version: 1,
        campaigns: vec![campaign],
    }
    .save(tmp.path())
    .expect("seed campaign store");
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
async fn generated_client_decide_campaign_appends_record_and_preserves_pending_result() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    seed_campaign(&tmp_campaigns, escalated_campaign("c"));
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let response = client
        .decide_campaign(DecideCampaignRequest {
            name: "c".to_string(),
            decision: "Use the generated tonic client path.".to_string(),
        })
        .await
        .expect("decide campaign should succeed")
        .into_inner();

    let detail = response.campaign.expect("campaign detail");
    assert_eq!(detail.status, "active");
    assert_eq!(detail.cycles_completed, 3);
    assert_eq!(detail.cycles_landed, 2);
    assert_eq!(detail.owner_decisions.len(), 1);
    assert_eq!(detail.owner_decisions[0].decision, "Use the generated tonic client path.");
    assert_eq!(detail.owner_decisions[0].authorized_by, "owner");
    assert!(!detail.owner_decisions[0].decided_at.is_empty());

    let stored = CampaignStore::load(tmp_campaigns.path()).expect("load store");
    let campaign = stored.find("c").expect("campaign must exist");
    assert_eq!(campaign.status, CampaignStatus::Active);
    assert_eq!(campaign.cycles_completed, 3);
    assert_eq!(campaign.cycles_landed, 2);
    assert_eq!(campaign.owner_decisions.len(), 1);
    assert_eq!(campaign.owner_decisions[0].decision, "Use the generated tonic client path.");
    assert_eq!(
        campaign
            .pending_run_result
            .as_ref()
            .and_then(|result| result.preservation_ref.as_deref()),
        Some("foundry-task/preserved-ref")
    );
}

#[tokio::test]
async fn generated_client_decide_campaign_invalid_state_returns_failed_precondition() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    let mut campaign = escalated_campaign("c");
    campaign.status = CampaignStatus::Active;
    seed_campaign(&tmp_campaigns, campaign);
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .decide_campaign(DecideCampaignRequest {
            name: "c".to_string(),
            decision: "Use the generated tonic client path.".to_string(),
        })
        .await
        .expect_err("decide campaign must reject non-escalated campaigns");

    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(err.message().contains("decide requires Escalated status"));
    assert!(
        !err.message().contains(&tmp_campaigns.path().display().to_string()),
        "error message must not expose the campaign store path"
    );
}

#[tokio::test]
async fn generated_client_decide_campaign_missing_campaign_returns_not_found() {
    let (service, _tmp_campaigns, _tmp_traces) = make_service();
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .decide_campaign(DecideCampaignRequest {
            name: "missing".to_string(),
            decision: "Use the generated tonic client path.".to_string(),
        })
        .await
        .expect_err("missing campaign must fail");

    assert_eq!(err.code(), Code::NotFound);
    assert!(err.message().contains("campaign 'missing' not found"));
}

#[tokio::test]
async fn generated_client_decide_campaign_malformed_store_returns_failed_precondition_without_path()
{
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    std::fs::write(tmp_campaigns.path(), b"{ not json }").expect("write malformed store");
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .decide_campaign(DecideCampaignRequest {
            name: "c".to_string(),
            decision: "Use the generated tonic client path.".to_string(),
        })
        .await
        .expect_err("malformed store must fail");

    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(err.message().contains("campaign store is malformed"));
    assert!(
        !err.message().contains(&tmp_campaigns.path().display().to_string()),
        "error message must not expose the campaign store path"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn generated_client_decide_campaign_persist_failure_returns_internal_and_store_unchanged() {
    let campaigns_dir = tempfile::tempdir().expect("campaigns tempdir");
    let campaigns_path = campaigns_dir.path().join("campaigns.json");
    CampaignStore {
        version: 1,
        campaigns: vec![escalated_campaign("c")],
    }
    .save(&campaigns_path)
    .expect("save initial campaigns");
    drop(
        CampaignStore::lock_exclusive(&campaigns_path)
            .expect("create lock file before making directory readonly"),
    );
    let before = std::fs::read(&campaigns_path).expect("read campaigns before failure");

    let mut permissions = std::fs::metadata(campaigns_dir.path())
        .expect("stat campaigns dir")
        .permissions();
    permissions.set_mode(0o555);
    std::fs::set_permissions(campaigns_dir.path(), permissions).expect("set readonly dir");

    let (service, _tmp_traces) = make_service_with_campaigns_path(campaigns_path.clone());
    let addr = start_server(service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .decide_campaign(DecideCampaignRequest {
            name: "c".to_string(),
            decision: "Use the daemon boundary.".to_string(),
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

    let stored = CampaignStore::load(&campaigns_path).expect("load store after failure");
    let campaign = stored.find("c").expect("campaign remains present");
    assert_eq!(campaign.status, CampaignStatus::Escalated);
    assert!(campaign.owner_decisions.is_empty());
}
