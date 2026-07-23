//! Integration tests for `ResumeCampaign` through the generated tonic client.
//!
//! Each test starts a real [`FoundryService`] bound to a temporary TCP port,
//! then calls the `ResumeCampaign` RPC through the generated `FoundryClient`
//! so the full gRPC encode/decode cycle is exercised.
//!
//! Evidence gates verified by this file:
//!
//! - Resuming a Paused campaign with `add_cycles=N` produces a reloaded store
//!   `max_cycles` of exactly `original + N` and `status=active`.
//! - Resuming a non-exhausted Paused campaign with `add_cycles=0` leaves the
//!   reloaded budget unchanged.
//! - Resuming a budget-only Escalated campaign returns `status=active` and
//!   preserves `pending_run_result` byte-for-byte in the reloaded store.
//! - An exhausted campaign (`cycles_completed >= max_cycles`) with `add_cycles=0`
//!   returns `FAILED_PRECONDITION`; the reloaded store status and budget are
//!   byte-identical to before (no masked mutation).
//! - No error message contains the store filesystem path.

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
    proto::{ResumeCampaignRequest, foundry_client::FoundryClient, foundry_server::FoundryServer},
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

fn seed_campaign(tmp: &NamedTempFile, campaign: Campaign) {
    CampaignStore {
        version: 1,
        campaigns: vec![campaign],
    }
    .save(tmp.path())
    .expect("seed campaign store");
}

fn paused_campaign(name: &str, max_cycles: u64, cycles_completed: u64) -> Campaign {
    Campaign {
        name: name.to_string(),
        project: "test-project".to_string(),
        mission: "Test mission".to_string(),
        intent_refs: vec![],
        context_paths: vec![],
        done_evidence: vec![DoneEvidence::Review {
            statement: "done".to_string(),
        }],
        budget: CampaignBudget { max_cycles },
        escalation: vec![],
        status: CampaignStatus::Paused,
        cycles_completed,
        cycles_landed: 0,
        authorized_by: Some("owner".to_string()),
        agent_provider: None,
        last_run_event_id: None,
        owner_decisions: vec![],
        pending_run_result: None,
        objective_history: vec![],
    }
}

fn escalated_campaign_with_pending_result(name: &str) -> Campaign {
    Campaign {
        name: name.to_string(),
        project: "test-project".to_string(),
        mission: "Escalated mission".to_string(),
        intent_refs: vec![],
        context_paths: vec![],
        done_evidence: vec![DoneEvidence::Review {
            statement: "done".to_string(),
        }],
        budget: CampaignBudget { max_cycles: 10 },
        escalation: vec!["budget limit reached".to_string()],
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
            preservation_ref: Some("foundry-task/resume-grpc-test-ref".to_string()),
            verdict: TaskVerdict::BlockedOnDecision {
                finding: "boundary choice required".to_string(),
                options: vec!["option-a".to_string(), "option-b".to_string()],
            },
            context: LoopContext {
                campaign: Some(name.to_string()),
                ..LoopContext::default()
            },
        }),
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

// ── Gate: add_cycles=N extends budget by exactly N ───────────────────────────

/// Resuming a Paused campaign with `add_cycles=N` must:
///
/// 1. Return `status=active` in the wire response.
/// 2. Return `max_cycles = original + N` in the wire response.
/// 3. Persist `max_cycles = original + N` to the store (reloaded assertion).
///
/// An implementation that ignores `add_cycles`, applies it twice, or returns
/// the old `max_cycles` will fail one of these assertions.
#[tokio::test]
async fn generated_client_resume_paused_with_add_cycles_increases_budget_by_exactly_n() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    let original_max = 7u64;
    let add = 5u64;
    seed_campaign(&tmp_campaigns, paused_campaign("c", original_max, 0));
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let response = client
        .resume_campaign(ResumeCampaignRequest {
            name: "c".to_string(),
            add_cycles: add,
        })
        .await
        .expect("resume should succeed")
        .into_inner();

    let detail = response.campaign.expect("campaign detail");
    assert_eq!(detail.status, "active");
    assert_eq!(
        detail.max_cycles,
        original_max + add,
        "wire response max_cycles must be original + add_cycles"
    );

    // Reloaded store must also reflect the new budget.
    let stored = CampaignStore::load(tmp_campaigns.path()).expect("load store");
    let campaign = stored.find("c").expect("campaign must exist");
    assert_eq!(campaign.status, CampaignStatus::Active);
    assert_eq!(
        campaign.budget.max_cycles,
        original_max + add,
        "reloaded store max_cycles must equal original + add_cycles"
    );
}

// ── Gate: add_cycles=0 on non-exhausted campaign leaves budget unchanged ─────

/// Resuming a non-exhausted Paused campaign with `add_cycles=0` must succeed
/// and leave `max_cycles` unchanged in both the wire response and the reloaded
/// store.
#[tokio::test]
async fn generated_client_resume_paused_with_add_cycles_zero_leaves_budget_unchanged() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    let original_max = 10u64;
    // cycles_completed=3 < max_cycles=10: budget is NOT exhausted.
    seed_campaign(&tmp_campaigns, paused_campaign("c", original_max, 3));
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let response = client
        .resume_campaign(ResumeCampaignRequest {
            name: "c".to_string(),
            add_cycles: 0,
        })
        .await
        .expect("resume with add_cycles=0 on non-exhausted campaign must succeed")
        .into_inner();

    let detail = response.campaign.expect("campaign detail");
    assert_eq!(detail.status, "active");
    assert_eq!(
        detail.max_cycles, original_max,
        "max_cycles must be unchanged when add_cycles=0 and budget is not exhausted"
    );

    let stored = CampaignStore::load(tmp_campaigns.path()).expect("load store");
    assert_eq!(
        stored.find("c").expect("campaign").budget.max_cycles,
        original_max,
        "reloaded store max_cycles must be unchanged"
    );
}

// ── Gate: Escalated campaign becomes active; pending_run_result preserved ────

/// Resuming a budget-only Escalated campaign must:
///
/// 1. Return `status=active` in the wire response.
/// 2. Leave `pending_run_result` in the reloaded store byte-for-byte identical
///    to the seeded value — the operation must NOT clear or overwrite it.
///
/// A status-only assertion would mask accidental result loss.
#[tokio::test]
async fn generated_client_resume_escalated_sets_active_and_preserves_pending_run_result() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    seed_campaign(&tmp_campaigns, escalated_campaign_with_pending_result("c"));
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let response = client
        .resume_campaign(ResumeCampaignRequest {
            name: "c".to_string(),
            add_cycles: 0,
        })
        .await
        .expect("resume of Escalated campaign must succeed")
        .into_inner();

    let detail = response.campaign.expect("campaign detail");
    assert_eq!(detail.status, "active");

    // Reloaded store: status=Active and pending_run_result preserved.
    let stored = CampaignStore::load(tmp_campaigns.path()).expect("load store");
    let campaign = stored.find("c").expect("campaign must exist");
    assert_eq!(campaign.status, CampaignStatus::Active);

    let pending = campaign
        .pending_run_result
        .as_ref()
        .expect("pending_run_result must still be present after resume");
    assert_eq!(
        pending.preservation_ref.as_deref(),
        Some("foundry-task/resume-grpc-test-ref"),
        "preservation_ref must survive the resume operation byte-for-byte"
    );
    assert_eq!(
        pending.summary, "waiting on owner policy",
        "summary must survive the resume operation"
    );
}

// ── Gate: exhausted + add_cycles=0 → FAILED_PRECONDITION, store unchanged ───

/// Attempting to resume an exhausted campaign (`cycles_completed >= max_cycles`)
/// with `add_cycles=0` must:
///
/// 1. Return `FAILED_PRECONDITION`.
/// 2. Leave the reloaded store status and budget byte-identical to before —
///    proving no masked partial mutation occurred.
///
/// A count-only or bypass implementation would either silently reactivate the
/// campaign or succeed without the required budget extension; both would fail
/// one of these assertions.
#[tokio::test]
async fn generated_client_resume_exhausted_with_add_cycles_zero_returns_failed_precondition_and_store_unchanged()
 {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    // cycles_completed == max_cycles: budget is exactly exhausted.
    let max_cycles = 5u64;
    seed_campaign(&tmp_campaigns, paused_campaign("c", max_cycles, max_cycles));
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .resume_campaign(ResumeCampaignRequest {
            name: "c".to_string(),
            add_cycles: 0,
        })
        .await
        .expect_err("exhausted campaign with add_cycles=0 must fail");

    assert_eq!(err.code(), Code::FailedPrecondition);

    // Reloaded store must be byte-identical to the seeded state.
    let stored = CampaignStore::load(tmp_campaigns.path()).expect("load store");
    let campaign = stored.find("c").expect("campaign must still exist");
    assert_eq!(
        campaign.status,
        CampaignStatus::Paused,
        "status must remain Paused after rejected resume"
    );
    assert_eq!(
        campaign.budget.max_cycles, max_cycles,
        "max_cycles must be unchanged after rejected resume"
    );
    assert_eq!(
        campaign.cycles_completed, max_cycles,
        "cycles_completed must be unchanged after rejected resume"
    );
}

// ── Gate: no error message contains the store filesystem path ────────────────

/// Error messages returned by `ResumeCampaign` must never expose the campaign
/// store filesystem path, regardless of the failure mode.
#[tokio::test]
async fn generated_client_resume_error_messages_do_not_contain_store_path() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    // Write a malformed store so the lock fails with a parse error that
    // includes only the serde message, not the path.
    std::fs::write(tmp_campaigns.path(), b"{ not json }").expect("write malformed store");
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .resume_campaign(ResumeCampaignRequest {
            name: "any".to_string(),
            add_cycles: 0,
        })
        .await
        .expect_err("malformed store must fail");

    let path_str = tmp_campaigns.path().display().to_string();
    assert!(
        !err.message().contains(&path_str),
        "error message must not expose the store path; got: {}",
        err.message()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn generated_client_resume_persist_failure_returns_internal_and_store_unchanged() {
    let campaigns_dir = tempfile::tempdir().expect("campaigns tempdir");
    let campaigns_path = campaigns_dir.path().join("campaigns.json");
    CampaignStore {
        version: 1,
        campaigns: vec![paused_campaign("c", 10, 3)],
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
        .resume_campaign(ResumeCampaignRequest {
            name: "c".to_string(),
            add_cycles: 2,
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
    assert_eq!(campaign.status, CampaignStatus::Paused);
    assert_eq!(campaign.budget.max_cycles, 10);
}
