//! Integration tests for `foundry campaign decide` via the gRPC online path.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_cli::campaign_commands::decide_and_render;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{
    Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
};
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

fn make_service() -> (FoundryService, NamedTempFile, TempDir) {
    let tmp_campaigns = NamedTempFile::new().expect("tempfile for campaigns");
    let campaigns_path = tmp_campaigns.path().to_path_buf();
    let (service, tmp_traces) = make_service_with_campaigns_path(campaigns_path);
    (service, tmp_campaigns, tmp_traces)
}

fn escalated_campaign(name: &str) -> Campaign {
    Campaign {
        name: name.to_string(),
        project: "test-project".to_string(),
        mission: "Need owner policy".to_string(),
        intent_refs: vec![],
        context_paths: vec![],
        done_evidence: vec![DoneEvidence::Review {
            statement: "done".to_string(),
        }],
        budget: CampaignBudget { max_cycles: 10 },
        escalation: vec!["owner decision required".to_string()],
        status: CampaignStatus::Escalated,
        cycles_completed: 2,
        cycles_landed: 1,
        authorized_by: Some("owner".to_string()),
        agent_provider: None,
        last_run_event_id: Some("run-2".to_string()),
        owner_decisions: vec![],
        pending_run_result: None,
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
async fn online_decide_renders_owner_decision_from_rpc_response() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    seed_campaign(&tmp_campaigns, escalated_campaign("c"));
    let addr = start_server(service).await;

    let rendered = decide_and_render(
        tmp_campaigns.path(),
        &addr,
        false,
        "c",
        "Use the generated tonic client path.",
    )
    .await
    .expect("online decide must succeed");

    let stored = CampaignStore::load(tmp_campaigns.path()).expect("load store");
    let campaign = stored.find("c").expect("campaign");
    assert_eq!(campaign.status, CampaignStatus::Active);
    assert_eq!(campaign.owner_decisions.len(), 1);
    assert!(rendered.contains("Owner decisions:"));
    assert!(rendered.contains("Use the generated tonic client path."));
    assert!(rendered.contains("owner"));
}

#[tokio::test]
async fn online_decide_renders_from_rpc_response_not_cli_side_file() {
    let daemon_campaigns = NamedTempFile::new().expect("daemon campaigns tempfile");
    let daemon_mission = "Mission-From-Daemon-Decision";
    let daemon_campaign = Campaign {
        mission: daemon_mission.to_string(),
        ..escalated_campaign("c")
    };
    seed_campaign(&daemon_campaigns, daemon_campaign);

    let cli_campaigns = NamedTempFile::new().expect("cli campaigns tempfile");
    let cli_mission = "Mission-From-CLI-File-Stale";
    let cli_campaign = Campaign {
        mission: cli_mission.to_string(),
        ..escalated_campaign("c")
    };
    seed_campaign(&cli_campaigns, cli_campaign);

    let (service, _tmp_traces) =
        make_service_with_campaigns_path(daemon_campaigns.path().to_path_buf());
    let addr = start_server(service).await;

    let rendered = decide_and_render(
        cli_campaigns.path(),
        &addr,
        false,
        "c",
        "Use the generated tonic client path.",
    )
    .await
    .expect("online decide must succeed");

    assert!(rendered.contains(daemon_mission));
    assert!(!rendered.contains(cli_mission));
}

#[tokio::test]
async fn online_decide_not_found_maps_to_daemon_error() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    CampaignStore::default().save(tmp_campaigns.path()).expect("save empty store");
    let addr = start_server(service).await;

    let err = decide_and_render(
        tmp_campaigns.path(),
        &addr,
        false,
        "missing",
        "Use the generated tonic client path.",
    )
    .await
    .expect_err("missing campaign must fail");

    let message = err.to_string();
    assert!(message.contains("daemon error"));
    assert!(message.contains("campaign 'missing' not found"));
}
