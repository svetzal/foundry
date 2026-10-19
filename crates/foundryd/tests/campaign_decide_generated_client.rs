//! Integration tests for `DecideCampaign` through the generated tonic client.

#![allow(clippy::unwrap_used, clippy::expect_used)]

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
        work_items_path: std::path::PathBuf::new(),
        events_dir: std::path::PathBuf::new(),
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
        work_items_path: std::path::PathBuf::new(),
        events_dir: std::path::PathBuf::new(),
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
        recovery: None,
        initiative: None,
        providers: foundry_sdk::gateway::StageProviders::default(),
        name: name.to_string(),
        project: "test-project".to_string(),
        mission: "Test mission".to_string(),
        intent_refs: vec![],
        context_paths: vec![],
        done_evidence: vec![DoneEvidence::Review {
            statement: "done".to_string(),
        }],
        budget: CampaignBudget {
            max_cycles: 10,
            ..Default::default()
        },
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
            land_blocked: None,
            proof_evidence: None,
            uncommitted_artifacts: None,
            trunk_arrivals: Vec::new(),
            verdict: TaskVerdict::BlockedOnDecision {
                finding: "boundary choice required".to_string(),
                options: vec!["grpc".to_string(), "json".to_string()],
            },
            context: LoopContext {
                campaign: Some(name.to_string()),
                ..LoopContext::default()
            },
            failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
        }),
        objective_history: vec![],
        open_escalation: None,
        writable_repositories: vec![],
    }
}

fn seed_campaign(tmp: &NamedTempFile, campaign: Campaign) {
    CampaignStore {
        findings: vec![],
        initiatives: vec![],
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
            add_cycles: 0,
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
    assert!(
        !detail.owner_decisions[0].decided_at.is_empty(),
        "expected `detail.owner_decisions[0].decided_at` to be non-empty"
    );

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

/// The seeded campaign in `status`, with everything an escalated decide would
/// touch: a spent budget, an open escalation record and a pending run result.
fn campaign_in(status: CampaignStatus) -> Campaign {
    let mut campaign = escalated_campaign("c");
    campaign.status = status;
    campaign.cycles_completed = campaign.budget.max_cycles;
    campaign.open_escalation = Some(foundry_sdk::campaign::CampaignEscalation {
        reason: "an earlier question".to_string(),
        escalated_at: chrono::DateTime::parse_from_rfc3339("2026-10-01T12:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc),
        cycles_completed: 10,
        cycles_landed: 2,
        question: None,
        choices: vec![],
        from_event_log: false,
    });
    campaign
}

async fn decide(
    client: &mut FoundryClient<tonic::transport::Channel>,
    decision: &str,
    add_cycles: u64,
) -> Result<foundryd::proto::CampaignDetail, tonic::Status> {
    client
        .decide_campaign(DecideCampaignRequest {
            name: "c".to_string(),
            decision: decision.to_string(),
            add_cycles,
        })
        .await
        .map(|response| response.into_inner().campaign.expect("campaign detail"))
}

/// An owner can record a binding decision on an active or paused campaign
/// through the daemon. The decision is stored and nothing else changes: the
/// status stays as it was, the budget is not touched, and the open escalation
/// record and pending run result survive. A spent budget does not block it.
/// A retry of the same decision is refused, and so is a decision that asks
/// for cycles; neither writes anything.
#[tokio::test]
async fn generated_client_decide_records_on_active_and_paused_campaigns_without_changing_status() {
    for status in [CampaignStatus::Active, CampaignStatus::Paused] {
        let (service, tmp_campaigns, _tmp_traces) = make_service();
        seed_campaign(&tmp_campaigns, campaign_in(status));
        let mut expected =
            serde_json::to_value(CampaignStore::load(tmp_campaigns.path()).unwrap()).unwrap();
        let addr = start_server(service).await;
        let mut client = FoundryClient::connect(addr).await.expect("connect");

        let detail = decide(&mut client, "  Keep the v1 endpoint until clients move.  ", 0)
            .await
            .unwrap_or_else(|status| panic!("{status:?}: decide must record: {status:?}"));

        assert_eq!(detail.status, status.to_string(), "the status does not change");
        assert_eq!(detail.max_cycles, 10, "the budget does not change");
        assert_eq!(detail.owner_decisions.len(), 1);
        assert_eq!(detail.owner_decisions[0].decision, "Keep the v1 endpoint until clients move.");
        assert_eq!(detail.owner_decisions[0].authorized_by, "owner");
        let stored = CampaignStore::load(tmp_campaigns.path()).expect("load store");
        let decided_at = stored.find("c").unwrap().owner_decisions[0].decided_at;
        expected["campaigns"][0]["owner_decisions"] = serde_json::json!([{
            "decision": "Keep the v1 endpoint until clients move.",
            "authorized_by": "owner",
            "decided_at": decided_at,
        }]);
        assert_eq!(
            serde_json::to_value(&stored).unwrap(),
            expected,
            "{status}: only the owner decision was added"
        );

        let after_first = std::fs::read(tmp_campaigns.path()).unwrap();
        let retry = decide(&mut client, "Keep the v1 endpoint until clients move.", 0)
            .await
            .expect_err("a retried decide must be refused");
        assert_eq!(retry.code(), Code::FailedPrecondition);
        assert_eq!(
            retry.message(),
            "campaign 'c' already records this owner decision as its latest; nothing to apply"
        );
        assert_eq!(std::fs::read(tmp_campaigns.path()).unwrap(), after_first, "{status}");

        let with_cycles = decide(&mut client, "Spend two more cycles.", 2)
            .await
            .expect_err("decide with add_cycles must be refused unless escalated");
        assert_eq!(with_cycles.code(), Code::FailedPrecondition);
        let message = if status == CampaignStatus::Paused {
            "campaign 'c' is 'paused'; decide adds cycles only to an escalated campaign; run `foundry campaign resume c --add-cycles 2` to extend a paused campaign's budget"
        } else {
            "campaign 'c' is 'active'; decide adds cycles only to an escalated campaign; record the decision without add_cycles"
        };
        assert_eq!(with_cycles.message(), message);
        assert_eq!(
            std::fs::read(tmp_campaigns.path()).unwrap(),
            after_first,
            "{status}: a refused decide with cycles records nothing"
        );
    }
}

/// A completed or cancelled campaign has nothing left to steer: decide is
/// refused and the store is left byte for byte as it was.
#[tokio::test]
async fn generated_client_decide_refuses_terminal_campaigns_and_leaves_the_store_unchanged() {
    for status in [CampaignStatus::Completed, CampaignStatus::Cancelled] {
        let (service, tmp_campaigns, _tmp_traces) = make_service();
        seed_campaign(&tmp_campaigns, campaign_in(status));
        let before = std::fs::read(tmp_campaigns.path()).unwrap();
        let addr = start_server(service).await;
        let mut client = FoundryClient::connect(addr).await.expect("connect");

        let err = decide(&mut client, "Use the generated tonic client path.", 0)
            .await
            .expect_err("decide must refuse a terminal campaign");

        assert_eq!(err.code(), Code::FailedPrecondition);
        assert_eq!(
            err.message(),
            format!(
                "campaign 'c' is '{status}'; decide requires Active, Paused or Escalated status"
            )
        );
        assert!(
            !err.message().contains(&tmp_campaigns.path().display().to_string()),
            "error message must not expose the campaign store path"
        );
        assert_eq!(std::fs::read(tmp_campaigns.path()).unwrap(), before, "{status}");
    }
}

/// A successful escalated decide with cycles returns the campaign to active.
/// Its retry lands on the active campaign and is refused as a repeat of the
/// latest decision: the cycles are not added twice.
#[tokio::test]
async fn generated_client_retried_escalated_decide_is_refused_and_adds_no_cycles() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    seed_campaign(&tmp_campaigns, campaign_in(CampaignStatus::Escalated));
    let addr = start_server(service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect");

    let detail = decide(&mut client, "One more pass.", 1).await.expect("escalated decide");
    assert_eq!(detail.status, "active");
    assert_eq!(detail.max_cycles, 11);
    assert!(detail.open_escalation.is_none());
    let after_first = std::fs::read(tmp_campaigns.path()).unwrap();

    let retry = decide(&mut client, "One more pass.", 1).await.expect_err("retry");
    assert_eq!(retry.code(), Code::FailedPrecondition);
    assert_eq!(
        retry.message(),
        "campaign 'c' already records this owner decision as its latest; nothing to apply"
    );
    assert_eq!(std::fs::read(tmp_campaigns.path()).unwrap(), after_first);
    assert_eq!(
        CampaignStore::load(tmp_campaigns.path())
            .unwrap()
            .find("c")
            .unwrap()
            .budget
            .max_cycles,
        11
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
            add_cycles: 0,
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
            add_cycles: 0,
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
        findings: vec![],
        initiatives: vec![],
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
            add_cycles: 0,
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
    assert!(
        campaign.owner_decisions.is_empty(),
        "expected `campaign.owner_decisions` to be empty: {:?}",
        campaign.owner_decisions
    );
}
