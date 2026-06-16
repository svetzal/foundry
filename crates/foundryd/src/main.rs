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
        Err(e) => {
            tracing::warn!(path = %registry_path.display(), error = %e, "registry not found, using empty registry");
            Arc::new(RwLock::new(foundry_sdk::registry::Registry {
                version: 2,
                projects: vec![],
            }))
        }
    };

    let event_writer = Arc::new(foundry_engine::event_writer::EventWriter::new(events_dir));

    let traces_dir = foundry_sdk::paths::traces_dir();
    let trace_writer = Arc::new(foundry_blocks::trace_writer::TraceWriter::new(
        traces_dir.to_str().expect("FOUNDRY_TRACES_DIR must be valid UTF-8"),
    ));

    let audits_dir = foundry_sdk::paths::audits_dir()
        .to_str()
        .expect("FOUNDRY_AUDITS_DIR must be valid UTF-8")
        .to_string();

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
            digests_dir,
            ops_digests_dir,
            ops_events_intake_dir,
            ops_watermark_path,
            triage_dir,
            supply_chain_dir,
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
        Err(load_err) => {
            let seed = AgentConfigStore::default_seed();
            seed.save(path).map_err(|save_err| {
                anyhow::anyhow!(
                    "failed to seed agent config at {}: {save_err} (original load error: {load_err})",
                    path.display()
                )
            })?;
            tracing::info!(path = %path.display(), "agent config seeded on first start");
            Ok(seed)
        }
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

struct BlockPaths {
    audits_dir: String,
    digests_dir: std::path::PathBuf,
    ops_digests_dir: std::path::PathBuf,
    ops_events_intake_dir: std::path::PathBuf,
    ops_watermark_path: std::path::PathBuf,
    triage_dir: std::path::PathBuf,
    supply_chain_dir: std::path::PathBuf,
}

// Sequential block registration wiring — length is inherent, not a design smell.
#[allow(clippy::too_many_lines)]
fn register_blocks(
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
    event_writer: Arc<foundry_engine::event_writer::EventWriter>,
    event_tx: &tokio::sync::broadcast::Sender<foundry_sdk::event::Event>,
    trace_writer: Arc<foundry_blocks::trace_writer::TraceWriter>,
    paths: BlockPaths,
) -> foundry_engine::engine::Engine {
    let BlockPaths {
        audits_dir,
        digests_dir,
        ops_digests_dir,
        ops_events_intake_dir,
        ops_watermark_path,
        triage_dir,
        supply_chain_dir,
    } = paths;
    let mut engine = foundry_engine::engine::Engine::new()
        .with_event_writer(event_writer)
        .with_event_broadcaster(event_tx.clone());
    engine.register(Box::new(orchestrator::FanOutMaintenance::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::ValidateProject::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::ComposeGreeting));
    engine.register(Box::new(foundry_blocks::blocks::DeliverGreeting));
    engine.register(Box::new(foundry_blocks::blocks::ScanDependencies::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::AuditReleaseTag::with_registry(
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::AuditMainBranch::new(registry.clone())));
    // Agent gateway for blocks that invoke an agentic CLI. All supported
    // backends (claude, opencode, codex) are constructed up front and wired into
    // a RoutingAgentGateway. Each request may carry a per-request provider
    // override (`agent_provider` in the request event, e.g. `foundry iterate
    // --agent codex`), which propagates through the chain; absent an override,
    // the router uses FOUNDRY_AGENT_PROVIDER (defaulting to "claude").
    //
    // The Anthropic subscription stops covering hands-free agent work on
    // 2026-06-15 — set FOUNDRY_AGENT_PROVIDER=opencode (or codex) to default
    // unattended runs to an OpenAI backend. All backends implement the same
    // AgentGateway trait, so every downstream block is provider-agnostic.
    let agent: Arc<dyn foundry_blocks::gateway::AgentGateway> = {
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
    };
    engine.register(Box::new(foundry_blocks::blocks::RemediateVulnerability::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::CommitAndPush::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::cut_release_step(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::execute_release_step(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::WatchPipeline::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::InstallLocally::new(registry.clone())));
    // Maintenance workflow: RouteProjectWorkflow routes validated projects to the
    // correct sub-workflow via ProjectIterationRequested or ProjectMaintenanceRequested.
    engine.register(Box::new(foundry_blocks::blocks::CleanupBranches::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::RouteProjectWorkflow));
    engine.register(Box::new(foundry_blocks::blocks::CompleteProjectRun));
    // Native gate orchestration blocks
    let shell: Arc<dyn foundry_blocks::gateway::ShellGateway> =
        Arc::new(foundry_blocks::gateway::ProcessShellGateway);
    engine.register(Box::new(foundry_blocks::blocks::ResolveGates::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::RunPreflightGates::new(
        shell.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::RunVerifyGates::new(shell, registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::RouteGateResult));
    engine.register(Box::new(foundry_blocks::blocks::RouteValidationResult));
    // Native maintain workflow blocks (Phase 2)
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
    // Native iterate workflow blocks (Phase 3)
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
    // Prompt workflow (direct prompt execution with gates)
    engine.register(Box::new(foundry_blocks::blocks::DirectPrompt));
    // Strategic loop workflow (nested iteration)
    engine.register(Box::new(foundry_blocks::blocks::StrategicAssessor::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::StrategicLoopController::new(
        agent.clone(),
        registry.clone(),
    )));
    // Pipeline health workflow
    engine.register(Box::new(foundry_blocks::blocks::CheckPipeline::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::RemediatePipeline::new(
        agent.clone(),
        registry.clone(),
    )));
    // Drift scout workflow
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
    // Commit-digest formation (daily proactive summary of registered projects)
    engine.register(Box::new(foundry_blocks::blocks::ObserveCommits::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::SummarizeCommits::new(agent.clone())));
    engine.register(Box::new(foundry_blocks::blocks::WriteCommitDigest::new(digests_dir)));
    // Ops-digest formation (periodic summary of MBOS operational events)
    engine.register(Box::new(foundry_blocks::blocks::ObserveEvents::new(
        ops_events_intake_dir,
        ops_watermark_path.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::SummarizeEvents::new(agent)));
    engine.register(Box::new(foundry_blocks::blocks::WriteOpsDigest::new(
        ops_digests_dir,
        ops_watermark_path,
    )));
    // Post-maintenance failure triage formation (propose-only)
    let events_dir = foundry_sdk::paths::events_dir();
    engine.register(Box::new(foundry_blocks::blocks::TriageMaintenance::new(
        events_dir, 14, // 14-day streak lookback
    )));
    engine.register(Box::new(foundry_blocks::blocks::WriteTriageDigest::new(triage_dir)));
    // Supply-chain scan formation (nightly working-tree dependency advisory scan).
    // Detection-only and advisory: scans every active project's lockfile, classifies
    // findings against each repo's committed `.supply-chain-allow.json`, and writes a
    // deterministic digest. Never mutates a working tree or fails a project run.
    engine.register(Box::new(foundry_blocks::blocks::ScanSupplyChain::new(registry.clone())));
    // Remediation triage sits between the scan and the digest: it classifies each
    // live finding as auto-fixable (a populated fix version) vs a policy call.
    // Non-mutating today; the auto-fix engine ships dark in a later increment.
    engine.register(Box::new(foundry_blocks::blocks::RemediateSupplyChain));
    engine
        .register(Box::new(foundry_blocks::blocks::WriteSupplyChainDigest::new(supply_chain_dir)));
    engine
}
