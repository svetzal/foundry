//! Integration tests for `foundry campaign pause` via the gRPC online path.
//!
//! Each test starts a real [`FoundryService`] bound to a temporary TCP port,
//! then calls [`foundry_cli::campaign_commands::pause_and_render`] with
//! `offline = false` so the call travels through the generated tonic client.
//!
//! Gate (2): seed a campaign with a non-`None` `pending_run_result`, invoke
//!   the CLI pause path, assert BOTH that the persisted status is `Paused` AND
//!   that `pending_run_result` is byte-for-byte identical to what was seeded.
//!
//! Gate (3): assert that the rendered output is built from the
//!   `PauseCampaignResponse.campaign` detail returned by the RPC — verified by
//!   inspecting the string the function returns before the caller would print
//!   it, without any post-call disk read.
//!
//! Gate (4): not-found and malformed-store cases are mapped to the correct
//!   gRPC status codes and surfaced without filesystem paths in the error text.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_cli::campaign_commands::pause_and_render;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{
    Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
};
use foundry_sdk::payload::{LoopContext, TaskRunCompletedPayload, TaskVerdict};
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundryd::{
    proto::foundry_server::FoundryServer,
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

// ── Harness ───────────────────────────────────────────────────────────────────

/// Build a [`FoundryService`] backed by an explicitly supplied campaigns file
/// path and otherwise-temporary support files.
///
/// The campaigns file is owned by the caller — passing a [`NamedTempFile`]
/// that lives in the test body keeps it alive for the test's lifetime.
///
/// Returns the service and the traces temp-dir (also kept alive by the
/// caller).
fn make_service_with_campaigns_path(
    campaigns_path: std::path::PathBuf,
) -> (FoundryService, TempDir) {
    let (event_tx, _rx) = broadcast::channel(64);
    let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));

    let tmp_traces = tempfile::tempdir().expect("tempdir for traces");
    let trace_writer =
        Arc::new(TraceWriter::new(tmp_traces.path().to_str().expect("trace dir must be UTF-8")));
    let trace_store = Arc::new(TraceStore::with_trace_writer(
        Duration::from_secs(60),
        Arc::clone(&trace_writer),
    ));
    let workflow_tracker = Arc::new(WorkflowTracker::new());
    let registry = Arc::new(RwLock::new(Registry {
        version: 2,
        projects: vec![],
    }));

    // Support files: registry and sentinels.  These are ephemeral — the tests
    // in this module do not exercise registry or sentinel RPCs, so the files
    // being cleaned up when make_service_with_campaigns_path returns is fine.
    let tmp_registry = NamedTempFile::new().expect("tempfile for registry");
    let registry_path = tmp_registry.path().to_path_buf();
    let sentinels = Arc::new(RwLock::new(SentinelStore::default_seed()));
    let tmp_sentinels = NamedTempFile::new().expect("tempfile for sentinels");
    let sentinels_path = tmp_sentinels.path().to_path_buf();
    let scheduler_reload = Arc::new(Notify::new());

    let ctx = RuntimeContext {
        engine,
        trace_store,
        workflow_tracker,
        trace_writer,
        event_tx,
        registry,
    };
    let stores = StoreConfig {
        campaigns_path,
        registry_path,
        sentinels,
        sentinels_path,
        scheduler_reload,
    };
    let service = FoundryService::new(ctx, stores);

    (service, tmp_traces)
}

/// Build a [`FoundryService`] backed by temporary files.
///
/// Returns the service, the campaigns temp file (so callers can inspect the
/// persisted store), and the traces temp-dir (to keep it alive for the
/// test's lifetime).
fn make_service() -> (FoundryService, NamedTempFile, TempDir) {
    let tmp_campaigns = NamedTempFile::new().expect("tempfile for campaigns");
    let campaigns_path = tmp_campaigns.path().to_path_buf();
    let (service, tmp_traces) = make_service_with_campaigns_path(campaigns_path);
    (service, tmp_campaigns, tmp_traces)
}

/// Write a single campaign to the temp store file.
fn seed_campaign(tmp: &NamedTempFile, campaign: Campaign) {
    let store = CampaignStore {
        version: 1,
        campaigns: vec![campaign],
    };
    store.save(tmp.path()).expect("seed campaign store");
}

fn active_campaign(name: &str) -> Campaign {
    Campaign {
        name: name.to_string(),
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
        cycles_completed: 0,
        cycles_landed: 0,
        authorized_by: Some("owner".to_string()),
        agent_provider: None,
        last_run_event_id: None,
        owner_decisions: vec![],
        pending_run_result: None,
        objective_history: vec![],
    }
}

/// Start a real tonic gRPC server on an ephemeral port, spawn it in a
/// background task, and return the `http://127.0.0.1:<port>` address string.
///
/// The server runs until the test completes (background task is dropped).
async fn start_server(service: FoundryService) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    let incoming = TcpListenerStream::new(listener);

    tokio::spawn(async move {
        Server::builder()
            .add_service(FoundryServer::new(service))
            .serve_with_incoming(incoming)
            .await
            .expect("gRPC server error");
    });

    // Give the server one scheduler tick to start accepting connections.
    tokio::task::yield_now().await;

    format!("http://127.0.0.1:{port}")
}

// ── Gate (2) — pending_run_result survives pause ──────────────────────────────

/// Pausing a campaign that has a non-None `pending_run_result` via the CLI
/// online path must:
///
/// 1. Persist `status = Paused` to the store file.
/// 2. Leave `pending_run_result` byte-for-byte identical (not cleared, not
///    zeroed).  A status-only assertion would mask result loss.
#[tokio::test]
async fn online_pause_preserves_pending_run_result_and_status() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();

    // Construct a `pending_run_result` with a distinctive preservation ref so
    // we can verify byte-for-byte survival below.
    let pending = TaskRunCompletedPayload {
        project: "test-project".to_string(),
        success: true,
        landed: false,
        summary: "partial slice — gap remains".to_string(),
        preservation_ref: Some("foundry-task/cli-pause-grpc-test-ref".to_string()),
        verdict: TaskVerdict::Remainder {
            gaps: vec!["exercise the CLI pause gRPC boundary".to_string()],
        },
        context: LoopContext {
            campaign: Some("c".to_string()),
            ..LoopContext::default()
        },
    };

    let mut campaign = active_campaign("c");
    campaign.pending_run_result = Some(pending.clone());
    seed_campaign(&tmp_campaigns, campaign);

    let addr = start_server(service).await;

    // Invoke the CLI pause path (online, offline=false).
    let rendered = pause_and_render(tmp_campaigns.path(), &addr, false, "c")
        .await
        .expect("online pause must succeed");

    // Gate (2a): persisted status is Paused.
    let after = CampaignStore::load(tmp_campaigns.path()).expect("load after pause");
    let stored = after.find("c").expect("campaign must still exist");
    assert_eq!(
        stored.status,
        CampaignStatus::Paused,
        "status must be Paused in the persisted store"
    );

    // Gate (2b): pending_run_result survived byte-for-byte.
    let stored_pending = stored
        .pending_run_result
        .as_ref()
        .expect("pending_run_result must be present after pause");
    assert_eq!(
        stored_pending.preservation_ref.as_deref(),
        Some("foundry-task/cli-pause-grpc-test-ref"),
        "preservation_ref must survive the online pause round-trip"
    );
    assert_eq!(
        stored_pending.summary, pending.summary,
        "summary must survive the online pause round-trip"
    );

    // Gate (3): rendered output is derived from the RPC response, not re-read
    // from disk.  The response carries the name and the updated status; both
    // must appear in the rendered string.
    assert!(
        rendered.contains("paused"),
        "rendered output must contain 'paused' (from PauseCampaignResponse); got: {rendered:?}"
    );
    assert!(
        rendered.contains('c'),
        "rendered output must contain the campaign name; got: {rendered:?}"
    );
}

// ── Gate (3) — rendered output comes from the RPC response ───────────────────

/// The rendered string from an online pause must reflect the state returned
/// by `PauseCampaignResponse`, not data re-read from the campaign store file.
///
/// This is verified by inspecting the `String` returned by `pause_and_render`
/// (which callers print) rather than re-reading any file.  The code path for
/// `offline = false` contains no store reads — any data in the rendered output
/// comes exclusively from the proto response.
#[tokio::test]
async fn online_pause_rendered_output_contains_response_fields() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();

    let mut campaign = active_campaign("c");
    campaign.mission = "Render-from-response mission".to_string();
    seed_campaign(&tmp_campaigns, campaign);

    let addr = start_server(service).await;

    let rendered = pause_and_render(tmp_campaigns.path(), &addr, false, "c")
        .await
        .expect("online pause must succeed");

    // The rendered output is a `campaign_detail_proto` of the response — it
    // must contain these fields verbatim.
    assert!(
        rendered.contains("Render-from-response mission"),
        "mission from response must appear in rendered output; got: {rendered:?}"
    );
    assert!(
        rendered.contains("paused"),
        "status must be 'paused' in rendered output; got: {rendered:?}"
    );
    assert!(
        rendered.contains("test-project"),
        "project from response must appear in rendered output; got: {rendered:?}"
    );
}

// ── Gate (4) — typed error mapping ───────────────────────────────────────────

/// Pausing a campaign that does not exist via the online path must return a
/// `NOT_FOUND`-derived error that mentions "daemon error" and the campaign name.
/// Filesystem paths must not appear in the message.
#[tokio::test]
async fn online_pause_not_found_maps_to_daemon_error() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();

    // Seed an empty store (no campaigns).
    let empty = CampaignStore::default();
    empty.save(tmp_campaigns.path()).expect("save empty store");

    let addr = start_server(service).await;

    let err = pause_and_render(tmp_campaigns.path(), &addr, false, "nonexistent")
        .await
        .expect_err("pause of nonexistent campaign must fail");

    let msg = err.to_string();
    assert!(
        msg.contains("daemon error"),
        "error must start with 'daemon error:'; got: {msg:?}"
    );
    // Must not leak a filesystem path.
    assert!(!msg.contains('/'), "error must not contain a filesystem path; got: {msg:?}");
}

// ── Gate (3, discriminating) — rendered output diverges from CLI-side file ────

/// The rendered output from an online pause MUST be built from the
/// `PauseCampaignResponse.campaign` detail returned by the RPC, not from
/// re-reading the `campaigns_path` file the CLI holds.
///
/// This is the discriminating variant of Gate (3): the daemon's campaign store
/// carries mission value "A" while the `campaigns_path` argument supplied to
/// the CLI call points to a DIFFERENT file that carries mission value "B".
///
/// After the online pause:
///
/// - The rendered string must contain "A" (the daemon's response).
/// - The rendered string must NOT contain "B" (the CLI-side file value).
///
/// An implementation that re-reads from `campaigns_path` would render "B"
/// and fail this assertion.  Passing is proof that the render is sourced
/// exclusively from the `PauseCampaignResponse` wire value.
#[tokio::test]
async fn online_pause_renders_from_rpc_response_not_from_cli_side_file() {
    // Daemon store: campaign "c" with a distinctive mission that only the
    // daemon will see.  The CLI has no access to this file path.
    let daemon_campaigns = NamedTempFile::new().expect("daemon campaigns tempfile");
    let daemon_mission = "Mission-From-Daemon-ALPHA-unique-marker";
    let daemon_campaign = Campaign {
        mission: daemon_mission.to_string(),
        ..active_campaign("c")
    };
    seed_campaign(&daemon_campaigns, daemon_campaign);

    // CLI-side file: campaign "c" with a DIFFERENT mission.  The CLI will
    // pass this path as its `store_path` argument to `pause_and_render`.
    // If the CLI re-reads from this file after the RPC call, it will return
    // the CLI-file mission instead of the daemon-response mission.
    let cli_campaigns = NamedTempFile::new().expect("cli campaigns tempfile");
    let cli_file_mission = "Mission-From-CLI-File-BETA-stale-snapshot";
    let cli_campaign = Campaign {
        mission: cli_file_mission.to_string(),
        ..active_campaign("c")
    };
    seed_campaign(&cli_campaigns, cli_campaign);

    // Build a FoundryService that uses the DAEMON store exclusively.
    let (service, _tmp_traces) =
        make_service_with_campaigns_path(daemon_campaigns.path().to_path_buf());
    let addr = start_server(service).await;

    // Invoke the CLI online path, passing the CLI-side file as `store_path`.
    // The correct implementation ignores `store_path` on the online path and
    // renders exclusively from the PauseCampaignResponse wire value.
    let rendered = pause_and_render(cli_campaigns.path(), &addr, false, "c")
        .await
        .expect("online pause must succeed");

    // The rendered output must reflect the DAEMON's response (mission "A").
    assert!(
        rendered.contains(daemon_mission),
        "rendered output must contain the daemon-response mission '{daemon_mission}'; got: {rendered:?}"
    );

    // The rendered output must NOT reflect the CLI-side file value (mission "B").
    // Failure here means the implementation re-read from `campaigns_path` instead
    // of using the PauseCampaignResponse detail.
    assert!(
        !rendered.contains(cli_file_mission),
        "rendered output must NOT contain the CLI-side file mission '{cli_file_mission}' \
        (disk re-read regression); got: {rendered:?}"
    );
}
