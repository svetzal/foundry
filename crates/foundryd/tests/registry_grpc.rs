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
use foundry_engine::engine::Engine;
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundryd::{
    proto::{
        RegistryAddRequest, RegistryEditRequest, RegistryListRequest, RegistryRemoveRequest,
        RegistryShowRequest, foundry_client::FoundryClient, foundry_server::Foundry,
        foundry_server::FoundryServer,
    },
    service::{FoundryService, RuntimeContext, StoreConfig},
    trace_store::TraceStore,
    workflow_tracker::WorkflowTracker,
};
use tempfile::{NamedTempFile, TempDir};
use tokio::sync::{Notify, broadcast};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tonic::{Code, Request};

/// Construct a minimal `FoundryService` with temporary backing files.
///
/// Returns the service, a temp file holding the registry JSON, and a temp
/// directory for trace files.  The caller must keep all three alive for the
/// duration of the test.
fn make_service() -> (FoundryService, NamedTempFile, TempDir) {
    make_service_with(Registry {
        version: 2,
        projects: vec![],
    })
}

fn make_service_with(registry_data: Registry) -> (FoundryService, NamedTempFile, TempDir) {
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
    let registry = Arc::new(RwLock::new(registry_data));

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

    (service, tmp_registry, tmp_traces)
}

fn make_service_with_registry_path(
    registry_data: Registry,
    registry_path: std::path::PathBuf,
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
    let registry = Arc::new(RwLock::new(registry_data));

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

    (service, tmp_traces)
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

fn seeded_registry(name: &str) -> Registry {
    Registry {
        version: 2,
        projects: vec![read_registry_entry(name)],
    }
}

fn read_registry_entry(name: &str) -> foundry_sdk::registry::ProjectEntry {
    foundry_sdk::registry::ProjectEntry {
        name: name.to_string(),
        path: format!("/tmp/{name}"),
        stack: foundry_sdk::registry::Stack::Rust,
        agent: "claude".to_string(),
        repo: format!("owner/{name}"),
        branch: "main".to_string(),
        skip: None,
        actions: foundry_sdk::registry::ActionFlags {
            iterate: true,
            maintain: false,
            push: true,
            audit: false,
            release: false,
        },
        install: None,
        installs_skill: None,
        notes: Some("seed note".to_string()),
        timeout_secs: Some(60),
        audit_exceptions: vec![],
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

#[tokio::test]
async fn list_returns_full_daemon_owned_project_fields() {
    let (service, _tmp_registry, _tmp_traces) = make_service();

    service
        .registry_add(Request::new(RegistryAddRequest {
            name: "alpha".to_string(),
            path: "/srv/alpha".to_string(),
            stack: "rust".to_string(),
            agent: "claude".to_string(),
            repo: "daemon/alpha".to_string(),
            branch: "main".to_string(),
            iterate: true,
            maintain: true,
            push: false,
            audit: true,
            release: false,
            install_command: String::new(),
            install_brew: String::new(),
            notes: "server note".to_string(),
            timeout_secs: 45,
        }))
        .await
        .expect("seed daemon registry");

    let response = service
        .registry_list(Request::new(foundryd::proto::RegistryListRequest {}))
        .await
        .expect("registry_list should succeed")
        .into_inner();

    assert_eq!(response.projects.len(), 1);
    let project = &response.projects[0];
    assert_eq!(project.name, "alpha");
    assert_eq!(project.path, "/srv/alpha");
    assert_eq!(project.repo, "daemon/alpha");
    assert_eq!(project.notes, "server note");
    assert_eq!(project.timeout_secs, 45);
    assert_eq!(project.branch, "main");
    assert!(project.iterate);
    assert!(project.maintain);
    assert!(project.audit);
}

#[tokio::test]
async fn show_returns_full_daemon_owned_project_fields() {
    let (service, _tmp_registry, _tmp_traces) = make_service();

    service
        .registry_add(Request::new(RegistryAddRequest {
            name: "alpha".to_string(),
            path: "/srv/alpha".to_string(),
            stack: "rust".to_string(),
            agent: "claude".to_string(),
            repo: "daemon/alpha".to_string(),
            branch: "develop".to_string(),
            iterate: false,
            maintain: true,
            push: true,
            audit: false,
            release: true,
            install_command: "./install.sh".to_string(),
            install_brew: String::new(),
            notes: "server note".to_string(),
            timeout_secs: 90,
        }))
        .await
        .expect("seed daemon registry");

    let project = service
        .registry_show(Request::new(foundryd::proto::RegistryShowRequest {
            name: "alpha".to_string(),
        }))
        .await
        .expect("registry_show should succeed")
        .into_inner()
        .project
        .expect("project payload");

    assert_eq!(project.name, "alpha");
    assert_eq!(project.path, "/srv/alpha");
    assert_eq!(project.agent, "claude");
    assert_eq!(project.repo, "daemon/alpha");
    assert_eq!(project.branch, "develop");
    assert_eq!(project.notes, "server note");
    assert_eq!(project.timeout_secs, 90);
    assert_eq!(project.install_command, "./install.sh");
    assert!(project.maintain);
    assert!(project.push);
    assert!(project.release);
}

#[tokio::test]
async fn generated_client_list_and_show_round_trip_exact_fields() {
    let (service, _tmp_registry, _tmp_traces) = make_service_with(seeded_registry("alpha"));
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let list = client
        .registry_list(RegistryListRequest {})
        .await
        .expect("registry_list should succeed")
        .into_inner();
    assert_eq!(list.projects.len(), 1);
    let listed = &list.projects[0];
    assert_eq!(listed.name, "alpha");
    assert_eq!(listed.path, "/tmp/alpha");
    assert_eq!(listed.repo, "owner/alpha");
    assert_eq!(listed.notes, "seed note");
    assert!(listed.iterate);
    assert!(listed.push);

    let shown = client
        .registry_show(RegistryShowRequest {
            name: "alpha".to_string(),
        })
        .await
        .expect("registry_show should succeed")
        .into_inner()
        .project
        .expect("project payload");
    assert_eq!(shown.name, "alpha");
    assert_eq!(shown.path, "/tmp/alpha");
    assert_eq!(shown.repo, "owner/alpha");
    assert_eq!(shown.notes, "seed note");
    assert_eq!(shown.branch, "main");
}

#[tokio::test]
async fn generated_client_registry_add_persist_failure_returns_internal_without_path_and_no_mutation()
 {
    let unreadable_dir = tempfile::tempdir().expect("tempdir for unreadable registry path");
    let registry_path = unreadable_dir.path().to_path_buf();
    let (service, _tmp_traces) = make_service_with_registry_path(
        Registry {
            version: 2,
            projects: vec![],
        },
        registry_path.clone(),
    );
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .registry_add(add_request("alpha"))
        .await
        .expect_err("registry_add should fail when registry path is unreadable");
    assert_eq!(err.code(), Code::Internal);
    assert_eq!(err.message(), "failed to persist registry state");
    assert!(
        !err.message().contains(registry_path.to_string_lossy().as_ref()),
        "status message must not leak the registry path"
    );

    let list = client
        .registry_list(RegistryListRequest {})
        .await
        .expect("registry_list after failed add should succeed")
        .into_inner();
    assert!(
        list.projects.is_empty(),
        "failed add must not mutate daemon-owned in-memory registry state"
    );
}

#[tokio::test]
async fn generated_client_registry_edit_persist_failure_returns_internal_without_path_and_no_mutation()
 {
    let unreadable_dir = tempfile::tempdir().expect("tempdir for unreadable registry path");
    let registry_path = unreadable_dir.path().to_path_buf();
    let (service, _tmp_traces) =
        make_service_with_registry_path(seeded_registry("alpha"), registry_path.clone());
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .registry_edit(edit_branch_request("alpha", "develop"))
        .await
        .expect_err("registry_edit should fail when registry path is unreadable");
    assert_eq!(err.code(), Code::Internal);
    assert_eq!(err.message(), "failed to persist registry state");
    assert!(
        !err.message().contains(registry_path.to_string_lossy().as_ref()),
        "status message must not leak the registry path"
    );

    let shown = client
        .registry_show(RegistryShowRequest {
            name: "alpha".to_string(),
        })
        .await
        .expect("registry_show after failed edit should succeed")
        .into_inner()
        .project
        .expect("project payload");
    assert_eq!(
        shown.branch, "main",
        "failed edit must leave daemon-owned in-memory registry state unchanged"
    );
}

#[tokio::test]
async fn generated_client_registry_remove_persist_failure_returns_internal_without_path_and_no_mutation()
 {
    let unreadable_dir = tempfile::tempdir().expect("tempdir for unreadable registry path");
    let registry_path = unreadable_dir.path().to_path_buf();
    let (service, _tmp_traces) =
        make_service_with_registry_path(seeded_registry("alpha"), registry_path.clone());
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .registry_remove(RegistryRemoveRequest {
            name: "alpha".to_string(),
        })
        .await
        .expect_err("registry_remove should fail when registry path is unreadable");
    assert_eq!(err.code(), Code::Internal);
    assert_eq!(err.message(), "failed to persist registry state");
    assert!(
        !err.message().contains(registry_path.to_string_lossy().as_ref()),
        "status message must not leak the registry path"
    );

    let shown = client
        .registry_show(RegistryShowRequest {
            name: "alpha".to_string(),
        })
        .await
        .expect("registry_show after failed remove should succeed")
        .into_inner()
        .project
        .expect("project payload");
    assert_eq!(
        shown.name, "alpha",
        "failed remove must leave daemon-owned in-memory registry state unchanged"
    );
}

#[tokio::test]
async fn generated_client_registry_edit_missing_project_returns_exact_not_found_status() {
    let (service, _tmp_registry, _tmp_traces) = make_service();
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .registry_edit(edit_branch_request("ghost", "develop"))
        .await
        .expect_err("editing a missing project must fail");

    assert_eq!(err.code(), Code::NotFound);
    assert_eq!(err.message(), "project 'ghost' not found");
}

#[tokio::test]
async fn generated_client_registry_remove_missing_project_returns_exact_not_found_status() {
    let (service, _tmp_registry, _tmp_traces) = make_service();
    let addr = start_server(service).await;

    let mut client = FoundryClient::connect(addr).await.expect("connect");
    let err = client
        .registry_remove(RegistryRemoveRequest {
            name: "ghost".to_string(),
        })
        .await
        .expect_err("removing a missing project must fail");

    assert_eq!(err.code(), Code::NotFound);
    assert_eq!(err.message(), "project 'ghost' not found");
}
