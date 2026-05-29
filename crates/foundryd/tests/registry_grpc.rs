//! Integration tests for the registry gRPC handlers.
//!
//! These tests exercise `FoundryService::registry_add`, `registry_edit`, and
//! `registry_remove` end-to-end: they construct a real `FoundryService` with a
//! temporary registry file, call the handlers directly (bypassing the TCP
//! transport layer), and assert that both the in-memory registry state and the
//! on-disk JSON file are updated correctly after each operation.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_core::registry::Registry;
use foundry_core::sentinel::SentinelStore;
use foundry_engine::engine::Engine;
use foundryd::{
    proto::{
        RegistryAddRequest, RegistryEditRequest, RegistryRemoveRequest, foundry_server::Foundry,
    },
    service::FoundryService,
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tonic::{Code, Request};

/// Construct a minimal `FoundryService` with temporary backing files.
///
/// Returns the service, a temp file holding the registry JSON, and a temp
/// directory for trace files.  The caller must keep all three alive for the
/// duration of the test.
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

    let sentinels = Arc::new(RwLock::new(SentinelStore::default_seed()));
    let tmp_sentinels = NamedTempFile::new().expect("tempfile for sentinels");
    let sentinels_path = tmp_sentinels.path().to_path_buf();
    let scheduler_reload = Arc::new(Notify::new());

    let service = FoundryService::new(
        engine,
        trace_store,
        event_tx,
        workflow_tracker,
        trace_writer,
        registry,
        registry_path,
        sentinels,
        sentinels_path,
        scheduler_reload,
    );

    (service, tmp_registry, tmp_traces)
}

/// Read and deserialise the registry JSON written to disk by the service.
///
/// Returns an empty registry if the file does not yet exist or cannot be
/// parsed — this keeps test assertions clean when verifying "nothing on disk".
fn read_registry(tmp: &NamedTempFile) -> Registry {
    if tmp.path().exists() {
        Registry::load(tmp.path()).unwrap_or(Registry {
            version: 2,
            projects: vec![],
        })
    } else {
        Registry {
            version: 2,
            projects: vec![],
        }
    }
}

/// Helper: a fully-populated `RegistryAddRequest` for a named project.
fn add_request(name: &str) -> RegistryAddRequest {
    RegistryAddRequest {
        name: name.to_string(),
        path: format!("/tmp/{name}"),
        stack: "rust".to_string(),
        agent: "claude".to_string(),
        repo: format!("owner/{name}"),
        branch: "main".to_string(),
        iterate: true,
        maintain: false,
        push: true,
        audit: false,
        release: false,
        install_command: String::new(),
        install_brew: String::new(),
        notes: String::new(),
        timeout_secs: 0,
    }
}

/// Helper: a `RegistryEditRequest` that changes only the `branch` field.
fn edit_branch_request(name: &str, branch: &str) -> RegistryEditRequest {
    RegistryEditRequest {
        name: name.to_string(),
        branch: branch.to_string(),
        // All other sentinel/value fields left at proto defaults (empty / false).
        path: String::new(),
        stack: String::new(),
        agent: String::new(),
        repo: String::new(),
        skip: String::new(),
        clear_skip: false,
        iterate: false,
        clear_iterate: false,
        maintain: false,
        clear_maintain: false,
        push: false,
        clear_push: false,
        audit: false,
        clear_audit: false,
        release: false,
        clear_release: false,
        install_command: String::new(),
        install_brew: String::new(),
        clear_install: false,
        notes: String::new(),
        clear_notes: false,
        timeout_secs: 0,
        clear_timeout: false,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_edit_remove_round_trip() {
    let (service, tmp_registry, _tmp_traces) = make_service();

    // --- Add -----------------------------------------------------------------
    let add_resp = service
        .registry_add(Request::new(add_request("round-trip")))
        .await
        .expect("registry_add should succeed");

    let added = add_resp.into_inner().project.expect("project in add response");
    assert_eq!(added.name, "round-trip");
    assert_eq!(added.branch, "main");
    assert_eq!(added.stack, "rust");
    assert!(added.iterate, "iterate flag should be true");
    assert!(!added.maintain, "maintain flag should be false");

    // Verify disk persistence after Add.
    let disk = read_registry(&tmp_registry);
    assert_eq!(disk.projects.len(), 1, "one project should be on disk after add");
    assert_eq!(disk.projects[0].name, "round-trip");
    assert_eq!(disk.projects[0].branch, "main");

    // --- Edit (change branch) ------------------------------------------------
    let edit_resp = service
        .registry_edit(Request::new(edit_branch_request("round-trip", "develop")))
        .await
        .expect("registry_edit should succeed");

    let edited = edit_resp.into_inner().project.expect("project in edit response");
    assert_eq!(edited.branch, "develop", "branch should be updated to develop");
    assert_eq!(edited.name, "round-trip", "name must be unchanged");
    assert_eq!(edited.stack, "rust", "stack must be unchanged");
    assert!(edited.iterate, "iterate flag must be preserved");

    // Verify disk persistence after Edit.
    let disk = read_registry(&tmp_registry);
    assert_eq!(disk.projects[0].branch, "develop", "disk branch should be updated");

    // --- Remove --------------------------------------------------------------
    service
        .registry_remove(Request::new(RegistryRemoveRequest {
            name: "round-trip".to_string(),
        }))
        .await
        .expect("registry_remove should succeed");

    let disk = read_registry(&tmp_registry);
    assert!(disk.projects.is_empty(), "disk should be empty after remove");

    // --- Remove again → NotFound ---------------------------------------------
    let err = service
        .registry_remove(Request::new(RegistryRemoveRequest {
            name: "round-trip".to_string(),
        }))
        .await
        .expect_err("second remove should return an error");

    assert_eq!(
        err.code(),
        Code::NotFound,
        "second remove should return NotFound, not {}",
        err.code()
    );
}

#[tokio::test]
async fn add_duplicate_returns_already_exists() {
    let (service, _tmp_registry, _tmp_traces) = make_service();

    service
        .registry_add(Request::new(add_request("alpha")))
        .await
        .expect("first add should succeed");

    let err = service
        .registry_add(Request::new(add_request("alpha")))
        .await
        .expect_err("duplicate add should fail");

    assert_eq!(err.code(), Code::AlreadyExists, "duplicate add should return AlreadyExists");
}

#[tokio::test]
async fn edit_nonexistent_returns_not_found() {
    let (service, _tmp_registry, _tmp_traces) = make_service();

    let err = service
        .registry_edit(Request::new(edit_branch_request("ghost", "main")))
        .await
        .expect_err("editing nonexistent project should fail");

    assert_eq!(err.code(), Code::NotFound, "should return NotFound for missing project");
}

#[tokio::test]
async fn add_with_invalid_stack_returns_invalid_argument() {
    let (service, _tmp_registry, _tmp_traces) = make_service();

    let mut req = add_request("bad-stack");
    req.stack = "cobol".to_string();

    let err = service
        .registry_add(Request::new(req))
        .await
        .expect_err("invalid stack should fail");

    assert_eq!(err.code(), Code::InvalidArgument, "invalid stack should return InvalidArgument");
}
