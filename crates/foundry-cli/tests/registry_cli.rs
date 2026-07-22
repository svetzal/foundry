//! Integration tests for `foundry registry` CLI behavior.

use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_cli::registry_commands;
use foundry_engine::engine::Engine;
use foundry_sdk::registry::{
    ActionFlags, ProjectEdits, ProjectEntry, ProjectSpec, Registry, Stack,
};
use foundry_sdk::sentinel::SentinelStore;
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

/// Write an empty but valid registry JSON file and return the temp file handle.
///
/// An empty `NamedTempFile` contains zero bytes, which is not valid JSON.
/// The offline path calls `Registry::load` on the path when the file exists,
/// so we must initialise it first.
fn init_registry() -> NamedTempFile {
    let tmp = NamedTempFile::new().expect("tempfile");
    let reg = Registry {
        version: 2,
        projects: vec![],
    };
    reg.save(tmp.path()).expect("save initial empty registry");
    tmp
}

fn daemon_project(name: &str) -> ProjectEntry {
    ProjectEntry {
        name: name.to_string(),
        path: format!("/srv/{name}"),
        stack: Stack::Rust,
        agent: "claude".to_string(),
        repo: format!("daemon/{name}"),
        branch: "main".to_string(),
        skip: None,
        actions: ActionFlags::default(),
        install: None,
        installs_skill: None,
        notes: Some(format!("notes from daemon for {name}")),
        timeout_secs: Some(42),
        audit_exceptions: vec![],
    }
}

fn simple_spec(name: &str, path: &str, stack: Stack) -> ProjectSpec {
    ProjectSpec {
        name: name.to_string(),
        path: path.to_string(),
        stack,
        agent: "claude".to_string(),
        repo: format!("o/{name}"),
        branch: "main".to_string(),
        iterate: false,
        maintain: false,
        push: false,
        audit: false,
        release: false,
        install_command: None,
        install_brew: None,
        notes: None,
        timeout_secs: None,
    }
}

// The addr string is passed to `FoundryClient::connect` only when `offline`
// is false.  With `offline = true` the value is never used; we still pass
// something realistic to keep the tests self-documenting.
const DUMMY_ADDR: &str = "http://127.0.0.1:9";

fn make_service_with_registry(
    registry: Registry,
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
    let registry = Arc::new(RwLock::new(registry));

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

    (FoundryService::new(ctx, stores), tmp_traces)
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
    registry_path: &std::path::Path,
    addr: &str,
    args: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_foundry"))
        .arg("--addr")
        .arg(addr)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("FOUNDRY_REGISTRY_PATH", registry_path)
        .output()
        .expect("run foundry binary")
}

fn seed_client_registry_trap(home: &std::path::Path) -> (std::path::PathBuf, Vec<u8>) {
    let registry_path = home.join(".foundry/registry.json");
    std::fs::create_dir_all(
        registry_path.parent().expect("client registry path must have a parent"),
    )
    .expect("create client registry parent");
    let bytes = br"not valid json and must stay untouched".to_vec();
    std::fs::write(&registry_path, &bytes).expect("seed client registry trap bytes");
    (registry_path, bytes)
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_offline_writes_project_to_file() {
    let tmp = init_registry();

    let spec = ProjectSpec {
        name: "test-proj".to_string(),
        path: "/tmp/test-proj".to_string(),
        stack: Stack::Rust,
        agent: "claude".to_string(),
        repo: "owner/test-proj".to_string(),
        branch: "main".to_string(),
        iterate: true,
        maintain: false,
        push: false,
        audit: false,
        release: false,
        install_command: None,
        install_brew: None,
        notes: None,
        timeout_secs: None,
    };

    registry_commands::add(tmp.path(), DUMMY_ADDR, true, spec)
        .await
        .expect("add offline should succeed");

    let registry = Registry::load(tmp.path()).expect("registry must be readable");
    assert_eq!(registry.projects.len(), 1);
    let p = &registry.projects[0];
    assert_eq!(p.name, "test-proj");
    assert_eq!(p.path, "/tmp/test-proj");
    assert_eq!(p.stack.to_string(), "rust");
    assert_eq!(p.agent, "claude");
    assert_eq!(p.repo, "owner/test-proj");
    assert_eq!(p.branch, "main");
    assert!(p.actions.iterate, "iterate flag should be true");
    assert!(!p.actions.maintain, "maintain flag should be false");
}

#[tokio::test]
async fn add_offline_duplicate_returns_error() {
    let tmp = init_registry();

    let do_add = || {
        registry_commands::add(
            tmp.path(),
            DUMMY_ADDR,
            true,
            simple_spec("alpha", "/tmp/alpha", Stack::Python),
        )
    };

    do_add().await.expect("first add should succeed");
    let err = do_add().await;
    assert!(err.is_err(), "duplicate project should return an error");
    assert!(
        err.unwrap_err().to_string().contains("already exists"),
        "error message should mention 'already exists'"
    );
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

#[tokio::test]
async fn remove_offline_deletes_project_from_file() {
    let tmp = init_registry();

    registry_commands::add(
        tmp.path(),
        DUMMY_ADDR,
        true,
        simple_spec("to-remove", "/tmp/to-remove", Stack::Python),
    )
    .await
    .expect("add should succeed");

    registry_commands::remove(tmp.path(), DUMMY_ADDR, true, "to-remove")
        .await
        .expect("remove offline should succeed");

    let registry = Registry::load(tmp.path()).expect("registry must be readable");
    assert!(registry.projects.is_empty(), "registry should be empty after remove");
}

#[tokio::test]
async fn remove_offline_nonexistent_returns_error() {
    let tmp = init_registry();

    let result = registry_commands::remove(tmp.path(), DUMMY_ADDR, true, "ghost").await;
    assert!(result.is_err(), "removing nonexistent project should fail");
    assert!(
        result.unwrap_err().to_string().contains("not found"),
        "error message should mention 'not found'"
    );
}

// ---------------------------------------------------------------------------
// edit
// ---------------------------------------------------------------------------

#[tokio::test]
async fn edit_offline_updates_branch() {
    let tmp = init_registry();

    registry_commands::add(
        tmp.path(),
        DUMMY_ADDR,
        true,
        simple_spec("editable", "/tmp/editable", Stack::TypeScript),
    )
    .await
    .expect("add should succeed");

    let edits = ProjectEdits {
        branch: Some("develop".to_string()),
        ..Default::default()
    };
    registry_commands::edit(tmp.path(), DUMMY_ADDR, true, "editable", edits)
        .await
        .expect("edit offline should succeed");

    let registry = Registry::load(tmp.path()).expect("registry must be readable");
    assert_eq!(registry.projects[0].branch, "develop", "branch should be updated");
    assert_eq!(registry.projects[0].name, "editable", "name should be unchanged");
}

#[tokio::test]
async fn edit_offline_nonexistent_returns_error() {
    let tmp = init_registry();

    let edits = ProjectEdits {
        branch: Some("develop".to_string()),
        ..Default::default()
    };
    let result = registry_commands::edit(tmp.path(), DUMMY_ADDR, true, "ghost", edits).await;

    assert!(result.is_err(), "editing nonexistent project should fail");
    assert!(
        result.unwrap_err().to_string().contains("not found"),
        "error message should mention 'not found'"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn online_list_reads_daemon_registry_without_creating_client_registry_file() {
    let daemon_registry = Registry {
        version: 2,
        projects: vec![daemon_project("server-only")],
    };
    let daemon_registry_file = NamedTempFile::new().expect("daemon registry tempfile");
    daemon_registry.save(daemon_registry_file.path()).expect("save daemon registry");
    let (service, _tmp_traces) = make_service_with_registry(
        daemon_registry.clone(),
        daemon_registry_file.path().to_path_buf(),
    );
    let addr = start_server(service).await;

    let client_home = tempfile::tempdir().expect("client home");
    let (client_registry, before) = seed_client_registry_trap(client_home.path());
    let output = run_foundry(client_home.path(), &client_registry, &addr, &["registry", "list"]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("server-only"));
    assert!(
        std::fs::read(&client_registry).expect("read client trap bytes after online list")
            == before,
        "online registry list must not read or mutate the client-side registry file"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn online_show_reads_exact_daemon_fields_without_client_registry_file() {
    let daemon_registry = Registry {
        version: 2,
        projects: vec![daemon_project("server-only")],
    };
    let daemon_registry_file = NamedTempFile::new().expect("daemon registry tempfile");
    daemon_registry.save(daemon_registry_file.path()).expect("save daemon registry");
    let (service, _tmp_traces) =
        make_service_with_registry(daemon_registry, daemon_registry_file.path().to_path_buf());
    let addr = start_server(service).await;

    let client_home = tempfile::tempdir().expect("client home");
    let (client_registry, before) = seed_client_registry_trap(client_home.path());
    let output = run_foundry(
        client_home.path(),
        &client_registry,
        &addr,
        &["registry", "show", "server-only"],
    );

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Name:      server-only"));
    assert!(stdout.contains("Path:      /srv/server-only"));
    assert!(stdout.contains("Repo:      daemon/server-only"));
    assert!(stdout.contains("Notes:     notes from daemon for server-only"));
    assert!(
        std::fs::read(&client_registry).expect("read client trap bytes after online show")
            == before,
        "online registry show must not read or mutate the client-side registry file"
    );
}

#[tokio::test]
async fn online_add_unreachable_daemon_leaves_client_registry_byte_identical() {
    let tmp = init_registry();
    let before = std::fs::read(tmp.path()).expect("read seeded registry");

    let err = registry_commands::add(
        tmp.path(),
        DUMMY_ADDR,
        false,
        simple_spec("alpha", "/tmp/alpha", Stack::Rust),
    )
    .await
    .expect_err("unreachable daemon must fail");

    assert_eq!(
        err.to_string(),
        "foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry add --name alpha --offline`"
    );
    let after = std::fs::read(tmp.path()).expect("read registry after failure");
    assert_eq!(after, before);
}

#[tokio::test]
async fn online_remove_unreachable_daemon_leaves_client_registry_byte_identical() {
    let tmp = init_registry();
    registry_commands::add(
        tmp.path(),
        DUMMY_ADDR,
        true,
        simple_spec("alpha", "/tmp/alpha", Stack::Rust),
    )
    .await
    .expect("seed offline add");
    let before = std::fs::read(tmp.path()).expect("read seeded registry");

    let err = registry_commands::remove(tmp.path(), DUMMY_ADDR, false, "alpha")
        .await
        .expect_err("unreachable daemon must fail");

    assert_eq!(
        err.to_string(),
        "foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry remove alpha --offline`"
    );
    let after = std::fs::read(tmp.path()).expect("read registry after failure");
    assert_eq!(after, before);
}

#[tokio::test]
async fn online_edit_unreachable_daemon_leaves_client_registry_byte_identical() {
    let tmp = init_registry();
    registry_commands::add(
        tmp.path(),
        DUMMY_ADDR,
        true,
        simple_spec("alpha", "/tmp/alpha", Stack::Rust),
    )
    .await
    .expect("seed offline add");
    let before = std::fs::read(tmp.path()).expect("read seeded registry");

    let err = registry_commands::edit(
        tmp.path(),
        DUMMY_ADDR,
        false,
        "alpha",
        ProjectEdits {
            branch: Some("develop".to_string()),
            ..Default::default()
        },
    )
    .await
    .expect_err("unreachable daemon must fail");

    assert_eq!(
        err.to_string(),
        "foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry edit alpha --offline`"
    );
    let after = std::fs::read(tmp.path()).expect("read registry after failure");
    assert_eq!(after, before);
}

#[test]
fn init_requires_explicit_offline_recovery_flag() {
    let tmp = NamedTempFile::new().expect("registry tempfile");
    std::fs::remove_file(tmp.path()).expect("remove empty tempfile path");

    let err = registry_commands::init(tmp.path(), false).expect_err("online init must fail");

    assert_eq!(
        err.to_string(),
        "`foundry registry init` is an offline recovery command; rerun with `--offline`"
    );
    assert!(!tmp.path().exists());
}

#[test]
fn offline_init_creates_recovery_registry_file() {
    let tmp = NamedTempFile::new().expect("registry tempfile");
    std::fs::remove_file(tmp.path()).expect("remove empty tempfile path");

    registry_commands::init(tmp.path(), true).expect("offline init should succeed");

    let registry = Registry::load(tmp.path()).expect("load created registry");
    assert_eq!(registry.version, 2);
    assert!(registry.projects.is_empty());
}
