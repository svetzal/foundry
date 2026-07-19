//! Integration tests for `foundry campaign resume` via the gRPC online path.
//!
//! Each test starts a real [`FoundryService`] bound to a temporary TCP port,
//! then calls [`foundry_cli::campaign_commands::resume_and_render`] with
//! `offline = false` so the call travels through the generated tonic client.
//!
//! Gates verified by this file:
//!
//! - Online resume of a Paused campaign with `add_cycles=N`: reloaded store
//!   `max_cycles` increases by exactly N and `status=active`.
//! - Online resume of an Escalated campaign: `status=active`, and
//!   `pending_run_result` is preserved byte-for-byte in the reloaded store.
//! - Not-found errors surface as "daemon error" without filesystem paths.
//! - Rendered output comes from the `ResumeCampaignResponse.campaign` detail,
//!   not from a re-read of the CLI-side store file (discriminating variant).

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_cli::campaign_commands::resume_and_render;
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

fn make_service() -> (FoundryService, NamedTempFile, TempDir) {
    let tmp_campaigns = NamedTempFile::new().expect("tempfile for campaigns");
    let campaigns_path = tmp_campaigns.path().to_path_buf();
    let (service, tmp_traces) = make_service_with_campaigns_path(campaigns_path);
    (service, tmp_campaigns, tmp_traces)
}

fn seed_campaign(tmp: &NamedTempFile, campaign: Campaign) {
    let store = CampaignStore {
        version: 1,
        campaigns: vec![campaign],
    };
    store.save(tmp.path()).expect("seed campaign store");
}

fn paused_campaign(name: &str) -> Campaign {
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
        status: CampaignStatus::Paused,
        cycles_completed: 3,
        cycles_landed: 2,
        authorized_by: Some("owner".to_string()),
        agent_provider: None,
        last_run_event_id: None,
        owner_decisions: vec![],
        pending_run_result: None,
    }
}

fn escalated_campaign_with_pending_result(name: &str) -> Campaign {
    Campaign {
        name: name.to_string(),
        project: "test-project".to_string(),
        mission: "Escalated mission".to_string(),
        intent_refs: vec![],
        context_paths: vec![],
        done_evidence: vec![DoneEvidence::Review {
            statement: "done".to_string(),
        }],
        budget: CampaignBudget { max_cycles: 10 },
        escalation: vec!["budget limit".to_string()],
        status: CampaignStatus::Escalated,
        cycles_completed: 3,
        cycles_landed: 2,
        authorized_by: Some("owner".to_string()),
        agent_provider: None,
        last_run_event_id: Some("run-3".to_string()),
        owner_decisions: vec![],
        pending_run_result: Some(TaskRunCompletedPayload {
            project: "test-project".to_string(),
            success: false,
            landed: false,
            summary: "escalated: boundary choice needed".to_string(),
            preservation_ref: Some("foundry-task/cli-resume-grpc-test-ref".to_string()),
            verdict: TaskVerdict::BlockedOnDecision {
                finding: "which path to take".to_string(),
                options: vec!["path-a".to_string(), "path-b".to_string()],
            },
            context: LoopContext {
                campaign: Some(name.to_string()),
                ..LoopContext::default()
            },
        }),
    }
}

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

    tokio::task::yield_now().await;

    format!("http://127.0.0.1:{port}")
}

// ── Online resume: Paused + add_cycles=N ─────────────────────────────────────

/// The CLI online resume path for a Paused campaign with `add_cycles=N` must:
///
/// 1. Persist `status = Active` to the reloaded store.
/// 2. Persist `max_cycles = original + N` to the reloaded store.
/// 3. Produce rendered output that contains "active" and the campaign name.
#[tokio::test]
async fn online_resume_paused_with_add_cycles_extends_budget_and_sets_active() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    let original_max = 10u64;
    let add = 3u64;

    let mut campaign = paused_campaign("c");
    campaign.budget.max_cycles = original_max;
    seed_campaign(&tmp_campaigns, campaign);

    let addr = start_server(service).await;

    let rendered = resume_and_render(tmp_campaigns.path(), &addr, false, "c", add)
        .await
        .expect("online resume must succeed");

    // Reloaded store: status=Active, max_cycles=original+add.
    let after = CampaignStore::load(tmp_campaigns.path()).expect("load after resume");
    let stored = after.find("c").expect("campaign must still exist");
    assert_eq!(
        stored.status,
        CampaignStatus::Active,
        "status must be Active in the persisted store"
    );
    assert_eq!(
        stored.budget.max_cycles,
        original_max + add,
        "max_cycles must be extended by add_cycles in the persisted store"
    );

    // Rendered output must reflect the active state and campaign identity.
    assert!(
        rendered.contains("active"),
        "rendered output must contain 'active'; got: {rendered:?}"
    );
    assert!(
        rendered.contains("c"),
        "rendered output must contain the campaign name; got: {rendered:?}"
    );
}

// ── Online resume: Escalated + pending_run_result preserved ──────────────────

/// Resuming an Escalated campaign via the CLI online path must:
///
/// 1. Persist `status = Active`.
/// 2. Leave `pending_run_result` byte-for-byte identical to the seeded value.
///
/// A status-only assertion would mask result loss.
#[tokio::test]
async fn online_resume_escalated_preserves_pending_run_result_and_sets_active() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    seed_campaign(&tmp_campaigns, escalated_campaign_with_pending_result("c"));

    let addr = start_server(service).await;

    let rendered = resume_and_render(tmp_campaigns.path(), &addr, false, "c", 0)
        .await
        .expect("online resume of Escalated campaign must succeed");

    // Reloaded store: status=Active and pending_run_result intact.
    let after = CampaignStore::load(tmp_campaigns.path()).expect("load after resume");
    let stored = after.find("c").expect("campaign must still exist");
    assert_eq!(
        stored.status,
        CampaignStatus::Active,
        "status must be Active after resuming an Escalated campaign"
    );

    let pending = stored
        .pending_run_result
        .as_ref()
        .expect("pending_run_result must be present after resume");
    assert_eq!(
        pending.preservation_ref.as_deref(),
        Some("foundry-task/cli-resume-grpc-test-ref"),
        "preservation_ref must survive the online resume round-trip"
    );
    assert_eq!(
        pending.summary, "escalated: boundary choice needed",
        "summary must survive the online resume round-trip"
    );

    assert!(
        rendered.contains("active"),
        "rendered output must contain 'active'; got: {rendered:?}"
    );
}

// ── Online resume: not-found maps to daemon error without path ────────────────

/// Resuming a campaign that does not exist via the online path must return a
/// "daemon error" that mentions the campaign name and contains no filesystem
/// paths.
#[tokio::test]
async fn online_resume_not_found_maps_to_daemon_error_without_path() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();

    // Seed an empty store.
    let empty = CampaignStore::default();
    empty.save(tmp_campaigns.path()).expect("save empty store");

    let addr = start_server(service).await;

    let err = resume_and_render(tmp_campaigns.path(), &addr, false, "nonexistent", 0)
        .await
        .expect_err("resume of nonexistent campaign must fail");

    let msg = err.to_string();
    assert!(msg.contains("daemon error"), "error must mention 'daemon error'; got: {msg:?}");
    assert!(!msg.contains('/'), "error must not contain a filesystem path; got: {msg:?}");
}

// ── Online resume: rendered output from RPC response, not CLI-side file ───────

/// The rendered output from an online resume MUST be built from the
/// `ResumeCampaignResponse.campaign` detail returned by the RPC, not from
/// re-reading the `campaigns_path` file the CLI holds.
///
/// The daemon's campaign store carries mission "A"; the CLI-side file carries
/// mission "B". After the online resume, the rendered string must contain "A"
/// and must NOT contain "B".
#[tokio::test]
async fn online_resume_renders_from_rpc_response_not_from_cli_side_file() {
    let daemon_campaigns = NamedTempFile::new().expect("daemon campaigns tempfile");
    let daemon_mission = "Mission-From-Daemon-RESUME-unique-marker";
    let daemon_campaign = Campaign {
        mission: daemon_mission.to_string(),
        ..paused_campaign("c")
    };
    seed_campaign(&daemon_campaigns, daemon_campaign);

    let cli_campaigns = NamedTempFile::new().expect("cli campaigns tempfile");
    let cli_file_mission = "Mission-From-CLI-File-RESUME-stale-snapshot";
    let cli_campaign = Campaign {
        mission: cli_file_mission.to_string(),
        ..paused_campaign("c")
    };
    seed_campaign(&cli_campaigns, cli_campaign);

    let (service, _tmp_traces) =
        make_service_with_campaigns_path(daemon_campaigns.path().to_path_buf());
    let addr = start_server(service).await;

    // CLI passes its own (stale) file; implementation must not re-read it.
    let rendered = resume_and_render(cli_campaigns.path(), &addr, false, "c", 0)
        .await
        .expect("online resume must succeed");

    assert!(
        rendered.contains(daemon_mission),
        "rendered output must contain the daemon-response mission '{daemon_mission}'; got: {rendered:?}"
    );
    assert!(
        !rendered.contains(cli_file_mission),
        "rendered output must NOT contain the CLI-side file mission '{cli_file_mission}' \
        (disk re-read regression); got: {rendered:?}"
    );
}
