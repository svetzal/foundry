//! Integration tests for the daemon-authoritative `foundry campaign` CLI boundary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{
    Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::throttle::Throttle;
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

const DUMMY_ADDR: &str = "http://127.0.0.1:0";
const DAEMON_ADD_MISSION: &str = "Daemon add mission marker";
const CLIENT_ADD_MISSION: &str = "Client add mission marker";
const DAEMON_SHOW_MISSION: &str = "Daemon show mission marker";
const CLIENT_SHOW_MISSION: &str = "Client show mission marker";
const DAEMON_PAUSE_MISSION: &str = "Daemon pause mission marker";
const CLIENT_PAUSE_MISSION: &str = "Client pause mission marker";
const DAEMON_RESUME_MISSION: &str = "Daemon resume mission marker";
const CLIENT_RESUME_MISSION: &str = "Client resume mission marker";
const DAEMON_DECIDE_MISSION: &str = "Daemon decide mission marker";
const CLIENT_DECIDE_MISSION: &str = "Client decide mission marker";
const DAEMON_COMPLETE_MISSION: &str = "Daemon complete mission marker";
const CLIENT_COMPLETE_MISSION: &str = "Client complete mission marker";
const DAEMON_LIST_AGENT: &str = "daemon-list-agent";
const CLIENT_LIST_AGENT: &str = "client-list-agent";
const DAEMON_ADVANCE_REASON: &str = "campaign_advance_completed";
const CLIENT_ADVANCE_MARKER: &str = "client advance stale marker";
const OFFLINE_DECISION: &str = "Use the direct file recovery path.";
const OFFLINE_COMPLETION_REASON: &str = "Production evidence confirms the mission shipped.";

fn make_campaign(name: &str, status: CampaignStatus) -> Campaign {
    Campaign {
        name: name.to_string(),
        project: "daemon-project".to_string(),
        mission: format!("Mission for {name}"),
        intent_refs: vec![],
        context_paths: vec![],
        done_evidence: vec![DoneEvidence::Review {
            statement: "done".to_string(),
        }],
        budget: CampaignBudget { max_cycles: 8 },
        escalation: vec![],
        status,
        cycles_completed: 2,
        cycles_landed: 1,
        authorized_by: Some("owner".to_string()),
        agent_provider: None,
        last_run_event_id: Some("run-2".to_string()),
        owner_decisions: vec![],
        pending_run_result: None,
        objective_history: vec![],
    }
}

fn make_campaign_with_mission(
    name: &str,
    status: CampaignStatus,
    mission: &str,
    agent_provider: Option<&str>,
) -> Campaign {
    let mut campaign = make_campaign(name, status);
    campaign.mission = mission.to_string();
    campaign.agent_provider = agent_provider.map(ToOwned::to_owned);
    campaign
}

fn save_campaigns(path: &std::path::Path, campaigns: Vec<Campaign>) {
    CampaignStore {
        version: 1,
        campaigns,
    }
    .save(path)
    .expect("save campaigns");
}

fn daemon_registry(repo_root: &std::path::Path) -> Registry {
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
        }],
    }
}

fn make_service(
    campaigns_path: std::path::PathBuf,
    registry: Registry,
) -> (FoundryService, broadcast::Sender<Event>, TempDir) {
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
    let registry = Arc::new(RwLock::new(registry));
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
        event_tx: event_tx.clone(),
        registry,
    };
    let stores = StoreConfig {
        campaigns_path,
        registry_path,
        sentinels,
        sentinels_path,
        scheduler_reload,
    };
    (FoundryService::new(ctx, stores), event_tx, tmp_traces)
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

fn run_foundry(
    home: &std::path::Path,
    campaigns_path: &std::path::Path,
    registry_path: &std::path::Path,
    addr: &str,
    args: &[String],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_foundry"))
        .arg("--addr")
        .arg(addr)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("FOUNDRY_CAMPAIGNS_PATH", campaigns_path)
        .env("FOUNDRY_REGISTRY_PATH", registry_path)
        .output()
        .expect("run foundry binary")
}

fn stdout_string(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout must be valid UTF-8")
}

fn stderr_string(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr must be valid UTF-8")
}

fn assert_success(output: &std::process::Output, context: &str) {
    assert!(
        output.status.success(),
        "{context}\nstdout: {}\nstderr: {}",
        stdout_string(output),
        stderr_string(output)
    );
}

fn write_definition(dir: &TempDir, name: &str, mission: &str) -> std::path::PathBuf {
    let path = dir.path().join(format!("{name}.json"));
    std::fs::write(
        &path,
        format!(
            "{{\"name\":\"{name}\",\"project\":\"daemon-project\",\"mission\":\"{mission}\",\"done_evidence\":[{{\"kind\":\"review\",\"statement\":\"done\"}}],\"authorized_by\":\"owner\"}}"
        ),
    )
    .expect("write definition");
    path
}

fn seed_client_registry(home: &std::path::Path, repo_root: &std::path::Path) -> std::path::PathBuf {
    let path = home.join(".foundry/registry.json");
    std::fs::create_dir_all(path.parent().expect("registry path parent")).expect("create parent");
    daemon_registry(repo_root).save(&path).expect("save registry");
    path
}

fn missing_client_campaigns_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".foundry/campaigns.json")
}

fn seed_client_campaigns_trap(home: &std::path::Path) -> (std::path::PathBuf, Vec<u8>) {
    let path = home.join(".foundry/campaigns.json");
    std::fs::create_dir_all(path.parent().expect("campaigns path parent")).expect("create parent");
    let bytes = br"trap bytes that must stay untouched".to_vec();
    std::fs::write(&path, &bytes).expect("seed trap campaigns file");
    (path, bytes)
}

fn seed_stale_client_campaigns(home: &std::path::Path) -> std::path::PathBuf {
    let path = home.join(".foundry/campaigns.json");
    std::fs::create_dir_all(path.parent().expect("campaigns path parent")).expect("create parent");
    save_campaigns(
        &path,
        vec![
            make_campaign_with_mission(
                "online-added",
                CampaignStatus::Completed,
                CLIENT_ADD_MISSION,
                Some("client-add-agent"),
            ),
            make_campaign_with_mission(
                "showme",
                CampaignStatus::Completed,
                CLIENT_SHOW_MISSION,
                Some(CLIENT_LIST_AGENT),
            ),
            make_campaign_with_mission(
                "pausable",
                CampaignStatus::Escalated,
                CLIENT_PAUSE_MISSION,
                Some("client-pause-agent"),
            ),
            make_campaign_with_mission(
                "resumable",
                CampaignStatus::Completed,
                CLIENT_RESUME_MISSION,
                Some("client-resume-agent"),
            ),
            make_campaign_with_mission(
                "escalated",
                CampaignStatus::Paused,
                CLIENT_DECIDE_MISSION,
                Some("client-decide-agent"),
            ),
            make_campaign_with_mission(
                "completable",
                CampaignStatus::Active,
                CLIENT_COMPLETE_MISSION,
                Some("client-complete-agent"),
            ),
            make_campaign_with_mission(
                "advanceable",
                CampaignStatus::Completed,
                CLIENT_ADVANCE_MARKER,
                Some("client-advance-agent"),
            ),
        ],
    );
    path
}

struct OnlineHarness {
    _repo_root: TempDir,
    client_home: TempDir,
    registry_path: std::path::PathBuf,
    daemon_campaigns: NamedTempFile,
    addr: String,
}

impl OnlineHarness {
    async fn new() -> Self {
        let repo_root = tempfile::tempdir().expect("repo tempdir");
        let client_home = tempfile::tempdir().expect("client tempdir");
        let registry_path = seed_client_registry(client_home.path(), repo_root.path());
        let daemon_campaigns = NamedTempFile::new().expect("daemon campaigns tempfile");
        save_campaigns(daemon_campaigns.path(), daemon_campaigns_fixture());
        let (service, event_tx, _tmp_traces) =
            make_service(daemon_campaigns.path().to_path_buf(), daemon_registry(repo_root.path()));
        spawn_advance_terminal_bridge(event_tx);
        let addr = start_server(service).await;

        Self {
            _repo_root: repo_root,
            client_home,
            registry_path,
            daemon_campaigns,
            addr,
        }
    }

    fn client_home(&self) -> &std::path::Path {
        self.client_home.path()
    }

    fn daemon_campaigns_path(&self) -> &std::path::Path {
        self.daemon_campaigns.path()
    }

    fn definition(&self) -> std::path::PathBuf {
        write_definition(&self.client_home, "online-added", DAEMON_ADD_MISSION)
    }

    fn run(&self, campaigns_path: &std::path::Path, args: &[String]) -> std::process::Output {
        run_foundry(self.client_home(), campaigns_path, &self.registry_path, &self.addr, args)
    }

    fn online_commands(&self) -> Vec<(String, Vec<String>)> {
        online_command_vectors(&self.definition())
    }
}

fn spawn_advance_terminal_bridge(event_tx: broadcast::Sender<Event>) {
    let mut rx = event_tx.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = rx.recv().await {
            if event.event_type == EventType::CampaignAdvanceRequested {
                let completed = Event::new(
                    EventType::CampaignAdvanceCompleted,
                    event.project.clone(),
                    Throttle::Full,
                    serde_json::json!({
                        "campaign": "advanceable",
                        "cycles_completed": 2,
                        "cycles_landed": 1,
                        "reason": DAEMON_ADVANCE_REASON
                    }),
                );
                let _ = event_tx.send(completed);
                break;
            }
        }
    });
}

fn online_command_vectors(definition: &std::path::Path) -> Vec<(String, Vec<String>)> {
    vec![
        (
            "add".to_string(),
            vec![
                "campaign".to_string(),
                "add".to_string(),
                definition.display().to_string(),
            ],
        ),
        ("list".to_string(), vec!["campaign".to_string(), "list".to_string()]),
        (
            "show".to_string(),
            vec![
                "campaign".to_string(),
                "show".to_string(),
                "showme".to_string(),
            ],
        ),
        (
            "pause".to_string(),
            vec![
                "campaign".to_string(),
                "pause".to_string(),
                "pausable".to_string(),
            ],
        ),
        (
            "resume".to_string(),
            vec![
                "campaign".to_string(),
                "resume".to_string(),
                "resumable".to_string(),
            ],
        ),
        (
            "decide".to_string(),
            vec![
                "campaign".to_string(),
                "decide".to_string(),
                "escalated".to_string(),
                "--decision".to_string(),
                "Use the daemon path.".to_string(),
            ],
        ),
        (
            "complete".to_string(),
            vec![
                "campaign".to_string(),
                "complete".to_string(),
                "completable".to_string(),
                "--reason".to_string(),
                "Production evidence confirms the mission shipped.".to_string(),
            ],
        ),
        (
            "advance".to_string(),
            vec![
                "campaign".to_string(),
                "advance".to_string(),
                "advanceable".to_string(),
            ],
        ),
    ]
}

fn assert_stdout_proves_daemon_boundary(label: &str, stdout: &str) {
    let (expected, forbidden) = match label {
        "add" => (DAEMON_ADD_MISSION, CLIENT_ADD_MISSION),
        "list" => (DAEMON_LIST_AGENT, CLIENT_LIST_AGENT),
        "show" => (DAEMON_SHOW_MISSION, CLIENT_SHOW_MISSION),
        "pause" => (DAEMON_PAUSE_MISSION, CLIENT_PAUSE_MISSION),
        "resume" => (DAEMON_RESUME_MISSION, CLIENT_RESUME_MISSION),
        "decide" => (DAEMON_DECIDE_MISSION, CLIENT_DECIDE_MISSION),
        "complete" => (DAEMON_COMPLETE_MISSION, CLIENT_COMPLETE_MISSION),
        "advance" => (DAEMON_ADVANCE_REASON, CLIENT_ADVANCE_MARKER),
        other => panic!("unexpected label: {other}"),
    };

    assert!(
        stdout.contains(expected),
        "online {label}: stdout must contain daemon-owned marker {expected:?}\nstdout: {stdout}"
    );
    assert!(
        !stdout.contains(forbidden),
        "online {label}: stdout must not contain stale client-side marker {forbidden:?}\nstdout: {stdout}"
    );
}

fn assert_daemon_mutations(campaigns_path: &std::path::Path) {
    let store = CampaignStore::load(campaigns_path).expect("load daemon campaigns");
    assert!(store.find("online-added").is_some());
    assert_eq!(store.find("pausable").expect("pausable").status, CampaignStatus::Paused);
    assert_eq!(store.find("resumable").expect("resumable").status, CampaignStatus::Active);
    let escalated = store.find("escalated").expect("escalated");
    assert_eq!(escalated.status, CampaignStatus::Active);
    assert_eq!(escalated.owner_decisions.len(), 1);
    assert_eq!(escalated.owner_decisions[0].decision, "Use the daemon path.");
    let completable = store.find("completable").expect("completable");
    assert_eq!(completable.status, CampaignStatus::Completed);
}

fn assert_unreachable_online_failure(output: &std::process::Output, label: &str) {
    assert!(!output.status.success(), "unreachable online {label}: command should fail");
    let stderr = stderr_string(output);
    assert!(
        stderr.contains("foundryd is not reachable at http://127.0.0.1:0"),
        "unreachable online {label}: {stderr}"
    );
}

fn offline_command(
    home: &std::path::Path,
    campaigns_path: &std::path::Path,
    registry_path: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    let owned_args = std::iter::once("--offline".to_string())
        .chain(args.iter().map(|arg| (*arg).to_string()))
        .collect::<Vec<_>>();
    run_foundry(home, campaigns_path, registry_path, DUMMY_ADDR, &owned_args)
}

fn assert_offline_campaign_status(
    campaigns_path: &std::path::Path,
    campaign: &str,
    status: CampaignStatus,
) {
    assert_eq!(
        CampaignStore::load(campaigns_path)
            .expect("load offline campaigns")
            .find(campaign)
            .expect("campaign must exist")
            .status,
        status
    );
}

fn augment_offline_recovery_campaigns(campaigns_path: &std::path::Path) {
    let mut store = CampaignStore::load(campaigns_path).expect("load campaigns");
    store.campaigns.push(make_campaign("resume-offline", CampaignStatus::Paused));
    let mut decide = make_campaign("decide-offline", CampaignStatus::Escalated);
    decide.escalation.push("owner choice".to_string());
    store.campaigns.push(decide);
    let mut complete = make_campaign("complete-offline", CampaignStatus::Paused);
    complete.pending_run_result = Some(foundry_sdk::payload::TaskRunCompletedPayload {
        project: "daemon-project".to_string(),
        success: true,
        landed: true,
        summary: "done".to_string(),
        preservation_ref: None,
        verdict: foundry_sdk::payload::TaskVerdict::Complete,
        context: foundry_sdk::payload::LoopContext {
            campaign: Some("complete-offline".to_string()),
            ..foundry_sdk::payload::LoopContext::default()
        },
    });
    store.campaigns.push(complete);
    store.save(campaigns_path).expect("save augmented campaigns");
}

fn assert_offline_recovery_results(campaigns_path: &std::path::Path) {
    let saved = CampaignStore::load(campaigns_path).expect("load final campaigns");
    assert_eq!(
        saved.find("resume-offline").expect("resume-offline").status,
        CampaignStatus::Active
    );
    let decided = saved.find("decide-offline").expect("decide-offline");
    assert_eq!(decided.status, CampaignStatus::Active);
    assert_eq!(decided.owner_decisions.len(), 1);
    let completed = saved.find("complete-offline").expect("complete-offline");
    assert_eq!(completed.status, CampaignStatus::Completed);
    assert!(completed.pending_run_result.is_none());
}

fn daemon_campaigns_fixture() -> Vec<Campaign> {
    vec![
        make_campaign_with_mission(
            "showme",
            CampaignStatus::Active,
            DAEMON_SHOW_MISSION,
            Some(DAEMON_LIST_AGENT),
        ),
        make_campaign_with_mission(
            "pausable",
            CampaignStatus::Active,
            DAEMON_PAUSE_MISSION,
            Some("daemon-pause-agent"),
        ),
        make_campaign_with_mission(
            "resumable",
            CampaignStatus::Paused,
            DAEMON_RESUME_MISSION,
            Some("daemon-resume-agent"),
        ),
        make_campaign_with_mission(
            "escalated",
            CampaignStatus::Escalated,
            DAEMON_DECIDE_MISSION,
            Some("daemon-decide-agent"),
        ),
        make_campaign_with_mission(
            "completable",
            CampaignStatus::Paused,
            DAEMON_COMPLETE_MISSION,
            Some("daemon-complete-agent"),
        ),
        make_campaign_with_mission(
            "advanceable",
            CampaignStatus::Active,
            "daemon advance mission marker",
            Some("daemon-advance-agent"),
        ),
    ]
}

#[tokio::test(flavor = "multi_thread")]
async fn online_campaign_commands_ignore_absent_client_campaign_path() {
    let harness = OnlineHarness::new().await;
    let client_campaigns = missing_client_campaigns_path(harness.client_home());

    for (label, args) in harness.online_commands() {
        let output = harness.run(&client_campaigns, &args);
        assert_success(&output, &format!("online {label}"));
        assert!(
            !client_campaigns.exists(),
            "online {label}: client campaigns path must remain absent"
        );
    }

    assert_daemon_mutations(harness.daemon_campaigns_path());
}

#[tokio::test(flavor = "multi_thread")]
async fn online_campaign_commands_ignore_trap_client_campaign_path() {
    let harness = OnlineHarness::new().await;
    let (client_campaigns, before) = seed_client_campaigns_trap(harness.client_home());

    for (label, args) in harness.online_commands() {
        let output = harness.run(&client_campaigns, &args);
        assert_success(&output, &format!("online {label}"));
        assert_eq!(
            std::fs::read(&client_campaigns).expect("read trap campaigns after online command"),
            before,
            "online {label}: client campaigns trap bytes must remain unchanged"
        );
    }

    assert_daemon_mutations(harness.daemon_campaigns_path());
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_online_campaign_commands_leave_absent_client_path_absent() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let client_home = tempfile::tempdir().expect("client tempdir");
    let registry_path = seed_client_registry(client_home.path(), repo_root.path());
    let client_campaigns = missing_client_campaigns_path(client_home.path());
    let definition = write_definition(&client_home, "online-added", DAEMON_ADD_MISSION);

    for (label, args) in online_command_vectors(&definition) {
        let output =
            run_foundry(client_home.path(), &client_campaigns, &registry_path, DUMMY_ADDR, &args);
        assert_unreachable_online_failure(&output, &label);
        assert!(
            !client_campaigns.exists(),
            "unreachable online {label}: client campaigns path must remain absent"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unreachable_online_campaign_commands_leave_trap_bytes_unchanged() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let client_home = tempfile::tempdir().expect("client tempdir");
    let registry_path = seed_client_registry(client_home.path(), repo_root.path());
    let (client_campaigns, before) = seed_client_campaigns_trap(client_home.path());
    let definition = write_definition(&client_home, "online-added", DAEMON_ADD_MISSION);

    for (label, args) in online_command_vectors(&definition) {
        let output =
            run_foundry(client_home.path(), &client_campaigns, &registry_path, DUMMY_ADDR, &args);
        assert_unreachable_online_failure(&output, &label);
        assert_eq!(
            std::fs::read(&client_campaigns).expect("read client trap after failed online command"),
            before,
            "unreachable online {label}: trap bytes must remain unchanged"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_campaign_recovery_still_works() {
    let repo_root = tempfile::tempdir().expect("repo tempdir");
    let client_home = tempfile::tempdir().expect("client tempdir");
    let registry_path = seed_client_registry(client_home.path(), repo_root.path());
    let campaigns_path = client_home.path().join(".foundry/campaigns.json");
    let definition = write_definition(&client_home, "offline-added", "Offline add mission marker");

    let definition_arg = definition.display().to_string();
    let add_output = offline_command(
        client_home.path(),
        &campaigns_path,
        &registry_path,
        &["campaign", "add", definition_arg.as_str()],
    );
    assert_success(&add_output, "offline add");

    let list_output =
        offline_command(client_home.path(), &campaigns_path, &registry_path, &["campaign", "list"]);
    assert_success(&list_output, "offline list");
    assert!(stdout_string(&list_output).contains("offline-added"));

    let show_output = offline_command(
        client_home.path(),
        &campaigns_path,
        &registry_path,
        &["campaign", "show", "offline-added"],
    );
    assert_success(&show_output, "offline show");
    assert!(stdout_string(&show_output).contains("offline-added"));

    let pause_output = offline_command(
        client_home.path(),
        &campaigns_path,
        &registry_path,
        &["campaign", "pause", "offline-added"],
    );
    assert_success(&pause_output, "offline pause");
    assert_offline_campaign_status(&campaigns_path, "offline-added", CampaignStatus::Paused);

    augment_offline_recovery_campaigns(&campaigns_path);

    let resume_output = offline_command(
        client_home.path(),
        &campaigns_path,
        &registry_path,
        &["campaign", "resume", "resume-offline"],
    );
    assert_success(&resume_output, "offline resume");

    let decide_output = offline_command(
        client_home.path(),
        &campaigns_path,
        &registry_path,
        &[
            "campaign",
            "decide",
            "decide-offline",
            "--decision",
            OFFLINE_DECISION,
        ],
    );
    assert_success(&decide_output, "offline decide");

    let complete_output = offline_command(
        client_home.path(),
        &campaigns_path,
        &registry_path,
        &[
            "campaign",
            "complete",
            "complete-offline",
            "--reason",
            OFFLINE_COMPLETION_REASON,
        ],
    );
    assert_success(&complete_output, "offline complete");

    assert_offline_recovery_results(&campaigns_path);
}

#[tokio::test(flavor = "multi_thread")]
async fn online_campaign_commands_render_daemon_owned_fields_not_client_store_fields() {
    let harness = OnlineHarness::new().await;
    let client_campaigns = seed_stale_client_campaigns(harness.client_home());
    let before = std::fs::read(&client_campaigns).expect("read stale client campaigns before run");

    for (label, args) in harness.online_commands() {
        let output = harness.run(&client_campaigns, &args);
        assert_success(&output, &format!("online {label}"));
        let stdout = stdout_string(&output);
        assert_stdout_proves_daemon_boundary(&label, &stdout);
        assert_eq!(
            std::fs::read(&client_campaigns)
                .expect("read stale client campaigns after online command"),
            before,
            "online {label}: stale client campaigns file must remain byte-identical"
        );
    }

    assert_daemon_mutations(harness.daemon_campaigns_path());
}
