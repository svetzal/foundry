use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use foundry_core::registry::Registry;
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

mod blocks;
mod engine;
mod service;
mod shell;
mod trace_store;

pub mod proto {
    #![allow(clippy::all, clippy::pedantic)]
    tonic::include_proto!("foundry");
}

/// Load the project registry from the well-known default location, falling
/// back to an empty registry if the file is absent or unreadable.
fn load_registry() -> Arc<Registry> {
    let default_path = dirs_path();
    match Registry::load(&default_path) {
        Ok(r) => {
            tracing::info!(path = %default_path.display(), projects = r.projects.len(), "loaded registry");
            Arc::new(r)
        }
        Err(err) => {
            tracing::warn!(path = %default_path.display(), error = %err, "registry not found, using empty registry");
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            })
        }
    }
}

fn dirs_path() -> std::path::PathBuf {
    // Honour FOUNDRY_REGISTRY env var for testing and custom deployments.
    if let Ok(val) = std::env::var("FOUNDRY_REGISTRY") {
        return std::path::PathBuf::from(val);
    }
    // Default: ~/Work/Operations/Automation/maintenance/registry.json
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    std::path::PathBuf::from(home).join("Work/Operations/Automation/maintenance/registry.json")
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("foundryd=info".parse()?))
        .init();

    let registry = load_registry();

    let mut engine = engine::Engine::new();
    engine.register(Box::new(blocks::ComposeGreeting));
    engine.register(Box::new(blocks::DeliverGreeting));
    engine.register(Box::new(blocks::ScanDependencies));
    engine.register(Box::new(blocks::AuditReleaseTag));
    engine.register(Box::new(blocks::AuditMainBranch));
    engine.register(Box::new(blocks::RemediateVulnerability));
    engine.register(Box::new(blocks::CommitAndPush));
    engine.register(Box::new(blocks::CutRelease));
    engine.register(Box::new(blocks::WatchPipeline));
    engine.register(Box::new(blocks::InstallLocally));
    engine.register(Box::new(blocks::RunHoneIterate::new(registry.clone())));

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
