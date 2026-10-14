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
    let registry = Arc::new(RwLock::new(sample_registry(home)));
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

/// The daemon's registry: one project, `sample`, checked out at the Foundry home.
fn sample_registry(home: &std::path::Path) -> Registry {
    serde_json::from_value(json!({"version":2,"projects":[{
        "name":"sample","path":home,"stack":"rust","agent":"codex","repo":"local/sample","branch":"main"}]}))
    .unwrap()
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
    let registry = sample_registry(home);
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
    assert!(
        completed.completion_receipts.is_empty(),
        "expected `completed.completion_receipts` to be empty: {:?}",
        completed.completion_receipts
    );
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
    assert!(!response.event_id.is_empty(), "expected `response.event_id` to be non-empty");
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
            .is_empty(),
        "holding a member queues no work"
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
    assert!(
        response.holds.is_empty(),
        "expected `response.holds` to be empty: {:?}",
        response.holds
    );
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

/// Commit `projection` as the source repository's `.et/projection.json`.
fn seed_tracker_source(source: &std::path::Path, projection: &serde_json::Value) {
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
    std::fs::write(source.join(".et/projection.json"), projection.to_string()).unwrap();
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
    seed_tracker_source(
        source.path(),
        &json!({
            "goals":[{"id":"outcome","description":"An approved outcome","success_criteria":"A verified contract"}],
            "interactions":[{"id":"contract","description":"Implement the contract","state":"create","goals":["outcome"]}], "journeys":[]
        }),
    );
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
            .is_empty(),
        "no work item is queued"
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

/// Consumer and provider execute this same versioned set, rather than inventing mocks independently.
#[tokio::test]
async fn initiative_attention_v1_provider_verifies_shared_contract() {
    let bytes = include_bytes!("../../../contracts/initiative-attention-v1.json");
    let contract: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(contract["contract"], "foundry.initiative-attention");
    assert_eq!(contract["version"], 1);
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("campaigns.json");
    seed_attention_contract(&contract["seed"], &path);
    let (mut client, server) = client(home.path()).await;
    for scenario in contract["scenarios"].as_array().unwrap() {
        let before = std::fs::read(&path).unwrap();
        let request = &scenario["request"];
        let result = match scenario["method"].as_str().unwrap() {
            "ListInitiatives" => client.list_initiatives(proto::ListInitiativesRequest {}).await
                .map(|response| json!({"initiatives":response.into_inner().initiatives.iter().map(attention_contract_projection).collect::<Vec<_>>()})),
            "GetInitiative" => client.get_initiative(proto::GetInitiativeRequest { name:request["name"].as_str().unwrap().into() }).await
                .map(|response| json!({"initiative":attention_contract_projection(&response.into_inner().initiative.unwrap())})),
            "DecideInitiative" => client.decide_initiative(proto::DecideInitiativeRequest {
                name:request["name"].as_str().unwrap().into(), decision:request["decision"].as_str().unwrap().into(),
                members:request["members"].as_array().unwrap().iter().map(|member| member.as_str().unwrap().into()).collect(), add_cycles:0,
            }).await.map(|response| json!({"initiative":attention_contract_projection(&response.into_inner().initiative.unwrap())})),
            method => panic!("unverified contract method {method}"),
        };
        if let Some(error) = scenario.get("error") {
            let actual = result.unwrap_err();
            assert_eq!(
                actual.code() as i32,
                i32::try_from(error["status"].as_i64().unwrap()).unwrap(),
                "{}",
                scenario["id"]
            );
            assert_eq!(actual.message(), error["message"].as_str().unwrap(), "{}", scenario["id"]);
        } else {
            assert_eq!(result.unwrap(), scenario["response"], "{}", scenario["id"]);
        }
        if scenario["unchanged"] == true {
            assert_eq!(std::fs::read(&path).unwrap(), before, "{} mutated state", scenario["id"]);
        }
        // The contract's question identity and oldest age also survive a durable reload.
        let restored = CampaignStore::load(&path).unwrap();
        if let Some(question) = restored.initiative_view("shared").unwrap().open_questions.first() {
            assert_eq!(question.opened_at.to_rfc3339(), "2026-10-01T12:00:00+00:00");
        }
    }
    for case in contract["additional_cases"].as_array().unwrap() {
        seed_attention_contract(&case["seed"], &path);
        let response = client
            .get_initiative(proto::GetInitiativeRequest {
                name: case["scenario"]["request"]["name"].as_str().unwrap().into(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            json!({"initiative":attention_contract_projection(&response.initiative.unwrap())}),
            case["scenario"]["response"],
            "{}",
            case["id"]
        );
    }
    server.abort();
}

fn seed_attention_contract(seed: &serde_json::Value, path: &std::path::Path) {
    let mut store = CampaignStore::default();
    for campaign in seed["campaigns"].as_array().unwrap() {
        store.add(serde_json::from_value(campaign.clone()).unwrap()).unwrap();
    }
    store
        .add_initiative(
            serde_json::from_value(seed["initiative"].clone()).unwrap(),
            &["sample".into()],
        )
        .unwrap();
    store.refresh_questions("shared").unwrap();
    let evidence = &seed["integration_result"];
    let snapshot = store
        .outcome_snapshot(
            "shared",
            serde_json::from_value(evidence["repositories"].clone()).unwrap(),
        )
        .unwrap();
    let mut result = evidence.clone();
    result.as_object_mut().unwrap().remove("repositories");
    result["snapshot"] = serde_json::to_value(snapshot).unwrap();
    store
        .record_integration_result("shared", serde_json::from_value(result).unwrap())
        .unwrap();
    store.save(path).unwrap();
}

/// This is the explicitly declared consumer projection in the versioned contract.
fn attention_contract_projection(initiative: &proto::Initiative) -> serde_json::Value {
    json!({
        "name":initiative.name,"outcome":initiative.outcome,"projects":initiative.projects,
        "authorizedBy":initiative.authorized_by,"status":initiative.status,
        "budget":budget_projection(initiative),
        "members":initiative.members.iter().map(|member| {
            let campaign = member.campaign.as_ref().unwrap();
            let mut value = json!({"kind":member.kind,"name":member.name,"project":member.project,
                "campaign":{"name":campaign.name,"project":campaign.project,"status":campaign.status}});
            if let Some(verdict) = &member.last_verdict {
                value["lastVerdict"] = json!(verdict);
                value["reviewerGaps"] = json!(member.reviewer_gaps);
            }
            value
        }).collect::<Vec<_>>(),
        "openQuestions":initiative.open_questions.iter().map(|question| json!({"id":question.id,"question":question.question,"members":question.members,"openedAt":question.opened_at})).collect::<Vec<_>>(),
        "deferredQuestionCount":initiative.deferred_question_count.to_string(),
        "doneEvidence":initiative.done_evidence.iter().map(evidence_projection).collect::<Vec<_>>(),
        "integrationResults":initiative.integration_results.iter().map(|result| json!({
            "id":result.id,"gateIndex":result.gate_index.to_string(),"host":result.host,"command":result.command,
            "passed":result.passed,"exitCode":result.exit_code,"report":result.report,"artifacts":result.artifacts,"recordedAt":result.recorded_at,
            "snapshot":{"repositories":result.snapshot.as_ref().unwrap().repositories.iter().map(|repo| json!({"project":repo.project,"root":repo.root,"registry":repo.registry,"trunk":repo.trunk,"head":repo.head,"status":repo.status,"diff":repo.diff})).collect::<Vec<_>>()}
        })).collect::<Vec<_>>(),
        "ownerDecisions":initiative.owner_decisions.iter().map(decision_projection).collect::<Vec<_>>()
    })
}

/// Protobuf JSON renders every uint64 as a decimal string.
fn budget_projection(initiative: &proto::Initiative) -> serde_json::Value {
    let budget = initiative.budget.as_ref().unwrap();
    json!({"maxCycles":budget.max_cycles.to_string(),"maxActiveMembers":budget.max_active_members.to_string(),
        "maxOpenQuestions":budget.max_open_questions.to_string()})
}

/// A gate states its command; a review states its outcome. `host` is a proto3 `optional`.
fn evidence_projection(evidence: &proto::InitiativeEvidence) -> serde_json::Value {
    if evidence.kind == "gate" {
        let mut gate = json!({"kind":evidence.kind,"command":evidence.command,"required":evidence.required,"artifacts":evidence.artifacts});
        if let Some(host) = &evidence.host {
            gate["host"] = json!(host);
        }
        gate
    } else {
        json!({"kind":evidence.kind,"statement":evidence.statement})
    }
}

/// The decision time is daemon-stamped, so it is checked rather than declared.
fn decision_projection(decision: &proto::InitiativeDecision) -> serde_json::Value {
    assert_rfc3339(&decision.decided_at);
    json!({"decision":decision.decision,"authorizedBy":decision.authorized_by,"members":decision.members})
}

fn assert_rfc3339(timestamp: &str) {
    assert!(
        chrono::DateTime::parse_from_rfc3339(timestamp).is_ok(),
        "{timestamp:?} is not RFC 3339"
    );
}

/// Consumer and provider execute this same versioned control set. Each group
/// starts a fresh daemon on a Foundry home seeded from named fragments, then
/// runs its scenarios in order against the real typed client.
#[tokio::test]
async fn initiative_control_v2_provider_verifies_shared_contract() {
    let bytes = include_bytes!("../../../contracts/initiative-control-v2.json");
    let contract: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(contract["contract"], "foundry.initiative-control");
    assert_eq!(contract["version"], 2);
    for group in contract["groups"].as_array().unwrap() {
        let fragments = group["seed"].as_array().unwrap().iter().map(|name| {
            let name = name.as_str().unwrap();
            contract["seeds"].get(name).unwrap_or_else(|| panic!("undeclared seed {name}"))
        });
        let fixture = ControlFixture::seed(fragments);
        let (mut client, server) = client(fixture.home()).await;
        let mut minted = std::collections::HashMap::new();
        for scenario in group["scenarios"].as_array().unwrap() {
            verify_control_scenario(&mut client, &fixture, scenario, &mut minted).await;
        }
        server.abort();
    }
}

/// The Tracker source repository's path inside the contract. The provider maps
/// it to the temporary repository in requests and back in responses.
const TRACKER_PLACEHOLDER: &str = "/sandbox/tracker";

/// A Foundry home seeded from contract fragments, plus the Tracker repository
/// that derive scenarios read.
struct ControlFixture {
    home: tempfile::TempDir,
    tracker: Option<tempfile::TempDir>,
}

impl ControlFixture {
    fn seed<'a>(fragments: impl IntoIterator<Item = &'a serde_json::Value>) -> Self {
        let home = tempfile::tempdir().unwrap();
        let mut tracker = None;
        let mut store = CampaignStore::default();
        for fragment in fragments {
            for campaign in fragment["campaigns"].as_array().into_iter().flatten() {
                store.add(serde_json::from_value(campaign.clone()).unwrap()).unwrap();
            }
            if let Some(initiative) = fragment.get("initiative") {
                let initiative: foundry_sdk::initiative::Initiative =
                    serde_json::from_value(initiative.clone()).unwrap();
                let name = initiative.name.clone();
                store.add_initiative(initiative, &["sample".into()]).unwrap();
                store.refresh_questions(&name).unwrap();
            }
            for finding in fragment["findings"].as_array().into_iter().flatten() {
                store.add_finding(serde_json::from_value(finding.clone()).unwrap()).unwrap();
            }
            if let Some(brief) = fragment.get("brief") {
                seed_recorded_brief(&mut store, home.path(), brief);
            }
            if let Some(proposal) = fragment.get("completion_proposal") {
                seed_outcome_repository(home.path());
                seed_completion_proposal(&mut store, home.path(), proposal);
            }
            if let Some(projection) = fragment.get("tracker_projection") {
                let source = tempfile::tempdir().unwrap();
                seed_tracker_source(source.path(), projection);
                tracker = Some(source);
            }
        }
        store.save(&home.path().join("campaigns.json")).unwrap();
        Self { home, tracker }
    }

    fn home(&self) -> &std::path::Path {
        self.home.path()
    }

    fn tracker_path(&self) -> Option<String> {
        self.tracker
            .as_ref()
            .map(|source| source.path().canonicalize().unwrap().to_string_lossy().into_owned())
    }

    fn to_daemon(&self, request: &serde_json::Value) -> serde_json::Value {
        match self.tracker_path() {
            Some(path) => replace_text(request, TRACKER_PLACEHOLDER, &path),
            None => request.clone(),
        }
    }

    fn to_contract(&self, response: &serde_json::Value) -> serde_json::Value {
        match self.tracker_path() {
            Some(path) => replace_text(response, &path, TRACKER_PLACEHOLDER),
            None => response.clone(),
        }
    }
}

fn replace_text(value: &serde_json::Value, from: &str, to: &str) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => json!(text.replace(from, to)),
        serde_json::Value::Array(items) => {
            items.iter().map(|item| replace_text(item, from, to)).collect()
        }
        serde_json::Value::Object(fields) => fields
            .iter()
            .map(|(key, field)| (key.clone(), replace_text(field, from, to)))
            .collect(),
        other => other.clone(),
    }
}

/// Record a brief hash beside file content that need not match it, as after an
/// edit made outside Foundry.
fn seed_recorded_brief(
    store: &mut CampaignStore,
    home: &std::path::Path,
    brief: &serde_json::Value,
) {
    let name = brief["initiative"].as_str().unwrap();
    let path = home.join("initiatives").join(name).join("BRIEF.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, brief["file"].as_str().unwrap()).unwrap();
    store.find_initiative_mut(name).unwrap().brief =
        Some(foundry_sdk::initiative::InitiativeBrief {
            path: path.to_string_lossy().into_owned(),
            hash: brief["hash"].as_str().unwrap().into(),
        });
}

/// Propose done against the live snapshot of the seeded `sample` trunk.
fn seed_completion_proposal(
    store: &mut CampaignStore,
    home: &std::path::Path,
    proposal: &serde_json::Value,
) {
    let name = proposal["initiative"].as_str().unwrap();
    let context = foundry_blocks::blocks::capture_initiative_external_context(
        store.find_initiative(name).unwrap(),
        &sample_registry(home),
    )
    .unwrap();
    let snapshot = store.outcome_snapshot(name, context.repositories).unwrap();
    store
        .propose_completion(
            name,
            proposal["id"].as_str().unwrap(),
            snapshot,
            serde_json::from_value(proposal["reviews"].clone()).unwrap(),
        )
        .unwrap();
}

async fn verify_control_scenario(
    client: &mut FoundryClient<tonic::transport::Channel>,
    fixture: &ControlFixture,
    scenario: &serde_json::Value,
    minted: &mut std::collections::HashMap<String, String>,
) {
    let id = scenario["id"].as_str().unwrap();
    let method = scenario["method"].as_str().unwrap();
    let store = fixture.home().join("campaigns.json");
    let before = std::fs::read(&store).unwrap();
    let request = fixture.to_daemon(&scenario["request"]);
    let result = call_control(client, method, &request, fixture.home()).await;
    match (scenario.get("response"), scenario.get("error")) {
        (Some(expected), None) => {
            let response = result.unwrap_or_else(|status| panic!("{id} was refused: {status:?}"));
            if let Some(event_id) = response.get("eventId").and_then(serde_json::Value::as_str)
                && method == "AdvanceInitiative"
            {
                await_steer_admission(fixture.home(), event_id).await;
            }
            let mut actual = fixture.to_contract(&response);
            bind_minted_values(&mut actual, expected, &scenario["volatile"], minted, id);
            assert_eq!(actual, *expected, "{id}");
        }
        (None, Some(error)) => {
            let status = result.expect_err(id);
            let code = i32::try_from(error["status"].as_i64().unwrap()).unwrap();
            assert_eq!(status.code() as i32, code, "{id}: {status:?}");
            assert_eq!(status.message(), error["message"].as_str().unwrap(), "{id}");
        }
        _ => panic!("{id} must declare exactly one of response or error"),
    }
    if scenario["unchanged"] == true {
        assert_eq!(std::fs::read(&store).unwrap(), before, "{id} mutated the campaign store");
    }
}

/// Values the daemon mints appear in the contract as placeholders at the
/// declared JSON pointers. Each must be a non-empty string, and within one
/// group placeholders and daemon values correspond one to one.
fn bind_minted_values(
    actual: &mut serde_json::Value,
    expected: &serde_json::Value,
    pointers: &serde_json::Value,
    minted: &mut std::collections::HashMap<String, String>,
    id: &str,
) {
    for pointer in pointers.as_array().into_iter().flatten() {
        let pointer = pointer.as_str().unwrap();
        let placeholder = expected
            .pointer(pointer)
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("{id}: {pointer} declares no placeholder"));
        let value = actual
            .pointer_mut(pointer)
            .unwrap_or_else(|| panic!("{id}: the daemon returned no {pointer}"));
        let daemon_value = value
            .as_str()
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| panic!("{id}: {pointer} is empty"))
            .to_owned();
        if let Some(other) = minted
            .iter()
            .find(|(other, bound)| *other != placeholder && **bound == daemon_value)
            .map(|(other, _)| other)
        {
            panic!("{id}: {placeholder} and {other} name the same daemon value");
        }
        let bound = minted.entry(placeholder.to_owned()).or_insert_with(|| daemon_value.clone());
        assert_eq!(*bound, daemon_value, "{id}: {placeholder} names two daemon values");
        *value = json!(placeholder);
    }
}

/// A later scenario sees an accepted steer as in flight only once the work
/// ledger records it, as the daemon's admission does asynchronously.
async fn await_steer_admission(home: &std::path::Path, event_id: &str) {
    let ledger = home.join("work-items.json");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let admitted =
                foundry_sdk::work_item::WorkItemStore::load(&ledger).unwrap().items.iter().any(
                    |item| {
                        item.pending_root
                            .iter()
                            .chain(&item.started_root)
                            .any(|root| root.id == event_id)
                    },
                );
            if admitted {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the accepted steer reaches the work ledger");
}

/// One Protobuf JSON message from the contract. Field names must be the
/// lowerCamelCase JSON names; defaults may be omitted, and uint64 is a string.
struct Message<'a>(&'a serde_json::Value);

impl<'a> Message<'a> {
    fn new(value: &'a serde_json::Value, fields: &[&str]) -> Self {
        for key in value.as_object().expect("a message is a JSON object").keys() {
            assert!(fields.contains(&key.as_str()), "{key} is not a field of this message");
        }
        Self(value)
    }

    fn text(&self, field: &str) -> String {
        self.optional_text(field).unwrap_or_default()
    }

    /// A proto3 `optional` string: absent is `None`, never `Some("")`.
    fn optional_text(&self, field: &str) -> Option<String> {
        self.0.get(field).map(|value| value.as_str().unwrap().to_owned())
    }

    fn texts(&self, field: &str) -> Vec<String> {
        self.0.get(field).map_or_else(Vec::new, |values| {
            values
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap().into())
                .collect()
        })
    }

    fn uint64(&self, field: &str) -> u64 {
        self.0
            .get(field)
            .map_or(0, |value| value.as_str().expect("uint64 is a JSON string").parse().unwrap())
    }

    fn flag(&self, field: &str) -> bool {
        self.0.get(field).is_some_and(|value| value.as_bool().unwrap())
    }

    fn child(&self, field: &str, fields: &[&str]) -> Option<Message<'a>> {
        self.0.get(field).map(|value| Message::new(value, fields))
    }

    fn children(&self, field: &str, fields: &[&str]) -> Vec<Message<'a>> {
        self.0.get(field).map_or_else(Vec::new, |values| {
            values
                .as_array()
                .unwrap()
                .iter()
                .map(|value| Message::new(value, fields))
                .collect()
        })
    }
}

async fn call_control(
    client: &mut FoundryClient<tonic::transport::Channel>,
    method: &str,
    request: &serde_json::Value,
    home: &std::path::Path,
) -> Result<serde_json::Value, tonic::Status> {
    let initiative = |response: Option<proto::Initiative>| json!({"initiative":control_projection(&response.unwrap(), home)});
    Ok(match method {
        "GetInitiative" => {
            let name = Message::new(request, &["name"]).text("name");
            let response = client.get_initiative(proto::GetInitiativeRequest { name }).await?;
            initiative(response.into_inner().initiative)
        }
        "AdvanceInitiative" => {
            let response = client.advance_initiative(advance_request(request)).await?.into_inner();
            let mut projection = initiative(response.initiative);
            if !response.event_id.is_empty() {
                projection["eventId"] = json!(response.event_id);
            }
            if let Some(steer) = response.in_flight {
                projection["inFlight"] = json!({"kind":steer.kind,"workItemId":steer.work_item_id,"state":steer.state,"eventId":steer.event_id});
            }
            projection
        }
        "ActOnInitiative" => {
            let response = client.act_on_initiative(act_request(request)).await?.into_inner();
            let mut projection = initiative(response.initiative);
            projection["eventId"] = json!(response.event_id);
            projection
        }
        "DecideInitiative" => {
            let response = client.decide_initiative(decide_request(request)).await?;
            initiative(response.into_inner().initiative)
        }
        "CompleteInitiative" | "CancelInitiative" => {
            let request = Message::new(request, &["name", "reason"]);
            let end = proto::EndInitiativeRequest {
                name: request.text("name"),
                reason: request.text("reason"),
            };
            let response = if method == "CompleteInitiative" {
                client.complete_initiative(end).await?
            } else {
                client.cancel_initiative(end).await?
            };
            initiative(response.into_inner().initiative)
        }
        "AddInitiative" => {
            let definition_json = Message::new(request, &["definitionJson"]).text("definitionJson");
            let add = proto::AddInitiativeRequest { definition_json };
            initiative(client.add_initiative(add).await?.into_inner().initiative)
        }
        "DeriveInitiative" => {
            let response = client.derive_initiative(derive_request(request)).await?;
            initiative(response.into_inner().initiative)
        }
        "SetInitiativeBrief" => {
            let request = Message::new(request, &["name", "content"]);
            let (name, content) = (request.text("name"), request.text("content"));
            let set = proto::SetInitiativeBriefRequest {
                name: name.clone(),
                content,
            };
            brief_response_projection(
                &client.set_initiative_brief(set).await?.into_inner(),
                home,
                &name,
            )
        }
        "GetInitiativeBrief" => {
            let name = Message::new(request, &["name"]).text("name");
            let get = proto::GetInitiativeRequest { name: name.clone() };
            brief_response_projection(
                &client.get_initiative_brief(get).await?.into_inner(),
                home,
                &name,
            )
        }
        "AddFinding" => {
            let definition_json = Message::new(request, &["definitionJson"]).text("definitionJson");
            let response = client.add_finding(proto::AddFindingRequest { definition_json }).await?;
            json!({"finding":finding_projection(&response.into_inner().finding.unwrap())})
        }
        "ListFindings" => {
            let request = Message::new(request, &["initiative", "member"]);
            let list = proto::ListFindingsRequest {
                initiative: request.text("initiative"),
                member: request.text("member"),
            };
            let findings = client.list_findings(list).await?.into_inner().findings;
            json!({"findings":findings.iter().map(finding_projection).collect::<Vec<_>>()})
        }
        "DecideCampaign" => {
            let request = decide_campaign_request(request);
            let campaign = client.decide_campaign(request).await?.into_inner().campaign.unwrap();
            json!({"campaign":campaign_detail_projection(&campaign)})
        }
        method => panic!("unverified contract method {method}"),
    })
}

/// `member` is a proto3 `optional`: absent steers the whole initiative.
fn advance_request(request: &serde_json::Value) -> proto::AdvanceInitiativeRequest {
    let request = Message::new(request, &["name", "member", "operatorOrigin"]);
    proto::AdvanceInitiativeRequest {
        name: request.text("name"),
        member: request.optional_text("member"),
        operator_origin: request.text("operatorOrigin"),
    }
}

/// `addCycles` is a uint64, so Protobuf JSON writes it as a string.
fn decide_request(request: &serde_json::Value) -> proto::DecideInitiativeRequest {
    let request = Message::new(request, &["name", "decision", "members", "addCycles"]);
    proto::DecideInitiativeRequest {
        name: request.text("name"),
        decision: request.text("decision"),
        members: request.texts("members"),
        add_cycles: request.uint64("addCycles"),
    }
}

fn decide_campaign_request(request: &serde_json::Value) -> proto::DecideCampaignRequest {
    let request = Message::new(request, &["name", "decision", "addCycles"]);
    proto::DecideCampaignRequest {
        name: request.text("name"),
        decision: request.text("decision"),
        add_cycles: request.uint64("addCycles"),
    }
}

fn act_request(request: &serde_json::Value) -> proto::ActOnInitiativeRequest {
    let fields = [
        "name",
        "operatorOrigin",
        "hold",
        "relay",
        "dispatch",
        "ask",
        "milestone",
    ];
    let request = Message::new(request, &fields);
    proto::ActOnInitiativeRequest {
        name: request.text("name"),
        operator_origin: request.text("operatorOrigin"),
        action: owner_action(&request),
    }
}

fn derive_request(request: &serde_json::Value) -> proto::DeriveInitiativeRequest {
    let fields = [
        "fromRef",
        "projects",
        "authorizedBy",
        "sourceRepo",
        "integrationGates",
    ];
    let request = Message::new(request, &fields);
    proto::DeriveInitiativeRequest {
        from_ref: request.text("fromRef"),
        projects: request.texts("projects"),
        authorized_by: request.text("authorizedBy"),
        source_repo: request.text("sourceRepo"),
        integration_gates: request
            .children(
                "integrationGates",
                &[
                    "kind",
                    "command",
                    "host",
                    "required",
                    "artifacts",
                    "statement",
                ],
            )
            .iter()
            .map(|gate| proto::InitiativeEvidence {
                kind: gate.text("kind"),
                command: gate.text("command"),
                host: gate.optional_text("host"),
                required: gate.flag("required"),
                artifacts: gate.texts("artifacts"),
                statement: gate.text("statement"),
            })
            .collect(),
    }
}

/// The `action` oneof is written as exactly one of its variant fields.
fn owner_action(request: &Message<'_>) -> Option<proto::act_on_initiative_request::Action> {
    use proto::act_on_initiative_request::Action;
    let mut actions = Vec::new();
    if let Some(hold) = request.child("hold", &["member", "until", "reason"]) {
        actions.push(Action::Hold(proto::HoldInitiativeMember {
            member: hold.text("member"),
            until: hold.texts("until"),
            reason: hold.text("reason"),
        }));
    }
    if let Some(relay) = request.child("relay", &["findingId", "members"]) {
        actions.push(Action::Relay(proto::RelayInitiativeFinding {
            finding_id: relay.text("findingId"),
            members: relay.texts("members"),
        }));
    }
    if let Some(dispatch) = request.child("dispatch", &["project", "objective", "member"]) {
        actions.push(Action::Dispatch(proto::DispatchInitiativeTask {
            project: dispatch.text("project"),
            objective: dispatch.text("objective"),
            member: dispatch.optional_text("member"),
        }));
    }
    if let Some(ask) = request.child("ask", &["question", "members"]) {
        actions.push(Action::Ask(proto::AskInitiativeOwner {
            question: ask.text("question"),
            members: ask.texts("members"),
        }));
    }
    if let Some(milestone) = request.child("milestone", &["member", "milestone", "evidence"]) {
        actions.push(Action::Milestone(proto::RecordInitiativeMilestone {
            member: milestone.text("member"),
            milestone: milestone.text("milestone"),
            evidence: milestone.texts("evidence"),
        }));
    }
    assert!(actions.len() <= 1, "a oneof carries at most one variant");
    actions.pop()
}

/// What a dashboard renders after a control. Daemon-stamped times and
/// operation ids are checked here rather than declared; a brief's absolute
/// path lies under the temporary home, so it is checked and left out.
fn control_projection(initiative: &proto::Initiative, home: &std::path::Path) -> serde_json::Value {
    let mut projection = json!({
        "name":initiative.name,"outcome":initiative.outcome,"status":initiative.status,
        "steeringEnabled":initiative.steering_enabled,"projects":initiative.projects,
        "intentRefs":initiative.intent_refs,"budget":budget_projection(initiative),
        "members":initiative.members.iter().map(member_projection).collect::<Vec<_>>(),
        "doneEvidence":initiative.done_evidence.iter().map(evidence_projection).collect::<Vec<_>>(),
        "ownerDecisions":initiative.owner_decisions.iter().map(decision_projection).collect::<Vec<_>>(),
        "openQuestions":initiative.open_questions.iter().map(|question| {
            assert_rfc3339(&question.opened_at);
            json!({"id":question.id,"question":question.question,"members":question.members})
        }).collect::<Vec<_>>(),
        "deferredQuestionCount":initiative.deferred_question_count.to_string(),
        "holds":initiative.holds.iter().map(|hold| json!({"member":hold.member,"until":hold.until,"reason":hold.reason,"owner":hold.owner})).collect::<Vec<_>>(),
        "milestones":initiative.milestones.iter().map(|milestone| json!({"member":milestone.member,"name":milestone.name,"evidence":milestone.evidence})).collect::<Vec<_>>(),
        "reservations":initiative.reservations.iter().map(|reservation| {
            assert!(!reservation.operation_id.is_empty(), "expected `reservation.operation_id` to be non-empty");
            json!({"member":reservation.member,"kind":reservation.kind})
        }).collect::<Vec<_>>(),
        "taskDispatches":initiative.task_dispatches.iter().map(|task| json!({"member":task.member,"workItem":task.work_item,"project":task.project})).collect::<Vec<_>>(),
        "pendingActions":initiative.pending_actions.iter().map(|action| json!({"initiative":action.initiative})).collect::<Vec<_>>(),
        "steeringHistoryCount":initiative.steering_history_count.to_string(),
    });
    if let Some(owner) = &initiative.authorized_by {
        projection["authorizedBy"] = json!(owner);
    }
    if let Some(brief) = &initiative.brief {
        assert_brief_under_home(brief, home, &initiative.name);
        projection["brief"] = json!({"hash":brief.hash});
    }
    if let Some(source) = &initiative.derive_source {
        projection["deriveSource"] = json!({"repo":source.repo,"from":source.from});
    }
    if let Some(proposal) = &initiative.completion_proposal {
        assert_rfc3339(&proposal.proposed_at);
        projection["completionProposal"] = json!({"id":proposal.id,
            "reviews":proposal.reviews.iter().map(|review| json!({"statement":review.statement,"affirmed":review.affirmed,"evidence":review.evidence})).collect::<Vec<_>>()});
    }
    if let Some(confirmation) = &initiative.completion_confirmation {
        assert_rfc3339(&confirmation.confirmed_at);
        projection["completionConfirmation"] =
            json!({"proposalId":confirmation.proposal_id,"reason":confirmation.reason});
        if let Some(error) = &confirmation.last_error {
            projection["completionConfirmation"]["lastError"] = json!(error);
        }
    }
    projection
}

/// Proto3 `optional` member fields appear only when the daemon sets them.
fn member_projection(member: &proto::InitiativeMember) -> serde_json::Value {
    let mut projection = json!({"kind":member.kind,"name":member.name,"after":member.after,"reviewerGaps":member.reviewer_gaps});
    for (field, value) in [
        ("intentRef", &member.intent_ref),
        ("description", &member.description),
        ("intentState", &member.intent_state),
        ("project", &member.project),
        ("lastVerdict", &member.last_verdict),
    ] {
        if let Some(value) = value {
            projection[field] = json!(value);
        }
    }
    if let Some(campaign) = &member.campaign {
        let mut summary = json!({"name":campaign.name,"project":campaign.project,"status":campaign.status,
            "cyclesCompleted":campaign.cycles_completed.to_string(),"maxCycles":campaign.max_cycles.to_string()});
        if let Some(initiative) = &campaign.initiative {
            summary["initiative"] = json!(initiative);
        }
        if let Some(escalation) = &campaign.open_escalation {
            summary["openEscalation"] = escalation_projection(escalation);
        }
        projection["campaign"] = summary;
    }
    projection
}

fn campaign_detail_projection(campaign: &proto::CampaignDetail) -> serde_json::Value {
    let mut projection = json!({"name":campaign.name,"project":campaign.project,"status":campaign.status,
        "cyclesCompleted":campaign.cycles_completed.to_string(),"maxCycles":campaign.max_cycles.to_string(),
        "authorizedBy":campaign.authorized_by,
        "ownerDecisions":campaign.owner_decisions.iter().map(|decision| {
            assert_rfc3339(&decision.decided_at);
            json!({"decision":decision.decision,"authorizedBy":decision.authorized_by})
        }).collect::<Vec<_>>()});
    if let Some(initiative) = &campaign.initiative {
        projection["initiative"] = json!(initiative);
    }
    if let Some(escalation) = &campaign.open_escalation {
        projection["openEscalation"] = escalation_projection(escalation);
    }
    projection
}

fn escalation_projection(escalation: &proto::CampaignEscalation) -> serde_json::Value {
    let mut projection = json!({"reason":escalation.reason,"escalatedAt":escalation.escalated_at});
    if let Some(question) = &escalation.question {
        projection["question"] = json!(question);
    }
    projection
}

fn finding_projection(finding: &proto::Finding) -> serde_json::Value {
    assert!(finding.audit.is_none(), "audit evidence is outside this contract");
    let from = finding.from.as_ref().unwrap();
    json!({"id":finding.id,"initiative":finding.initiative,"from":{"kind":from.kind,"id":from.id},
        "to":finding.to,"kind":finding.kind,"text":finding.text,"refs":finding.refs,
        "consumedBy":finding.consumed_by.iter().map(|consumed| json!({"member":consumed.member,"workItem":consumed.work_item})).collect::<Vec<_>>(),
        "recordedAt":finding.recorded_at})
}

fn brief_response_projection(
    response: &proto::InitiativeBriefResponse,
    home: &std::path::Path,
    name: &str,
) -> serde_json::Value {
    let brief = response.brief.as_ref().unwrap();
    assert_brief_under_home(brief, home, name);
    json!({"brief":{"hash":brief.hash},"content":response.content})
}

fn assert_brief_under_home(brief: &proto::InitiativeBrief, home: &std::path::Path, name: &str) {
    assert_eq!(
        std::path::Path::new(&brief.path),
        home.join("initiatives").join(name).join("BRIEF.md")
    );
}
