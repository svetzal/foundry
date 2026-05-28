use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Notify;
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

use foundry_core::sentinel::{SentinelStore, merge_default_seed_into};

mod agent_stream;
mod blocks;
mod charter;
mod engine;
mod event_writer;
mod gate_file;
mod gate_runner;
mod gateway;
mod gather_store;
mod legacy_event_check;
mod orchestrator;
mod scanner;
mod scheduler;
mod service;
mod shell;
mod span_context;
mod summary;
mod trace_store;
mod trace_writer;
mod workflow_tracker;

pub mod proto {
    #![allow(clippy::all, clippy::pedantic)]
    tonic::include_proto!("foundry");
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("foundryd=info".parse()?))
        .init();

    let events_dir = foundry_core::paths::events_dir();
    if let Some(legacy) = legacy_event_check::detect_legacy_event_names(&events_dir) {
        eprintln!(
            "ERROR: foundryd 0.17.0 detected legacy event-type name '{legacy}' on disk.\n\
             Run scripts/migrate-event-names.sh once to backfill, then restart foundryd."
        );
        std::process::exit(2);
    }

    let registry_path = foundry_core::paths::registry_path();
    let registry = match foundry_core::registry::Registry::load(&registry_path) {
        Ok(r) => {
            tracing::info!(path = %registry_path.display(), projects = r.active_projects().len(), "registry loaded");
            Arc::new(RwLock::new(r))
        }
        Err(e) => {
            tracing::warn!(path = %registry_path.display(), error = %e, "registry not found, using empty registry");
            Arc::new(RwLock::new(foundry_core::registry::Registry {
                version: 2,
                projects: vec![],
            }))
        }
    };

    let event_writer = Arc::new(event_writer::EventWriter::new(events_dir));

    let traces_dir = foundry_core::paths::traces_dir();
    let trace_writer = Arc::new(trace_writer::TraceWriter::new(
        traces_dir.to_str().expect("FOUNDRY_TRACES_DIR must be valid UTF-8"),
    ));

    let audits_dir = foundry_core::paths::audits_dir()
        .to_str()
        .expect("FOUNDRY_AUDITS_DIR must be valid UTF-8")
        .to_string();

    let (event_tx, _) = tokio::sync::broadcast::channel(256);

    let engine =
        register_blocks(&registry, event_writer, &event_tx, trace_writer.clone(), audits_dir);

    let engine = Arc::new(engine);
    let trace_store = Arc::new(trace_store::TraceStore::with_trace_writer(
        Duration::from_secs(3600),
        trace_writer.clone(),
    ));
    let workflow_tracker = Arc::new(workflow_tracker::WorkflowTracker::new());

    // Sentinel store — load (or auto-seed on first start) the file-backed
    // schedule that replaces launchd/com.mojility.foundry-maintenance.plist.
    let sentinels_path = foundry_core::paths::sentinels_path();
    let sentinels = Arc::new(RwLock::new(load_or_seed_sentinels(&sentinels_path)?));
    let scheduler_reload = Arc::new(Notify::new());

    spawn_scheduler(
        &sentinels,
        &scheduler_reload,
        &engine,
        &trace_store,
        &workflow_tracker,
        &trace_writer,
        &event_tx,
        &registry,
    );

    let service = service::FoundryService::new(
        engine,
        trace_store,
        event_tx,
        workflow_tracker,
        trace_writer,
        registry,
        registry_path,
        sentinels,
        sentinels_path,
        scheduler_reload,
    );

    let addr = "127.0.0.1:50051".parse()?;
    tracing::info!("foundryd listening on {addr}");

    Server::builder()
        .add_service(proto::foundry_server::FoundryServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}

/// Load the sentinel store from disk, auto-seeding the default canonical set
/// on first start. On subsequent starts the loaded store is additively
/// merged with the current canonical seed so new Foundry releases that ship
/// extra default sentinels reach existing installs automatically without
/// touching user toggles or hand-edited entries.
fn load_or_seed_sentinels(path: &std::path::Path) -> Result<SentinelStore> {
    match SentinelStore::load(path) {
        Ok(mut s) => {
            tracing::info!(
                path = %path.display(),
                count = s.sentinels.len(),
                "sentinels loaded",
            );
            let appended = merge_default_seed_into(&mut s);
            if appended {
                s.save(path).map_err(|save_err| {
                    anyhow::anyhow!(
                        "failed to persist merged sentinel seed at {}: {save_err}",
                        path.display()
                    )
                })?;
                tracing::info!(
                    path = %path.display(),
                    count = s.sentinels.len(),
                    "appended new canonical sentinel entries from default seed",
                );
            }
            Ok(s)
        }
        Err(load_err) => {
            let seed = SentinelStore::default_seed();
            seed.save(path).map_err(|save_err| {
                anyhow::anyhow!(
                    "failed to seed sentinels at {}: {save_err} (original load error: {load_err})",
                    path.display()
                )
            })?;
            tracing::info!(
                path = %path.display(),
                count = seed.sentinels.len(),
                "sentinels seeded on first start",
            );
            Ok(seed)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_scheduler(
    sentinels: &Arc<RwLock<SentinelStore>>,
    reload: &Arc<Notify>,
    engine: &Arc<engine::Engine>,
    trace_store: &Arc<trace_store::TraceStore>,
    workflow_tracker: &Arc<workflow_tracker::WorkflowTracker>,
    trace_writer: &Arc<trace_writer::TraceWriter>,
    event_tx: &tokio::sync::broadcast::Sender<foundry_core::event::Event>,
    registry: &Arc<RwLock<foundry_core::registry::Registry>>,
) {
    // Pipe sentinel firings through the same trace/workflow_tracker
    // machinery the gRPC `emit()` handler uses.
    let emit: scheduler::EmitFn = {
        let engine = Arc::clone(engine);
        let trace_store = Arc::clone(trace_store);
        let workflow_tracker = Arc::clone(workflow_tracker);
        let trace_writer = Arc::clone(trace_writer);
        let event_tx = event_tx.clone();
        let registry = Arc::clone(registry);
        Arc::new(move |event| {
            service::spawn_workflow(
                event,
                Arc::clone(&engine),
                Arc::clone(&trace_store),
                Arc::clone(&workflow_tracker),
                Arc::clone(&trace_writer),
                event_tx.clone(),
                Arc::clone(&registry),
            );
        })
    };

    let scheduler = scheduler::Scheduler::new(Arc::clone(sentinels), Arc::clone(reload), emit);
    tokio::spawn(scheduler.run());
}

fn register_blocks(
    registry: &Arc<RwLock<foundry_core::registry::Registry>>,
    event_writer: Arc<event_writer::EventWriter>,
    event_tx: &tokio::sync::broadcast::Sender<foundry_core::event::Event>,
    trace_writer: Arc<trace_writer::TraceWriter>,
    audits_dir: String,
) -> engine::Engine {
    let mut engine = engine::Engine::new()
        .with_event_writer(event_writer)
        .with_event_broadcaster(event_tx.clone());
    engine.register(Box::new(orchestrator::FanOutMaintenance::new(registry.clone())));
    engine.register(Box::new(blocks::ValidateProject::new(registry.clone())));
    engine.register(Box::new(blocks::ComposeGreeting));
    engine.register(Box::new(blocks::DeliverGreeting));
    engine.register(Box::new(blocks::ScanDependencies::new(registry.clone())));
    engine.register(Box::new(blocks::AuditReleaseTag::with_registry(registry.clone())));
    engine.register(Box::new(blocks::AuditMainBranch::new(registry.clone())));
    // Agent gateway for blocks that invoke Claude CLI
    let shell_for_agent: Arc<dyn gateway::ShellGateway> = Arc::new(gateway::ProcessShellGateway);
    let agent: Arc<dyn gateway::AgentGateway> =
        Arc::new(gateway::ClaudeAgentGateway::new_with_streaming(
            shell_for_agent,
            Arc::new(agent_stream::ProcessAgentStreamRunner),
            foundry_core::paths::agent_sessions_dir(),
            event_tx.clone(),
        ));
    engine.register(Box::new(blocks::RemediateVulnerability::new(agent.clone(), registry.clone())));
    engine.register(Box::new(blocks::CommitAndPush::new(registry.clone())));
    engine.register(Box::new(blocks::cut_release_step(registry.clone())));
    engine.register(Box::new(blocks::execute_release_step(agent.clone(), registry.clone())));
    engine.register(Box::new(blocks::WatchPipeline::new(registry.clone())));
    engine.register(Box::new(blocks::InstallLocally::new(registry.clone())));
    // Maintenance workflow: RouteProjectWorkflow routes validated projects to the
    // correct sub-workflow via ProjectIterationRequested or ProjectMaintenanceRequested.
    engine.register(Box::new(blocks::CleanupBranches::new(registry.clone())));
    engine.register(Box::new(blocks::RouteProjectWorkflow));
    engine.register(Box::new(blocks::CompleteProjectRun));
    // Native gate orchestration blocks
    let shell: Arc<dyn gateway::ShellGateway> = Arc::new(gateway::ProcessShellGateway);
    engine.register(Box::new(blocks::ResolveGates::new(registry.clone())));
    engine.register(Box::new(blocks::RunPreflightGates::new(shell.clone(), registry.clone())));
    engine.register(Box::new(blocks::RunVerifyGates::new(shell, registry.clone())));
    engine.register(Box::new(blocks::RouteGateResult));
    engine.register(Box::new(blocks::RouteValidationResult));
    // Native maintain workflow blocks (Phase 2)
    engine.register(Box::new(blocks::ExecuteMaintain::new(agent.clone(), registry.clone())));
    engine.register(Box::new(blocks::RetryExecution::new(agent.clone(), registry.clone())));
    engine.register(Box::new(blocks::SummarizeResult::new(agent.clone(), registry.clone())));
    // Native iterate workflow blocks (Phase 3)
    engine.register(Box::new(blocks::CheckCharter::new(registry.clone())));
    engine.register(Box::new(blocks::AssessProject::new(agent.clone(), registry.clone())));
    engine.register(Box::new(blocks::TriageAssessment::new(agent.clone(), registry.clone())));
    engine.register(Box::new(blocks::CreatePlan::new(agent.clone(), registry.clone())));
    // Prompt workflow (direct prompt execution with gates)
    engine.register(Box::new(blocks::DirectPrompt));
    // Strategic loop workflow (nested iteration)
    engine.register(Box::new(blocks::StrategicAssessor::new(agent.clone(), registry.clone())));
    engine
        .register(Box::new(blocks::StrategicLoopController::new(agent.clone(), registry.clone())));
    // Pipeline health workflow
    engine.register(Box::new(blocks::CheckPipeline::new(registry.clone())));
    engine.register(Box::new(blocks::RemediatePipeline::new(agent.clone(), registry.clone())));
    // Drift scout workflow
    engine.register(Box::new(blocks::ScoutDrift::new(agent.clone(), registry.clone())));
    engine.register(Box::new(blocks::ExecutePlan::new(agent, registry.clone())));
    engine.register(Box::new(blocks::GenerateSummary::new(trace_writer, audits_dir)));
    engine
}
