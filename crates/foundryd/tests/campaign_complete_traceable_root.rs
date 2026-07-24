//! Integration test for `CompleteCampaign` through the generated tonic client.
//!
//! A real [`FoundryService`] is bound to a temporary TCP port and the
//! `CompleteCampaign` RPC is driven through the generated `FoundryClient`, so
//! the full gRPC encode/decode and engine dispatch path is exercised — not a
//! mock of the emission path.
//!
//! Evidence gate verified by this file:
//!
//! - Completing an authorized campaign dispatches a `CampaignCompleted` root
//!   event through the engine that carries a minted 32-char-hex `trace_id`, a
//!   minted 16-char-hex `span_id`, and no `parent_span_id`.
//!
//! Live gap this guards against: `complete()` built the terminal event with a
//! bare `Event::new` and dispatched it via `spawn_workflow`; because
//! `stamp_context` only inherits, the terminal-surfacing block that sinks on
//! `CampaignCompleted` propagated `None` for both ids.

#![allow(clippy::unwrap_used, clippy::expect_used)]

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
    proto::{
        CompleteCampaignRequest, foundry_client::FoundryClient, foundry_server::FoundryServer,
    },
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

// ── Test harness ─────────────────────────────────────────────────────────────

/// Construct a `FoundryService` backed by temporary files and return a
/// broadcast receiver so the test can observe emitted events.
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

/// An authorized, active campaign — the precondition `complete()` requires to
/// emit a terminal event.
fn authorized_campaign(name: &str) -> Campaign {
    Campaign {
        name: name.to_string(),
        // Intentionally absent from the empty registry so the asynchronous
        // formation that may follow dispatch cannot perform a real land that
        // would race the event-observation assertions.
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

// ── Proof: the dispatched terminal root carries a trace ───────────────────────

/// Completing an authorized campaign must dispatch a `CampaignCompleted` root
/// that mints a trace and a root span.
///
/// `stamp_context` only ever *inherits* the ids from the trigger, so a bare
/// root propagates `None` through the terminal-surfacing block and every event
/// it emits — leaving the manual-completion path unreconstructable by
/// `foundry trace`, exactly like the pre-fix `advance` path.
#[tokio::test]
async fn generated_client_complete_dispatches_a_traceable_root_event() {
    let (service, tmp_campaigns, mut rx, _tmp_traces) = make_service();
    seed_campaign(&tmp_campaigns, authorized_campaign("c"));
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client
        .complete_campaign(CompleteCampaignRequest {
            name: "c".to_string(),
            reason: "wrapped up out of band".to_string(),
        })
        .await
        .expect("complete of an authorized campaign must succeed");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut root = None;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) if event.event_type == EventType::CampaignCompleted => {
                root = Some(event);
                break;
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    let root = root.expect("CampaignCompleted must be broadcast via the engine path");

    let trace_id = root.trace_id.expect(
        "completion root must mint a trace id; without one the terminal chain is untraceable",
    );
    assert_eq!(
        trace_id.len(),
        32,
        "trace id must be the standard 32-char hex form, got {trace_id:?}"
    );
    assert!(
        trace_id.chars().all(|c| c.is_ascii_hexdigit()),
        "trace id must be lowercase hex, got {trace_id:?}"
    );

    let span_id = root
        .span_id
        .expect("completion root must mint a span id; it opens the terminal chain");
    assert_eq!(
        span_id.len(),
        16,
        "span id must be the standard 16-char hex form, got {span_id:?}"
    );
    assert!(
        span_id.chars().all(|c| c.is_ascii_hexdigit()),
        "span id must be lowercase hex, got {span_id:?}"
    );

    assert!(root.parent_span_id.is_none(), "a manual completion root has no parent span");
}
