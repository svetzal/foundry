use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("foundryd=info".parse()?))
        .init();

    // Registry is empty by default; a future task will add registry loading from disk.
    let registry = Arc::new(foundry_core::registry::Registry::default());

    let mut engine = engine::Engine::new();
    engine.register(Box::new(blocks::ComposeGreeting));
    engine.register(Box::new(blocks::DeliverGreeting));
    engine.register(Box::new(blocks::ScanDependencies));
    engine.register(Box::new(blocks::AuditReleaseTag));
    engine.register(Box::new(blocks::AuditMainBranch));
    engine.register(Box::new(blocks::RemediateVulnerability::new(registry.clone())));
    engine.register(Box::new(blocks::CommitAndPush));
    engine.register(Box::new(blocks::CutRelease::new(registry.clone())));
    engine.register(Box::new(blocks::WatchPipeline));
    engine.register(Box::new(blocks::InstallLocally::new(registry.clone())));

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
