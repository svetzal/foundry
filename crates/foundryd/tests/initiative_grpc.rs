//! Real typed client coverage for initiatives sharing the campaign store.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{Campaign, CampaignStore};
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundryd::proto::{self, foundry_client::FoundryClient, foundry_server::FoundryServer};
use foundryd::service::{FoundryService, RuntimeContext, StoreConfig};
use foundryd::trace_store::TraceStore;
use foundryd::workflow_tracker::WorkflowTracker;
use serde_json::json;
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;

async fn client(
    home: &std::path::Path,
) -> (FoundryClient<tonic::transport::Channel>, tokio::task::JoinHandle<()>) {
    let (event_tx, _receiver) = broadcast::channel(64);
    let writer = Arc::new(TraceWriter::new(home.join("traces").to_str().unwrap()));
    let registry: Registry = serde_json::from_value(json!({"version":2,"projects":[{
        "name":"sample","path":home,"stack":"rust","agent":"codex","repo":"local/sample","branch":"main"}]})).unwrap();
    let registry = Arc::new(RwLock::new(registry));
    let event_writer =
        Arc::new(foundry_engine::event_writer::EventWriter::new(home.join("events")));
    let admission = foundryd::work_admission::DurableWorkAdmission::new(
        home.join("work-items.json"),
        registry.clone(),
        foundry_sdk::pacing::PacingPaths {
            limits: home.join("pacing.json"),
            state: home.join("pacing-state.json"),
        },
        event_tx.clone(),
        event_writer.clone(),
        writer.clone(),
    )
    .with_campaigns_path(home.join("campaigns.json"));
    let ctx = RuntimeContext {
        engine: Arc::new(
            Engine::new()
                .with_event_writer(event_writer)
                .with_event_broadcaster(event_tx.clone())
                .with_work_admission(Arc::new(admission)),
        ),
        trace_store: Arc::new(TraceStore::with_trace_writer(
            Duration::from_secs(60),
            writer.clone(),
        )),
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer: writer,
        event_tx,
        registry,
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
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(FoundryServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let client = FoundryClient::connect(format!("http://{addr}")).await.unwrap();
    (client, server)
}

fn seed(home: &std::path::Path) {
    let mut store = CampaignStore::default();
    for name in ["first", "second"] {
        let campaign: Campaign = serde_json::from_value(json!({"name":name,"project":"sample","mission":"A member mission",
            "status":"active","authorized_by":"Stacey","done_evidence":[{"kind":"review","statement":"The mission is met"}]})).unwrap();
        store.add(campaign).unwrap();
    }
    store.save(&home.join("campaigns.json")).unwrap();
}

fn definition() -> String {
    json!({"name":"shared","outcome":"The two members use the same contract","projects":["sample"],
        "authorized_by":"Stacey","members":[{"kind":"campaign","name":"first"},{"kind":"campaign","name":"second"}],
        "done_evidence":[{"kind":"review","statement":"The contract works"}],
        "providers":{"formation":"claude","execution":"codex","review":"claude"}}).to_string()
}

fn seed_outcome_repository(home: &std::path::Path) {
    for args in [
        vec!["init", "--quiet", "--initial-branch=main"],
        vec!["config", "user.name", "Test"],
        vec!["config", "user.email", "test@example.invalid"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(home)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    std::fs::write(home.join(".gitignore"), "*\n!.gitignore\n!README.md\n").unwrap();
    std::fs::write(home.join("README.md"), "Synthetic trunk").unwrap();
    for args in [
        vec!["add", ".gitignore", "README.md"],
        vec!["commit", "--quiet", "-m", "Seed trunk"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(home)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    let origin = home.join("origin.git");
    for args in [
        vec![
            "init",
            "--quiet",
            "--bare",
            "--initial-branch=main",
            origin.to_str().unwrap(),
        ],
        vec!["remote", "add", "origin", origin.to_str().unwrap()],
        vec!["push", "--quiet", "origin", "main"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(home)
                .output()
                .unwrap()
                .status
                .success()
        );
    }
}

fn propose_test_outcome(home: &std::path::Path) {
    use foundry_sdk::initiative::{DoneEvidence, IntegrationGateResult, OutcomeReview};
    let path = home.join("campaigns.json");
    let registry: Registry = serde_json::from_value(json!({"version":2,"projects":[{"name":"sample","path":home,"stack":"rust","agent":"codex","repo":"local/sample","branch":"main"}]})).unwrap();
    let mut guard = CampaignStore::lock_exclusive(&path).unwrap();
    guard
        .store
        .find_initiative_mut("shared")
        .unwrap()
        .done_evidence
        .push(DoneEvidence::Gate {
            command: "true".into(),
            host: None,
            required: true,
            artifacts: vec![],
        });
    let context = foundry_blocks::blocks::capture_initiative_external_context(
        guard.store.find_initiative("shared").unwrap(),
        &registry,
    )
    .unwrap();
    let snapshot = guard.store.outcome_snapshot("shared", context.repositories).unwrap();
    guard
        .store
        .record_integration_result(
            "shared",
            IntegrationGateResult {
                id: "wire_gate".into(),
                gate_index: 1,
                host: "synthetic".into(),
                command: "true".into(),
                snapshot: snapshot.clone(),
                passed: true,
                exit_code: Some(0),
                report: "gate-capture.md".into(),
                artifacts: vec![],
                recorded_at: chrono::Utc::now(),
            },
        )
        .unwrap();
    guard
        .store
        .propose_completion(
            "shared",
            "wire_done",
            snapshot,
            vec![OutcomeReview {
                statement: "The contract works".into(),
                affirmed: true,
                evidence: vec!["gate-capture.md".into()],
            }],
        )
        .unwrap();
    guard.save().unwrap();
    drop(guard);
}

#[tokio::test]
async fn typed_outcome_evidence_and_owner_confirmation_survive_rpc_round_trip() {
    let home = tempfile::tempdir().unwrap();
    seed_outcome_repository(home.path());
    seed(home.path());
    let (mut client, server) = client(home.path()).await;
    client
        .add_initiative(proto::AddInitiativeRequest {
            definition_json: definition(),
        })
        .await
        .unwrap();
    propose_test_outcome(home.path());
    let shown = client
        .get_initiative(proto::GetInitiativeRequest {
            name: "shared".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .initiative
        .unwrap();
    assert_eq!(shown.integration_results[0].report, "gate-capture.md");
    assert_eq!(shown.integration_results[0].exit_code, Some(0));
    assert_eq!(
        shown.integration_results[0].snapshot.as_ref().unwrap().repositories[0].project,
        "sample"
    );
    assert!(shown.completion_proposal.as_ref().unwrap().reviews[0].affirmed);
    assert!(shown.completion_confirmation.is_none());
    let completed = client
        .complete_initiative(proto::EndInitiativeRequest {
            name: "shared".into(),
            reason: "Owner verified the trunk evidence".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .initiative
        .unwrap();
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.completion_confirmation.as_ref().unwrap().proposal_id, "wire_done");
    assert!(completed.completion_receipts.is_empty());
    let retried = client
        .complete_initiative(proto::EndInitiativeRequest {
            name: "shared".into(),
            reason: "Owner retried".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .initiative
        .unwrap();
    assert_eq!(retried, completed);
    server.abort();
}

#[tokio::test]
async fn typed_commands_fan_out_decisions_and_serve_owned_brief_and_findings() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path());
    let (mut client, server) = client(home.path()).await;
    let added = client
        .add_initiative(proto::AddInitiativeRequest {
            definition_json: definition(),
        })
        .await
        .unwrap()
        .into_inner()
        .initiative
        .unwrap();
    assert_eq!(added.members.len(), 2);
    assert_eq!(added.budget.unwrap().max_active_members, 2);
    let mut second_client = client.clone();
    let (first, second) = tokio::join!(
        client.decide_initiative(proto::DecideInitiativeRequest {
            name: "shared".into(),
            decision: "Use contract one".into(),
            members: vec![],
            add_cycles: 0
        }),
        second_client.decide_initiative(proto::DecideInitiativeRequest {
            name: "shared".into(),
            decision: "Keep both compatible".into(),
            members: vec![],
            add_cycles: 0
        })
    );
    first.unwrap();
    second.unwrap();
    let stored = CampaignStore::load(&home.path().join("campaigns.json")).unwrap();
    for member in &stored.campaigns {
        assert_eq!(member.owner_decisions.len(), 2);
    }
    let brief = client
        .set_initiative_brief(proto::SetInitiativeBriefRequest {
            name: "shared".into(),
            content: "Use the tested contract.\n".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(brief.content, "Use the tested contract.\n");
    assert_eq!(
        std::path::Path::new(&brief.brief.unwrap().path),
        home.path().join("initiatives/shared/BRIEF.md")
    );
    let fetched = client
        .get_initiative_brief(proto::GetInitiativeRequest {
            name: "shared".into(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(fetched.content, brief.content);
    let finding = client.add_finding(proto::AddFindingRequest { definition_json:json!({"id":"fnd_manual","initiative":"shared",
        "from":{"kind":"owner","id":"Stacey"},"to":["first"],"kind":"regression","text":"The old contract loses an argument","refs":["sample@123"]}).to_string() }).await.unwrap().into_inner().finding.unwrap();
    assert_eq!(finding.id, "fnd_manual");
    let findings = client
        .list_findings(proto::ListFindingsRequest {
            initiative: "shared".into(),
            member: "first".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .findings;
    assert_eq!(findings, vec![finding]);
    let shown = client
        .get_initiative(proto::GetInitiativeRequest {
            name: "shared".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .initiative
        .unwrap();
    assert_eq!(shown.owner_decisions.len(), 2);
    assert_eq!(
        shown.members[0].campaign.as_ref().unwrap().initiative.as_deref(),
        Some("shared")
    );
    let incomplete = client
        .complete_initiative(proto::EndInitiativeRequest {
            name: "shared".into(),
            reason: "Verified both members".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(incomplete.code(), tonic::Code::FailedPrecondition);
    assert!(incomplete.message().contains("no done proposal"));
    server.abort();
}

#[tokio::test]
async fn failed_mutation_preserves_the_store_and_returns_internal_through_the_client() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path());
    let (mut client, server) = client(home.path()).await;
    client
        .add_initiative(proto::AddInitiativeRequest {
            definition_json: definition(),
        })
        .await
        .unwrap();
    let path = home.path().join("campaigns.json");
    let before = std::fs::read(&path).unwrap();
    std::fs::create_dir(path.with_extension("json.tmp")).unwrap();
    let error = client
        .decide_initiative(proto::DecideInitiativeRequest {
            name: "shared".into(),
            decision: "No lost updates".into(),
            members: vec![],
            add_cycles: 0,
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Internal);
    assert_eq!(std::fs::read(path).unwrap(), before);
    server.abort();
}

#[tokio::test]
async fn concurrent_typed_steers_admit_once_and_explicit_owner_override_is_not_lost() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path());
    let (mut first, server) = client(home.path()).await;
    first
        .add_initiative(proto::AddInitiativeRequest {
            definition_json: definition(),
        })
        .await
        .unwrap();
    let mut second = first.clone();
    let request = || proto::AdvanceInitiativeRequest {
        name: "shared".into(),
        member: None,
        operator_origin: String::new(),
    };
    let (one, two) =
        tokio::join!(first.advance_initiative(request()), second.advance_initiative(request()));
    let responses = [one.unwrap().into_inner(), two.unwrap().into_inner()];
    assert_eq!(responses.iter().filter(|response| !response.event_id.is_empty()).count(), 1);
    assert_eq!(responses.iter().filter(|response| response.in_flight.is_some()).count(), 1);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let ledger =
                foundry_sdk::work_item::WorkItemStore::load(&home.path().join("work-items.json"))
                    .unwrap();
            if ledger.items.len() == 1 {
                assert_eq!(ledger.items[0].source.as_ref().unwrap().reference, "shared");
                assert_eq!(ledger.items[0].lane, foundry_sdk::work_item::WorkLane::Campaign);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let override_result = first
        .advance_initiative(proto::AdvanceInitiativeRequest {
            name: "shared".into(),
            member: Some("first".into()),
            operator_origin: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(override_result.code(), tonic::Code::FailedPrecondition);
    let stored = CampaignStore::load(&home.path().join("campaigns.json")).unwrap();
    assert!(stored.find_initiative("shared").unwrap().steering_enabled);
    server.abort();
}

#[tokio::test]
async fn owner_hold_stays_passive_and_member_pin_releases_it() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path());
    let (mut client, server) = client(home.path()).await;
    client
        .add_initiative(proto::AddInitiativeRequest {
            definition_json: definition(),
        })
        .await
        .unwrap();
    let response = client
        .act_on_initiative(proto::ActOnInitiativeRequest {
            name: "shared".into(),
            operator_origin: "host test".into(),
            action: Some(proto::act_on_initiative_request::Action::Hold(
                proto::HoldInitiativeMember {
                    member: "first".into(),
                    until: vec![],
                    reason: "Owner review pending".into(),
                },
            )),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!response.event_id.is_empty());
    let event_id = response.event_id;
    let response = response.initiative.unwrap();
    assert!(!response.steering_enabled);
    let fact = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let events = foundry_blocks::blocks::triage_core::read_events_jsonl(
                &home.path().join("events"),
                chrono::Utc::now() - chrono::Duration::minutes(1),
            );
            if let Some(fact) = events.into_iter().find(|event| event.id == event_id) {
                break fact;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fact.event_type, foundry_sdk::event::EventType::InitiativeOwnerActionRecorded);
    let payload: foundry_sdk::payload::InitiativeOwnerActionRecordedPayload =
        fact.parse_payload().unwrap();
    assert_eq!(payload.operator_origin, "host test");
    assert!(
        matches!(payload.action,foundry_sdk::payload::InitiativeOwnerControlAction::Hold {member,..} if member=="first")
    );
    assert!(
        foundry_sdk::work_item::WorkItemStore::load(&home.path().join("work-items.json"))
            .unwrap()
            .items
            .is_empty()
    );
    assert!(response.holds[0].owner);
    let response = client
        .advance_initiative(proto::AdvanceInitiativeRequest {
            name: "shared".into(),
            member: Some("first".into()),
            operator_origin: String::new(),
        })
        .await
        .unwrap()
        .into_inner()
        .initiative
        .unwrap();
    assert!(response.holds.is_empty());
    assert!(response.steering_enabled);
    server.abort();
}

#[tokio::test]
async fn shared_budget_changes_only_through_initiative_and_never_after_terminal() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path());
    let (mut client, server) = client(home.path()).await;
    client
        .add_initiative(proto::AddInitiativeRequest {
            definition_json: definition(),
        })
        .await
        .unwrap();
    let result = client
        .decide_initiative(proto::DecideInitiativeRequest {
            name: "shared".into(),
            decision: "Spend three more cycles".into(),
            members: vec![],
            add_cycles: 3,
        })
        .await
        .unwrap()
        .into_inner()
        .initiative
        .unwrap();
    assert_eq!(result.budget.unwrap().max_cycles, 23);
    assert!(!result.steering_enabled);
    client
        .cancel_initiative(proto::EndInitiativeRequest {
            name: "shared".into(),
            reason: "Stop the proof".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        client
            .advance_initiative(proto::AdvanceInitiativeRequest {
                name: "shared".into(),
                member: None,
                operator_origin: String::new()
            })
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    server.abort();
}

fn seed_tracker_source(source: &std::path::Path) {
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "test@example.invalid"],
        vec!["config", "user.name", "Test"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(source)
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::create_dir(source.join(".et")).unwrap();
    std::fs::write(source.join(".et/projection.json"), json!({
        "goals":[{"id":"outcome","description":"An approved outcome","success_criteria":"A verified contract"}],
        "interactions":[{"id":"contract","description":"Implement the contract","state":"create","goals":["outcome"]}], "journeys":[]
    }).to_string()).unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["add", ".et/projection.json"])
            .current_dir(source)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args(["commit", "--quiet", "-m", "Seed intent"])
            .current_dir(source)
            .status()
            .unwrap()
            .success()
    );
}

#[tokio::test]
async fn derivation_reads_intent_on_the_daemon_and_keeps_targets_registered() {
    let home = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    seed_tracker_source(source.path());
    let (mut client, server) = client(home.path()).await;
    let request = |projects| proto::DeriveInitiativeRequest {
        from_ref: "et:goal:outcome".into(),
        projects,
        authorized_by: "Stacey".into(),
        source_repo: source.path().to_string_lossy().into_owned(),
        integration_gates: vec![],
    };
    assert_eq!(
        client
            .derive_initiative(request(vec!["unregistered".into()]))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
    let mut definition = request(vec!["sample".into()]);
    definition.integration_gates = vec![proto::InitiativeEvidence {
        kind: "review".into(),
        statement: "Cannot replace Tracker reviews".into(),
        ..Default::default()
    }];
    assert_eq!(
        client.derive_initiative(definition).await.unwrap_err().code(),
        tonic::Code::InvalidArgument
    );
    let mut definition = request(vec!["sample".into()]);
    definition.integration_gates = vec![proto::InitiativeEvidence {
        kind: "gate".into(),
        command: "python3 scripts/integrate.py".into(),
        host: Some("build-host".into()),
        required: true,
        artifacts: vec!["sample/contract.json".into()],
        ..Default::default()
    }];
    let derived = client
        .derive_initiative(definition)
        .await
        .unwrap()
        .into_inner()
        .initiative
        .unwrap();
    assert_eq!(derived.members.len(), 1);
    assert_eq!(derived.members[0].kind, "candidate");
    assert!(
        derived.done_evidence.iter().any(
            |evidence| evidence.kind == "review" && evidence.statement == "A verified contract"
        )
    );
    let gate = derived.done_evidence.iter().find(|evidence| evidence.kind == "gate").unwrap();
    assert_eq!(gate.command, "python3 scripts/integrate.py");
    assert_eq!(gate.host.as_deref(), Some("build-host"));
    assert!(gate.required);
    assert_eq!(gate.artifacts, ["sample/contract.json"]);
    assert!(!derived.steering_enabled);
    assert_eq!(
        derived.derive_source.unwrap().repo,
        source.path().canonicalize().unwrap().to_string_lossy()
    );
    assert!(
        foundry_sdk::work_item::WorkItemStore::load(&home.path().join("work-items.json"))
            .unwrap()
            .items
            .is_empty()
    );
    server.abort();
}

#[tokio::test]
async fn manual_campaign_completion_records_authoritative_milestone() {
    let home = tempfile::tempdir().unwrap();
    seed(home.path());
    let (mut client, server) = client(home.path()).await;
    client
        .add_initiative(proto::AddInitiativeRequest {
            definition_json: definition(),
        })
        .await
        .unwrap();
    client
        .complete_campaign(proto::CompleteCampaignRequest {
            name: "first".into(),
            reason: "Owner verified the contract".into(),
        })
        .await
        .unwrap();
    let store = CampaignStore::load(&home.path().join("campaigns.json")).unwrap();
    let initiative = store.find_initiative("shared").unwrap();
    let milestone = initiative
        .milestones
        .iter()
        .find(|milestone| milestone.member == "first" && milestone.name == "completed")
        .unwrap();
    assert!(milestone.evidence.iter().any(|proof| proof == "Owner verified the contract"));
    assert_eq!(
        store.find("first").unwrap().status,
        foundry_sdk::campaign::CampaignStatus::Completed
    );
    assert!(!initiative.steering_enabled);
    server.abort();
}
