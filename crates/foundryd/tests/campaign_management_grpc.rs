//! Integration tests for the campaign lifecycle gRPC handlers:
//! `PauseCampaign`, `ResumeCampaign`, and `AdvanceCampaign`.
//!
//! Each test constructs a real `FoundryService` with a temporary campaign
//! store file and invokes the service method directly, asserting wire
//! behavior including status codes, persisted state, and emitted events.
//!
//! DONE-GATE ARTIFACT: this file is the named artifact required by the
//! campaign plan for the gRPC lifecycle boundary objective.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::campaign::{
    Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{LoopContext, TaskRunCompletedPayload, TaskVerdict};
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundryd::{
    proto::{
        AdvanceCampaignRequest, CompleteCampaignRequest, PauseCampaignRequest,
        ResumeCampaignRequest, foundry_server::Foundry,
    },
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tonic::{Code, Request};

// ── Test harness ─────────────────────────────────────────────────────────────

/// Construct a `FoundryService` backed by temporary files and return a
/// broadcast receiver so tests can observe emitted events.
fn make_service() -> (FoundryService, NamedTempFile, broadcast::Receiver<Event>, TempDir) {
    let (event_tx, rx) = broadcast::channel(64);
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

    (service, tmp_campaigns, rx, tmp_traces)
}

fn make_service_with_campaigns_path(
    campaigns_path: std::path::PathBuf,
) -> (FoundryService, broadcast::Receiver<Event>, TempDir) {
    let (event_tx, rx) = broadcast::channel(64);
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

    (service, rx, tmp_traces)
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
    }
}

fn paused_campaign(name: &str, max_cycles: u64) -> Campaign {
    Campaign {
        name: name.to_string(),
        project: "test-project".to_string(),
        mission: "Test mission".to_string(),
        intent_refs: vec![],
        context_paths: vec![],
        done_evidence: vec![DoneEvidence::Review {
            statement: "done".to_string(),
        }],
        budget: CampaignBudget { max_cycles },
        escalation: vec![],
        status: CampaignStatus::Paused,
        cycles_completed: 0,
        cycles_landed: 0,
        authorized_by: Some("owner".to_string()),
        agent_provider: None,
        last_run_event_id: None,
        owner_decisions: vec![],
        pending_run_result: None,
    }
}

fn write_campaigns(file: &NamedTempFile, campaigns: Vec<Campaign>) {
    let store = CampaignStore {
        version: 1,
        campaigns,
    };
    store.save(file.path()).expect("campaign store should save");
}

fn write_single(file: &NamedTempFile, campaign: Campaign) {
    write_campaigns(file, vec![campaign]);
}

// ── CompleteCampaign ──────────────────────────────────────────────────────────

#[tokio::test]
async fn complete_paused_campaign_persists_audit_record_and_emits_terminal_event() {
    let (service, tmp_campaigns, mut rx, _tmp_traces) = make_service();
    let mut campaign = paused_campaign("c", 10);
    campaign.pending_run_result = Some(TaskRunCompletedPayload {
        project: "test-project".to_string(),
        success: true,
        landed: true,
        summary: "shipped".to_string(),
        preservation_ref: Some("abc123".to_string()),
        verdict: TaskVerdict::Complete,
        context: LoopContext {
            campaign: Some("c".to_string()),
            ..LoopContext::default()
        },
    });
    write_single(&tmp_campaigns, campaign);

    let response = service
        .complete_campaign(Request::new(CompleteCampaignRequest {
            name: "c".to_string(),
            reason: "Production evidence confirms the mission shipped.".to_string(),
        }))
        .await
        .expect("complete should succeed");
    assert_eq!(response.into_inner().campaign.unwrap().status, "completed");

    let stored = CampaignStore::load(tmp_campaigns.path()).expect("load store");
    let completed = stored.find("c").expect("campaign exists");
    assert_eq!(completed.status, CampaignStatus::Completed);
    assert!(completed.pending_run_result.is_none());
    assert_eq!(completed.owner_decisions.len(), 1);
    assert_eq!(
        completed.owner_decisions[0].decision,
        "Completed externally: Production evidence confirms the mission shipped."
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_completed = false;
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) if event.event_type == EventType::CampaignCompleted => {
                saw_completed = true;
                break;
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    assert!(saw_completed, "CampaignCompleted must be broadcast via the engine path");
}

#[tokio::test]
async fn complete_requires_non_empty_reason_and_authorized_owner() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    write_single(&tmp_campaigns, paused_campaign("c", 10));

    let empty = service
        .complete_campaign(Request::new(CompleteCampaignRequest {
            name: "c".to_string(),
            reason: "   ".to_string(),
        }))
        .await
        .expect_err("blank reason must fail");
    assert_eq!(empty.code(), Code::InvalidArgument);

    let mut unauthorized = paused_campaign("c", 10);
    unauthorized.authorized_by = None;
    write_single(&tmp_campaigns, unauthorized);
    let error = service
        .complete_campaign(Request::new(CompleteCampaignRequest {
            name: "c".to_string(),
            reason: "shipped".to_string(),
        }))
        .await
        .expect_err("unauthorized completion must fail");
    assert_eq!(error.code(), Code::FailedPrecondition);
}

#[cfg(unix)]
#[tokio::test]
async fn complete_persist_failure_returns_internal_keeps_store_bytes_and_emits_no_event() {
    let campaigns_dir = tempfile::tempdir().expect("campaigns tempdir");
    let campaigns_path = campaigns_dir.path().join("campaigns.json");
    CampaignStore {
        version: 1,
        campaigns: vec![paused_campaign("c", 10)],
    }
    .save(&campaigns_path)
    .expect("save initial campaigns");
    drop(
        CampaignStore::lock_exclusive(&campaigns_path)
            .expect("create lock file before making directory readonly"),
    );
    let before = std::fs::read(&campaigns_path).expect("read campaigns before failure");

    let mut permissions = std::fs::metadata(campaigns_dir.path())
        .expect("stat campaigns dir")
        .permissions();
    permissions.set_mode(0o555);
    std::fs::set_permissions(campaigns_dir.path(), permissions).expect("set readonly dir");

    let (service, mut rx, _tmp_traces) = make_service_with_campaigns_path(campaigns_path.clone());
    let err = service
        .complete_campaign(Request::new(CompleteCampaignRequest {
            name: "c".to_string(),
            reason: "Production evidence confirms the mission shipped.".to_string(),
        }))
        .await
        .expect_err("persist failure must fail");

    let mut restore = std::fs::metadata(campaigns_dir.path())
        .expect("stat campaigns dir for restore")
        .permissions();
    restore.set_mode(0o755);
    std::fs::set_permissions(campaigns_dir.path(), restore).expect("restore dir perms");

    assert_eq!(err.code(), Code::Internal);
    assert!(err.message().contains("campaign store save failed"));
    assert!(!err.message().contains(&campaigns_path.display().to_string()));
    assert_eq!(std::fs::read(&campaigns_path).expect("read campaigns after failure"), before);

    let stored = CampaignStore::load(&campaigns_path).expect("load store after failure");
    let campaign = stored.find("c").expect("campaign remains present");
    assert_eq!(campaign.status, CampaignStatus::Paused);
    assert!(campaign.owner_decisions.is_empty());

    while let Ok(event) = rx.try_recv() {
        assert_ne!(
            event.event_type,
            EventType::CampaignCompleted,
            "CampaignCompleted must not be broadcast when save fails"
        );
    }
}

// ── PauseCampaign ─────────────────────────────────────────────────────────────

/// Pausing an active campaign must return status=paused in the response AND
/// persist Paused to the store file. An implementation that only updates
/// in-memory state without saving will fail the store assertion.
#[tokio::test]
async fn pause_active_campaign_returns_paused_and_persists_to_store() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    write_single(&tmp_campaigns, active_campaign("c"));

    let resp = service
        .pause_campaign(Request::new(PauseCampaignRequest {
            name: "c".to_string(),
        }))
        .await
        .expect("pause should succeed");
    let detail = resp.into_inner().campaign.expect("campaign in response");

    // Wire response reflects Paused.
    assert_eq!(detail.status, "paused", "response status must be 'paused'");

    // Persisted store also shows Paused.
    let stored = CampaignStore::load(tmp_campaigns.path()).expect("load store");
    assert_eq!(
        stored.find("c").expect("campaign must exist").status,
        CampaignStatus::Paused,
        "persisted campaign must be Paused"
    );
}

/// `PauseCampaign` on a missing campaign must return `NOT_FOUND`.
#[tokio::test]
async fn pause_missing_campaign_returns_not_found() {
    let (service, _tmp_campaigns, _rx, _tmp_traces) = make_service();

    let err = service
        .pause_campaign(Request::new(PauseCampaignRequest {
            name: "nonexistent".to_string(),
        }))
        .await
        .expect_err("missing campaign must fail");
    assert_eq!(err.code(), Code::NotFound);
}

/// A malformed campaign store must return `FAILED_PRECONDITION` and the error
/// message must NOT expose the store file path.
#[tokio::test]
async fn pause_malformed_store_returns_failed_precondition_without_store_path() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    std::fs::write(tmp_campaigns.path(), b"{ not valid json }").expect("write malformed store");

    let err = service
        .pause_campaign(Request::new(PauseCampaignRequest {
            name: "any".to_string(),
        }))
        .await
        .expect_err("malformed store must fail");

    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(
        !err.message().contains(&tmp_campaigns.path().display().to_string()),
        "error message must not expose store path: {}",
        err.message()
    );
}

/// An unreadable campaign store (directory at the store path) must return
/// INTERNAL and the error message must NOT expose the store file path.
#[tokio::test]
async fn pause_unreadable_store_returns_internal_without_store_path() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    let store_path = tmp_campaigns.path().to_path_buf();
    // Replace the file with a directory to simulate an unreadable store.
    std::fs::remove_file(&store_path).expect("remove campaign file");
    std::fs::create_dir(&store_path).expect("create dir at campaign path");

    let err = service
        .pause_campaign(Request::new(PauseCampaignRequest {
            name: "any".to_string(),
        }))
        .await
        .expect_err("directory path must fail");

    assert_eq!(err.code(), Code::Internal);
    assert!(
        !err.message().contains(&store_path.display().to_string()),
        "error message must not expose store path: {}",
        err.message()
    );

    // Cleanup.
    std::fs::remove_dir(&store_path).ok();
}

// ── pause-while-running preserves pending_run_result ─────────────────────────

/// Pausing a campaign that has a `pending_run_result` must not clear or overwrite
/// it. After a subsequent resume the pending result must still be present and
/// unconsumed so the `AdvanceCampaign` block can replay it.
#[tokio::test]
async fn pause_preserves_pending_run_result_through_resume() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();

    let pending = TaskRunCompletedPayload {
        project: "test-project".to_string(),
        success: true,
        landed: true,
        summary: "first slice landed with a gap".to_string(),
        preservation_ref: Some("foundry-task/preserved-ref".to_string()),
        verdict: TaskVerdict::Remainder {
            gaps: vec!["exercise the gRPC boundary".to_string()],
        },
        context: LoopContext {
            campaign: Some("c".to_string()),
            ..LoopContext::default()
        },
    };

    let mut campaign = active_campaign("c");
    campaign.pending_run_result = Some(pending.clone());
    write_single(&tmp_campaigns, campaign);

    // Pause: must NOT clear pending_run_result.
    service
        .pause_campaign(Request::new(PauseCampaignRequest {
            name: "c".to_string(),
        }))
        .await
        .expect("pause should succeed");

    let after_pause = CampaignStore::load(tmp_campaigns.path()).expect("load after pause");
    assert!(
        after_pause.find("c").expect("campaign").pending_run_result.is_some(),
        "pending_run_result must be preserved after pause"
    );

    // Resume: must also NOT clear pending_run_result.
    service
        .resume_campaign(Request::new(ResumeCampaignRequest {
            name: "c".to_string(),
            add_cycles: 0,
        }))
        .await
        .expect("resume should succeed");

    let after_resume = CampaignStore::load(tmp_campaigns.path()).expect("load after resume");
    let stored_campaign = after_resume.find("c").expect("campaign after resume");
    assert_eq!(stored_campaign.status, CampaignStatus::Active);
    assert!(
        stored_campaign.pending_run_result.is_some(),
        "pending_run_result must remain unconsumed after resume"
    );
    // The preservation_ref specifically must survive the pause/resume round-trip.
    assert_eq!(
        stored_campaign
            .pending_run_result
            .as_ref()
            .and_then(|r| r.preservation_ref.as_deref()),
        Some("foundry-task/preserved-ref"),
        "preservation_ref must survive the pause/resume round-trip"
    );
}

// ── ResumeCampaign ─────────────────────────────────────────────────────────────

/// Resuming with `add_cycles=0` must succeed and not change `max_cycles`.
#[tokio::test]
async fn resume_with_add_cycles_zero_sets_active_without_changing_budget() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    let original_max = 10u64;
    write_single(&tmp_campaigns, paused_campaign("c", original_max));

    let resp = service
        .resume_campaign(Request::new(ResumeCampaignRequest {
            name: "c".to_string(),
            add_cycles: 0,
        }))
        .await
        .expect("resume should succeed");
    let detail = resp.into_inner().campaign.expect("campaign");

    assert_eq!(detail.status, "active");
    assert_eq!(
        detail.max_cycles, original_max,
        "max_cycles must be unchanged when `add_cycles=0`"
    );
}

/// Resuming with `add_cycles=N` must increase `max_cycles` by exactly N, not 0
/// and not 2N. An implementation that ignores `add_cycles` or applies it twice will
/// fail the exact-value assertion.
#[tokio::test]
async fn resume_with_add_cycles_extends_budget_by_exactly_n() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    let original_max = 10u64;
    let add = 7u64;
    write_single(&tmp_campaigns, paused_campaign("c", original_max));

    let resp = service
        .resume_campaign(Request::new(ResumeCampaignRequest {
            name: "c".to_string(),
            add_cycles: add,
        }))
        .await
        .expect("resume should succeed");
    let detail = resp.into_inner().campaign.expect("campaign");

    assert_eq!(detail.status, "active");
    assert_eq!(
        detail.max_cycles,
        original_max + add,
        "`max_cycles` must increase by exactly `add_cycles`"
    );

    // Persisted store must also reflect the new budget.
    let stored = CampaignStore::load(tmp_campaigns.path()).expect("load store");
    assert_eq!(
        stored.find("c").expect("campaign").budget.max_cycles,
        original_max + add,
        "persisted `max_cycles` must equal original + `add_cycles`"
    );
}

/// A second resume must NOT double-apply `add_cycles`. After the first resume
/// the campaign is Active, so a second resume returns `FAILED_PRECONDITION`
/// (invalid transition), and `max_cycles` remains exactly original + `first_add`.
#[tokio::test]
async fn second_resume_does_not_double_apply_add_cycles() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    write_single(&tmp_campaigns, paused_campaign("c", 10));

    // First resume: add 5 cycles.
    service
        .resume_campaign(Request::new(ResumeCampaignRequest {
            name: "c".to_string(),
            add_cycles: 5,
        }))
        .await
        .expect("first resume should succeed");

    // Second resume: campaign is now Active, not Paused → must fail.
    let err = service
        .resume_campaign(Request::new(ResumeCampaignRequest {
            name: "c".to_string(),
            add_cycles: 5,
        }))
        .await
        .expect_err("second resume must fail (campaign is Active, not Paused)");
    assert_eq!(err.code(), Code::FailedPrecondition);

    // `max_cycles` must be 15 (10 + 5), never 20 (which would indicate double-apply).
    let stored = CampaignStore::load(tmp_campaigns.path()).expect("load");
    assert_eq!(
        stored.find("c").expect("campaign").budget.max_cycles,
        15,
        "`max_cycles` must be exactly 15 (10 + 5); double-apply would yield 20"
    );
}

/// Resuming without an `authorized_by` owner must return `FAILED_PRECONDITION`.
#[tokio::test]
async fn resume_without_authorized_by_returns_failed_precondition() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    let mut campaign = paused_campaign("c", 10);
    campaign.authorized_by = None;
    write_single(&tmp_campaigns, campaign);

    let err = service
        .resume_campaign(Request::new(ResumeCampaignRequest {
            name: "c".to_string(),
            add_cycles: 0,
        }))
        .await
        .expect_err("unauthorized resume must fail");
    assert_eq!(err.code(), Code::FailedPrecondition);
}

/// `add_cycles` that would overflow `u64` must return `FAILED_PRECONDITION`.
#[tokio::test]
async fn resume_with_overflowing_add_cycles_returns_failed_precondition() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    // Set max_cycles to u64::MAX so any add > 0 overflows.
    write_single(&tmp_campaigns, paused_campaign("c", u64::MAX));

    let err = service
        .resume_campaign(Request::new(ResumeCampaignRequest {
            name: "c".to_string(),
            add_cycles: 1,
        }))
        .await
        .expect_err("overflow must fail");
    assert_eq!(err.code(), Code::FailedPrecondition);
}

/// `ResumeCampaign` on a missing campaign must return `NOT_FOUND`.
#[tokio::test]
async fn resume_missing_campaign_returns_not_found() {
    let (service, _tmp_campaigns, _rx, _tmp_traces) = make_service();

    let err = service
        .resume_campaign(Request::new(ResumeCampaignRequest {
            name: "nonexistent".to_string(),
            add_cycles: 0,
        }))
        .await
        .expect_err("missing campaign must fail");
    assert_eq!(err.code(), Code::NotFound);
}

/// A malformed store on `ResumeCampaign` must return `FAILED_PRECONDITION` without
/// exposing the store path.
#[tokio::test]
async fn resume_malformed_store_returns_failed_precondition_without_store_path() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    std::fs::write(tmp_campaigns.path(), b"{ bad json }").expect("write");

    let err = service
        .resume_campaign(Request::new(ResumeCampaignRequest {
            name: "any".to_string(),
            add_cycles: 0,
        }))
        .await
        .expect_err("malformed store must fail");

    assert_eq!(err.code(), Code::FailedPrecondition);
    assert!(
        !err.message().contains(&tmp_campaigns.path().display().to_string()),
        "error message must not expose store path: {}",
        err.message()
    );
}

/// An unreadable store on `ResumeCampaign` must return `INTERNAL` without exposing
/// the store path.
#[tokio::test]
async fn resume_unreadable_store_returns_internal_without_store_path() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    let store_path = tmp_campaigns.path().to_path_buf();
    std::fs::remove_file(&store_path).expect("remove");
    std::fs::create_dir(&store_path).expect("create dir");

    let err = service
        .resume_campaign(Request::new(ResumeCampaignRequest {
            name: "any".to_string(),
            add_cycles: 0,
        }))
        .await
        .expect_err("directory path must fail");

    assert_eq!(err.code(), Code::Internal);
    assert!(
        !err.message().contains(&store_path.display().to_string()),
        "error message must not expose store path: {}",
        err.message()
    );

    std::fs::remove_dir(&store_path).ok();
}

// ── AdvanceCampaign ───────────────────────────────────────────────────────────

/// Advancing an active campaign must dispatch a `CampaignAdvanceRequested` event
/// and return the current campaign state.  The advance itself is asynchronous;
/// we verify the event was broadcast, not the outcome.
#[tokio::test]
async fn advance_active_campaign_dispatches_event_and_returns_current_state() {
    let (service, tmp_campaigns, mut rx, _tmp_traces) = make_service();
    write_single(&tmp_campaigns, active_campaign("c"));

    let resp = service
        .advance_campaign(Request::new(AdvanceCampaignRequest {
            name: "c".to_string(),
        }))
        .await
        .expect("advance should succeed");
    let detail = resp.into_inner().campaign.expect("campaign");

    // Response carries current (pre-advance) campaign state.
    assert_eq!(detail.name, "c");
    assert_eq!(detail.status, "active", "response must reflect active status at dispatch time");

    // `CampaignAdvanceRequested` must be broadcast through the engine path.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut saw_advance = false;
    loop {
        match tokio::time::timeout_at(deadline, rx.recv()).await {
            Ok(Ok(event)) if event.event_type == EventType::CampaignAdvanceRequested => {
                saw_advance = true;
                break;
            }
            Ok(Ok(_)) => {}
            _ => break,
        }
    }
    assert!(saw_advance, "`CampaignAdvanceRequested` must be broadcast via the engine path");
}

/// Advancing a paused campaign must return `FAILED_PRECONDITION` (invalid
/// transition).
#[tokio::test]
async fn advance_paused_campaign_returns_failed_precondition() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    write_single(&tmp_campaigns, paused_campaign("c", 10));

    let err = service
        .advance_campaign(Request::new(AdvanceCampaignRequest {
            name: "c".to_string(),
        }))
        .await
        .expect_err("advance on paused campaign must fail");
    assert_eq!(err.code(), Code::FailedPrecondition);
}

/// Advancing an escalated campaign must return `FAILED_PRECONDITION`.
#[tokio::test]
async fn advance_escalated_campaign_returns_failed_precondition() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    let mut campaign = active_campaign("c");
    campaign.status = CampaignStatus::Escalated;
    write_single(&tmp_campaigns, campaign);

    let err = service
        .advance_campaign(Request::new(AdvanceCampaignRequest {
            name: "c".to_string(),
        }))
        .await
        .expect_err("advance on escalated campaign must fail");
    assert_eq!(err.code(), Code::FailedPrecondition);
}

/// Advancing a completed campaign must return `FAILED_PRECONDITION`.
#[tokio::test]
async fn advance_completed_campaign_returns_failed_precondition() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    let mut campaign = active_campaign("c");
    campaign.status = CampaignStatus::Completed;
    write_single(&tmp_campaigns, campaign);

    let err = service
        .advance_campaign(Request::new(AdvanceCampaignRequest {
            name: "c".to_string(),
        }))
        .await
        .expect_err("advance on completed campaign must fail");
    assert_eq!(err.code(), Code::FailedPrecondition);
}

/// `AdvanceCampaign` on a missing campaign must return `NOT_FOUND`.
#[tokio::test]
async fn advance_missing_campaign_returns_not_found() {
    let (service, _tmp_campaigns, _rx, _tmp_traces) = make_service();

    let err = service
        .advance_campaign(Request::new(AdvanceCampaignRequest {
            name: "nonexistent".to_string(),
        }))
        .await
        .expect_err("missing campaign must fail");
    assert_eq!(err.code(), Code::NotFound);
}

// ── Concurrent store safety ───────────────────────────────────────────────────

/// Two lifecycle operations on distinct campaigns in the same store must both
/// persist without a lost update.  The exclusive file lock ensures each
/// read-modify-write sees the latest state; without it, the second save would
/// clobber the first (reverting the earlier campaign to Active).
#[tokio::test]
async fn concurrent_lifecycle_ops_on_different_campaigns_no_lost_update() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    write_campaigns(
        &tmp_campaigns,
        vec![active_campaign("campaign-a"), active_campaign("campaign-b")],
    );

    // Pause both campaigns.  Even if the lock causes serialization, both
    // effects must be visible in the persisted store.
    let r1 = service
        .pause_campaign(Request::new(PauseCampaignRequest {
            name: "campaign-a".to_string(),
        }))
        .await;
    let r2 = service
        .pause_campaign(Request::new(PauseCampaignRequest {
            name: "campaign-b".to_string(),
        }))
        .await;

    r1.expect("pause campaign-a must succeed");
    r2.expect("pause campaign-b must succeed");

    // Both campaigns must be Paused in the final persisted store.
    // Without the exclusive lock, the second write could reload stale state
    // (campaign-b=Active, campaign-a=Active) and clobber the first write's
    // campaign-a=Paused result.
    let store = CampaignStore::load(tmp_campaigns.path()).expect("load store");
    assert_eq!(
        store.find("campaign-a").expect("campaign-a must exist").status,
        CampaignStatus::Paused,
        "campaign-a must be Paused — without the lock this could be clobbered"
    );
    assert_eq!(
        store.find("campaign-b").expect("campaign-b must exist").status,
        CampaignStatus::Paused,
        "campaign-b must be Paused — both writes must have persisted"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_mixed_lifecycle_ops_on_distinct_campaigns_no_lost_update() {
    let (service, tmp_campaigns, _rx, _tmp_traces) = make_service();
    let mut escalated = active_campaign("campaign-c");
    escalated.status = CampaignStatus::Escalated;
    escalated.escalation = vec!["owner decision required".to_string()];
    write_campaigns(
        &tmp_campaigns,
        vec![
            active_campaign("campaign-a"),
            paused_campaign("campaign-b", 8),
            escalated,
        ],
    );

    let pause = service.pause_campaign(Request::new(PauseCampaignRequest {
        name: "campaign-a".to_string(),
    }));
    let resume = service.resume_campaign(Request::new(ResumeCampaignRequest {
        name: "campaign-b".to_string(),
        add_cycles: 2,
    }));
    let decide = service.decide_campaign(Request::new(foundryd::proto::DecideCampaignRequest {
        name: "campaign-c".to_string(),
        decision: "Use the daemon boundary.".to_string(),
    }));

    let (pause_result, resume_result, decide_result) = tokio::join!(pause, resume, decide);
    pause_result.expect("pause must succeed");
    resume_result.expect("resume must succeed");
    decide_result.expect("decide must succeed");

    let store = CampaignStore::load(tmp_campaigns.path()).expect("load store");

    let campaign_a = store.find("campaign-a").expect("campaign-a exists");
    assert_eq!(campaign_a.status, CampaignStatus::Paused);

    let campaign_b = store.find("campaign-b").expect("campaign-b exists");
    assert_eq!(campaign_b.status, CampaignStatus::Active);
    assert_eq!(campaign_b.budget.max_cycles, 10);

    let campaign_c = store.find("campaign-c").expect("campaign-c exists");
    assert_eq!(campaign_c.status, CampaignStatus::Active);
    assert_eq!(campaign_c.owner_decisions.len(), 1);
    assert_eq!(campaign_c.owner_decisions[0].decision, "Use the daemon boundary.");
}
