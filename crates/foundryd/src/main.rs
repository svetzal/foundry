use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

mod blocks;
mod engine;
mod event_writer;
mod gate_file;
mod gate_runner;
mod gateway;
mod orchestrator;
mod scanner;
mod service;
mod shell;
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

    let registry_path = env::var("FOUNDRY_REGISTRY_PATH").unwrap_or_else(|_| {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{home}/.foundry/registry.json")
    });

    let registry = match foundry_core::registry::Registry::load(std::path::Path::new(
        &registry_path,
    )) {
        Ok(r) => {
            tracing::info!(path = %registry_path, projects = r.active_projects().len(), "registry loaded");
            Arc::new(r)
        }
        Err(e) => {
            tracing::warn!(path = %registry_path, error = %e, "registry not found, using empty registry");
            Arc::new(foundry_core::registry::Registry {
                version: 2,
                projects: vec![],
            })
        }
    };

    let events_dir = env::var("FOUNDRY_EVENTS_DIR").unwrap_or_else(|_| {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{home}/.foundry/events")
    });
    let event_writer = Arc::new(event_writer::EventWriter::new(&events_dir));

    let traces_dir = env::var("FOUNDRY_TRACES_DIR").unwrap_or_else(|_| {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{home}/.foundry/traces")
    });
    let trace_writer = Arc::new(trace_writer::TraceWriter::new(&traces_dir));

    let audits_dir = env::var("FOUNDRY_AUDITS_DIR").unwrap_or_else(|_| {
        let home = env::var("HOME").unwrap_or_else(|_| ".".to_string());
        format!("{home}/.foundry/audits")
    });

    let (event_tx, _) = tokio::sync::broadcast::channel(256);

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
    engine.register(Box::new(blocks::RemediateVulnerability::new(registry.clone())));
    engine.register(Box::new(blocks::CommitAndPush::new(registry.clone())));
    engine.register(Box::new(blocks::CutRelease::new(registry.clone())));
    engine.register(Box::new(blocks::WatchPipeline::new(registry.clone())));
    engine.register(Box::new(blocks::InstallLocally::new(registry.clone())));
    // Maintenance workflow: RouteProjectWorkflow routes validated projects to the
    // correct sub-workflow via IterationRequested or MaintenanceRequested.
    engine.register(Box::new(blocks::RouteProjectWorkflow));
    engine.register(Box::new(blocks::RunHoneIterate::new(registry.clone())));
    engine.register(Box::new(blocks::RunHoneMaintain::new(registry.clone())));
    // Native gate orchestration blocks
    let shell: Arc<dyn gateway::ShellGateway> = Arc::new(gateway::ProcessShellGateway);
    engine.register(Box::new(blocks::ResolveGates::new(registry.clone())));
    engine.register(Box::new(blocks::RunPreflightGates::new(shell.clone(), registry.clone())));
    engine.register(Box::new(blocks::RunVerifyGates::new(shell, registry.clone())));
    engine.register(Box::new(blocks::RouteGateResult));
    engine.register(Box::new(blocks::RouteValidationResult));
    engine.register(Box::new(blocks::GenerateSummary::new(trace_writer.clone(), audits_dir)));

    let engine = Arc::new(engine);
    let trace_store = Arc::new(trace_store::TraceStore::with_trace_writer(
        Duration::from_secs(3600),
        trace_writer.clone(),
    ));
    let workflow_tracker = Arc::new(workflow_tracker::WorkflowTracker::new());
    let service =
        service::FoundryService::new(engine, trace_store, event_tx, workflow_tracker, trace_writer);

    let addr = "[::1]:50051".parse()?;
    tracing::info!("foundryd listening on {addr}");

    Server::builder()
        .add_service(proto::foundry_server::FoundryServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
