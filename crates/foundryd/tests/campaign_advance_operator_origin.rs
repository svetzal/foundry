//! Integration test for operator origin on a manual `AdvanceCampaign`.
//!
//! Evidence gates verified by this file:
//!
//! - `AdvanceCampaign` carrying an `operator_origin` dispatches a
//!   `CampaignAdvanceRequested` whose payload carries that value verbatim.
//! - The cycle that advance dispatches records an item whose `origin`, read back
//!   through `GetWorkItem`, carries the client hostname and the operator text
//!   alongside the campaign name and cycle.
//! - An `AdvanceCampaign` that omits the field dispatches exactly as before: no
//!   `operator_origin` on the payload at all.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{
    Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::task_block::TaskBlock;
use foundry_sdk::throttle::Throttle;
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
    let registry = Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: "test-project".to_string(),
            path: "/tmp/test-project".to_string(),
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

    // The cycle this advance dispatches records its item from that operator
    // origin, alongside the campaign name and cycle the dispatch itself implies.
    let cycle = Event::new(
        EventType::ExecutionRequested,
        "test-project".to_string(),
        Throttle::Full,
        serde_json::json!({
            "project": "test-project",
            "workflow": "task",
            "prompt": "Close the next gap in the CLI surface.",
            "campaign": "tidy-cli",
            "campaign_cycle": 4,
            "operator_origin": advance.payload["operator_origin"].as_str().unwrap(),
        }),
    );
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
    assert_eq!(read_back.origin, "campaign tidy-cli cycle 4 (host workbench: by hand)");
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
}
