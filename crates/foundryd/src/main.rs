use std::env;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

mod blocks;
mod engine;
mod event_writer;
mod orchestrator;
mod scanner;
mod service;
mod shell;
#[allow(dead_code)]
mod summary;
mod trace_store;

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

    let mut engine = engine::Engine::new().with_event_writer(event_writer);
    engine.register(Box::new(blocks::ValidateProject::new(registry.clone())));
    engine.register(Box::new(blocks::ComposeGreeting));
    engine.register(Box::new(blocks::DeliverGreeting));
    engine.register(Box::new(blocks::ScanDependencies));
    engine.register(Box::new(blocks::AuditReleaseTag::new(registry.clone())));
    engine.register(Box::new(blocks::AuditMainBranch::new(registry.clone())));
    engine.register(Box::new(blocks::RemediateVulnerability::new(registry.clone())));
    engine.register(Box::new(blocks::CommitAndPush::new(registry.clone())));
    engine.register(Box::new(blocks::CutRelease::new(registry.clone())));
    engine.register(Box::new(blocks::WatchPipeline::stub()));
    engine.register(Box::new(blocks::RunHoneIterate::new(registry.clone())));
    engine.register(Box::new(blocks::InstallLocally::new(registry.clone())));
    engine.register(Box::new(blocks::RemediateVulnerability::new(registry.clone())));
    engine.register(Box::new(blocks::CommitAndPush::new(registry.clone())));
    engine.register(Box::new(blocks::CutRelease::new(registry.clone())));
    engine.register(Box::new(blocks::WatchPipeline::stub()));
    engine.register(Box::new(blocks::InstallLocally::new(registry.clone())));
    engine.register(Box::new(blocks::RunHoneMaintain));

    let engine = Arc::new(engine);
    let trace_store = Arc::new(trace_store::TraceStore::new(Duration::from_secs(3600)));
    let service = service::FoundryService::new(engine, trace_store);

    let addr = "[::1]:50051".parse()?;
    tracing::info!("foundryd listening on {addr}");

    Server::builder()
        .add_service(proto::foundry_server::FoundryServer::new(service))
        .serve(addr)
        .await?;

    Ok(())
}
