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
    let ctx = RuntimeContext {
        engine: Arc::new(Engine::new().with_event_broadcaster(event_tx.clone())),
        trace_store: Arc::new(TraceStore::with_trace_writer(
            Duration::from_secs(60),
            writer.clone(),
        )),
        workflow_tracker: Arc::new(WorkflowTracker::new()),
        trace_writer: writer,
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
            members: vec![]
        }),
        second_client.decide_initiative(proto::DecideInitiativeRequest {
            name: "shared".into(),
            decision: "Keep both compatible".into(),
            members: vec![]
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
    let completed = client
        .complete_initiative(proto::EndInitiativeRequest {
            name: "shared".into(),
            reason: "Verified both members".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .initiative
        .unwrap();
    assert_eq!(completed.status, "completed");
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
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Internal);
    assert_eq!(std::fs::read(path).unwrap(), before);
    server.abort();
}
