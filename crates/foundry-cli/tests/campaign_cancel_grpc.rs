//! Integration tests for `foundry campaign cancel` via the gRPC online path.
//!
//! Each test starts a real [`FoundryService`] bound to a temporary TCP port,
//! then calls [`foundry_cli::campaign_commands::cancel_and_render`] with
//! `offline = false` so the call travels through the generated tonic client.
//!
//! Gates verified here:
//!
//! - The rendered output is built from `CancelCampaignResponse.campaign`, with
//!   no post-call disk read — the daemon is authoritative for campaign state.
//! - The persisted status is `cancelled` and the reason lands in the
//!   append-only owner record.
//! - Not-found and malformed-store cases map to distinct gRPC codes and their
//!   messages leak no filesystem paths.
//! - `--offline --now` is refused rather than silently downgraded to a graceful
//!   cancel, and the refusal names the corrective command.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_cli::campaign_commands::cancel_and_render;
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

// ── Harness ───────────────────────────────────────────────────────────────────

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
    let registry = Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![],
    }));

    let tmp_registry = NamedTempFile::new().expect("tempfile for registry");
    let registry_path = tmp_registry.path().to_path_buf();
    let tmp_sentinels = NamedTempFile::new().expect("tempfile for sentinels");
    let sentinels_path = tmp_sentinels.path().to_path_buf();

    let ctx = RuntimeContext {
        engine,
        trace_store,
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer,
        event_tx,
        registry,
    };
    let stores = StoreConfig {
        campaigns_path,
        registry_path,
        sentinels: Arc::new(RwLock::new(SentinelStore::default_seed())),
        sentinels_path,
        scheduler_reload: Arc::new(Notify::new()),
    };

    (FoundryService::new(ctx, stores), tmp_traces)
}

fn make_service() -> (FoundryService, NamedTempFile, TempDir) {
    let tmp_campaigns = NamedTempFile::new().expect("tempfile for campaigns");
    let campaigns_path = tmp_campaigns.path().to_path_buf();
    let (service, tmp_traces) = make_service_with_campaigns_path(campaigns_path);
    (service, tmp_campaigns, tmp_traces)
}

fn seed_campaign(tmp: &NamedTempFile, campaign: Campaign) {
    CampaignStore {
        version: 1,
        campaigns: vec![campaign],
    }
    .save(tmp.path())
    .expect("seed campaign store");
}

fn active_campaign(name: &str) -> Campaign {
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
        escalation: vec![],
        status: CampaignStatus::Active,
        cycles_completed: 2,
        cycles_landed: 1,
        authorized_by: Some("owner".to_string()),
        agent_provider: None,
        last_run_event_id: None,
        owner_decisions: vec![],
        pending_run_result: None,
        objective_history: vec![],
    }
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

// ── Proofs ────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn online_cancel_renders_daemon_state_and_persists_the_transition() {
    let (service, tmp_campaigns, _traces) = make_service();
    seed_campaign(&tmp_campaigns, active_campaign("c"));
    let path = tmp_campaigns.path().to_path_buf();
    let addr = start_server(service).await;

    let rendered = cancel_and_render(
        std::path::Path::new("/nonexistent/must-not-be-read"),
        &addr,
        false,
        "c",
        "abandoned",
        false,
        false,
    )
    .await
    .expect("online cancel must succeed");

    // The store path passed in is deliberately bogus: the online path is
    // daemon-authoritative and must never touch FOUNDRY_CAMPAIGNS_PATH.
    assert!(
        rendered.contains("cancelled"),
        "rendered detail must show the new status:\n{rendered}"
    );
    assert!(rendered.contains('c'), "rendered detail must name the campaign:\n{rendered}");

    let store = CampaignStore::load(&path).expect("load store");
    let stored = store.find("c").expect("campaign");
    assert_eq!(stored.status, CampaignStatus::Cancelled);
    assert_eq!(stored.owner_decisions.len(), 1);
    assert!(
        stored.owner_decisions[0].decision.contains("abandoned"),
        "the reason must reach the append-only owner record, got {:?}",
        stored.owner_decisions[0].decision
    );
}

#[tokio::test]
async fn unknown_campaign_and_malformed_store_map_to_distinct_codes_without_paths() {
    let (service, tmp_campaigns, _traces) = make_service();
    seed_campaign(&tmp_campaigns, active_campaign("c"));
    let addr = start_server(service).await;

    let missing = cancel_and_render(
        std::path::Path::new("/nonexistent"),
        &addr,
        false,
        "nope",
        "abandoned",
        false,
        false,
    )
    .await
    .expect_err("an unknown campaign must be refused");
    let text = missing.to_string();
    assert!(text.contains("NotFound") || text.contains("not found"), "got {text}");
    assert!(
        !text.contains(tmp_campaigns.path().to_str().unwrap()),
        "leaked a store path: {text}"
    );

    // A malformed store is a distinct failure class from a missing campaign.
    let malformed = NamedTempFile::new().expect("tempfile");
    std::fs::write(malformed.path(), "{ not json").expect("write malformed store");
    let (service, _traces) = make_service_with_campaigns_path(malformed.path().to_path_buf());
    let addr = start_server(service).await;

    let error = cancel_and_render(
        std::path::Path::new("/nonexistent"),
        &addr,
        false,
        "c",
        "abandoned",
        false,
        false,
    )
    .await
    .expect_err("a malformed store must be refused");
    let text = error.to_string();
    assert!(text.contains("FailedPrecondition") || text.contains("malformed"), "got {text}");
    assert!(
        !text.contains(malformed.path().to_str().unwrap()),
        "leaked a store path: {text}"
    );
}

/// `--now` aborts a workflow inside foundryd. Offline there is no daemon, so
/// silently downgrading would report a kill that never happened.
#[tokio::test]
async fn offline_now_is_refused_with_a_corrective_command() {
    let tmp = NamedTempFile::new().expect("tempfile");
    seed_campaign(&tmp, active_campaign("c"));

    let error =
        cancel_and_render(tmp.path(), "http://127.0.0.1:0", true, "c", "abandoned", true, false)
            .await
            .expect_err("--offline --now must be refused");

    let text = error.to_string();
    assert!(text.contains("no daemon"), "the refusal must say why: {text}");
    assert!(
        text.contains("foundry campaign cancel c --reason"),
        "the refusal must name the corrective command: {text}"
    );

    // The store must be untouched — a refused command changes nothing.
    let store = CampaignStore::load(tmp.path()).expect("load store");
    assert_eq!(store.find("c").expect("campaign").status, CampaignStatus::Active);
}

/// A graceful cancel has already preserved its work, so `--discard-work` alone
/// would silently do nothing. The refusal must explain that rather than just
/// printing a usage line.
#[tokio::test]
async fn discard_work_without_now_is_refused_with_an_explanation() {
    let tmp = NamedTempFile::new().expect("tempfile");
    seed_campaign(&tmp, active_campaign("c"));

    let error =
        cancel_and_render(tmp.path(), "http://127.0.0.1:0", true, "c", "abandoned", false, true)
            .await
            .expect_err("--discard-work without --now must be refused");

    let text = error.to_string();
    assert!(
        text.contains("commits and preserves its work"),
        "the refusal must explain why there is nothing to discard: {text}"
    );
    assert!(
        text.contains("--now --discard-work"),
        "the refusal must show both valid invocations: {text}"
    );

    let store = CampaignStore::load(tmp.path()).expect("load store");
    assert_eq!(store.find("c").expect("campaign").status, CampaignStatus::Active);
}

#[tokio::test]
async fn offline_cancel_records_the_transition_directly_in_the_store() {
    let tmp = NamedTempFile::new().expect("tempfile");
    seed_campaign(&tmp, active_campaign("c"));

    let rendered =
        cancel_and_render(tmp.path(), "http://127.0.0.1:0", true, "c", "abandoned", false, false)
            .await
            .expect("offline cancel must succeed without a daemon");
    assert!(rendered.contains("cancelled"), "{rendered}");

    let store = CampaignStore::load(tmp.path()).expect("load store");
    let stored = store.find("c").expect("campaign");
    assert_eq!(stored.status, CampaignStatus::Cancelled);
    assert_eq!(stored.owner_decisions.len(), 1);

    // Idempotent, and no duplicate audit entry.
    cancel_and_render(tmp.path(), "http://127.0.0.1:0", true, "c", "abandoned", false, false)
        .await
        .expect("re-cancelling offline must not error");
    let store = CampaignStore::load(tmp.path()).expect("reload store");
    assert_eq!(store.find("c").expect("campaign").owner_decisions.len(), 1);
}
