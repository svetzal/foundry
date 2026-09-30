//! Integration test for operator origin on a manual `AdvanceCampaign`.
//!
//! Evidence gates verified by this file:
//!
//! - An `AdvanceCampaign` RPC carrying an `operator_origin` dispatches a
//!   `CampaignAdvanceRequested` whose payload carries that value verbatim.
//! - The real `AdvanceCampaign` block, fed that very request, forwards the
//!   origin onto the `ExecutionRequested` it emits for the next cycle — the
//!   cycle event here is the block's own output, never a hand-built stand-in.
//! - That cycle records an item whose `origin`, read back through
//!   `GetWorkItem`, carries the operator text alongside the campaign name and
//!   the cycle number the block itself chose.
//! - An `AdvanceCampaign` that omits the field dispatches exactly as before: no
//!   `operator_origin` on either payload, and an item origin of exactly
//!   `campaign <name> cycle <n>`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{
    Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gateway::fakes::{FakeAgentGateway, FakeShellGateway};
use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::task_block::TaskBlock;
use foundryd::{
    proto::{
        AdvanceCampaignRequest, GetWorkItemRequest, foundry_client::FoundryClient,
        foundry_server::FoundryServer,
    },
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

struct Harness {
    service: FoundryService,
    campaigns: NamedTempFile,
    work_items_dir: TempDir,
    registry: Arc<RwLock<Registry>>,
    events: broadcast::Receiver<Event>,
    _traces: TempDir,
    /// The registered project's checkout. Formation reads it, so it has to
    /// exist for the real block to get past the registry lookup.
    _project: TempDir,
}

fn make_harness() -> Harness {
    let (event_tx, events) = broadcast::channel(64);
    let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));
    let traces = tempfile::tempdir().expect("tempdir for traces");
    let trace_writer = Arc::new(TraceWriter::new(traces.path().to_str().unwrap()));
    let trace_store = Arc::new(TraceStore::with_trace_writer(
        Duration::from_secs(60),
        Arc::clone(&trace_writer),
    ));
    let project = tempfile::tempdir().expect("tempdir for the project checkout");
    let registry = Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: "test-project".to_string(),
            path: project.path().to_str().unwrap().to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: String::new(),
            branch: "main".to_string(),
            skip: None,
            notes: None,
            actions: ActionFlags::default(),
            install: None,
            installs_skill: None,
            timeout_secs: None,
            audit_exceptions: Vec::new(),
            update_policy: None,
        }],
    }));
    let campaigns = NamedTempFile::new().expect("tempfile for campaigns");
    // A path inside a fresh directory rather than an empty file: the ledger
    // creates its own store, and an empty file is not a valid one.
    let work_items_dir = tempfile::tempdir().expect("tempdir for work items");
    let sentinels_file = NamedTempFile::new().expect("tempfile for sentinels");

    let ctx = RuntimeContext {
        engine,
        trace_store,
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer,
        event_tx,
        registry: Arc::clone(&registry),
    };
    let stores = StoreConfig {
        work_items_path: work_items_dir.path().join("work-items.json"),
        events_dir: std::path::PathBuf::new(),
        campaigns_path: campaigns.path().to_path_buf(),
        registry_path: NamedTempFile::new().expect("tempfile for registry").path().to_path_buf(),
        sentinels: Arc::new(RwLock::new(SentinelStore::default_seed())),
        sentinels_path: sentinels_file.path().to_path_buf(),
        scheduler_reload: Arc::new(Notify::new()),
    };
    Harness {
        service: FoundryService::new(ctx, stores),
        campaigns,
        work_items_dir,
        registry,
        events,
        _traces: traces,
        _project: project,
    }
}

fn active_campaign() -> Campaign {
    Campaign {
        name: "tidy-cli".to_string(),
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
        last_run_event_id: None,
        owner_decisions: vec![],
        pending_run_result: None,
        objective_history: vec![],
    }
}

async fn start_server(service: FoundryService) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("local addr").port();
    tokio::spawn(async move {
        Server::builder()
            .add_service(FoundryServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("gRPC server error");
    });
    tokio::task::yield_now().await;
    format!("http://127.0.0.1:{port}")
}

/// Wait for the `CampaignAdvanceRequested` the RPC dispatched.
async fn await_advance(events: &mut broadcast::Receiver<Event>) -> Event {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(event)) if event.event_type == EventType::CampaignAdvanceRequested => {
                return event;
            }
            Ok(Ok(_)) => {}
            _ => panic!("no CampaignAdvanceRequested observed within 5 s"),
        }
    }
}

/// Run the real `AdvanceCampaign` block over the request the daemon dispatched
/// and return the `ExecutionRequested` it emitted for the next cycle.
async fn advance_the_campaign(
    request: &Event,
    campaigns_path: std::path::PathBuf,
    registry: Arc<RwLock<Registry>>,
) -> Event {
    let block = foundry_blocks::blocks::AdvanceCampaign::new(
        FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Close the next gap in the CLI surface.\",\"reason\":\"gap\"}\n```",
        ),
        FakeShellGateway::success(),
        registry,
        campaigns_path,
    );
    block
        .execute(request)
        .await
        .expect("the advance block must form the next cycle")
        .events
        .into_iter()
        .find(|event| event.event_type == EventType::ExecutionRequested)
        .expect("the advance must dispatch the next cycle")
}

#[tokio::test]
async fn an_advance_with_an_origin_carries_it_to_the_cycle_and_into_the_ledger() {
    let harness = make_harness();
    CampaignStore {
        version: 1,
        campaigns: vec![active_campaign()],
    }
    .save(harness.campaigns.path())
    .expect("seed campaigns");
    let work_items_path = harness.work_items_dir.path().join("work-items.json");
    let campaigns_path = harness.campaigns.path().to_path_buf();
    let registry = Arc::clone(&harness.registry);
    let mut events = harness.events.resubscribe();
    let addr = start_server(harness.service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client
        .advance_campaign(AdvanceCampaignRequest {
            name: "tidy-cli".to_string(),
            operator_origin: "host workbench: by hand".to_string(),
        })
        .await
        .expect("advance must succeed");

    let advance = await_advance(&mut events).await;
    assert_eq!(
        advance.payload["operator_origin"], "host workbench: by hand",
        "the manual advance must carry the operator origin verbatim"
    );

    // The cycle is the one the real `AdvanceCampaign` block emits from that very
    // request — not a hand-built stand-in. Forwarding the origin is the block's
    // job, so the block has to be the thing that does it here.
    let cycle = advance_the_campaign(&advance, campaigns_path, Arc::clone(&registry)).await;
    assert_eq!(
        cycle.payload.get("operator_origin").and_then(serde_json::Value::as_str),
        Some("host workbench: by hand"),
        "the block must forward the advance's operator origin onto the cycle"
    );
    let cycle_number = cycle
        .payload
        .get("campaign_cycle")
        .and_then(serde_json::Value::as_u64)
        .expect("the dispatched cycle must name its number");

    foundry_blocks::blocks::RecordWorkItem::new(work_items_path.clone(), registry)
        .execute(&cycle)
        .await
        .expect("record the dispatched cycle");

    let stored = foundry_sdk::work_item::WorkItemStore::load(&work_items_path).unwrap();
    let item_id = stored.items[0].id.clone();
    let read_back = client
        .get_work_item(GetWorkItemRequest {
            id: item_id.clone(),
        })
        .await
        .expect("GetWorkItem must find the recorded cycle")
        .into_inner()
        .item
        .expect("GetWorkItemResponse must carry the item");

    assert_eq!(read_back.id, item_id);
    assert_eq!(
        read_back.origin,
        format!("campaign tidy-cli cycle {cycle_number} (host workbench: by hand)")
    );
}

#[tokio::test]
async fn an_advance_without_an_origin_dispatches_exactly_as_before() {
    let harness = make_harness();
    CampaignStore {
        version: 1,
        campaigns: vec![active_campaign()],
    }
    .save(harness.campaigns.path())
    .expect("seed campaigns");
    let work_items_path = harness.work_items_dir.path().join("work-items.json");
    let campaigns_path = harness.campaigns.path().to_path_buf();
    let registry = Arc::clone(&harness.registry);
    let registry_for_ledger = Arc::clone(&harness.registry);
    let mut events = harness.events.resubscribe();
    let addr = start_server(harness.service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    client
        .advance_campaign(AdvanceCampaignRequest {
            name: "tidy-cli".to_string(),
            operator_origin: String::new(),
        })
        .await
        .expect("advance must succeed");

    let advance = await_advance(&mut events).await;
    assert!(
        advance.payload.get("operator_origin").is_none(),
        "an advance with no operator origin must dispatch the payload it always did"
    );

    let cycle = advance_the_campaign(&advance, campaigns_path, registry).await;
    assert!(
        cycle.payload.get("operator_origin").is_none(),
        "the block must leave the key absent on a cycle nobody annotated"
    );
    let cycle_number = cycle
        .payload
        .get("campaign_cycle")
        .and_then(serde_json::Value::as_u64)
        .expect("the dispatched cycle must name its number");

    foundry_blocks::blocks::RecordWorkItem::new(work_items_path.clone(), registry_for_ledger)
        .execute(&cycle)
        .await
        .expect("record the dispatched cycle");
    let stored = foundry_sdk::work_item::WorkItemStore::load(&work_items_path).unwrap();
    let read_back = client
        .get_work_item(GetWorkItemRequest {
            id: stored.items[0].id.clone(),
        })
        .await
        .expect("GetWorkItem must find the recorded cycle")
        .into_inner()
        .item
        .expect("GetWorkItemResponse must carry the item");
    assert_eq!(
        read_back.origin,
        format!("campaign tidy-cli cycle {cycle_number}"),
        "an un-annotated cycle keeps exactly the origin it always had"
    );
}
