//! Integration tests for `EditCampaign` through the generated tonic client.
//!
//! The daemon's shell is a fake, so the done-gate probe runs no process.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::shell::CommandResult;
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{Campaign, CampaignStatus, CampaignStore, DoneEvidence};
use foundry_sdk::gateway::fakes::FakeShellGateway;
use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};
use foundry_sdk::sentinel::SentinelStore;
use foundryd::{
    proto::{EditCampaignRequest, foundry_client::FoundryClient, foundry_server::FoundryServer},
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::Code;
use tonic::transport::Server;

fn registry_with_project(repo_root: &std::path::Path) -> Registry {
    Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: "daemon-project".to_string(),
            path: repo_root.display().to_string(),
            stack: Stack::Rust,
            agent: "codex".to_string(),
            repo: "daemon/project".to_string(),
            branch: "main".to_string(),
            skip: None,
            actions: ActionFlags::default(),
            install: None,
            installs_skill: None,
            notes: None,
            timeout_secs: None,
            audit_exceptions: vec![],
            update_policy: None,
        }],
    }
}

struct Daemon {
    addr: String,
    campaigns: NamedTempFile,
    shell: Arc<FakeShellGateway>,
    _traces: TempDir,
}

async fn start(registry: Registry, seed: Vec<Campaign>, shell: Arc<FakeShellGateway>) -> Daemon {
    let campaigns = NamedTempFile::new().expect("tempfile for campaigns");
    CampaignStore {
        campaigns: seed,
        ..CampaignStore::default()
    }
    .save(campaigns.path())
    .expect("seed campaigns");

    let (event_tx, _rx) = broadcast::channel(64);
    let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));
    let traces = tempfile::tempdir().expect("tempdir for traces");
    let trace_writer =
        Arc::new(TraceWriter::new(traces.path().to_str().expect("trace dir must be UTF-8")));
    let trace_store = Arc::new(TraceStore::with_trace_writer(
        Duration::from_secs(60),
        Arc::clone(&trace_writer),
    ));
    let ctx = RuntimeContext {
        engine,
        trace_store,
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer,
        event_tx,
        registry: Arc::new(RwLock::new(registry)),
    };
    let stores = StoreConfig {
        work_items_path: std::path::PathBuf::new(),
        events_dir: std::path::PathBuf::new(),
        campaigns_path: campaigns.path().to_path_buf(),
        registry_path: NamedTempFile::new().expect("registry").path().to_path_buf(),
        sentinels: Arc::new(RwLock::new(SentinelStore::default_seed())),
        sentinels_path: NamedTempFile::new().expect("sentinels").path().to_path_buf(),
        scheduler_reload: Arc::new(Notify::new()),
    };
    let service = FoundryService::new(ctx, stores).with_shell(shell.clone());

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
    Daemon {
        addr: format!("http://127.0.0.1:{port}"),
        campaigns,
        shell,
        _traces: traces,
    }
}

fn campaign(name: &str, status: &str, authorized: bool) -> Campaign {
    let mut value = serde_json::json!({
        "name": name,
        "project": "daemon-project",
        "mission": "Remove real timeouts from the test suite",
        "status": status,
        "cycles_completed": 3,
        "budget": {"max_cycles": 3},
        "done_evidence": [{"kind": "gate", "command": "! rg -n Timeout src", "required": true}]
    });
    if authorized {
        value["authorized_by"] = "owner".into();
    }
    if status == "escalated" {
        value["open_escalation"] = serde_json::json!({
            "reason": "done gate still matches after the mission landed",
            "escalated_at": "2026-10-19T03:00:00Z",
            "cycles_completed": 3,
            "cycles_landed": 3
        });
    }
    serde_json::from_value(value).expect("campaign fixture")
}

const REPLACEMENT: &str = r#"[
    {"kind": "gate", "command": "! rg -n 'Thread.sleep|Duration::from_secs' src/test", "required": true},
    {"kind": "review", "statement": "java.net.SocketTimeoutException is a type name, not a real timeout"}
]"#;

fn edit(name: &str, evidence: &str, reason: &str) -> EditCampaignRequest {
    EditCampaignRequest {
        name: name.to_string(),
        done_evidence_json: evidence.to_string(),
        reason: reason.to_string(),
    }
}

fn result(stdout: &str, exit_code: i32) -> CommandResult {
    CommandResult {
        stdout: stdout.to_string(),
        stderr: String::new(),
        exit_code,
        success: exit_code == 0,
    }
}

/// The escalated campaign that started this: its lexical gate matched a type
/// name. The owner replaces the evidence; the campaign returns to active on a
/// spent budget, the record keeps both evidence sets, and the response carries
/// the new gate's trunk probe.
#[tokio::test]
async fn edit_replaces_evidence_records_the_amendment_reactivates_and_probes_trunk() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let daemon = start(
        registry_with_project(repo_root.path()),
        vec![campaign("timeouts", "escalated", true)],
        FakeShellGateway::always(result("", 0)),
    )
    .await;
    let mut client = FoundryClient::connect(daemon.addr.clone()).await.expect("connect");

    let response = client
        .edit_campaign(edit("timeouts", REPLACEMENT, "the grep matched SocketTimeoutException"))
        .await
        .expect("edit succeeds")
        .into_inner();

    let detail = response.campaign.expect("detail");
    assert_eq!(detail.status, "active");
    assert!(detail.open_escalation.is_none());
    assert_eq!(detail.max_cycles, 3, "edit adds no cycles");
    let record = detail.owner_decisions.last().expect("owner decision");
    assert_eq!(
        record.decision,
        "Done evidence amended: the grep matched SocketTimeoutException"
    );
    assert_eq!(record.authorized_by, "owner");
    assert!(chrono::DateTime::parse_from_rfc3339(&record.decided_at).is_ok());
    let amendment = record.done_evidence_amendment.as_ref().expect("amendment");
    assert_eq!(amendment.previous.len(), 1);
    assert_eq!(amendment.previous[0].command, "! rg -n Timeout src");
    assert_eq!(amendment.replacement.len(), 2);
    assert_eq!(amendment.replacement[1].kind, "review");

    assert_eq!(response.gate_probe_error, None);
    assert_eq!(response.gate_probes.len(), 1);
    assert_eq!(response.gate_probes[0].name, "campaign_done_1");
    assert!(response.gate_probes[0].passed);
    let invocations = daemon.shell.invocations();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0].working_dir, repo_root.path().display().to_string());
    assert_eq!(
        invocations[0].args,
        vec![
            "-c".to_string(),
            "! rg -n 'Thread.sleep|Duration::from_secs' src/test".to_string()
        ]
    );

    let stored = CampaignStore::load(daemon.campaigns.path()).expect("load");
    let stored = stored.find("timeouts").expect("stored");
    assert_eq!(stored.status, CampaignStatus::Active);
    assert_eq!(stored.done_evidence.len(), 2);
    assert!(matches!(&stored.done_evidence[1], DoneEvidence::Review { .. }));
    assert!(stored.owner_decisions.last().unwrap().done_evidence_amendment.is_some());
}

/// Active and paused campaigns keep their status; a staged campaign can
/// correct a gate before its first cycle.
#[tokio::test]
async fn edit_keeps_the_status_of_a_campaign_that_is_not_escalated() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let daemon = start(
        registry_with_project(repo_root.path()),
        vec![
            campaign("staged", "staged", true),
            campaign("active", "active", true),
            campaign("paused", "paused", true),
        ],
        FakeShellGateway::success(),
    )
    .await;
    let mut client = FoundryClient::connect(daemon.addr.clone()).await.expect("connect");
    for name in ["staged", "active", "paused"] {
        let detail = client
            .edit_campaign(edit(name, REPLACEMENT, "narrow the gate"))
            .await
            .expect("edit succeeds")
            .into_inner()
            .campaign
            .expect("detail");
        assert_eq!(detail.status, name, "{name} keeps its status");
    }
}

/// Every refusal saves nothing and runs no gate.
#[tokio::test]
async fn edit_refusals_change_nothing_and_probe_nothing() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let daemon = start(
        registry_with_project(repo_root.path()),
        vec![
            campaign("escalated", "escalated", true),
            campaign("unowned", "escalated", false),
            campaign("completed", "completed", true),
            campaign("cancelled", "cancelled", true),
        ],
        FakeShellGateway::success(),
    )
    .await;
    let mut client = FoundryClient::connect(daemon.addr.clone()).await.expect("connect");
    let before = std::fs::read(daemon.campaigns.path()).expect("read store");
    let current = r#"[{"kind": "gate", "command": "! rg -n Timeout src", "required": true}]"#;
    let remote =
        r#"[{"kind": "gate", "command": "ssh odin 'systemctl is-active x'", "required": true}]"#;

    for (request, code, message) in [
        (
            edit("escalated", REPLACEMENT, "  "),
            Code::InvalidArgument,
            "reason must be non-empty",
        ),
        (
            edit("escalated", "{ not json", "why"),
            Code::InvalidArgument,
            "done evidence JSON is invalid",
        ),
        (
            edit("escalated", "[]", "why"),
            Code::InvalidArgument,
            "at least one done_evidence",
        ),
        (
            edit("escalated", remote, "why"),
            Code::InvalidArgument,
            "cannot run in a task worktree",
        ),
        (
            edit("escalated", current, "why"),
            Code::FailedPrecondition,
            "already has this done evidence",
        ),
        (
            edit("unowned", REPLACEMENT, "why"),
            Code::FailedPrecondition,
            "edit requires authorized_by",
        ),
        (
            edit("completed", REPLACEMENT, "why"),
            Code::FailedPrecondition,
            "edit requires Staged, Active, Paused or Escalated",
        ),
        (
            edit("cancelled", REPLACEMENT, "why"),
            Code::FailedPrecondition,
            "edit requires Staged, Active, Paused or Escalated",
        ),
        (
            edit("missing", REPLACEMENT, "why"),
            Code::NotFound,
            "campaign 'missing' not found",
        ),
    ] {
        let label = format!("{} / {}", request.name, request.done_evidence_json);
        let err = client.edit_campaign(request).await.expect_err("edit is refused");
        assert_eq!(err.code(), code, "{label}: {}", err.message());
        assert!(err.message().contains(message), "{label}: {}", err.message());
    }

    assert_eq!(std::fs::read(daemon.campaigns.path()).expect("read store"), before);
    assert!(daemon.shell.invocations().is_empty(), "a refused edit runs no gate");
}

/// A campaign whose project is no longer registered still takes the
/// amendment; the response says the gates could not be probed.
#[tokio::test]
async fn edit_of_an_unregistered_project_applies_and_reports_the_probe_was_not_run() {
    let daemon = start(
        Registry {
            version: 2,
            projects: vec![],
        },
        vec![campaign("orphan", "escalated", true)],
        FakeShellGateway::success(),
    )
    .await;
    let mut client = FoundryClient::connect(daemon.addr.clone()).await.expect("connect");

    let response = client
        .edit_campaign(edit("orphan", REPLACEMENT, "narrow the gate"))
        .await
        .expect("edit succeeds")
        .into_inner();

    assert_eq!(response.campaign.expect("detail").status, "active");
    assert_eq!(response.gate_probes, Vec::<foundryd::proto::CampaignGateProbe>::new());
    assert!(
        response
            .gate_probe_error
            .as_deref()
            .is_some_and(|e| e.contains("not registered")),
        "{:?}",
        response.gate_probe_error
    );
    assert!(daemon.shell.invocations().is_empty());
}
