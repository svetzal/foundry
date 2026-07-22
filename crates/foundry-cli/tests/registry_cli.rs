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

fn daemon_project_with_actions(name: &str) -> ProjectEntry {
    ProjectEntry {
        actions: ActionFlags {
            iterate: true,
            maintain: true,
            push: false,
            audit: true,
            release: false,
        },
        ..daemon_project(name)
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

fn missing_client_registry_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".foundry/registry.json")
}

fn stdout_string(output: &std::process::Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout must be valid UTF-8")
}

fn stderr_string(output: &std::process::Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr must be valid UTF-8")
}

fn assert_command_succeeded(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        stdout_string(output),
        stderr_string(output)
    );
}

fn seed_offline_registry(home: &std::path::Path) -> std::path::PathBuf {
    let registry_path = home.join(".foundry/registry.json");
    std::fs::create_dir_all(
        registry_path.parent().expect("registry path should have a parent directory"),
    )
    .expect("create registry parent");
    Registry {
        version: 2,
        projects: vec![ProjectEntry {
            name: "offline-seeded".to_string(),
            path: "/offline/seeded".to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: "offline/seeded".to_string(),
            branch: "main".to_string(),
            skip: None,
            actions: ActionFlags::default(),
            install: None,
            installs_skill: None,
            notes: Some("seeded directly".to_string()),
            timeout_secs: Some(90),
            audit_exceptions: vec![],
        }],
    }
    .save(&registry_path)
    .expect("save initial offline registry");
    registry_path
}

fn assert_offline_cli_list_and_show(
    client_home: &std::path::Path,
    registry_path: &std::path::Path,
) {
    let list_output =
        run_foundry(client_home, registry_path, DUMMY_ADDR, &["--offline", "registry", "list"]);
    assert_command_succeeded(&list_output);
    assert!(stdout_string(&list_output).contains("offline-seeded"));

    let show_output = run_foundry(
        client_home,
        registry_path,
        DUMMY_ADDR,
        &["--offline", "registry", "show", "offline-seeded"],
    );
    assert_command_succeeded(&show_output);
    let show_stdout = stdout_string(&show_output);
    assert!(show_stdout.contains("Name:      offline-seeded"));
    assert!(show_stdout.contains("Path:      /offline/seeded"));
    assert!(show_stdout.contains("Repo:      offline/seeded"));
    assert!(show_stdout.contains("Notes:     seeded directly"));
}

fn run_offline_cli_mutations(client_home: &std::path::Path, registry_path: &std::path::Path) {
    let add_output = run_foundry(
        client_home,
        registry_path,
        DUMMY_ADDR,
        &[
            "--offline",
            "registry",
            "add",
            "--name",
            "offline-added",
            "--path",
            "/offline/added",
            "--stack",
            "rust",
            "--agent",
            "claude",
            "--repo",
            "offline/added",
            "--branch",
            "main",
            "--notes",
            "added via offline cli",
        ],
    );
    assert_command_succeeded(&add_output);

    let edit_output = run_foundry(
        client_home,
        registry_path,
        DUMMY_ADDR,
        &[
            "--offline",
            "registry",
            "edit",
            "offline-added",
            "--branch",
            "develop",
            "--notes",
            "edited via offline cli",
        ],
    );
    assert_command_succeeded(&edit_output);

    let remove_output = run_foundry(
        client_home,
        registry_path,
        DUMMY_ADDR,
        &["--offline", "registry", "remove", "offline-seeded"],
    );
    assert_command_succeeded(&remove_output);
}

fn assert_offline_registry_final_state(registry_path: &std::path::Path) {
    let registry = Registry::load(registry_path).expect("load registry after offline CLI flow");
    assert_eq!(registry.projects.len(), 1, "exactly one project should remain");
    let project = &registry.projects[0];
    assert_eq!(project.name, "offline-added");
    assert_eq!(project.path, "/offline/added");
    assert_eq!(project.repo, "offline/added");
    assert_eq!(project.branch, "develop");
    assert_eq!(project.notes.as_deref(), Some("edited via offline cli"));
}

fn assert_registry_file_absent(path: &std::path::Path, context: &str) {
    assert!(
        !path.exists(),
        "{context}: expected client registry file to remain absent at {}",
        path.display()
    );
}

fn assert_stdout_contains(output: &std::process::Output, needle: &str, context: &str) {
    assert!(
        stdout_string(output).contains(needle),
        "{context}\nstdout: {}\nstderr: {}",
        stdout_string(output),
        stderr_string(output)
    );
}

fn assert_daemon_project_fields(
    registry_path: &std::path::Path,
    name: &str,
    path: &str,
    repo: &str,
    notes: &str,
) {
    let daemon_registry = Registry::load(registry_path).expect("load daemon registry");
    let project = daemon_registry
        .projects
        .iter()
        .find(|project| project.name == name)
        .expect("daemon registry should contain expected project");
    assert_eq!(project.path, path);
    assert_eq!(project.repo, repo);
    assert_eq!(project.notes.as_deref(), Some(notes));
}

fn run_online_registry_show(
    client_home: &std::path::Path,
    client_registry: &std::path::Path,
    addr: &str,
    name: &str,
) -> std::process::Output {
    run_foundry(client_home, client_registry, addr, &["registry", "show", name])
}

fn assert_show_displays_exact_fields(
    output: &std::process::Output,
    name: &str,
    path: &str,
    repo: &str,
    notes: &str,
) {
    assert_command_succeeded(output);
    let stdout = stdout_string(output);
    assert!(stdout.contains(&format!("Name:      {name}")));
    assert!(stdout.contains(&format!("Path:      {path}")));
    assert!(stdout.contains(&format!("Repo:      {repo}")));
    assert!(stdout.contains(&format!("Notes:     {notes}")));
}

fn assert_show_displays_exact_fields_and_actions(
    output: &std::process::Output,
    name: &str,
    path: &str,
    repo: &str,
    notes: &str,
    actions: &str,
) {
    assert_show_displays_exact_fields(output, name, path, repo, notes);
    assert!(stdout_string(output).contains(&format!("Actions:   {actions}")));
}

fn assert_online_unreachable_keeps_client_registry_absent(
    client_home: &std::path::Path,
    args: &[&str],
    expected_stderr: &str,
    context: &str,
) {
    let client_registry = missing_client_registry_path(client_home);
    assert_registry_file_absent(&client_registry, context);

    let output = run_foundry(client_home, &client_registry, DUMMY_ADDR, args);

    assert!(!output.status.success(), "{context}: command should fail");
    assert_eq!(stderr_string(&output).trim(), expected_stderr);
    assert_registry_file_absent(&client_registry, context);
}

fn run_online_registry_add(
    client_home: &std::path::Path,
    client_registry: &std::path::Path,
    addr: &str,
) -> std::process::Output {
    run_foundry(
        client_home,
        client_registry,
        addr,
        &[
            "registry",
            "add",
            "--name",
            "alpha",
            "--path",
            "/srv/alpha",
            "--stack",
            "rust",
            "--agent",
            "claude",
            "--repo",
            "daemon/alpha",
            "--branch",
            "main",
            "--notes",
            "daemon-owned add",
        ],
    )
}

fn run_online_registry_edit(
    client_home: &std::path::Path,
    client_registry: &std::path::Path,
    addr: &str,
) -> std::process::Output {
    run_foundry(
        client_home,
        client_registry,
        addr,
        &[
            "registry",
            "edit",
            "alpha",
            "--path",
            "/srv/alpha-edited",
            "--repo",
            "daemon/alpha-edited",
            "--notes",
            "daemon-owned edit",
        ],
    )
}

fn run_online_registry_remove(
    client_home: &std::path::Path,
    client_registry: &std::path::Path,
    addr: &str,
) -> std::process::Output {
    run_foundry(client_home, client_registry, addr, &["registry", "remove", "alpha"])
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
        projects: vec![daemon_project_with_actions("server-only")],
    };
    let daemon_registry_file = NamedTempFile::new().expect("daemon registry tempfile");
    daemon_registry.save(daemon_registry_file.path()).expect("save daemon registry");
    let (service, _tmp_traces) = make_service_with_registry(
        daemon_registry.clone(),
        daemon_registry_file.path().to_path_buf(),
    );
    let addr = start_server(service).await;

    let client_home = tempfile::tempdir().expect("client home");
    let client_registry = missing_client_registry_path(client_home.path());
    let output = run_foundry(client_home.path(), &client_registry, &addr, &["registry", "list"]);

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("server-only"));
    assert!(stdout.contains("iterate, maintain, audit"));
    assert_registry_file_absent(
        &client_registry,
        "online registry list must not create a client-side registry file",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn online_show_reads_exact_daemon_fields_without_client_registry_file() {
    let daemon_registry = Registry {
        version: 2,
        projects: vec![daemon_project_with_actions("server-only")],
    };
    let daemon_registry_file = NamedTempFile::new().expect("daemon registry tempfile");
    daemon_registry.save(daemon_registry_file.path()).expect("save daemon registry");
    let (service, _tmp_traces) =
        make_service_with_registry(daemon_registry, daemon_registry_file.path().to_path_buf());
    let addr = start_server(service).await;

    let client_home = tempfile::tempdir().expect("client home");
    let client_registry = missing_client_registry_path(client_home.path());
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
    assert_show_displays_exact_fields_and_actions(
        &output,
        "server-only",
        "/srv/server-only",
        "daemon/server-only",
        "notes from daemon for server-only",
        "iterate, maintain, audit",
    );
    assert_registry_file_absent(
        &client_registry,
        "online registry show must not create a client-side registry file",
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn online_cli_mutations_target_daemon_registry_without_creating_client_registry_file() {
    let daemon_registry_file = NamedTempFile::new().expect("daemon registry tempfile");
    Registry {
        version: 2,
        projects: vec![],
    }
    .save(daemon_registry_file.path())
    .expect("save empty daemon registry");
    let (service, _tmp_traces) = make_service_with_registry(
        Registry {
            version: 2,
            projects: vec![],
        },
        daemon_registry_file.path().to_path_buf(),
    );
    let addr = start_server(service).await;

    let client_home = tempfile::tempdir().expect("client home");
    let client_registry = missing_client_registry_path(client_home.path());

    let add_output = run_online_registry_add(client_home.path(), &client_registry, &addr);
    assert_command_succeeded(&add_output);
    assert_stdout_contains(
        &add_output,
        "Added project 'alpha' to registry.",
        "online add should confirm the daemon mutation",
    );
    assert_registry_file_absent(
        &client_registry,
        "online registry add must not create a client-side registry file",
    );
    assert_daemon_project_fields(
        daemon_registry_file.path(),
        "alpha",
        "/srv/alpha",
        "daemon/alpha",
        "daemon-owned add",
    );

    let list_output =
        run_foundry(client_home.path(), &client_registry, &addr, &["registry", "list"]);
    assert_command_succeeded(&list_output);
    assert_stdout_contains(
        &list_output,
        "alpha",
        "online list should reflect daemon-owned registry state",
    );
    assert_registry_file_absent(
        &client_registry,
        "online registry list must not create a client-side registry file after add",
    );

    let show_output =
        run_online_registry_show(client_home.path(), &client_registry, &addr, "alpha");
    assert_show_displays_exact_fields(
        &show_output,
        "alpha",
        "/srv/alpha",
        "daemon/alpha",
        "daemon-owned add",
    );
    assert!(stdout_string(&show_output).contains("Actions:   none"));

    let edit_output = run_online_registry_edit(client_home.path(), &client_registry, &addr);
    assert_command_succeeded(&edit_output);
    assert_stdout_contains(
        &edit_output,
        "Updated project 'alpha'.",
        "online edit should confirm the daemon mutation",
    );
    assert_registry_file_absent(
        &client_registry,
        "online registry edit must not create a client-side registry file",
    );
    assert_daemon_project_fields(
        daemon_registry_file.path(),
        "alpha",
        "/srv/alpha-edited",
        "daemon/alpha-edited",
        "daemon-owned edit",
    );

    let show_after_edit =
        run_online_registry_show(client_home.path(), &client_registry, &addr, "alpha");
    assert_show_displays_exact_fields(
        &show_after_edit,
        "alpha",
        "/srv/alpha-edited",
        "daemon/alpha-edited",
        "daemon-owned edit",
    );
    assert!(stdout_string(&show_after_edit).contains("Actions:   none"));

    let remove_output = run_online_registry_remove(client_home.path(), &client_registry, &addr);
    assert_command_succeeded(&remove_output);
    assert_stdout_contains(
        &remove_output,
        "Removed project 'alpha' from registry.",
        "online remove should confirm the daemon mutation",
    );
    assert_registry_file_absent(
        &client_registry,
        "online registry remove must not create a client-side registry file",
    );

    let daemon_registry =
        Registry::load(daemon_registry_file.path()).expect("load daemon registry after remove");
    assert!(daemon_registry.projects.is_empty());
}

#[tokio::test]
async fn online_list_unreachable_daemon_leaves_client_registry_byte_identical() {
    let client_home = tempfile::tempdir().expect("client home");
    let (client_registry, before) = seed_client_registry_trap(client_home.path());

    let output =
        run_foundry(client_home.path(), &client_registry, DUMMY_ADDR, &["registry", "list"]);

    assert!(!output.status.success(), "online list should fail when daemon is unreachable");
    assert_eq!(
        stderr_string(&output).trim(),
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry list --offline`"
    );
    assert_eq!(
        std::fs::read(&client_registry).expect("read client trap bytes after failed online list"),
        before,
        "online registry list must not read or mutate the client-side registry file"
    );
}

#[tokio::test]
async fn online_list_unreachable_daemon_leaves_absent_client_registry_absent() {
    let client_home = tempfile::tempdir().expect("client home");
    assert_online_unreachable_keeps_client_registry_absent(
        client_home.path(),
        &["registry", "list"],
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry list --offline`",
        "online registry list must leave an absent client-side registry file absent",
    );
}

#[tokio::test]
async fn online_show_unreachable_daemon_leaves_client_registry_byte_identical() {
    let client_home = tempfile::tempdir().expect("client home");
    let (client_registry, before) = seed_client_registry_trap(client_home.path());

    let output = run_foundry(
        client_home.path(),
        &client_registry,
        DUMMY_ADDR,
        &["registry", "show", "alpha"],
    );

    assert!(!output.status.success(), "online show should fail when daemon is unreachable");
    assert_eq!(
        stderr_string(&output).trim(),
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry show alpha --offline`"
    );
    assert_eq!(
        std::fs::read(&client_registry).expect("read client trap bytes after failed online show"),
        before,
        "online registry show must not read or mutate the client-side registry file"
    );
}

#[tokio::test]
async fn online_show_unreachable_daemon_leaves_absent_client_registry_absent() {
    let client_home = tempfile::tempdir().expect("client home");
    assert_online_unreachable_keeps_client_registry_absent(
        client_home.path(),
        &["registry", "show", "alpha"],
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry show alpha --offline`",
        "online registry show must leave an absent client-side registry file absent",
    );
}

#[tokio::test]
async fn online_add_unreachable_daemon_leaves_client_registry_byte_identical() {
    let client_home = tempfile::tempdir().expect("client home");
    let (client_registry, before) = seed_client_registry_trap(client_home.path());
    let output = run_foundry(
        client_home.path(),
        &client_registry,
        DUMMY_ADDR,
        &[
            "registry",
            "add",
            "--name",
            "alpha",
            "--path",
            "/tmp/alpha",
            "--stack",
            "rust",
            "--agent",
            "claude",
            "--repo",
            "o/alpha",
            "--branch",
            "main",
        ],
    );

    assert!(!output.status.success(), "online add should fail when daemon is unreachable");
    assert_eq!(
        stderr_string(&output).trim(),
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry add --name alpha --offline`"
    );
    let after = std::fs::read(&client_registry).expect("read registry after failed online add");
    assert_eq!(after, before);
}

#[tokio::test]
async fn online_add_unreachable_daemon_leaves_absent_client_registry_absent() {
    let client_home = tempfile::tempdir().expect("client home");
    assert_online_unreachable_keeps_client_registry_absent(
        client_home.path(),
        &[
            "registry",
            "add",
            "--name",
            "alpha",
            "--path",
            "/tmp/alpha",
            "--stack",
            "rust",
            "--agent",
            "claude",
            "--repo",
            "o/alpha",
            "--branch",
            "main",
        ],
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry add --name alpha --offline`",
        "online registry add must leave an absent client-side registry file absent",
    );
}

#[tokio::test]
async fn online_remove_unreachable_daemon_leaves_client_registry_byte_identical() {
    let client_home = tempfile::tempdir().expect("client home");
    let (client_registry, before) = seed_client_registry_trap(client_home.path());
    let output = run_foundry(
        client_home.path(),
        &client_registry,
        DUMMY_ADDR,
        &["registry", "remove", "alpha"],
    );

    assert!(!output.status.success(), "online remove should fail when daemon is unreachable");
    assert_eq!(
        stderr_string(&output).trim(),
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry remove alpha --offline`"
    );
    let after = std::fs::read(&client_registry).expect("read registry after failed online remove");
    assert_eq!(after, before);
}

#[tokio::test]
async fn online_remove_unreachable_daemon_leaves_absent_client_registry_absent() {
    let client_home = tempfile::tempdir().expect("client home");
    assert_online_unreachable_keeps_client_registry_absent(
        client_home.path(),
        &["registry", "remove", "alpha"],
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry remove alpha --offline`",
        "online registry remove must leave an absent client-side registry file absent",
    );
}

#[tokio::test]
async fn online_edit_unreachable_daemon_leaves_client_registry_byte_identical() {
    let client_home = tempfile::tempdir().expect("client home");
    let (client_registry, before) = seed_client_registry_trap(client_home.path());
    let output = run_foundry(
        client_home.path(),
        &client_registry,
        DUMMY_ADDR,
        &["registry", "edit", "alpha", "--branch", "develop"],
    );

    assert!(!output.status.success(), "online edit should fail when daemon is unreachable");
    assert_eq!(
        stderr_string(&output).trim(),
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry edit alpha --offline`"
    );
    let after = std::fs::read(&client_registry).expect("read registry after failed online edit");
    assert_eq!(after, before);
}

#[test]
fn online_edit_unreachable_daemon_leaves_absent_client_registry_absent() {
    let client_home = tempfile::tempdir().expect("client home");
    assert_online_unreachable_keeps_client_registry_absent(
        client_home.path(),
        &["registry", "edit", "alpha", "--branch", "develop"],
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry edit alpha --offline`",
        "online registry edit must leave an absent client-side registry file absent",
    );
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

#[test]
fn offline_cli_list_show_add_edit_and_remove_work_against_direct_file_store() {
    let client_home = tempfile::tempdir().expect("client home");
    let registry_path = seed_offline_registry(client_home.path());

    assert_offline_cli_list_and_show(client_home.path(), &registry_path);
    run_offline_cli_mutations(client_home.path(), &registry_path);
    assert_offline_registry_final_state(&registry_path);
}
