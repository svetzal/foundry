//! Integration tests for `PauseCampaign` through the generated tonic client.
//!
//! Each test starts a real [`FoundryService`] bound to a temporary TCP port,
//! then calls the `PauseCampaign` RPC through the generated `FoundryClient`
//! so the full gRPC encode/decode cycle is exercised.
//!
//! Evidence gates verified by this file:
//!
//! 1. Pausing an Active campaign returns `PauseCampaignResponse` whose
//!    `CampaignDetail` has `status="paused"` with the correct name and
//!    counters, AND the reloaded `CampaignStore::load` shows that campaign
//!    persisted as `Paused` (exact persisted state, not just the response).
//! 2. Pausing a campaign that carries a `pending_run_result` preserves it:
//!    after the paused response, the reloaded store shows `pending_run_result`
//!    byte-identical to before (compare via `serde_json::to_value`).
//! 3. Pausing a name absent from a non-empty store returns `Code::NotFound`
//!    and leaves the store byte-identical.
//! 4. A malformed store returns `Code::FailedPrecondition`.
//! 5. An unreadable store (directory path) returns `Code::Internal`.
//!    Proofs 4 and 5 are asserted as distinct typed codes in the same file.
//! 6. No returned gRPC status message contains the campaign store filesystem
//!    path.
//! 7. Two concurrent `PauseCampaign` RPCs targeting *different* campaigns in
//!    the same store both succeed and both persist — no lost update.  A
//!    `Barrier(2)` synchronises the request start so neither RPC fires until
//!    both clients are ready, maximising overlap in the server-side
//!    read-modify-write window.  A naive load-mutate-write implementation
//!    without exclusive file locking would let one writer overwrite the
//!    other's update, leaving one campaign still Active after both calls
//!    return `Ok`; the reloaded-store assertion catches that defect.

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
    proto::{PauseCampaignRequest, foundry_client::FoundryClient, foundry_server::FoundryServer},
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Barrier, Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic::transport::Server;

// ── Test harness ──────────────────────────────────────────────────────────────

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

/// Build a [`FoundryService`] with an explicitly supplied `campaigns_path`.
///
/// This variant is used for the unreadable-store proof, where the campaigns
/// path is set to a directory so `CampaignStore::lock_exclusive` hits an I/O
/// error on load.
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

fn seed_campaigns(tmp: &NamedTempFile, campaigns: Vec<Campaign>) {
    CampaignStore {
        version: 1,
        campaigns,
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
        cycles_completed: 3,
        cycles_landed: 2,
        authorized_by: Some("owner".to_string()),
        agent_provider: None,
        last_run_event_id: Some("run-3".to_string()),
        owner_decisions: vec![],
        pending_run_result: None,
        objective_history: vec![],
    }
}

fn active_campaign_with_pending_result(name: &str) -> Campaign {
    Campaign {
        pending_run_result: Some(TaskRunCompletedPayload {
            project: "test-project".to_string(),
            success: false,
            landed: false,
            summary: "waiting for owner decision".to_string(),
            preservation_ref: Some("foundry-task/pause-generated-client-test-ref".to_string()),
            verdict: TaskVerdict::BlockedOnDecision {
                finding: "boundary choice required".to_string(),
                options: vec!["option-a".to_string(), "option-b".to_string()],
            },
            context: LoopContext {
                campaign: Some(name.to_string()),
                ..LoopContext::default()
            },
        }),
        ..active_campaign(name)
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

// ── Proof 1: Active campaign → response status=paused, correct fields, persisted ─

/// Pausing an Active campaign through the generated gRPC client must:
///
/// 1. Return a `PauseCampaignResponse` whose `CampaignDetail` has
///    `status="paused"`, the correct name, and the correct counters.
/// 2. Persist the campaign as `Paused` in the `CampaignStore` (reloaded
///    assertion — exact persisted state, not just the wire response).
///
/// An implementation that returns the wrong status in the response, or that
/// does not persist the state change, will fail one of these assertions.
#[tokio::test]
async fn generated_client_pause_active_returns_paused_detail_and_persists_state() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    seed_campaign(&tmp_campaigns, active_campaign("c"));
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let response = client
        .pause_campaign(PauseCampaignRequest {
            name: "c".to_string(),
        })
        .await
        .expect("pause of Active campaign must succeed")
        .into_inner();

    // ── Proof 1a: wire response carries the correct paused detail ────────────
    let detail = response.campaign.expect("PauseCampaignResponse must contain campaign detail");
    assert_eq!(detail.name, "c", "response name must match the seeded campaign name");
    assert_eq!(detail.status, "paused", "response status must be 'paused'");
    assert_eq!(
        detail.cycles_completed, 3,
        "response cycles_completed must reflect the seeded value"
    );
    assert_eq!(detail.cycles_landed, 2, "response cycles_landed must reflect the seeded value");
    assert_eq!(detail.max_cycles, 10, "response max_cycles must reflect the seeded budget");

    // ── Proof 1b: reloaded store reflects the persisted Paused state ─────────
    let stored = CampaignStore::load(tmp_campaigns.path()).expect("load store after pause");
    let campaign = stored.find("c").expect("campaign must still exist after pause");
    assert_eq!(
        campaign.status,
        CampaignStatus::Paused,
        "reloaded store must show campaign as Paused"
    );
    assert_eq!(
        campaign.cycles_completed, 3,
        "reloaded store cycles_completed must be unchanged by pause"
    );
    assert_eq!(
        campaign.budget.max_cycles, 10,
        "reloaded store max_cycles must be unchanged by pause"
    );
}

// ── Proof 2: pending_run_result is preserved byte-for-byte through pause ──────

/// Pausing a campaign that carries a `pending_run_result` must not clear or
/// overwrite it.  After the RPC returns, the reloaded store's
/// `pending_run_result` must be byte-for-byte identical to the seeded value
/// (compared via `serde_json::to_value`).
///
/// A status-only assertion would mask accidental result loss.
#[tokio::test]
async fn generated_client_pause_preserves_pending_run_result_byte_for_byte() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    let campaign = active_campaign_with_pending_result("c");

    // Capture the pending_run_result value before the call so we can compare
    // after the round-trip.
    let pending_before = serde_json::to_value(
        campaign
            .pending_run_result
            .as_ref()
            .expect("test fixture has pending_run_result"),
    )
    .expect("pending_run_result is JSON-serializable");

    seed_campaign(&tmp_campaigns, campaign);
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let response = client
        .pause_campaign(PauseCampaignRequest {
            name: "c".to_string(),
        })
        .await
        .expect("pause must succeed even with a pending_run_result")
        .into_inner();

    let detail = response.campaign.expect("PauseCampaignResponse must contain campaign detail");
    assert_eq!(detail.status, "paused", "response status must be 'paused'");

    // Reloaded store: pending_run_result must be byte-for-byte identical.
    let stored = CampaignStore::load(tmp_campaigns.path()).expect("load store after pause");
    let campaign = stored.find("c").expect("campaign must still exist");
    assert_eq!(
        campaign.status,
        CampaignStatus::Paused,
        "status must be Paused in the reloaded store"
    );

    let pending_after = serde_json::to_value(
        campaign
            .pending_run_result
            .as_ref()
            .expect("pending_run_result must still be present after pause"),
    )
    .expect("reloaded pending_run_result is JSON-serializable");

    assert_eq!(
        pending_before, pending_after,
        "pending_run_result must be byte-for-byte identical after pause \
         (pause must never clear or overwrite an in-flight outcome)"
    );
}

// ── Proof 3: Missing name in non-empty store → NOT_FOUND, store byte-identical ─

/// Pausing a campaign name absent from a non-empty store must return
/// `Code::NotFound` and leave the store byte-identical to before the call.
///
/// Using a non-empty store (seeded with a different campaign) distinguishes a
/// genuine `NOT_FOUND` from a misconfigured empty-store error path.
#[tokio::test]
async fn generated_client_pause_missing_name_returns_not_found_and_store_unchanged() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    // Seed a different campaign so the store is non-empty but "absent" is
    // genuinely missing.
    seed_campaign(&tmp_campaigns, active_campaign("present"));

    let store_before = std::fs::read(tmp_campaigns.path()).expect("read store before");
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .pause_campaign(PauseCampaignRequest {
            name: "absent".to_string(),
        })
        .await
        .expect_err("pausing a missing campaign must fail");

    assert_eq!(err.code(), Code::NotFound, "missing campaign must yield Code::NotFound");

    let store_after = std::fs::read(tmp_campaigns.path()).expect("read store after");
    assert_eq!(
        store_before, store_after,
        "campaign store must be byte-identical after a NOT_FOUND pause rejection"
    );
}

// ── Proofs 4 & 5: malformed store → FailedPrecondition; unreadable → Internal ─

/// A malformed store (invalid JSON) must return `Code::FailedPrecondition`.
/// An unreadable store (directory path) must return `Code::Internal`.
///
/// Both codes are asserted as distinct typed codes in the same test so the
/// malformed-vs-unreadable distinction over the wire is proven.
#[tokio::test]
async fn generated_client_pause_malformed_store_returns_failed_precondition_and_unreadable_returns_internal()
 {
    // ── Proof 4: malformed store → Code::FailedPrecondition ─────────────────
    {
        let (service, tmp_campaigns, _tmp_traces) = make_service();
        std::fs::write(tmp_campaigns.path(), b"{ not valid json }").expect("write malformed store");
        let addr = start_server(service).await;

        let mut client = FoundryClient::connect(addr).await.expect("connect");
        let err = client
            .pause_campaign(PauseCampaignRequest {
                name: "any".to_string(),
            })
            .await
            .expect_err("malformed store must fail");

        assert_eq!(
            err.code(),
            Code::FailedPrecondition,
            "malformed campaign store must yield Code::FailedPrecondition, got: {:?}",
            err.code()
        );
    }

    // ── Proof 5: unreadable store (directory path) → Code::Internal ─────────
    {
        // A directory path causes `lock_exclusive` → `CampaignStore::load` to
        // hit an I/O error when it tries to open the "file".
        let tmp_dir = tempfile::tempdir().expect("tempdir for unreadable store");
        let dir_path = tmp_dir.path().to_path_buf();
        let store_path_string = dir_path.display().to_string();

        let (service, _tmp_traces) = make_service_with_campaigns_path(dir_path);
        let addr = start_server(service).await;

        let mut client = FoundryClient::connect(addr).await.expect("connect");
        let err = client
            .pause_campaign(PauseCampaignRequest {
                name: "any".to_string(),
            })
            .await
            .expect_err("unreadable (directory) store must fail");

        assert_eq!(
            err.code(),
            Code::Internal,
            "unreadable campaign store must yield Code::Internal, got: {:?}",
            err.code()
        );

        // Also verify the path is not exposed in the Internal error.
        assert!(
            !err.message().contains(&store_path_string),
            "Internal error message must not expose the store path; got: {}",
            err.message()
        );
    }
}

// ── Proof 7: concurrent pause of two campaigns does not lose either update ────

/// Two concurrent `PauseCampaign` RPCs targeting *different* campaigns in the
/// same store must both succeed and both persist, with no lost update.
///
/// ## Why this test is discriminating
///
/// A `Barrier(2)` releases both futures simultaneously so neither RPC fires
/// until both clients are ready, maximising overlap in the server-side
/// read-modify-write window.  `tokio::join!` drives both concurrently;
/// with `flavor = "multi_thread"` they may run on separate OS threads.
///
/// A naive `load → mutate → write` implementation without exclusive file
/// locking would let one writer observe stale state and silently overwrite the
/// other's update.  After both calls return `Ok`, the store would still show
/// one campaign as `Active` — a lost update.  The reloaded-store assertion
/// below catches exactly that defect: if *either* campaign is not `Paused`
/// the test fails.
///
/// A sequential arrangement cannot satisfy this test by construction: the
/// `Barrier(2)` requires *both* futures to be concurrently live before either
/// can proceed past the barrier.  In a truly sequential execution the first
/// future would block at the barrier indefinitely because no second future is
/// running alongside it.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_pause_of_two_campaigns_does_not_lose_either_update() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();

    // Seed both campaigns into the same store file.
    seed_campaigns(
        &tmp_campaigns,
        vec![
            active_campaign("campaign-alpha"),
            active_campaign("campaign-beta"),
        ],
    );

    let addr = start_server(service).await;

    // Two independently connected clients — each holds a separate gRPC channel
    // so the server receives two genuinely concurrent incoming streams.
    let mut client_alpha =
        FoundryClient::connect(addr.clone()).await.expect("connect client-alpha");
    let mut client_beta = FoundryClient::connect(addr.clone()).await.expect("connect client-beta");

    // The barrier synchronises the request *start*: neither RPC fires until
    // both futures have arrived, maximising overlap in the server-side
    // read-modify-write window.
    let barrier = Arc::new(Barrier::new(2));
    let b_alpha = Arc::clone(&barrier);
    let b_beta = Arc::clone(&barrier);

    let (r_alpha, r_beta) = tokio::join!(
        async move {
            b_alpha.wait().await;
            client_alpha
                .pause_campaign(PauseCampaignRequest {
                    name: "campaign-alpha".to_string(),
                })
                .await
        },
        async move {
            b_beta.wait().await;
            client_beta
                .pause_campaign(PauseCampaignRequest {
                    name: "campaign-beta".to_string(),
                })
                .await
        },
    );

    // Both RPCs must succeed — a store-safety failure often surfaces as an
    // internal error on one arm, not a panic, so check both explicitly.
    let r_alpha = r_alpha.expect("pause of campaign-alpha must succeed");
    let r_beta = r_beta.expect("pause of campaign-beta must succeed");

    // Each response must identify the correct campaign and carry the correct status.
    let detail_alpha = r_alpha
        .into_inner()
        .campaign
        .expect("alpha response must contain a CampaignDetail");
    assert_eq!(detail_alpha.name, "campaign-alpha", "response name mismatch for alpha");
    assert_eq!(detail_alpha.status, "paused", "alpha response must report status=paused");

    let detail_beta = r_beta
        .into_inner()
        .campaign
        .expect("beta response must contain a CampaignDetail");
    assert_eq!(detail_beta.name, "campaign-beta", "response name mismatch for beta");
    assert_eq!(detail_beta.status, "paused", "beta response must report status=paused");

    // Reload the store and assert no lost update: BOTH campaigns must be Paused
    // and all unrelated fields must be preserved byte-for-byte.
    let stored =
        CampaignStore::load(tmp_campaigns.path()).expect("reload store after concurrent pause");

    let alpha = stored
        .find("campaign-alpha")
        .expect("campaign-alpha must still exist after concurrent pause");
    assert_eq!(
        alpha.status,
        CampaignStatus::Paused,
        "campaign-alpha must be Paused in the reloaded store (lost-update check)"
    );
    assert_eq!(alpha.cycles_completed, 3, "campaign-alpha cycles_completed must be unchanged");
    assert_eq!(alpha.cycles_landed, 2, "campaign-alpha cycles_landed must be unchanged");
    assert_eq!(alpha.budget.max_cycles, 10, "campaign-alpha max_cycles must be unchanged");
    assert_eq!(alpha.project, "test-project", "campaign-alpha project must be unchanged");
    assert_eq!(alpha.mission, "Test mission", "campaign-alpha mission must be unchanged");

    let beta = stored
        .find("campaign-beta")
        .expect("campaign-beta must still exist after concurrent pause");
    assert_eq!(
        beta.status,
        CampaignStatus::Paused,
        "campaign-beta must be Paused in the reloaded store (lost-update check)"
    );
    assert_eq!(beta.cycles_completed, 3, "campaign-beta cycles_completed must be unchanged");
    assert_eq!(beta.cycles_landed, 2, "campaign-beta cycles_landed must be unchanged");
    assert_eq!(beta.budget.max_cycles, 10, "campaign-beta max_cycles must be unchanged");
    assert_eq!(beta.project, "test-project", "campaign-beta project must be unchanged");
    assert_eq!(beta.mission, "Test mission", "campaign-beta mission must be unchanged");

    // The total campaign count must be unchanged — no phantom campaigns created
    // or dropped as a side-effect of the concurrent writes.
    assert_eq!(
        stored.campaigns.len(),
        2,
        "store must contain exactly the two seeded campaigns after concurrent pause"
    );
}

// ── Proof 6: no error message contains the store filesystem path ──────────────

/// Error messages returned by `PauseCampaign` must never expose the campaign
/// store filesystem path, regardless of the failure mode.
///
/// Verified for `NOT_FOUND` (missing campaign in a non-empty store) and
/// `FailedPrecondition` (malformed store).
#[tokio::test]
async fn generated_client_pause_error_messages_do_not_contain_store_path() {
    // ── NOT_FOUND: pausing a missing campaign ────────────────────────────────
    {
        let (service, tmp_campaigns, _tmp_traces) = make_service();
        seed_campaign(&tmp_campaigns, active_campaign("present"));
        let path_str = tmp_campaigns.path().display().to_string();
        let addr = start_server(service).await;

        let mut client = FoundryClient::connect(addr).await.expect("connect");
        let err = client
            .pause_campaign(PauseCampaignRequest {
                name: "absent".to_string(),
            })
            .await
            .expect_err("pausing a missing campaign must fail");

        assert!(
            !err.message().contains(&path_str),
            "NOT_FOUND message must not expose the store path; got: {}",
            err.message()
        );
    }

    // ── FailedPrecondition: malformed store ──────────────────────────────────
    {
        let (service, tmp_campaigns, _tmp_traces) = make_service();
        std::fs::write(tmp_campaigns.path(), b"{ not valid json }").expect("write malformed store");
        let path_str = tmp_campaigns.path().display().to_string();
        let addr = start_server(service).await;

        let mut client = FoundryClient::connect(addr).await.expect("connect");
        let err = client
            .pause_campaign(PauseCampaignRequest {
                name: "any".to_string(),
            })
            .await
            .expect_err("malformed store must fail");

        assert!(
            !err.message().contains(&path_str),
            "FailedPrecondition message must not expose the store path; got: {}",
            err.message()
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn generated_client_pause_persist_failure_returns_internal_and_store_unchanged() {
    let campaigns_dir = tempfile::tempdir().expect("campaigns tempdir");
    let campaigns_path = campaigns_dir.path().join("campaigns.json");
    CampaignStore {
        version: 1,
        campaigns: vec![active_campaign("c")],
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
        .pause_campaign(PauseCampaignRequest {
            name: "c".to_string(),
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
    assert_eq!(
        stored.find("c").expect("campaign remains present").status,
        CampaignStatus::Active
    );
}
