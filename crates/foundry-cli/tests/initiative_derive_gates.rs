//! A real CLI carries declared integration gates through the typed daemon RPC.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::CampaignStore;
use foundry_sdk::initiative::DoneEvidence;
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundryd::proto::foundry_server::FoundryServer;
use foundryd::service::{FoundryService, RuntimeContext, StoreConfig};
use foundryd::trace_store::TraceStore;
use foundryd::workflow_tracker::WorkflowTracker;
use serde_json::json;
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;

fn git(root: &Path, args: &[&str]) {
    assert!(
        std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap()
            .status
            .success()
    );
}

fn intent_repository(root: &Path) {
    std::fs::create_dir(root).unwrap();
    git(root, &["init", "--quiet", "--initial-branch=main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    std::fs::create_dir(root.join(".et")).unwrap();
    std::fs::write(root.join(".et/projection.json"), json!({"goals":[{"id":"outcome","description":"A usable outcome","success_criteria":"The owner can use the contract","state":"create"}],"interactions":[{"id":"contract","description":"The contract works","state":"create","goals":["outcome"]}],"journeys":[]}).to_string()).unwrap();
    git(root, &["add", ".et/projection.json"]);
    git(root, &["commit", "--quiet", "-m", "Seed synthetic intent"]);
}

/// Model an older server's method table while leaving its legacy decision
/// method live. A client fallback would mutate the seeded store and fail proof.
#[derive(Clone)]
struct DecisionCompatibilityServer {
    inner: FoundryServer<FoundryService>,
    legacy: bool,
}

impl tonic::server::NamedService for DecisionCompatibilityServer {
    const NAME: &'static str = "foundry.Foundry";
}

impl tonic::codegen::Service<tonic::codegen::http::Request<tonic::body::Body>>
    for DecisionCompatibilityServer
{
    type Response = tonic::codegen::http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = tonic::codegen::BoxFuture<Self::Response, Self::Error>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        <FoundryServer<FoundryService> as tonic::codegen::Service<
            tonic::codegen::http::Request<tonic::body::Body>,
        >>::poll_ready(&mut self.inner, cx)
    }

    fn call(
        &mut self,
        mut request: tonic::codegen::http::Request<tonic::body::Body>,
    ) -> Self::Future {
        if self.legacy
            && matches!(
                request.uri().path().rsplit('/').next(),
                Some("RecordInitiativeDecision" | "RecoverCampaign" | "RecoverInitiative")
            )
        {
            *request.uri_mut() = "/foundry.Foundry/UnavailableDecisionMethod".parse().unwrap();
        }
        self.inner.call(request)
    }
}

async fn daemon(home: &Path, source: &Path) -> (String, tokio::task::JoinHandle<()>) {
    daemon_with_legacy_decisions(home, source, false).await
}

async fn daemon_with_legacy_decisions(
    home: &Path,
    source: &Path,
    legacy: bool,
) -> (String, tokio::task::JoinHandle<()>) {
    let (event_tx, _receiver) = broadcast::channel(64);
    let trace_writer = Arc::new(TraceWriter::new(home.join("traces").to_str().unwrap()));
    let registry: Registry = serde_json::from_value(json!({"version":2,"projects":[{"name":"sample","path":source,"stack":"rust","agent":"codex","repo":"local/sample","branch":"main"}]})).unwrap();
    let ctx = RuntimeContext {
        engine: Arc::new(Engine::new()),
        trace_store: Arc::new(TraceStore::with_trace_writer(
            Duration::from_secs(60),
            trace_writer.clone(),
        )),
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer,
        event_tx,
        registry: Arc::new(RwLock::new(registry)),
    };
    let service = FoundryService::new(
        ctx,
        StoreConfig {
            campaigns_path: home.join("campaigns.json"),
            work_items_path: home.join("work-items.json"),
            events_dir: home.join("events"),
            registry_path: home.join("registry.json"),
            sentinels: Arc::new(RwLock::new(SentinelStore::default_seed())),
            sentinels_path: home.join("sentinels.json"),
            scheduler_reload: Arc::new(Notify::new()),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(DecisionCompatibilityServer {
                inner: FoundryServer::new(service),
                legacy,
            })
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    (address, server)
}

#[tokio::test]
async fn cli_derives_with_gate_file_preserving_tracker_reviews_and_declared_host_artifacts() {
    let home = tempfile::tempdir().unwrap();
    let source = home.path().join("source");
    intent_repository(&source);
    let file = home.path().join("gates.json");
    std::fs::write(&file, r#"[{"kind":"gate","command":"python3 sample/integrate.py","host":"build-host","required":true,"artifacts":["sample/contract.json"]}]"#).unwrap();
    let (address, server) = daemon(home.path(), &source).await;
    let output = derive_cli(&address, &source, &file).await;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("Derived initiative 'outcome'"));
    let store = CampaignStore::load(&home.path().join("campaigns.json")).unwrap();
    let initiative = store.find_initiative("outcome").unwrap();
    assert!(initiative.done_evidence.iter().any(|evidence| matches!(evidence, DoneEvidence::Review { statement } if statement == "The owner can use the contract")));
    assert!(initiative.done_evidence.iter().any(|evidence| matches!(evidence, DoneEvidence::Review { statement } if statement == "The contract works")));
    let gate = initiative
        .done_evidence
        .iter()
        .find(|evidence| matches!(evidence, DoneEvidence::Gate { .. }))
        .unwrap();
    assert!(
        matches!(gate, DoneEvidence::Gate { command, host, required:true, artifacts } if command == "python3 sample/integrate.py" && host.as_deref() == Some("build-host") && artifacts == &["sample/contract.json"])
    );
    assert!(!initiative.steering_enabled);
    let before = std::fs::read(home.path().join("campaigns.json")).unwrap();
    std::fs::write(&file, r#"[{"kind":"review","statement":"Bypass Tracker"}]"#).unwrap();
    let rejected = derive_cli(&address, &source, &file).await;
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("only gate declarations"));
    assert_eq!(std::fs::read(home.path().join("campaigns.json")).unwrap(), before);
    server.abort();
}

async fn derive_cli(address: &str, source: &Path, file: &Path) -> std::process::Output {
    tokio::process::Command::new(env!("CARGO_BIN_EXE_foundry"))
        .args([
            "--addr",
            address,
            "initiative",
            "derive",
            "--from",
            "et:goal:outcome",
            "--projects",
            "sample",
            "--authorized-by",
            "Test",
            "--integration-gates",
            file.to_str().unwrap(),
        ])
        .current_dir(source)
        .output()
        .await
        .unwrap()
}

#[tokio::test]
async fn cli_policy_answers_and_notes_preserve_unrelated_questions_online_and_offline() {
    for offline in [false, true] {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store.add(serde_json::from_value(json!({"name":"member","project":"sample","mission":"Ship contract", "status":"active", "authorized_by":"Test", "done_evidence":[{"kind":"review","statement":"Works"}]})).unwrap()).unwrap();
        store.add_initiative(serde_json::from_value(json!({"name":"shared","outcome":"Works","projects":["sample"],"authorized_by":"Test","members":[{"kind":"campaign","name":"member"}],"done_evidence":[{"kind":"review","statement":"Works"}]})).unwrap(), &["sample".into()]).unwrap();
        let first = store.record_question("shared", "Which API?", &[]).unwrap();
        let second = store.record_question("shared", "Which timeout?", &[]).unwrap();
        store.save(&path).unwrap();
        let (address, server) = daemon(home.path(), home.path()).await;
        let run = |args: Vec<String>| {
            let path = path.clone();
            let address = address.clone();
            async move {
                let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_foundry"));
                command.env("FOUNDRY_CAMPAIGNS_PATH", path).arg("--addr").arg(address);
                if offline {
                    command.arg("--offline");
                }
                command.args(args).output().await.unwrap()
            }
        };
        let output = run([
            "initiative",
            "decide",
            "shared",
            "--decision",
            "Keep compatibility",
        ]
        .map(str::to_owned)
        .to_vec())
        .await;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(String::from_utf8_lossy(&output.stdout).contains("No questions were answered"));
        let before_note = CampaignStore::load(&path).unwrap();
        assert_eq!(before_note.initiative_view("shared").unwrap().open_questions.len(), 2);
        let output = run([
            "initiative",
            "note",
            "shared",
            "--text",
            "Build passed",
            "--json",
        ]
        .map(str::to_owned)
        .to_vec())
        .await;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let note: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(note["status_notes"][0]["text"], "Build passed");
        assert_eq!(note["owner_decisions"].as_array().unwrap().len(), 1);
        assert_eq!(note["open_questions"].as_array().unwrap().len(), 2);
        let output = run(vec![
            "initiative".into(),
            "decide".into(),
            "shared".into(),
            "--question".into(),
            first.id.clone(),
            "--decision".into(),
            "Use API v2".into(),
        ])
        .await;
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let restored = CampaignStore::load(&path).unwrap();
        let current = restored.initiative_view("shared").unwrap();
        assert_eq!(current.open_questions, [second]);
        assert_eq!(current.owner_decisions.last().unwrap().question_ids, [first.id]);
        server.abort();
    }
}

#[tokio::test]
async fn cli_refuses_older_decision_rpc_without_falling_back_or_mutating() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("campaigns.json");
    let mut store = CampaignStore::default();
    store.add(serde_json::from_value(json!({"name":"member","project":"sample","mission":"Ship contract", "status":"active", "authorized_by":"Test", "done_evidence":[{"kind":"review","statement":"Works"}]})).unwrap()).unwrap();
    store.add_initiative(serde_json::from_value(json!({"name":"shared","outcome":"Works","projects":["sample"],"authorized_by":"Test","members":[{"kind":"campaign","name":"member"}],"done_evidence":[{"kind":"review","statement":"Works"}]})).unwrap(), &["sample".into()]).unwrap();
    let question = store.record_question("shared", "Which API?", &[]).unwrap();
    store.save(&path).unwrap();
    let before = std::fs::read(&path).unwrap();
    let (address, server) = daemon_with_legacy_decisions(home.path(), home.path(), true).await;
    for question_ids in [vec![], vec!["--question", question.id.as_str()]] {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_foundry"))
            .args([
                "--addr",
                &address,
                "initiative",
                "decide",
                "shared",
                "--decision",
                "Use v2",
            ])
            .args(question_ids)
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("does not support question-specific")
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    server.abort();
}

fn recovery_fixture(home: &Path) {
    use foundry_sdk::recovery::RecoveryKind;
    let mut store = CampaignStore::default();
    store.add(serde_json::from_value(json!({"name":"member","project":"sample","mission":"Ship contract", "status":"paused", "authorized_by":"Test", "done_evidence":[{"kind":"review","statement":"Works"}]})).unwrap()).unwrap();
    store.add_initiative(serde_json::from_value(json!({"name":"shared","outcome":"Works","projects":["sample"],"authorized_by":"Test","members":[{"kind":"campaign","name":"member"}],"done_evidence":[{"kind":"review","statement":"Works"}]})).unwrap(), &["sample".into()]).unwrap();
    store
        .record_campaign_recovery(
            "member",
            "recovery_runner",
            RecoveryKind::RunnerUnavailable,
            "Runner transport unavailable",
            chrono::Utc::now(),
        )
        .unwrap();
    store
        .record_initiative_recovery(
            "shared",
            "recovery_evidence",
            RecoveryKind::MissingEvidence,
            "Current integration proof is missing",
            chrono::Utc::now(),
        )
        .unwrap();
    store.save(&home.join("campaigns.json")).unwrap();
}

#[tokio::test]
async fn recovery_cli_reads_typed_state_and_preserves_pause_questions_and_policy() {
    let home = tempfile::tempdir().unwrap();
    recovery_fixture(home.path());
    let path = home.path().join("campaigns.json");
    let before = CampaignStore::load(&path).unwrap();
    let (address, server) = daemon(home.path(), home.path()).await;
    let show = tokio::process::Command::new(env!("CARGO_BIN_EXE_foundry"))
        .args(["--addr", &address, "campaign", "show", "member", "--json"])
        .output()
        .await
        .unwrap();
    assert!(show.status.success(), "{}", String::from_utf8_lossy(&show.stderr));
    let shown: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(shown["recovery"]["kind"], "runner_unavailable");
    let recovered = tokio::process::Command::new(env!("CARGO_BIN_EXE_foundry"))
        .args([
            "--addr",
            &address,
            "campaign",
            "recover",
            "member",
            "--recovery",
            "recovery_runner",
            "--evidence",
            "Provider reachable again",
            "--json",
        ])
        .output()
        .await
        .unwrap();
    assert!(recovered.status.success(), "{}", String::from_utf8_lossy(&recovered.stderr));
    let response: serde_json::Value = serde_json::from_slice(&recovered.stdout).unwrap();
    assert_eq!(response["event_id"], "");
    assert!(response["waiting_reason"].as_str().unwrap().contains("paused"));
    let mut after = CampaignStore::load(&path).unwrap();
    after.find_mut("member").unwrap().recovery.as_mut().unwrap().resolution = None;
    assert_eq!(serde_json::to_value(before).unwrap(), serde_json::to_value(after).unwrap());
    server.abort();
}

#[tokio::test]
async fn recovery_cli_refuses_offline_and_older_daemons_without_decision_or_resume_fallback() {
    let home = tempfile::tempdir().unwrap();
    recovery_fixture(home.path());
    let path = home.path().join("campaigns.json");
    let before = std::fs::read(&path).unwrap();
    let (address, server) = daemon_with_legacy_decisions(home.path(), home.path(), true).await;
    for (scope, name, id) in [
        ("campaign", "member", "recovery_runner"),
        ("initiative", "shared", "recovery_evidence"),
    ] {
        for offline in [false, true] {
            let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_foundry"));
            command.env("FOUNDRY_CAMPAIGNS_PATH", &path).args(["--addr", &address]);
            if offline {
                command.arg("--offline");
            }
            let output = command
                .args([
                    scope,
                    "recover",
                    name,
                    "--recovery",
                    id,
                    "--evidence",
                    "Prerequisite corrected",
                ])
                .output()
                .await
                .unwrap();
            assert!(!output.status.success());
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(
                if offline {
                    error.contains("remove --offline")
                } else {
                    error.contains("does not support typed recovery")
                },
                "{error}"
            );
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }
    server.abort();
}

#[tokio::test]
async fn initiative_show_reports_dependency_readiness_online_and_store_only_offline() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("campaigns.json");
    let mut store = CampaignStore::default();
    for name in ["producer", "consumer"] {
        store.add(serde_json::from_value(json!({"name":name,"project":"sample","mission":"Deliver the whole contract","status":"active","authorized_by":"Test","done_evidence":[{"kind":"review","statement":"Works"}]})).unwrap()).unwrap();
    }
    store.find_mut("producer").unwrap().cycles_landed = 1;
    store.add_initiative(serde_json::from_value(json!({"name":"shared","outcome":"Works","projects":["sample"],"authorized_by":"Test","members":[{"kind":"campaign","name":"producer"},{"kind":"campaign","name":"consumer","after":["producer"]}],"done_evidence":[{"kind":"review","statement":"Works"}]})).unwrap(), &["sample".into()]).unwrap();
    store.save(&path).unwrap();
    let before = std::fs::read(&path).unwrap();
    let (address, server) = daemon(home.path(), home.path()).await;
    for offline in [false, true] {
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_foundry"));
        command
            .args(["--addr", &address, "initiative", "show", "shared", "--json"])
            .env("FOUNDRY_CAMPAIGNS_PATH", &path);
        if offline {
            command.arg("--offline");
        }
        let result = command.output().await.unwrap();
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        let shown: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(shown["readiness"]["live"], !offline);
        let member = shown["readiness"]["members"]
            .as_array()
            .unwrap()
            .iter()
            .find(|member| member["member"] == "consumer")
            .unwrap();
        assert_eq!(member["ready"], false);
        assert_eq!(member["dependencies"][0]["milestone"], "completed");
        assert_eq!(member["dependencies"][0]["satisfied"], false);
        assert_eq!(
            shown["open_questions"].as_array().unwrap().as_slice(),
            [] as [serde_json::Value; 0]
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }
    let human = tokio::process::Command::new(env!("CARGO_BIN_EXE_foundry"))
        .args(["--addr", &address, "initiative", "show", "shared"])
        .output()
        .await
        .unwrap();
    assert!(human.status.success());
    let out = String::from_utf8(human.stdout).unwrap();
    assert!(out.contains("consumer: waiting"));
    assert!(out.contains("producer: waiting for evidence"));
    assert!(out.contains("live store and work ledger"));
    assert_eq!(std::fs::read(&path).unwrap(), before);
    server.abort();
}
