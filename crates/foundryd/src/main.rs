#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use tokio::sync::Notify;
use tonic::transport::Server;
use tonic::transport::server::TcpIncoming;
use tracing_subscriber::EnvFilter;

use foundry_sdk::agent_config::AgentConfigStore;
use foundry_sdk::sentinel::{SentinelStore, merge_default_seed_into};

mod instance_lock;
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

fn resolve_listen_addr(configured: Option<&str>) -> Result<std::net::SocketAddr> {
    configured
        .unwrap_or(foundry_sdk::paths::DEFAULT_DAEMON_LISTEN_ADDR)
        .parse()
        .map_err(Into::into)
}

/// Foundry daemon — event-driven workflow engine for engineering automation.
///
/// `foundryd` takes no arguments. It is configured through environment
/// variables (`FOUNDRYD_LISTEN_ADDR`, `FOUNDRYD_LOCK_PATH`, `FOUNDRY_*`).
/// Only one daemon may run per Foundry home: a second start is refused before
/// it changes anything on disk.
#[derive(Debug, Parser)]
#[command(name = "foundryd", version, about, long_about = None)]
struct Cli {}

/// Why this process must not become the daemon. Rendered as one line on
/// stderr; the process then exits non-zero having changed nothing.
#[derive(Debug)]
enum StartRefusal {
    AlreadyRunning {
        lock_path: std::path::PathBuf,
        pid: Option<u32>,
    },
    LockUnavailable {
        lock_path: std::path::PathBuf,
        error: std::io::Error,
    },
    AddressInUse {
        addr: std::net::SocketAddr,
        holder_pid: Option<u32>,
    },
    CannotListen {
        addr: std::net::SocketAddr,
        error: std::io::Error,
    },
}

impl std::fmt::Display for StartRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning { lock_path, pid } => {
                let who = pid.map_or_else(|| "pid unknown".to_string(), |pid| format!("pid {pid}"));
                write!(
                    f,
                    "foundryd: another foundryd is already running ({who}; lock {}); refusing to start, nothing was changed",
                    lock_path.display()
                )
            }
            Self::LockUnavailable { lock_path, error } => write!(
                f,
                "foundryd: cannot take the instance lock {}: {error}; refusing to start, nothing was changed",
                lock_path.display()
            ),
            Self::AddressInUse { addr, holder_pid } => {
                let who = holder_pid.map_or_else(
                    || "another process is bound to it".to_string(),
                    |pid| format!("foundryd pid {pid} is running"),
                );
                write!(
                    f,
                    "foundryd: cannot listen on {addr}: address already in use ({who}); refusing to start, nothing was changed"
                )
            }
            Self::CannotListen { addr, error } => write!(
                f,
                "foundryd: cannot listen on {addr}: {error}; refusing to start, nothing was changed"
            ),
        }
    }
}

/// Become the one daemon for this Foundry home: bind the listen address, then
/// take the single-instance lock.
///
/// This runs before any recovery sweep, store write, scheduler start or event
/// emission. Binding first means an address clash writes nothing at all; the
/// lock is only rewritten (with this pid) once it is held.
fn claim_instance(
    addr: std::net::SocketAddr,
    lock_path: &std::path::Path,
) -> Result<(TcpIncoming, instance_lock::InstanceLock), StartRefusal> {
    let incoming = bind_listener(addr).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            StartRefusal::AddressInUse {
                addr,
                holder_pid: instance_lock::running_holder(lock_path),
            }
        } else {
            StartRefusal::CannotListen { addr, error }
        }
    })?;
    let lock = instance_lock::acquire(lock_path).map_err(|error| match error {
        instance_lock::LockError::Held { pid } => StartRefusal::AlreadyRunning {
            lock_path: lock_path.to_path_buf(),
            pid,
        },
        instance_lock::LockError::Io(error) => StartRefusal::LockUnavailable {
            lock_path: lock_path.to_path_buf(),
            error,
        },
    })?;
    Ok((incoming, lock))
}

/// Bind the gRPC listen address with `TCP_NODELAY` requested on every
/// accepted connection, matching what `Server::builder().serve(addr)` did
/// before the listener was bound up front by [`claim_instance`].
///
/// A bare `TcpIncoming::bind` leaves nodelay unset, so small gRPC frames
/// (unary replies, `Watch` events) would wait on Nagle's algorithm.
fn bind_listener(addr: std::net::SocketAddr) -> std::io::Result<TcpIncoming> {
    Ok(TcpIncoming::bind(addr)?.with_nodelay(Some(true)))
}

fn init_tracing() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("foundryd=info".parse()?))
        .init();
    Ok(())
}

/// [`claim_instance`] for the configured address and lock path, or print the one-line
/// refusal and exit non-zero with nothing on disk changed.
fn claim_instance_or_exit()
-> Result<(std::net::SocketAddr, TcpIncoming, instance_lock::InstanceLock)> {
    let addr = resolve_listen_addr(foundry_sdk::paths::daemon_listen_addr().as_deref())?;
    match claim_instance(addr, &foundry_sdk::paths::daemon_lock_path()) {
        Ok((incoming, lock)) => {
            tracing::info!(
                lock = %lock.path().display(),
                pid = std::process::id(),
                "foundryd instance lock held"
            );
            Ok((addr, incoming, lock))
        }
        Err(refusal) => {
            eprintln!("{refusal}");
            std::process::exit(1);
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // `--help`, `--version`, bad arguments: handled before anything else.
    Cli::parse();
    init_tracing()?;
    let (addr, incoming, instance_lock) = claim_instance_or_exit()?;

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

    let event_writer = Arc::new(foundry_engine::event_writer::EventWriter::new(events_dir.clone()));

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
    spawn_event_audit_writer(&event_tx, Arc::clone(&event_writer));

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

    // Before anything can dispatch: close out work the previous process was
    // stopped in the middle of, so a restart never leaves an item or an agent
    // session running.
    service::settle_running_work_items_on_start(&ctx, &foundry_sdk::paths::work_items_path()).await;
    service::end_interrupted_agent_sessions_on_start(&ctx, &events_dir).await;

    service::spawn_interrupted_cycle_recovery(&ctx, events_dir.clone());
    spawn_scheduler(&ctx, &sentinels, &scheduler_reload);

    let service = service::FoundryService::new(
        ctx,
        service::StoreConfig {
            campaigns_path: foundry_sdk::paths::campaigns_path(),
            work_items_path: foundry_sdk::paths::work_items_path(),
            events_dir,
            registry_path,
            sentinels,
            sentinels_path,
            scheduler_reload,
        },
    );

    tracing::info!("foundryd listening on {addr}");

    Server::builder()
        .add_service(proto::foundry_server::FoundryServer::new(service))
        .serve_with_incoming(incoming)
        .await?;

    // Held until the server stops, so no second daemon can start meanwhile.
    drop(instance_lock);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Cli, StartRefusal, bind_listener, claim_instance, resolve_listen_addr};
    use clap::Parser as _;

    #[test]
    fn resolve_listen_addr_uses_historical_loopback_default() {
        let addr = resolve_listen_addr(None).expect("default listen addr must parse");
        assert_eq!(addr.to_string(), "127.0.0.1:50051");
    }

    #[test]
    fn resolve_listen_addr_uses_configured_override() {
        let addr = resolve_listen_addr(Some("127.0.0.1:55123"))
            .expect("configured listen addr must parse");
        assert_eq!(addr.to_string(), "127.0.0.1:55123");
    }

    #[test]
    fn cli_accepts_no_arguments() {
        assert!(Cli::try_parse_from(["foundryd"]).is_ok());
    }

    #[test]
    fn cli_version_and_help_are_informational_exits() {
        use clap::error::ErrorKind;
        for (arg, kind) in [
            ("--version", ErrorKind::DisplayVersion),
            ("-V", ErrorKind::DisplayVersion),
            ("--help", ErrorKind::DisplayHelp),
            ("-h", ErrorKind::DisplayHelp),
        ] {
            let err = Cli::try_parse_from(["foundryd", arg]).unwrap_err();
            assert_eq!(err.kind(), kind, "{arg}");
            assert_eq!(err.exit_code(), 0, "{arg}");
        }
    }

    #[test]
    fn cli_rejects_unknown_flags_and_arguments() {
        for arg in ["--verison", "--foreground", "start"] {
            let err = Cli::try_parse_from(["foundryd", arg]).unwrap_err();
            assert_ne!(err.exit_code(), 0, "{arg} must be a usage error");
        }
    }

    #[tokio::test]
    async fn claim_instance_refuses_a_held_lock_and_names_the_holder() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("foundryd.lock");
        let _held = super::instance_lock::acquire(&lock_path).unwrap();

        let refusal = claim_instance("127.0.0.1:0".parse().unwrap(), &lock_path).unwrap_err();

        assert!(matches!(refusal, StartRefusal::AlreadyRunning { pid: Some(_), .. }));
        let line = refusal.to_string();
        assert!(line.contains(&format!("pid {}", std::process::id())), "{line}");
        assert!(!line.contains('\n'), "one line: {line}");
    }

    #[tokio::test]
    async fn claim_instance_refuses_a_bound_address_without_creating_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("foundryd.lock");
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = taken.local_addr().unwrap();

        let refusal = claim_instance(addr, &lock_path).unwrap_err();

        assert!(matches!(
            refusal,
            StartRefusal::AddressInUse {
                holder_pid: None,
                ..
            }
        ));
        assert!(refusal.to_string().contains("address already in use"));
        assert!(!lock_path.exists(), "an address clash writes nothing");
    }

    #[tokio::test]
    async fn claim_instance_names_the_running_daemon_when_its_address_is_taken() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("foundryd.lock");
        let _held = super::instance_lock::acquire(&lock_path).unwrap();
        let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();

        let refusal = claim_instance(taken.local_addr().unwrap(), &lock_path).unwrap_err();

        assert!(
            refusal
                .to_string()
                .contains(&format!("foundryd pid {} is running", std::process::id())),
            "{refusal}"
        );
    }

    #[tokio::test]
    async fn claim_instance_holds_the_lock_and_the_address() {
        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("foundryd.lock");

        let (_incoming, lock) = claim_instance("127.0.0.1:0".parse().unwrap(), &lock_path).unwrap();

        assert_eq!(lock.path(), lock_path);
        assert!(matches!(
            super::instance_lock::acquire(&lock_path),
            Err(super::instance_lock::LockError::Held { .. })
        ));
    }

    /// A plain accepted socket has `TCP_NODELAY` off; the daemon's listener
    /// must turn it on, as `Server::builder().serve(addr)` used to.
    #[tokio::test]
    async fn claim_instance_listener_sets_nodelay_on_accepted_connections() {
        use tokio_stream::StreamExt as _;

        let dir = tempfile::tempdir().unwrap();
        let lock_path = dir.path().join("foundryd.lock");
        let (mut incoming, _lock) =
            claim_instance("127.0.0.1:0".parse().unwrap(), &lock_path).unwrap();
        let addr = incoming.local_addr().unwrap();

        let _client = tokio::net::TcpStream::connect(addr).await.unwrap();
        let accepted = incoming.next().await.unwrap().unwrap();

        assert!(accepted.nodelay().unwrap(), "TCP_NODELAY is requested on accepted connections");
    }

    #[tokio::test]
    async fn bind_listener_sets_nodelay_where_a_bare_bind_does_not() {
        use tokio_stream::StreamExt as _;

        let mut bare = super::TcpIncoming::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let _bare_client =
            tokio::net::TcpStream::connect(bare.local_addr().unwrap()).await.unwrap();
        assert!(!bare.next().await.unwrap().unwrap().nodelay().unwrap(), "baseline: off");

        let mut ours = bind_listener("127.0.0.1:0".parse().unwrap()).unwrap();
        let _client = tokio::net::TcpStream::connect(ours.local_addr().unwrap()).await.unwrap();
        assert!(ours.next().await.unwrap().unwrap().nodelay().unwrap());
    }
}

fn spawn_event_audit_writer(
    event_tx: &tokio::sync::broadcast::Sender<foundry_sdk::event::Event>,
    event_writer: Arc<foundry_engine::event_writer::EventWriter>,
) {
    let mut rx = event_tx.subscribe();

    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    if let Err(error) = event_writer.write(&event) {
                        tracing::warn!(
                            error = %error,
                            event_id = %event.id,
                            "failed to persist broadcast audit event"
                        );
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::error!(
                        missed,
                        "event audit writer lagged; broadcast events may be absent from JSONL"
                    );
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
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

/// Seed the token price book on first start, fill missing models and refresh
/// recognised old defaults. Mirrors [`load_or_seed_agent_config`].
///
/// Rates are runtime data an operator edits between runs, so nothing here is
/// held in memory — the gateway reads the file when it prices a session. This
/// exists only so the file is present and current to edit.
fn seed_token_rates(path: &std::path::Path) {
    use foundry_sdk::token_rates::{RateBook, merge_default_seed_into as merge_rate_seed};
    match RateBook::load(path) {
        Ok(mut book) => {
            if merge_rate_seed(&mut book) {
                if let Err(e) = book.save(path) {
                    tracing::warn!(error = %e, path = %path.display(), "could not persist merged token rates");
                } else {
                    tracing::info!(path = %path.display(), "updated token rates from default seed");
                }
            }
        }
        Err(foundry_sdk::error::StoreError::NotFound { .. }) => {
            if let Err(e) = RateBook::default_seed().save(path) {
                tracing::warn!(error = %e, path = %path.display(), "could not seed token rates");
            } else {
                tracing::info!(path = %path.display(), "token rates seeded on first start");
            }
        }
        // A corrupt book must not stop the daemon: pricing degrades to the
        // baked seed, and the operator's file is left untouched to be fixed.
        Err(e) => {
            tracing::warn!(error = %e, path = %path.display(), "token rate book unreadable; pricing will use baked defaults");
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
    seed_token_rates(&foundry_sdk::paths::token_rates_path());

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

    // Registered first so a run-shaped item is in the ledger `running` before
    // the block that dispatches the run is reached: blocks run in
    // registration order for a given trigger.
    register_run_ledger_blocks(&mut engine, registry);
    register_core_blocks(&mut engine, registry);
    register_release_blocks(&mut engine, &agent, registry);
    register_gate_blocks(&mut engine, &shell, registry);
    register_maintain_blocks(&mut engine, &agent, &shell, registry);
    register_iterate_blocks(&mut engine, &agent, registry);
    register_campaign_blocks(&mut engine, &agent, &shell, registry);
    register_pipeline_blocks(&mut engine, &agent, registry, trace_writer, paths.audits_dir);
    register_digest_blocks(&mut engine, &agent, &shell, registry, paths.digest);

    engine
}

/// The work-item ledger's run-shaped bracket: one block opens a maintenance
/// run, a release or a remediation from the root of its chain, and one closes
/// it from that chain's own typed terminal. Neither changes a dispatch.
fn register_run_ledger_blocks(
    engine: &mut foundry_engine::engine::Engine,
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
) {
    engine.register(Box::new(foundry_blocks::blocks::RecordRunWorkItem::new(
        foundry_sdk::paths::work_items_path(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::SettleRunWorkItem::new(
        foundry_sdk::paths::work_items_path(),
    )));
}

fn register_campaign_blocks(
    engine: &mut foundry_engine::engine::Engine,
    agent: &Arc<dyn foundry_blocks::gateway::AgentGateway>,
    shell: &Arc<dyn foundry_blocks::gateway::ShellGateway>,
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
) {
    engine.register(Box::new(foundry_blocks::blocks::RequestCampaignAdvance));
    engine.register(Box::new(foundry_blocks::blocks::SurfaceCampaignTerminal));
    engine.register(Box::new(foundry_blocks::blocks::DisposeCampaignWork::new(
        registry.clone(),
        foundry_sdk::paths::work_items_path(),
    )));
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

/// Native maintain workflow (Phase 2): classify dependencies, execute, retry, summarise.
fn register_maintain_blocks(
    engine: &mut foundry_engine::engine::Engine,
    agent: &Arc<dyn foundry_blocks::gateway::AgentGateway>,
    shell: &Arc<dyn foundry_blocks::gateway::ShellGateway>,
    registry: &Arc<RwLock<foundry_sdk::registry::Registry>>,
) {
    engine.register(Box::new(foundry_blocks::blocks::ClassifyDependencyUpdates::new(
        shell.clone(),
        registry.clone(),
    )));
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
    // The ledger blocks bracket the task chain: `RecordWorkItem` opens the item
    // from the workflow's root `ExecutionRequested`, so it is in the file
    // `running` before any later event can fail, `SettleFailedDispatch` closes
    // it when the chain stops ahead of the coding agent, and `SettleWorkItem`
    // closes it from the terminal task result.
    engine.register(Box::new(foundry_blocks::blocks::RecordWorkItem::new(
        foundry_sdk::paths::work_items_path(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::SettleFailedDispatch::new(
        foundry_sdk::paths::work_items_path(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::SettleWorkItem::with_registry(
        foundry_sdk::paths::work_items_path(),
        registry.clone(),
    )));
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
    engine.register(Box::new(foundry_blocks::blocks::PlanMajorUpgrades::new(
        Arc::clone(&trace_writer),
        registry.clone(),
        Arc::new(foundry_blocks::gateway::ProcessShellGateway),
    )));
    engine.register(Box::new(foundry_blocks::blocks::GenerateSummary::new(
        trace_writer,
        audits_dir,
        registry.clone(),
        Arc::new(foundry_blocks::gateway::ProcessShellGateway),
    )));
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
    engine.register(Box::new(foundry_blocks::blocks::ReconcileWork::new(
        registry.clone(),
        foundry_sdk::paths::work_items_path(),
        foundry_sdk::paths::worktrees_dir(),
        foundry_sdk::paths::events_dir(),
        foundry_sdk::paths::reconcile_dir(),
    )));
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

#[cfg(test)]
mod audit_writer_tests {
    use super::*;

    #[tokio::test]
    async fn watch_only_event_is_persisted() {
        let tmp = tempfile::tempdir().unwrap();
        let writer = Arc::new(foundry_engine::event_writer::EventWriter::new(tmp.path()));
        let (event_tx, _) = tokio::sync::broadcast::channel(8);
        spawn_event_audit_writer(&event_tx, writer);

        let event = foundry_sdk::event::Event::new(
            foundry_sdk::event::EventType::Custom("watch_only".to_string()),
            "test-project".to_string(),
            foundry_sdk::throttle::Throttle::Full,
            serde_json::json!({"status": "observed"}),
        );
        let event_id = event.id.clone();
        event_tx.send(event).unwrap();

        let mut persisted = false;
        for _attempt in 0..100 {
            tokio::task::yield_now().await;

            persisted = std::fs::read_dir(tmp.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
                .any(|content| content.contains(&event_id));

            if persisted {
                break;
            }
        }

        assert!(persisted, "Watch-only event should be written to JSONL");
    }
}
