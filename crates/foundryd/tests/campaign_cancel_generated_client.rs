//! Integration tests for `CancelCampaign` through the generated tonic client.
//!
//! A real [`FoundryService`] is bound to a temporary TCP port and the RPC is
//! driven through the generated `FoundryClient`, so the full gRPC
//! encode/decode and engine dispatch path is exercised.
//!
//! Evidence gates verified by this file:
//!
//! - Cancelling dispatches a `CampaignCancelled` root event carrying a minted
//!   32-char-hex `trace_id`, a minted 16-char-hex `span_id`, and no
//!   `parent_span_id` — a manual cancellation opens its own root chain.
//! - The disposition flags survive the round trip into the event payload, so
//!   `DisposeCampaignWork` can act on what the operator actually asked for.
//! - Cancellation is idempotent and emits no second event.
//! - An unauthorized campaign can still be cancelled. `complete` requires an
//!   owner; cancel deliberately does not, or an unauthorized campaign would be
//!   stranded with no terminal state reachable at all.
//! - A completed campaign is refused with `FAILED_PRECONDITION`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{
    Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::CampaignCancelledPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundryd::{
    proto::{CancelCampaignRequest, foundry_client::FoundryClient, foundry_server::FoundryServer},
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

// ── Test harness ─────────────────────────────────────────────────────────────

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

    (FoundryService::new(ctx, stores), tmp_campaigns, rx, tmp_traces)
}

fn seed_campaign(tmp: &NamedTempFile, campaign: Campaign) {
    CampaignStore {
        version: 1,
        campaigns: vec![campaign],
    }
    .save(tmp.path())
    .expect("seed campaign store");
}

fn campaign(name: &str, status: CampaignStatus, authorized: bool) -> Campaign {
    Campaign {
        name: name.to_string(),
        // Intentionally absent from the empty registry so any asynchronous
        // follow-on cannot perform real git work during the assertions.
        project: "test-project".to_string(),
        mission: "Test mission".to_string(),
        intent_refs: vec![],
        context_paths: vec![],
        done_evidence: vec![DoneEvidence::Review {
            statement: "done".to_string(),
        }],
        budget: CampaignBudget { max_cycles: 10 },
        escalation: vec![],
        status,
        cycles_completed: 3,
        cycles_landed: 2,
        authorized_by: authorized.then(|| "owner".to_string()),
        agent_provider: None,
        last_run_event_id: Some("run-3".to_string()),
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

/// Drain the broadcast until a `CampaignCancelled` arrives, or time out.
async fn next_cancelled(rx: &mut broadcast::Receiver<Event>) -> Option<Event> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) if event.event_type == EventType::CampaignCancelled => {
                return Some(event);
            }
            Ok(Ok(_)) => {}
            _ => return None,
        }
    }
}

fn request(name: &str, terminate_now: bool, discard_work: bool) -> CancelCampaignRequest {
    CancelCampaignRequest {
        name: name.to_string(),
        reason: "abandoned in favour of a different approach".to_string(),
        terminate_now,
        discard_work,
    }
}

// ── Proofs ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn cancel_dispatches_a_traceable_root_carrying_the_disposition() {
    let (service, tmp_campaigns, mut rx, _traces) = make_service();
    seed_campaign(&tmp_campaigns, campaign("c", CampaignStatus::Active, true));
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let response = client
        .cancel_campaign(request("c", true, true))
        .await
        .expect("cancel of an active campaign must succeed")
        .into_inner();

    assert_eq!(response.campaign.expect("campaign detail").status, "cancelled");
    assert!(!response.event_id.is_empty(), "a real cancellation reports its event id");

    let root = next_cancelled(&mut rx).await.expect("CampaignCancelled must be broadcast");

    let trace_id = root.trace_id.clone().expect("cancellation root must mint a trace id");
    assert_eq!(trace_id.len(), 32, "trace id must be 32-char hex, got {trace_id:?}");
    assert!(trace_id.chars().all(|c| c.is_ascii_hexdigit()), "got {trace_id:?}");

    let span_id = root.span_id.clone().expect("cancellation root must mint a span id");
    assert_eq!(span_id.len(), 16, "span id must be 16-char hex, got {span_id:?}");
    assert!(span_id.chars().all(|c| c.is_ascii_hexdigit()), "got {span_id:?}");

    assert!(root.parent_span_id.is_none(), "a manual cancellation root has no parent span");

    // The flags must survive into the payload — `DisposeCampaignWork` reads
    // them to decide whether to preserve or discard the orphaned worktree.
    let payload: CampaignCancelledPayload =
        root.parse_payload().expect("payload must deserialize as a cancellation");
    assert!(payload.terminated_now);
    assert!(payload.discard_work);
    assert_eq!(payload.terminal.campaign, "c");
    assert_eq!(payload.terminal.cycles_completed, 3);
    assert_eq!(payload.terminal.cycles_landed, 2);
    assert_eq!(payload.terminal.reason, "abandoned in favour of a different approach");
}

#[tokio::test]
async fn cancelling_twice_is_idempotent_and_emits_no_second_event() {
    let (service, tmp_campaigns, mut rx, _traces) = make_service();
    seed_campaign(&tmp_campaigns, campaign("c", CampaignStatus::Active, true));
    let path = tmp_campaigns.path().to_path_buf();
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client.cancel_campaign(request("c", false, false)).await.expect("first cancel");
    next_cancelled(&mut rx).await.expect("first cancel emits");

    let second = client
        .cancel_campaign(request("c", false, false))
        .await
        .expect("re-cancelling must not error")
        .into_inner();

    assert_eq!(second.campaign.expect("detail").status, "cancelled");
    assert!(second.event_id.is_empty(), "a no-op cancellation reports no event id");
    assert!(
        next_cancelled(&mut rx).await.is_none(),
        "a second cancellation must not emit a duplicate terminal event"
    );

    let store = CampaignStore::load(&path).expect("load store");
    let stored = store.find("c").expect("campaign");
    assert_eq!(stored.status, CampaignStatus::Cancelled);
    assert_eq!(
        stored.owner_decisions.len(),
        1,
        "the audit record must not gain a duplicate entry"
    );
}

/// Unlike `complete`, cancel must not require an owner — otherwise an
/// unauthorized campaign can never reach any terminal state.
#[tokio::test]
async fn an_unauthorized_campaign_can_still_be_cancelled() {
    let (service, tmp_campaigns, _rx, _traces) = make_service();
    seed_campaign(&tmp_campaigns, campaign("c", CampaignStatus::Staged, false));
    let path = tmp_campaigns.path().to_path_buf();
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client
        .cancel_campaign(request("c", false, false))
        .await
        .expect("an unauthorized campaign must still be stoppable");

    let store = CampaignStore::load(&path).expect("load store");
    let stored = store.find("c").expect("campaign");
    assert_eq!(stored.status, CampaignStatus::Cancelled);
    // Nothing to attach an owner decision to; the reason still reached the event.
    assert!(stored.owner_decisions.is_empty());
}

#[tokio::test]
async fn cancel_rejects_empty_reason_unknown_campaign_and_completed_campaign() {
    let (service, tmp_campaigns, _rx, _traces) = make_service();
    seed_campaign(&tmp_campaigns, campaign("done", CampaignStatus::Completed, true));
    let addr = start_server(service).await;
    let mut client = FoundryClient::connect(addr).await.expect("connect");

    let empty = client
        .cancel_campaign(CancelCampaignRequest {
            name: "done".to_string(),
            reason: "   ".to_string(),
            terminate_now: false,
            discard_work: false,
        })
        .await
        .expect_err("an empty reason must be refused");
    assert_eq!(empty.code(), tonic::Code::InvalidArgument);

    let missing = client
        .cancel_campaign(request("nope", false, false))
        .await
        .expect_err("an unknown campaign must be refused");
    assert_eq!(missing.code(), tonic::Code::NotFound);

    let completed = client
        .cancel_campaign(request("done", false, false))
        .await
        .expect_err("a completed campaign has nothing in flight to cancel");
    assert_eq!(completed.code(), tonic::Code::FailedPrecondition);
}
