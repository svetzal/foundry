//! Integration tests for `AdvanceCampaign` through the generated tonic client.
//!
//! Each test starts a real [`FoundryService`] bound to a temporary TCP port,
//! then calls the `AdvanceCampaign` RPC through the generated `FoundryClient`
//! so the full gRPC encode/decode cycle is exercised.
//!
//! Evidence gates verified by this file:
//!
//! - Advancing an Active campaign returns `AdvanceCampaignResponse` with the
//!   pre-advance campaign state (status=active, correct name and counters) AND
//!   the `CampaignAdvanceRequested` event is broadcast via the engine path.
//! - Advancing a Paused campaign returns `FAILED_PRECONDITION`; the reloaded
//!   store is byte-identical to before the call and no event was dispatched.
//! - Advancing an Escalated campaign returns `FAILED_PRECONDITION`; store
//!   byte-identical, no event dispatched.
//! - Advancing a Completed campaign returns `FAILED_PRECONDITION`; store
//!   byte-identical, no event dispatched.
//! - Advancing a name absent from a non-empty store returns `NOT_FOUND`; store
//!   byte-identical, no event dispatched.
//! - No error message exposes the campaign store filesystem path.

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
use foundryd::{
    proto::{AdvanceCampaignRequest, foundry_client::FoundryClient, foundry_server::FoundryServer},
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic::transport::Server;

// ── Test harness ─────────────────────────────────────────────────────────────

/// Construct a `FoundryService` backed by temporary files and return a
/// broadcast receiver so tests can observe emitted events.
fn make_service() -> (FoundryService, NamedTempFile, broadcast::Receiver<Event>, TempDir) {
    let (event_tx, rx) = broadcast::channel(64);
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

    (service, tmp_campaigns, rx, tmp_traces)
}

fn seed_campaign(tmp: &NamedTempFile, campaign: Campaign) {
    CampaignStore {
        version: 1,
        campaigns: vec![campaign],
    }
    .save(tmp.path())
    .expect("seed campaign store");
}

/// An active campaign with non-zero counters so the response fields are
/// distinguishable from defaults.
fn active_campaign(name: &str) -> Campaign {
    Campaign {
        name: name.to_string(),
        // "test-project" is intentionally absent from the empty registry so the
        // asynchronous formation that follows dispatch cannot perform a real land
        // that would race the event-observation assertions.
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
    }
}

fn paused_campaign(name: &str) -> Campaign {
    Campaign {
        status: CampaignStatus::Paused,
        ..active_campaign(name)
    }
}

fn escalated_campaign(name: &str) -> Campaign {
    Campaign {
        status: CampaignStatus::Escalated,
        escalation: vec!["budget limit reached".to_string()],
        ..active_campaign(name)
    }
}

fn completed_campaign(name: &str) -> Campaign {
    Campaign {
        status: CampaignStatus::Completed,
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

/// Drain all pending events from the receiver and assert that none of them are
/// `CampaignAdvanceRequested`.  For rejection cases the RPC handler returns
/// before dispatching any event, so the channel must be empty (or contain only
/// unrelated events).
fn assert_no_advance_event_dispatched(rx: &mut broadcast::Receiver<Event>) {
    while let Ok(event) = rx.try_recv() {
        assert_ne!(
            event.event_type,
            EventType::CampaignAdvanceRequested,
            "CampaignAdvanceRequested must NOT be broadcast for a rejected advance"
        );
    }
}

// ── Proof 1: Active campaign → response reflects pre-advance state + event ───

/// Advancing an Active campaign through the generated gRPC client must:
///
/// 1. Return `AdvanceCampaignResponse` whose `CampaignDetail` reflects the
///    pre-advance state: `status=active`, the correct name, and the correct
///    counters (`cycles_completed`, `cycles_landed`, `max_cycles`).
/// 2. Broadcast a `CampaignAdvanceRequested` event through the engine path,
///    observable via the `event_tx` broadcast receiver — not just as an
///    in-memory side-effect, but travelling the full engine dispatch path.
///
/// An implementation that returns a blank or post-mutation detail, or that
/// dispatches a different event type, will fail one of these assertions.
#[tokio::test]
async fn generated_client_advance_active_returns_pre_advance_state_and_dispatches_event() {
    let (service, tmp_campaigns, mut rx, _tmp_traces) = make_service();
    seed_campaign(&tmp_campaigns, active_campaign("c"));
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let response = client
        .advance_campaign(AdvanceCampaignRequest {
            name: "c".to_string(),
        })
        .await
        .expect("advance of Active campaign must succeed")
        .into_inner();

    // ── Proof 1a: response carries the pre-advance campaign state ────────────
    let detail = response.campaign.expect("AdvanceCampaignResponse must contain campaign detail");
    assert_eq!(detail.name, "c", "response name must match the seeded campaign name");
    assert_eq!(
        detail.status, "active",
        "response status must be 'active' (the state at dispatch time, not after)"
    );
    assert_eq!(
        detail.cycles_completed, 3,
        "response cycles_completed must reflect the seeded value"
    );
    assert_eq!(detail.cycles_landed, 2, "response cycles_landed must reflect the seeded value");
    assert_eq!(detail.max_cycles, 10, "response max_cycles must reflect the seeded budget");

    // ── Proof 1b: CampaignAdvanceRequested was broadcast via the engine path ─
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_advance = false;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) if event.event_type == EventType::CampaignAdvanceRequested => {
                saw_advance = true;
                break;
            }
            Ok(Ok(_)) => {} // unrelated event; keep draining
            _ => break,     // timeout or channel closed
        }
    }
    assert!(
        saw_advance,
        "CampaignAdvanceRequested must be broadcast via the engine path; \
         not observed within 5 s"
    );
}

// ── Proof 2: Paused campaign → FAILED_PRECONDITION, store unchanged, no event

/// Advancing a Paused campaign must return `FAILED_PRECONDITION`.
/// The reloaded store must be byte-identical to before the call (no partial
/// mutation), and no `CampaignAdvanceRequested` event must be broadcast.
///
/// An implementation that accepts an invalid transition or silently mutates
/// the store before rejecting will fail one of these assertions.
#[tokio::test]
async fn generated_client_advance_paused_returns_failed_precondition_store_unchanged_no_event() {
    let (service, tmp_campaigns, mut rx, _tmp_traces) = make_service();
    seed_campaign(&tmp_campaigns, paused_campaign("c"));

    let store_before = std::fs::read(tmp_campaigns.path()).expect("read store before");
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .advance_campaign(AdvanceCampaignRequest {
            name: "c".to_string(),
        })
        .await
        .expect_err("advancing a Paused campaign must fail");

    assert_eq!(err.code(), Code::FailedPrecondition);

    // Store must be byte-identical — no masked partial mutation.
    let store_after = std::fs::read(tmp_campaigns.path()).expect("read store after");
    assert_eq!(
        store_before, store_after,
        "campaign store must be byte-identical after a rejected advance of a Paused campaign"
    );

    // No CampaignAdvanceRequested event must have been dispatched.
    assert_no_advance_event_dispatched(&mut rx);
}

// ── Proof 3: Escalated campaign → FAILED_PRECONDITION, store unchanged, no event

/// Advancing an Escalated campaign must return `FAILED_PRECONDITION`.
/// The reloaded store must be byte-identical to before and no
/// `CampaignAdvanceRequested` event must be broadcast.
#[tokio::test]
async fn generated_client_advance_escalated_returns_failed_precondition_store_unchanged_no_event() {
    let (service, tmp_campaigns, mut rx, _tmp_traces) = make_service();
    seed_campaign(&tmp_campaigns, escalated_campaign("c"));

    let store_before = std::fs::read(tmp_campaigns.path()).expect("read store before");
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .advance_campaign(AdvanceCampaignRequest {
            name: "c".to_string(),
        })
        .await
        .expect_err("advancing an Escalated campaign must fail");

    assert_eq!(err.code(), Code::FailedPrecondition);

    let store_after = std::fs::read(tmp_campaigns.path()).expect("read store after");
    assert_eq!(
        store_before, store_after,
        "campaign store must be byte-identical after a rejected advance of an Escalated campaign"
    );

    assert_no_advance_event_dispatched(&mut rx);
}

// ── Proof 4: Completed campaign → FAILED_PRECONDITION, store unchanged, no event

/// Advancing a Completed campaign must return `FAILED_PRECONDITION`.
/// The reloaded store must be byte-identical to before and no
/// `CampaignAdvanceRequested` event must be broadcast.
#[tokio::test]
async fn generated_client_advance_completed_returns_failed_precondition_store_unchanged_no_event() {
    let (service, tmp_campaigns, mut rx, _tmp_traces) = make_service();
    seed_campaign(&tmp_campaigns, completed_campaign("c"));

    let store_before = std::fs::read(tmp_campaigns.path()).expect("read store before");
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .advance_campaign(AdvanceCampaignRequest {
            name: "c".to_string(),
        })
        .await
        .expect_err("advancing a Completed campaign must fail");

    assert_eq!(err.code(), Code::FailedPrecondition);

    let store_after = std::fs::read(tmp_campaigns.path()).expect("read store after");
    assert_eq!(
        store_before, store_after,
        "campaign store must be byte-identical after a rejected advance of a Completed campaign"
    );

    assert_no_advance_event_dispatched(&mut rx);
}

// ── Proof 5: Missing name in non-empty store → NOT_FOUND, store unchanged, no event

/// Advancing a campaign name absent from a non-empty store must return
/// `NOT_FOUND`.  The store must be byte-identical to before and no
/// `CampaignAdvanceRequested` event must be broadcast.
///
/// Using a non-empty store (seeded with a different campaign) distinguishes a
/// genuine `NOT_FOUND` from a misconfigured empty-store error path.
#[tokio::test]
async fn generated_client_advance_missing_name_returns_not_found_store_unchanged_no_event() {
    let (service, tmp_campaigns, mut rx, _tmp_traces) = make_service();
    // Seed a different campaign so the store is non-empty but "absent" is genuinely missing.
    seed_campaign(&tmp_campaigns, active_campaign("present"));

    let store_before = std::fs::read(tmp_campaigns.path()).expect("read store before");
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .advance_campaign(AdvanceCampaignRequest {
            name: "absent".to_string(),
        })
        .await
        .expect_err("advancing a missing campaign must fail");

    assert_eq!(err.code(), Code::NotFound);

    let store_after = std::fs::read(tmp_campaigns.path()).expect("read store after");
    assert_eq!(
        store_before, store_after,
        "campaign store must be byte-identical after a NOT_FOUND advance"
    );

    assert_no_advance_event_dispatched(&mut rx);
}

// ── Proof 6: No error message exposes the store filesystem path ───────────────

/// Error messages returned by `AdvanceCampaign` must never expose the campaign
/// store filesystem path, regardless of the failure mode.
///
/// Verified for both the `NOT_FOUND` path (missing campaign in a non-empty
/// store) and the `FAILED_PRECONDITION` path (wrong status transition).
#[tokio::test]
async fn generated_client_advance_error_messages_do_not_contain_store_path() {
    // ── FAILED_PRECONDITION: advancing a Paused campaign ────────────────────
    {
        let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
        seed_campaign(&tmp_campaigns, paused_campaign("c"));
        let path_str = tmp_campaigns.path().display().to_string();
        let addr = start_server(service).await;

        let mut client = FoundryClient::connect(addr).await.expect("connect");
        let err = client
            .advance_campaign(AdvanceCampaignRequest {
                name: "c".to_string(),
            })
            .await
            .expect_err("advancing a Paused campaign must fail");

        assert!(
            !err.message().contains(&path_str),
            "FAILED_PRECONDITION message must not expose the store path; got: {}",
            err.message()
        );
    }

    // ── NOT_FOUND: advancing a missing campaign ──────────────────────────────
    {
        let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
        seed_campaign(&tmp_campaigns, active_campaign("present"));
        let path_str = tmp_campaigns.path().display().to_string();
        let addr = start_server(service).await;

        let mut client = FoundryClient::connect(addr).await.expect("connect");
        let err = client
            .advance_campaign(AdvanceCampaignRequest {
                name: "absent".to_string(),
            })
            .await
            .expect_err("advancing a missing campaign must fail");

        assert!(
            !err.message().contains(&path_str),
            "NOT_FOUND message must not expose the store path; got: {}",
            err.message()
        );
    }
}
