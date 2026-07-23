#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use tokio::sync::Notify;
use tonic::transport::Server;
use tracing_subscriber::EnvFilter;

use foundry_sdk::agent_config::AgentConfigStore;
use foundry_sdk::sentinel::{SentinelStore, merge_default_seed_into};

mod legacy_event_check;
mod orchestrator;
mod scheduler;
mod service;
mod trace_store;
mod workflow_tracker;

pub mod proto {
    #![allow(clippy::all, clippy::pedantic)]
    tonic::include_proto!("foundry");
}

/// Resolve a startup-configured directory path to UTF-8.
///
/// A non-UTF-8 path here means the environment is misconfigured in a way
/// nothing downstream can recover from; abort before the daemon serves
/// traffic rather than fail confusingly later (Failure Policy, AGENTS.md).
fn require_utf8_path(path: &std::path::Path, env_var: &str) -> String {
    match path.to_str() {
        Some(s) => s.to_string(),
        None => panic!("{env_var} must be valid UTF-8"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("foundryd=info".parse()?))
        .init();

    let events_dir = foundry_sdk::paths::events_dir();
    if let Some(legacy) = legacy_event_check::detect_legacy_event_names(&events_dir) {
        eprintln!(
            "ERROR: foundryd 0.17.0 detected legacy event-type name '{legacy}' on disk.\n\
             Run scripts/migrate-event-names.sh once to backfill, then restart foundryd."
        );
        std::process::exit(2);
    }

    let registry_path = foundry_sdk::paths::registry_path();
    let registry = match foundry_sdk::registry::Registry::load(&registry_path) {
        Ok(r) => {
            tracing::info!(path = %registry_path.display(), projects = r.active_projects().len(), "registry loaded");
            Arc::new(RwLock::new(r))
        }
        Err(foundry_sdk::error::StoreError::NotFound { .. }) => {
            tracing::warn!(path = %registry_path.display(), "registry not found, using empty registry");
            Arc::new(RwLock::new(foundry_sdk::registry::Registry {
                version: 2,
                projects: vec![],
            }))
        }
        Err(e) => {
            tracing::error!(path = %registry_path.display(), error = %e, "registry file is corrupt or unreadable — refusing to start with an empty registry to prevent data loss");
            std::process::exit(2);
        }
    };

    let event_writer = Arc::new(foundry_engine::event_writer::EventWriter::new(events_dir));

    let traces_dir = foundry_sdk::paths::traces_dir();
    let trace_writer = Arc::new(foundry_blocks::trace_writer::TraceWriter::new(
        &require_utf8_path(&traces_dir, "FOUNDRY_TRACES_DIR"),
    ));

    let audits_dir = require_utf8_path(&foundry_sdk::paths::audits_dir(), "FOUNDRY_AUDITS_DIR");

    let digests_dir = foundry_sdk::paths::digests_dir();
    let ops_digests_dir = foundry_sdk::paths::ops_digests_dir();
    let ops_events_intake_dir = foundry_sdk::paths::ops_events_intake_dir();
    let ops_watermark_path = foundry_sdk::paths::ops_watermark_path();
    let triage_dir = foundry_sdk::paths::triage_dir();
    let supply_chain_dir = foundry_sdk::paths::supply_chain_dir();

    let (event_tx, _) = tokio::sync::broadcast::channel(256);

    let engine = register_blocks(
        &registry,
        event_writer,
        &event_tx,
        trace_writer.clone(),
        BlockPaths {
            audits_dir,
            digest: DigestPaths {
                digests_dir,
                ops_digests_dir,
                ops_events_intake_dir,
                ops_watermark_path,
                triage_dir,
                supply_chain_dir,
            },
        },
    );

    let engine = Arc::new(engine);
    let trace_store = Arc::new(trace_store::TraceStore::with_trace_writer(
        Duration::from_secs(3600),
        trace_writer.clone(),
    ));
    let workflow_tracker = Arc::new(workflow_tracker::WorkflowTracker::new());

    // Sentinel store — load (or auto-seed on first start) the file-backed
    // schedule that replaces launchd/com.mojility.foundry-maintenance.plist.
    let sentinels_path = foundry_sdk::paths::sentinels_path();
    let sentinels = Arc::new(RwLock::new(load_or_seed_sentinels(&sentinels_path)?));
    let scheduler_reload = Arc::new(Notify::new());

    let ctx = service::RuntimeContext {
        engine,
        trace_store,
        workflow_tracker,
        trace_writer,
        event_tx,
        registry,
    };

    spawn_scheduler(&ctx, &sentinels, &scheduler_reload);

    let service = service::FoundryService::new(
        ctx,
        service::StoreConfig {
            campaigns_path: foundry_sdk::paths::campaigns_path(),
            registry_path,
            sentinels,
            sentinels_path,
            scheduler_reload,
        },
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
        Err(foundry_sdk::error::StoreError::NotFound { .. }) => {
            let seed = SentinelStore::default_seed();
            seed.save(path).map_err(|save_err| {
                anyhow::anyhow!("failed to seed sentinels at {}: {save_err}", path.display())
            })?;
            tracing::info!(
                path = %path.display(),
                count = seed.sentinels.len(),
                "sentinels seeded on first start",
            );
            Ok(seed)
        }
        Err(e) => Err(anyhow::anyhow!(
            "sentinel file at {} is corrupt or unreadable: {e}",
            path.display()
        )),
    }
}

/// Load the agent model config, seeding it on first start and additively
/// merging any provider/tier/effort keys missing from the user's file. Mirrors
/// [`load_or_seed_sentinels`].
fn load_or_seed_agent_config(path: &std::path::Path) -> Result<AgentConfigStore> {
    use foundry_sdk::agent_config::merge_default_seed_into as merge_agent_seed;
    match AgentConfigStore::load(path) {
        Ok(mut store) => {
            tracing::info!(path = %path.display(), "agent config loaded");
            if merge_agent_seed(&mut store) {
                store.save(path).map_err(|save_err| {
                    anyhow::anyhow!(
                        "failed to persist merged agent config at {}: {save_err}",
                        path.display()
                    )
                })?;
                tracing::info!(
                    path = %path.display(),
                    "filled missing agent config keys from default seed",
                );
            }
            Ok(store)
        }
        Err(foundry_sdk::error::StoreError::NotFound { .. }) => {
            let seed = AgentConfigStore::default_seed();
            seed.save(path).map_err(|save_err| {
                anyhow::anyhow!("failed to seed agent config at {}: {save_err}", path.display())
            })?;
            tracing::info!(path = %path.display(), "agent config seeded on first start");
            Ok(seed)
        }
        Err(e) => Err(anyhow::anyhow!(
            "agent config file at {} is corrupt or unreadable: {e}",
            path.display()
        )),
    }
}

fn spawn_scheduler(
    ctx: &service::RuntimeContext,
    sentinels: &Arc<RwLock<SentinelStore>>,
    reload: &Arc<Notify>,
) {
    // Pipe sentinel firings through the same trace/workflow_tracker
    // machinery the gRPC `emit()` handler uses.
    let ctx = ctx.clone();
    let emit: scheduler::EmitFn = Arc::new(move |event| {
        service::spawn_workflow(event, &ctx);
    });

    let scheduler = scheduler::Scheduler::new(Arc::clone(sentinels), Arc::clone(reload), emit);
    tokio::spawn(scheduler.run());
}

struct DigestPaths {
    digests_dir: std::path::PathBuf,
    ops_digests_dir: std::path::PathBuf,
    ops_events_intake_dir: std::path::PathBuf,
    ops_watermark_path: std::path::PathBuf,
    triage_dir: std::path::PathBuf,
    supply_chain_dir: std::path::PathBuf,
}

struct BlockPaths {
    audits_dir: String,
    digest: DigestPaths,
}

/// Construct the agent gateway: resolve the default provider, load/seed the
/// model config, build a backend gateway for each supported provider, and wrap
/// them in a [`foundry_blocks::gateway::RoutingAgentGateway`].
///
/// All supported backends (claude, opencode, codex) are constructed up front.
/// Each request may carry a per-request provider override (`agent_provider` in
/// the request event), which propagates through the chain; absent an override,
/// the router uses `FOUNDRY_AGENT_PROVIDER` (defaulting to `claude`).
///
/// The router also owns an in-memory provider circuit breaker: once a backend
/// reports a terminal provider/account failure (hard spend-limit, revoked-auth),
/// later requests for that provider are rejected for the lifetime of the daemon
/// instead of spawning more doomed sessions. Breaker state is process-local;
/// restarting `foundryd` clears it.
fn build_agent_gateway(
    event_tx: &tokio::sync::broadcast::Sender<foundry_sdk::event::Event>,
) -> Arc<dyn foundry_blocks::gateway::AgentGateway> {
    use foundry_blocks::gateway::AgentProvider;
    let make_shell = || -> Arc<dyn foundry_blocks::gateway::ShellGateway> {
        Arc::new(foundry_blocks::gateway::ProcessShellGateway)
    };
    let make_runner = || Arc::new(foundry_blocks::agent_stream::ProcessAgentStreamRunner);
    let sessions_dir = foundry_sdk::paths::agent_sessions_dir();

    let default = match std::env::var("FOUNDRY_AGENT_PROVIDER") {
        Ok(raw) => raw.parse::<AgentProvider>().unwrap_or_else(|_| {
            tracing::warn!(
                provider = %raw,
                "unknown FOUNDRY_AGENT_PROVIDER; falling back to claude"
            );
            AgentProvider::Claude
        }),
        Err(_) => AgentProvider::Claude,
    };

    // Per-provider tier→model and effort→token maps. Defaults are baked in;
    // ~/.foundry/agents.json overrides them (seed-merged on startup). A
    // load/seed failure degrades to baked defaults rather than crashing.
    let agent_config = load_or_seed_agent_config(&foundry_sdk::paths::agent_config_path())
        .unwrap_or_else(|e| {
            tracing::warn!(error = %e, "failed to load/seed agent config; using baked defaults");
            AgentConfigStore::default_seed()
        });
    tracing::info!(default_provider = %default, "agent providers: claude, opencode, codex");

    let mut gateways: std::collections::HashMap<
        AgentProvider,
        Arc<dyn foundry_blocks::gateway::AgentGateway>,
    > = std::collections::HashMap::new();
    gateways.insert(
        AgentProvider::Claude,
        Arc::new(
            foundry_blocks::gateway::ClaudeAgentGateway::new_with_streaming(
                make_shell(),
                make_runner(),
                sessions_dir.clone(),
                event_tx.clone(),
            )
            .with_models(agent_config.resolved(AgentProvider::Claude)),
        ),
    );
    gateways.insert(
        AgentProvider::Opencode,
        Arc::new(
            foundry_blocks::gateway::OpencodeAgentGateway::new_with_streaming(
                make_shell(),
                make_runner(),
                sessions_dir.clone(),
                event_tx.clone(),
            )
            .with_models(agent_config.resolved(AgentProvider::Opencode)),
        ),
    );
    gateways.insert(
        AgentProvider::Codex,
        Arc::new(
            foundry_blocks::gateway::CodexAgentGateway::new_with_streaming(
                make_shell(),
                make_runner(),
                sessions_dir.clone(),
                event_tx.clone(),
            )
            .with_models(agent_config.resolved(AgentProvider::Codex)),
        ),
    );
    Arc::new(foundry_blocks::gateway::RoutingAgentGateway::new(gateways, default))
}

fn register_blocks(
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
    event_writer: Arc<foundry_engine::event_writer::EventWriter>,
    event_tx: &tokio::sync::broadcast::Sender<foundry_sdk::event::Event>,
    trace_writer: Arc<foundry_blocks::trace_writer::TraceWriter>,
    paths: BlockPaths,
) -> foundry_engine::engine::Engine {
    let mut engine = foundry_engine::engine::Engine::new()
        .with_event_writer(event_writer)
        .with_event_broadcaster(event_tx.clone());
    let agent = build_agent_gateway(event_tx);
    let shell: Arc<dyn foundry_blocks::gateway::ShellGateway> =
        Arc::new(foundry_blocks::gateway::ProcessShellGateway);

    register_core_blocks(&mut engine, registry);
    register_release_blocks(&mut engine, &agent, registry);
    register_gate_blocks(&mut engine, &shell, registry);
    register_maintain_blocks(&mut engine, &agent, registry);
    register_iterate_blocks(&mut engine, &agent, registry);
    register_campaign_blocks(&mut engine, &agent, &shell, registry);
    register_pipeline_blocks(&mut engine, &agent, registry, trace_writer, paths.audits_dir);
    register_digest_blocks(&mut engine, &agent, &shell, registry, paths.digest);

    engine
}

fn register_campaign_blocks(
    engine: &mut foundry_engine::engine::Engine,
    agent: &Arc<dyn foundry_blocks::gateway::AgentGateway>,
    shell: &Arc<dyn foundry_blocks::gateway::ShellGateway>,
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
) {
    engine.register(Box::new(foundry_blocks::blocks::RequestCampaignAdvance));
    engine.register(Box::new(foundry_blocks::blocks::SurfaceCampaignTerminal));
    engine.register(Box::new(foundry_blocks::blocks::AdvanceCampaign::new(
        agent.clone(),
        shell.clone(),
        registry.clone(),
        foundry_sdk::paths::campaigns_path(),
    )));
}

/// Core maintenance routing: project fan-out, validation, audit, greeting, and routing.
fn register_core_blocks(
    engine: &mut foundry_engine::engine::Engine,
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
) {
    engine.register(Box::new(orchestrator::FanOutMaintenance::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::ValidateProject::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::ComposeGreeting));
    engine.register(Box::new(foundry_blocks::blocks::DeliverGreeting));
    engine.register(Box::new(foundry_blocks::blocks::ScanDependencies::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::AuditReleaseTag::with_registry(
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::AuditMainBranch::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::CleanupBranches::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::RouteProjectWorkflow));
    engine.register(Box::new(foundry_blocks::blocks::CompleteProjectRun));
}

/// Release workflow: vulnerability remediation, commit, cut, execute, watch, install.
fn register_release_blocks(
    engine: &mut foundry_engine::engine::Engine,
    agent: &Arc<dyn foundry_blocks::gateway::AgentGateway>,
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
) {
    engine.register(Box::new(foundry_blocks::blocks::RemediateVulnerability::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::CommitAndPush::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::CutRelease::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::ExecuteRelease::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::WatchPipeline::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::InstallLocally::new(registry.clone())));
}

/// Native gate orchestration: resolve, preflight, verify, route.
fn register_gate_blocks(
    engine: &mut foundry_engine::engine::Engine,
    shell: &Arc<dyn foundry_blocks::gateway::ShellGateway>,
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
) {
    engine.register(Box::new(foundry_blocks::blocks::ResolveGates::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::RunPreflightGates::new(
        shell.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::RunVerifyGates::new(
        shell.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::RouteGateResult));
    engine.register(Box::new(foundry_blocks::blocks::RouteValidationResult));
}

/// Native maintain workflow (Phase 2): execute, retry, summarise.
fn register_maintain_blocks(
    engine: &mut foundry_engine::engine::Engine,
    agent: &Arc<dyn foundry_blocks::gateway::AgentGateway>,
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
) {
    engine.register(Box::new(foundry_blocks::blocks::ExecuteMaintain::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::RetryExecution::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::SummarizeResult::new(
        agent.clone(),
        registry.clone(),
    )));
}

/// Native iterate workflow (Phase 3): charter, assess, triage, plan, direct prompt, strategic loop.
fn register_iterate_blocks(
    engine: &mut foundry_engine::engine::Engine,
    agent: &Arc<dyn foundry_blocks::gateway::AgentGateway>,
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
) {
    engine.register(Box::new(foundry_blocks::blocks::CheckCharter::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::AssessProject::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::TriageAssessment::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::CreatePlan::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::DirectPrompt));
    engine.register(Box::new(foundry_blocks::blocks::ReviewTask::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::FinalizeTask::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::StrategicAssessor::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::StrategicLoopController::new(
        agent.clone(),
        registry.clone(),
    )));
}

/// Pipeline health, drift scout, plan execution, and audit summary.
fn register_pipeline_blocks(
    engine: &mut foundry_engine::engine::Engine,
    agent: &Arc<dyn foundry_blocks::gateway::AgentGateway>,
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
    trace_writer: Arc<foundry_blocks::trace_writer::TraceWriter>,
    audits_dir: String,
) {
    engine.register(Box::new(foundry_blocks::blocks::CheckPipeline::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::RemediatePipeline::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::ScoutDrift::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::ExecutePlan::new(
        agent.clone(),
        registry.clone(),
    )));
    engine
        .register(Box::new(foundry_blocks::blocks::GenerateSummary::new(trace_writer, audits_dir)));
}

/// Digest formation: commit, ops, triage, and supply-chain writers.
fn register_digest_blocks(
    engine: &mut foundry_engine::engine::Engine,
    agent: &Arc<dyn foundry_blocks::gateway::AgentGateway>,
    shell: &Arc<dyn foundry_blocks::gateway::ShellGateway>,
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
    paths: DigestPaths,
) {
    // Commit-digest formation (daily proactive summary of registered projects).
    engine.register(Box::new(foundry_blocks::blocks::ObserveCommits::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::SummarizeCommits::new(agent.clone())));
    engine.register(Box::new(foundry_blocks::blocks::WriteCommitDigest::new(paths.digests_dir)));
    // Ops-digest formation (periodic summary of MBOS operational events).
    engine.register(Box::new(foundry_blocks::blocks::ObserveEvents::new(
        paths.ops_events_intake_dir,
        paths.ops_watermark_path.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::SummarizeEvents::new(agent.clone())));
    engine.register(Box::new(foundry_blocks::blocks::WriteOpsDigest::new(
        paths.ops_digests_dir,
        paths.ops_watermark_path,
    )));
    // Post-maintenance failure triage formation (propose-only).
    let events_dir = foundry_sdk::paths::events_dir();
    engine.register(Box::new(foundry_blocks::blocks::TriageMaintenance::new(
        events_dir, 14, // 14-day streak lookback
    )));
    engine.register(Box::new(foundry_blocks::blocks::WriteTriageDigest::new(paths.triage_dir)));
    // Supply-chain scan formation (nightly working-tree dependency advisory scan).
    engine.register(Box::new(foundry_blocks::blocks::ScanSupplyChain::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::RemediateSupplyChain::new(
        shell.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::WriteSupplyChainDigest::new(
        paths.supply_chain_dir,
    )));
}
