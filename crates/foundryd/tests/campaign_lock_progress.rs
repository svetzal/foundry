//! Campaign flock contention must leave a single Tokio worker free to run formations and reads.
//!
//! A formation no longer holds the flock across its agent, so here an outside
//! process (an `--offline` CLI) holds it instead while controls queue on it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use foundry_blocks::{
    blocks::AdvanceCampaign,
    gateway::{AgentGateway, AgentRequest, AgentResponse},
    trace_writer::TraceWriter,
};
use foundry_engine::engine::Engine;
use foundry_sdk::gateway::fakes::FakeShellGateway;
use foundry_sdk::{
    campaign::{Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence},
    registry::Registry,
    sentinel::SentinelStore,
};
use foundryd::{
    proto::{
        AdvanceCampaignRequest, ListCampaignsRequest, PauseCampaignRequest, foundry_server::Foundry,
    },
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use std::{
    pin::Pin,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, broadcast};
use tonic::Request;

struct ControlledAgent {
    first: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl AgentGateway for ControlledAgent {
    fn invoke<'a>(
        &'a self,
        _request: &'a AgentRequest,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            if self.first.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(AgentResponse::success(
                r#"{"decision":"escalate","reason":"owner review needed"}"#,
            ))
        })
    }
}

fn active_campaign(name: &str) -> Campaign {
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
        escalation: vec![],
        status: CampaignStatus::Active,
        cycles_completed: 0,
        cycles_landed: 0,
        authorized_by: Some("owner".to_string()),
        agent_provider: None,
        last_run_event_id: None,
        owner_decisions: vec![],
        pending_run_result: None,
        objective_history: vec![],
        open_escalation: None,
        writable_repositories: vec![],
    }
}

/// How long the driver waits before declaring the only worker stalled.
const HANG_GUARD: Duration = Duration::from_secs(120);

#[test]
fn campaign_controls_wait_without_stalling_formation_or_reads() {
    // The test driver runs outside the only worker, so its deadline can report
    // a regression even when a synchronous flock parks that worker forever.
    // The deadline is only a hang guard, never a speed assertion: a correct
    // run's outcome is fixed by ordering alone (the read completes while the
    // flock is still held), so the bound sits far above any instrumented
    // (coverage) run and fires only when the worker never progresses.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    runtime.spawn(async move {
        prove_progress().await;
        finished_tx.send(()).unwrap();
    });
    let result = finished_rx.recv_timeout(HANG_GUARD);
    // A regressed worker cannot unwind; do not let runtime shutdown hide the
    // bounded failure by waiting forever for that worker.
    runtime.shutdown_background();
    result.expect("campaign contention stalled the Tokio worker");
}

fn formation_service(
    dir: &tempfile::TempDir,
    path: &std::path::Path,
    agent: Arc<ControlledAgent>,
) -> (Arc<FoundryService>, broadcast::Receiver<foundry_sdk::event::Event>) {
    let registry: Registry = serde_json::from_value(serde_json::json!({
        "version": 2, "projects": [{"name":"test-project", "path":dir.path(), "stack":"rust", "agent":"codex", "repo":"", "branch":"main"}]
    })).unwrap();
    let registry = Arc::new(RwLock::new(registry));
    let (event_tx, events) = broadcast::channel(64);
    let mut engine = Engine::new().with_event_broadcaster(event_tx.clone());
    engine.register(Box::new(AdvanceCampaign::new(
        agent,
        FakeShellGateway::success(),
        registry.clone(),
        path.to_path_buf(),
        path.with_file_name("work_items.json"),
    )));
    let trace_writer = Arc::new(TraceWriter::new(dir.path().to_str().unwrap()));
    let service = Arc::new(FoundryService::new(
        RuntimeContext {
            engine: Arc::new(engine),
            trace_store: Arc::new(TraceStore::with_trace_writer(
                Duration::from_secs(60),
                trace_writer.clone(),
            )),
            workflow_tracker: Arc::new(WorkflowTracker::new()),
            trace_writer,
            event_tx,
            registry,
        },
        StoreConfig {
            campaigns_path: path.to_path_buf(),
            work_items_path: dir.path().join("work.json"),
            events_dir: dir.path().join("events"),
            registry_path: dir.path().join("registry.json"),
            sentinels: Arc::new(RwLock::new(SentinelStore::default_seed())),
            sentinels_path: dir.path().join("sentinels.json"),
            scheduler_reload: Arc::new(Notify::new()),
        },
    ));
    (service, events)
}

async fn prove_progress() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("campaigns.json");
    let mut store = CampaignStore::default();
    store.add(active_campaign("forming")).unwrap();
    store.add(active_campaign("second")).unwrap();
    store.save(&path).unwrap();
    let agent = Arc::new(ControlledAgent {
        first: AtomicBool::new(true),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let (service, mut events) = formation_service(&dir, &path, agent.clone());
    service
        .advance_campaign(Request::new(AdvanceCampaignRequest {
            name: "forming".into(),
            operator_origin: String::new(),
        }))
        .await
        .unwrap();
    agent.entered.notified().await; // The formation is parked in its agent.
    // An outside holder of the flock, as an `--offline` command would be.
    let (release_flock, flock_released) = std::sync::mpsc::channel::<()>();
    let (flock_held, held) = std::sync::mpsc::channel::<()>();
    let holder_path = path.clone();
    let holder = std::thread::spawn(move || {
        let _guard = CampaignStore::lock_exclusive(&holder_path).unwrap();
        flock_held.send(()).unwrap();
        flock_released.recv().unwrap();
    });
    held.recv().unwrap();
    let second = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .advance_campaign(Request::new(AdvanceCampaignRequest {
                    name: "second".into(),
                    operator_origin: String::new(),
                }))
                .await
        })
    };
    let pause = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .pause_campaign(Request::new(PauseCampaignRequest {
                    name: "forming".into(),
                }))
                .await
        })
    };
    // Let both control requests reach the held lock before scheduling a read.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!second.is_finished());
    assert!(!pause.is_finished());
    let read = {
        let service = service.clone();
        tokio::spawn(async move {
            service
                .list_campaigns(Request::new(ListCampaignsRequest {
                    project: String::new(),
                }))
                .await
        })
    };
    // The flock is still held: the read can only complete here if waiting on
    // it parks no worker. A stalled worker never gets here; the hang guard
    // reports it.
    let response = read.await.unwrap().unwrap();
    assert_eq!(response.into_inner().campaigns.len(), 2);
    release_flock.send(()).unwrap();
    holder.join().unwrap();
    // Both controls finish while the first formation is still in its agent:
    // a formation does not hold the flock.
    second.await.unwrap().unwrap();
    let paused = pause.await.unwrap().unwrap().into_inner().campaign.unwrap();
    assert_eq!(paused.status, "paused");
    agent.release.notify_one();
    // The second formation must also finish; waiting controls must not hold up
    // the agent output or the next formation's acquisition of the same flock.
    loop {
        let event = events.recv().await.unwrap();
        if event.event_type == foundry_sdk::event::EventType::CampaignAdvanceCompleted
            && event.payload.get("campaign").and_then(serde_json::Value::as_str) == Some("second")
        {
            break;
        }
    }
    let store = CampaignStore::load(&path).unwrap();
    // Paused while it formed: the formed escalation is discarded, the pause stands.
    let forming = store.find("forming").unwrap();
    assert_eq!(forming.status, CampaignStatus::Paused);
    assert!(forming.open_escalation.is_none());
    assert_eq!(store.find("second").unwrap().status, CampaignStatus::Escalated);
}
