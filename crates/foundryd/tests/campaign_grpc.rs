//! Integration tests for the `ListCampaigns` and `GetCampaign` gRPC handlers.
//!
//! These tests construct a real `FoundryService` with temporary campaign-store
//! files, invoke `FoundryService::list_campaigns` and `FoundryService::get_campaign`
//! directly, and assert the observed wire behaviour for empty stores, filtering,
//! ordering, malformed stores, field-level mapping, and per-request store reload.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{
    Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundryd::{
    proto::{GetCampaignRequest, ListCampaignsRequest, foundry_server::Foundry},
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tonic::{Code, Request};

fn make_service() -> (FoundryService, NamedTempFile, TempDir) {
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
    let tmp_campaigns = NamedTempFile::new().expect("tempfile for campaigns");
    let campaigns_path = tmp_campaigns.path().to_path_buf();

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

    (service, tmp_campaigns, tmp_traces)
}

fn sample_campaign(name: &str, project: &str) -> Campaign {
    Campaign {
        name: name.to_string(),
        project: project.to_string(),
        mission: format!("Mission for {name}"),
        intent_refs: vec![],
        context_paths: vec![],
        done_evidence: vec![DoneEvidence::Review {
            statement: "Reviewed.".to_string(),
        }],
        budget: CampaignBudget { max_cycles: 9 },
        escalation: vec!["human judgment required".to_string()],
        status: CampaignStatus::Active,
        cycles_completed: 3,
        cycles_landed: 2,
        authorized_by: Some("Stacey".to_string()),
        agent_provider: Some("codex".to_string()),
        last_run_event_id: Some("run-123".to_string()),
    }
}

fn write_campaigns(file: &NamedTempFile, campaigns: Vec<Campaign>) {
    let store = CampaignStore {
        version: 1,
        campaigns,
    };
    store.save(file.path()).expect("campaign store should save");
}

#[tokio::test]
async fn list_campaigns_returns_empty_for_empty_file() {
    let (service, _tmp_campaigns, _tmp_traces) = make_service();

    let response = service
        .list_campaigns(Request::new(ListCampaignsRequest {
            project: String::new(),
        }))
        .await
        .expect("list_campaigns should succeed for empty store")
        .into_inner();

    assert!(response.campaigns.is_empty());
}

#[tokio::test]
async fn list_campaigns_returns_empty_for_missing_store() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    std::fs::remove_file(tmp_campaigns.path()).expect("campaign temp file should be removable");

    let response = service
        .list_campaigns(Request::new(ListCampaignsRequest {
            project: String::new(),
        }))
        .await
        .expect("list_campaigns should succeed for missing store")
        .into_inner();

    assert!(response.campaigns.is_empty());
}

#[tokio::test]
async fn list_campaigns_returns_name_sorted_inventory() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    write_campaigns(
        &tmp_campaigns,
        vec![
            sample_campaign("zeta", "proj-a"),
            sample_campaign("alpha", "proj-a"),
            sample_campaign("beta", "proj-b"),
        ],
    );

    let response = service
        .list_campaigns(Request::new(ListCampaignsRequest {
            project: String::new(),
        }))
        .await
        .expect("list_campaigns should succeed")
        .into_inner();

    let names: Vec<_> = response.campaigns.iter().map(|campaign| campaign.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "beta", "zeta"]);
}

#[tokio::test]
async fn list_campaigns_filters_by_exact_project() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    write_campaigns(
        &tmp_campaigns,
        vec![
            sample_campaign("alpha", "proj-a"),
            sample_campaign("beta", "proj-b"),
            sample_campaign("gamma", "proj-a"),
        ],
    );

    let response = service
        .list_campaigns(Request::new(ListCampaignsRequest {
            project: "proj-a".to_string(),
        }))
        .await
        .expect("list_campaigns should succeed")
        .into_inner();

    let names: Vec<_> = response.campaigns.iter().map(|campaign| campaign.name.as_str()).collect();
    assert_eq!(names, vec!["alpha", "gamma"]);
    assert!(response.campaigns.iter().all(|campaign| campaign.project == "proj-a"));
}

#[tokio::test]
async fn list_campaigns_reports_malformed_store() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    std::fs::write(tmp_campaigns.path(), "{ not json").expect("should write malformed store");

    let err = service
        .list_campaigns(Request::new(ListCampaignsRequest {
            project: String::new(),
        }))
        .await
        .expect_err("malformed store should fail");

    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(err.message().contains("campaign store is malformed"));
    assert!(
        !err.message().contains(&tmp_campaigns.path().display().to_string()),
        "error message must not expose store path: {}",
        err.message()
    );
}

#[tokio::test]
async fn list_campaigns_reports_unreadable_store() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    std::fs::remove_file(tmp_campaigns.path()).expect("campaign temp file should be removable");
    std::fs::create_dir(tmp_campaigns.path()).expect("should replace file path with directory");

    let err = service
        .list_campaigns(Request::new(ListCampaignsRequest {
            project: String::new(),
        }))
        .await
        .expect_err("directory path should fail");

    assert_eq!(err.code(), Code::Internal);
    assert!(err.message().contains("campaign store is unreadable"));
    assert!(
        !err.message().contains(&tmp_campaigns.path().display().to_string()),
        "error message must not expose store path: {}",
        err.message()
    );
}

#[tokio::test]
async fn list_campaigns_exposes_expected_wire_fields() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    let mut campaign = sample_campaign("alpha", "proj-a");
    campaign.mission = "Expose the live inventory".to_string();
    campaign.status = CampaignStatus::Escalated;
    campaign.cycles_completed = 7;
    campaign.cycles_landed = 5;
    campaign.budget.max_cycles = 12;
    campaign.authorized_by = Some("Stacey Vetzal".to_string());
    campaign.agent_provider = Some("codex".to_string());
    campaign.last_run_event_id = Some("evt-999".to_string());
    write_campaigns(&tmp_campaigns, vec![campaign]);

    let response = service
        .list_campaigns(Request::new(ListCampaignsRequest {
            project: String::new(),
        }))
        .await
        .expect("list_campaigns should succeed")
        .into_inner();

    let campaign = response.campaigns.into_iter().next().expect("one campaign");
    assert_eq!(campaign.name, "alpha");
    assert_eq!(campaign.project, "proj-a");
    assert_eq!(campaign.mission, "Expose the live inventory");
    assert_eq!(campaign.status, "escalated");
    assert_eq!(campaign.cycles_completed, 7);
    assert_eq!(campaign.cycles_landed, 5);
    assert_eq!(campaign.max_cycles, 12);
    assert_eq!(campaign.authorized_by, "Stacey Vetzal");
    assert_eq!(campaign.agent_provider, "codex");
    assert_eq!(campaign.last_run_event_id, "evt-999");
}

// ---------------------------------------------------------------------------
// GetCampaign tests
// ---------------------------------------------------------------------------

/// Build a fully-populated campaign that exercises every field in CampaignDetail,
/// including non-empty intent_refs and context_paths, a required gate, an optional
/// gate, a review evidence item, ≥1 escalation entry, authorized_by, agent_provider,
/// last_run_event_id, a non-default budget, and non-zero cycle counts.
fn fully_populated_campaign(name: &str, project: &str) -> Campaign {
    Campaign {
        name: name.to_string(),
        project: project.to_string(),
        mission: "Prove correctness end to end.".to_string(),
        intent_refs: vec![
            "intent.core-correctness".to_string(),
            "intent.edge-cases".to_string(),
        ],
        context_paths: vec!["docs/AGENTS.generated.md".to_string()],
        done_evidence: vec![
            DoneEvidence::Gate {
                command: "cargo test --workspace".to_string(),
                required: true,
            },
            DoneEvidence::Gate {
                command: "cargo clippy --workspace".to_string(),
                required: false,
            },
            DoneEvidence::Review {
                statement: "All acceptance criteria verified by reviewer.".to_string(),
            },
        ],
        budget: CampaignBudget { max_cycles: 7 },
        escalation: vec!["Owner decision required for behaviour changes.".to_string()],
        status: CampaignStatus::Active,
        cycles_completed: 3,
        cycles_landed: 2,
        authorized_by: Some("Stacey".to_string()),
        agent_provider: Some("codex".to_string()),
        last_run_event_id: Some("evt-detail-42".to_string()),
    }
}

#[tokio::test]
async fn get_campaign_returns_full_detail_for_populated_campaign() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    write_campaigns(&tmp_campaigns, vec![fully_populated_campaign("full-detail", "proj-fd")]);

    let detail = service
        .get_campaign(Request::new(GetCampaignRequest {
            name: "full-detail".to_string(),
        }))
        .await
        .expect("get_campaign should succeed for a populated campaign")
        .into_inner()
        .campaign
        .expect("CampaignDetail must be present");

    // Identity and definition fields.
    assert_eq!(detail.name, "full-detail");
    assert_eq!(detail.project, "proj-fd");
    assert_eq!(detail.mission, "Prove correctness end to end.");
    assert_eq!(detail.intent_refs, vec!["intent.core-correctness", "intent.edge-cases"]);
    assert_eq!(detail.context_paths, vec!["docs/AGENTS.generated.md"]);

    // Done evidence: required gate, optional gate, review — all three must round-trip.
    assert_eq!(detail.done_evidence.len(), 3);

    let required_gate = &detail.done_evidence[0];
    assert_eq!(required_gate.kind, "gate");
    assert_eq!(required_gate.command, "cargo test --workspace");
    assert!(required_gate.required, "first gate must be required=true");

    let optional_gate = &detail.done_evidence[1];
    assert_eq!(optional_gate.kind, "gate");
    assert_eq!(optional_gate.command, "cargo clippy --workspace");
    assert!(!optional_gate.required, "second gate must be required=false (optional)");

    let review = &detail.done_evidence[2];
    assert_eq!(review.kind, "review");
    assert_eq!(review.statement, "All acceptance criteria verified by reviewer.");

    // Escalation.
    assert_eq!(detail.escalation, vec!["Owner decision required for behaviour changes."]);

    // Optional fields.
    assert_eq!(detail.authorized_by, "Stacey");
    assert_eq!(detail.agent_provider, "codex");
    assert_eq!(detail.last_run_event_id, "evt-detail-42");

    // Budget and runtime state.
    assert_eq!(detail.max_cycles, 7);
    assert_eq!(detail.status, "active");
    assert_eq!(detail.cycles_completed, 3);
    assert_eq!(detail.cycles_landed, 2);
}

#[tokio::test]
async fn get_campaign_returns_not_found_for_absent_name_in_populated_store() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    // Store has one campaign, but we ask for a different name.
    write_campaigns(&tmp_campaigns, vec![fully_populated_campaign("existing", "proj-a")]);

    let err = service
        .get_campaign(Request::new(GetCampaignRequest {
            name: "nonexistent".to_string(),
        }))
        .await
        .expect_err("absent campaign name should return NOT_FOUND");

    assert_eq!(err.code(), Code::NotFound);
}

#[tokio::test]
async fn get_campaign_returns_not_found_for_absent_name_in_missing_store() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    // Remove the store file entirely — missing store ≡ empty store for this query.
    std::fs::remove_file(tmp_campaigns.path()).expect("campaign temp file should be removable");

    let err = service
        .get_campaign(Request::new(GetCampaignRequest {
            name: "nonexistent".to_string(),
        }))
        .await
        .expect_err("absent campaign in missing store should return NOT_FOUND, not INTERNAL");

    assert_eq!(
        err.code(),
        Code::NotFound,
        "missing store must yield NOT_FOUND for the named campaign, not INTERNAL"
    );
}

#[tokio::test]
async fn get_campaign_returns_failed_precondition_for_malformed_store() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    std::fs::write(tmp_campaigns.path(), "{ not json").expect("should write malformed store");

    let err = service
        .get_campaign(Request::new(GetCampaignRequest {
            name: "any".to_string(),
        }))
        .await
        .expect_err("malformed store should fail");

    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(err.message().contains("campaign store is malformed"));
    assert!(
        !err.message().contains(&tmp_campaigns.path().display().to_string()),
        "error message must not expose store path: {}",
        err.message()
    );
}

#[tokio::test]
async fn get_campaign_returns_internal_for_unreadable_store() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    std::fs::remove_file(tmp_campaigns.path()).expect("campaign temp file should be removable");
    std::fs::create_dir(tmp_campaigns.path()).expect("should replace file path with directory");

    let err = service
        .get_campaign(Request::new(GetCampaignRequest {
            name: "any".to_string(),
        }))
        .await
        .expect_err("directory path should fail");

    assert_eq!(err.code(), Code::Internal);
    assert!(err.message().contains("campaign store is unreadable"));
    assert!(
        !err.message().contains(&tmp_campaigns.path().display().to_string()),
        "error message must not expose store path: {}",
        err.message()
    );
}

#[tokio::test]
async fn get_campaign_reflects_post_load_store_change() {
    let (service, tmp_campaigns, _tmp_traces) = make_service();
    write_campaigns(&tmp_campaigns, vec![fully_populated_campaign("changeable", "proj-c")]);

    // First query — mission as originally written.
    let first = service
        .get_campaign(Request::new(GetCampaignRequest {
            name: "changeable".to_string(),
        }))
        .await
        .expect("first query should succeed")
        .into_inner()
        .campaign
        .expect("campaign should be present");
    assert_eq!(first.mission, "Prove correctness end to end.");

    // Overwrite the store on disk with an updated mission.
    let mut updated = fully_populated_campaign("changeable", "proj-c");
    updated.mission = "Updated mission after post-write.".to_string();
    write_campaigns(&tmp_campaigns, vec![updated]);

    // Second query must reload from disk and reflect the update (no in-memory cache).
    let second = service
        .get_campaign(Request::new(GetCampaignRequest {
            name: "changeable".to_string(),
        }))
        .await
        .expect("second query should succeed")
        .into_inner()
        .campaign
        .expect("campaign should be present");
    assert_eq!(
        second.mission, "Updated mission after post-write.",
        "get_campaign must re-load from disk on every call; a cached result would fail this assertion"
    );
}
