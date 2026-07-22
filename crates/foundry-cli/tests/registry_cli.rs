//! Integration tests for `foundry registry` CLI behavior.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_blocks::trace_writer::TraceWriter;
use foundry_cli::registry_commands;
use foundry_engine::engine::Engine;
use foundry_sdk::registry::{
    ActionFlags, InstallConfig, ProjectEdits, ProjectEntry, ProjectSpec, Registry, Stack,
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

fn fully_populated_project(name: &str) -> ProjectEntry {
    ProjectEntry {
        name: name.to_string(),
        path: format!("/srv/{name}"),
        stack: Stack::Rust,
        agent: "codex".to_string(),
        repo: format!("daemon/{name}"),
        branch: "release".to_string(),
        skip: Some("Paused for rollout".to_string()),
        actions: ActionFlags {
            iterate: true,
            maintain: true,
            push: true,
            audit: false,
            release: true,
        },
        install: Some(InstallConfig::Command("./install.sh".to_string())),
        installs_skill: None,
        notes: Some(format!("daemon note for {name}")),
        timeout_secs: Some(75),
        audit_exceptions: vec![],
    }
}

fn online_added_project() -> ProjectEntry {
    ProjectEntry {
        name: "alpha".to_string(),
        path: "/srv/alpha".to_string(),
        stack: Stack::Rust,
        agent: "codex".to_string(),
        repo: "daemon/alpha".to_string(),
        branch: "release".to_string(),
        skip: None,
        actions: ActionFlags {
            iterate: true,
            maintain: true,
            push: true,
            audit: true,
            release: true,
        },
        install: Some(InstallConfig::Command("./install.sh".to_string())),
        installs_skill: None,
        notes: Some("daemon-owned add".to_string()),
        timeout_secs: Some(75),
        audit_exceptions: vec![],
    }
}

fn online_edited_project() -> ProjectEntry {
    ProjectEntry {
        skip: Some("Waiting for deploy".to_string()),
        install: Some(InstallConfig::Brew("foundry".to_string())),
        notes: Some("daemon-owned edit".to_string()),
        timeout_secs: Some(90),
        actions: ActionFlags {
            iterate: true,
            maintain: true,
            push: false,
            audit: true,
            release: false,
        },
        path: "/srv/alpha-edited".to_string(),
        repo: "daemon/alpha-edited".to_string(),
        ..online_added_project()
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
            actions: ActionFlags {
                iterate: true,
                maintain: false,
                push: true,
                audit: true,
                release: false,
            },
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

#[cfg(unix)]
fn make_persist_failure_registry_fixture(
    registry: &Registry,
) -> (TempDir, std::path::PathBuf, Vec<u8>) {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let registry_path = tempdir.path().join("registry.json");
    registry.save(&registry_path).expect("save seeded registry");
    let before = std::fs::read(&registry_path).expect("read seeded registry");
    let mut permissions =
        std::fs::metadata(&registry_path).expect("stat registry file").permissions();
    permissions.set_mode(0o444);
    std::fs::set_permissions(&registry_path, permissions).expect("set registry readonly");
    (tempdir, registry_path, before)
}

fn assert_project_entry_matches_exact(actual: &ProjectEntry, expected: &ProjectEntry) {
    assert_eq!(actual.name, expected.name);
    assert_eq!(actual.path, expected.path);
    assert_eq!(actual.stack, expected.stack);
    assert_eq!(actual.agent, expected.agent);
    assert_eq!(actual.repo, expected.repo);
    assert_eq!(actual.branch, expected.branch);
    assert_eq!(actual.skip, expected.skip);
    assert_eq!(actual.actions.iterate, expected.actions.iterate);
    assert_eq!(actual.actions.maintain, expected.actions.maintain);
    assert_eq!(actual.actions.push, expected.actions.push);
    assert_eq!(actual.actions.audit, expected.actions.audit);
    assert_eq!(actual.actions.release, expected.actions.release);
    match (&actual.install, &expected.install) {
        (Some(InstallConfig::Command(actual)), Some(InstallConfig::Command(expected))) => {
            assert_eq!(actual, expected);
        }
        (Some(InstallConfig::Brew(actual)), Some(InstallConfig::Brew(expected))) => {
            assert_eq!(actual, expected);
        }
        (None, None) => {}
        other => panic!("install config mismatch: {other:?}"),
    }
    assert_eq!(actual.notes, expected.notes);
    assert_eq!(actual.timeout_secs, expected.timeout_secs);
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

fn assert_daemon_project_fields(registry_path: &std::path::Path, expected: &ProjectEntry) {
    let daemon_registry = Registry::load(registry_path).expect("load daemon registry");
    let project = daemon_registry
        .projects
        .iter()
        .find(|project| project.name == expected.name)
        .expect("daemon registry should contain expected project");
    assert_project_entry_matches_exact(project, expected);
}

fn run_online_registry_show(
    client_home: &std::path::Path,
    client_registry: &std::path::Path,
    addr: &str,
    name: &str,
) -> std::process::Output {
    run_foundry(client_home, client_registry, addr, &["registry", "show", name])
}

fn assert_show_displays_exact_fields(output: &std::process::Output, expected: &ProjectEntry) {
    assert_command_succeeded(output);
    let stdout = stdout_string(output);
    assert!(stdout.contains(&format!("Name:      {}", expected.name)));
    assert!(stdout.contains(&format!("Path:      {}", expected.path)));
    assert!(stdout.contains(&format!("Stack:     {}", expected.stack)));
    assert!(stdout.contains(&format!("Agent:     {}", expected.agent)));
    assert!(stdout.contains(&format!("Repo:      {}", expected.repo)));
    assert!(stdout.contains(&format!("Branch:    {}", expected.branch)));
    match &expected.skip {
        Some(skip) => assert!(stdout.contains(&format!("Skip:      {skip}"))),
        None => assert!(stdout.contains("Skip:      no")),
    }
    if let Some(notes) = &expected.notes {
        assert!(stdout.contains(&format!("Notes:     {notes}")));
    }
    match &expected.install {
        Some(InstallConfig::Command(command)) => {
            assert!(stdout.contains(&format!("Install:   command: {command}")));
        }
        Some(InstallConfig::Brew(formula)) => {
            assert!(stdout.contains(&format!("Install:   brew: {formula}")));
        }
        None => {}
    }
    match expected.timeout_secs {
        Some(timeout_secs) => assert!(stdout.contains(&format!("Timeout:   {timeout_secs}s"))),
        None => assert!(stdout.contains("Timeout:   3600s (default)")),
    }
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

fn assert_online_unreachable_keeps_client_registry_byte_identical(
    client_home: &std::path::Path,
    args: &[&str],
    expected_stderr: &str,
    context: &str,
) {
    let (client_registry, before) = seed_client_registry_trap(client_home);

    let output = run_foundry(client_home, &client_registry, DUMMY_ADDR, args);

    assert!(!output.status.success(), "{context}: command should fail");
    assert_eq!(stderr_string(&output).trim(), expected_stderr);
    assert_eq!(
        std::fs::read(&client_registry).expect("read trap registry after failed online command"),
        before,
        "{context}: command must not read or mutate the client-side registry file"
    );
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
            "codex",
            "--repo",
            "daemon/alpha",
            "--branch",
            "release",
            "--iterate",
            "--maintain",
            "--push",
            "--audit",
            "--release",
            "--install-command",
            "./install.sh",
            "--timeout-secs",
            "75",
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
            "--skip",
            "Waiting for deploy",
            "--push",
            "false",
            "--release",
            "false",
            "--install-brew",
            "foundry",
            "--timeout-secs",
            "90",
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
    let expected = fully_populated_project("server-only");
    let daemon_registry = Registry {
        version: 2,
        projects: vec![expected.clone()],
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
    assert_show_displays_exact_fields(&output, &expected);
    assert!(stdout_string(&output).contains("Actions:   iterate, maintain, push, release"));
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
    assert_daemon_project_fields(daemon_registry_file.path(), &online_added_project());

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
    assert_show_displays_exact_fields(&show_output, &online_added_project());
    assert!(
        stdout_string(&show_output).contains("Actions:   iterate, maintain, push, audit, release")
    );

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
    assert_daemon_project_fields(daemon_registry_file.path(), &online_edited_project());

    let show_after_edit =
        run_online_registry_show(client_home.path(), &client_registry, &addr, "alpha");
    assert_show_displays_exact_fields(&show_after_edit, &online_edited_project());
    assert!(stdout_string(&show_after_edit).contains("Actions:   iterate, maintain, audit"));

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
    assert_online_unreachable_keeps_client_registry_byte_identical(
        client_home.path(),
        &["registry", "list"],
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry list --offline`",
        "online registry list",
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
    assert_online_unreachable_keeps_client_registry_byte_identical(
        client_home.path(),
        &["registry", "show", "alpha"],
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry show alpha --offline`",
        "online registry show",
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
    assert_online_unreachable_keeps_client_registry_byte_identical(
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
        "online registry add",
    );
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
    assert_online_unreachable_keeps_client_registry_byte_identical(
        client_home.path(),
        &["registry", "remove", "alpha"],
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry remove alpha --offline`",
        "online registry remove",
    );
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
    assert_online_unreachable_keeps_client_registry_byte_identical(
        client_home.path(),
        &["registry", "edit", "alpha", "--branch", "develop"],
        "Error: foundryd is not reachable at http://127.0.0.1:9; start the daemon or rerun with `foundry registry edit alpha --offline`",
        "online registry edit",
    );
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
fn online_init_without_offline_leaves_client_registry_byte_identical() {
    let client_home = tempfile::tempdir().expect("client home");
    assert_online_unreachable_keeps_client_registry_byte_identical(
        client_home.path(),
        &["registry", "init"],
        "Error: `foundry registry init` is an offline recovery command; rerun with `--offline`",
        "online registry init",
    );
}

#[test]
fn online_init_without_offline_leaves_absent_client_registry_absent() {
    let client_home = tempfile::tempdir().expect("client home");
    assert_online_unreachable_keeps_client_registry_absent(
        client_home.path(),
        &["registry", "init"],
        "Error: `foundry registry init` is an offline recovery command; rerun with `--offline`",
        "online registry init must leave an absent client-side registry file absent",
    );
}

#[test]
fn offline_init_creates_recovery_registry_file_via_cli() {
    let client_home = tempfile::tempdir().expect("client home");
    let registry_path = missing_client_registry_path(client_home.path());
    assert_registry_file_absent(&registry_path, "offline init precondition");

    let output = run_foundry(
        client_home.path(),
        &registry_path,
        DUMMY_ADDR,
        &["--offline", "registry", "init"],
    );
    assert_command_succeeded(&output);

    let registry = Registry::load(&registry_path).expect("load created offline registry");
    assert_eq!(registry.version, 2);
    assert!(registry.projects.is_empty());
}

#[test]
fn offline_list_reads_direct_registry_file_via_cli() {
    let client_home = tempfile::tempdir().expect("client home");
    let registry_path = seed_offline_registry(client_home.path());

    let output = run_foundry(
        client_home.path(),
        &registry_path,
        DUMMY_ADDR,
        &["--offline", "registry", "list"],
    );
    assert_command_succeeded(&output);
    let stdout = stdout_string(&output);
    assert!(stdout.contains("offline-seeded"));
    assert!(stdout.contains("iterate, push, audit"));
}

#[test]
fn offline_show_reads_exact_registry_fields_via_cli() {
    let client_home = tempfile::tempdir().expect("client home");
    let registry_path = seed_offline_registry(client_home.path());

    let output = run_foundry(
        client_home.path(),
        &registry_path,
        DUMMY_ADDR,
        &["--offline", "registry", "show", "offline-seeded"],
    );
    assert_show_displays_exact_fields(
        &output,
        &Registry::load(&registry_path).expect("load seeded offline registry").projects[0],
    );
    assert!(stdout_string(&output).contains("Actions:   iterate, push, audit"));
}

#[test]
fn offline_add_writes_exact_registry_fields_via_cli() {
    let client_home = tempfile::tempdir().expect("client home");
    let registry_path = seed_offline_registry(client_home.path());

    let output = run_foundry(
        client_home.path(),
        &registry_path,
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
            "--iterate",
            "--maintain",
            "--notes",
            "added via offline cli",
        ],
    );
    assert_command_succeeded(&output);

    let registry = Registry::load(&registry_path).expect("load registry after offline add");
    let project = registry
        .projects
        .iter()
        .find(|project| project.name == "offline-added")
        .expect("offline-added project must exist");
    assert_project_entry_matches_exact(
        project,
        &ProjectEntry {
            name: "offline-added".to_string(),
            path: "/offline/added".to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: "offline/added".to_string(),
            branch: "main".to_string(),
            skip: None,
            actions: ActionFlags {
                iterate: true,
                maintain: true,
                push: false,
                audit: false,
                release: false,
            },
            install: None,
            installs_skill: None,
            notes: Some("added via offline cli".to_string()),
            timeout_secs: None,
            audit_exceptions: vec![],
        },
    );
}

#[test]
fn offline_edit_updates_exact_registry_fields_via_cli() {
    let client_home = tempfile::tempdir().expect("client home");
    let registry_path = seed_offline_registry(client_home.path());

    let output = run_foundry(
        client_home.path(),
        &registry_path,
        DUMMY_ADDR,
        &[
            "--offline",
            "registry",
            "edit",
            "offline-seeded",
            "--path",
            "/offline/seeded-edited",
            "--repo",
            "offline/seeded-edited",
            "--notes",
            "edited via offline cli",
            "--maintain",
            "true",
            "--release",
            "true",
        ],
    );
    assert_command_succeeded(&output);

    let registry = Registry::load(&registry_path).expect("load registry after offline edit");
    let project = registry
        .projects
        .iter()
        .find(|project| project.name == "offline-seeded")
        .expect("offline-seeded project must exist");
    assert_project_entry_matches_exact(
        project,
        &ProjectEntry {
            name: "offline-seeded".to_string(),
            path: "/offline/seeded-edited".to_string(),
            stack: Stack::Rust,
            agent: "claude".to_string(),
            repo: "offline/seeded-edited".to_string(),
            branch: "main".to_string(),
            skip: None,
            actions: ActionFlags {
                iterate: true,
                maintain: true,
                push: true,
                audit: true,
                release: true,
            },
            install: None,
            installs_skill: None,
            notes: Some("edited via offline cli".to_string()),
            timeout_secs: Some(90),
            audit_exceptions: vec![],
        },
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn online_cli_surfaces_typed_registry_errors_and_preserves_daemon_state() {
    let daemon_registry_file = NamedTempFile::new().expect("daemon registry tempfile");
    let seeded = fully_populated_project("alpha");
    Registry {
        version: 2,
        projects: vec![seeded.clone()],
    }
    .save(daemon_registry_file.path())
    .expect("save daemon registry");
    let (service, _tmp_traces) = make_service_with_registry(
        Registry {
            version: 2,
            projects: vec![seeded.clone()],
        },
        daemon_registry_file.path().to_path_buf(),
    );
    let addr = start_server(service).await;

    let client_home = tempfile::tempdir().expect("client home");
    let client_registry = missing_client_registry_path(client_home.path());

    let duplicate_add = run_foundry(
        client_home.path(),
        &client_registry,
        &addr,
        &[
            "registry",
            "add",
            "--name",
            "alpha",
            "--path",
            "/dup/alpha",
            "--stack",
            "rust",
            "--agent",
            "claude",
            "--repo",
            "dup/alpha",
        ],
    );
    assert_eq!(
        stderr_string(&duplicate_add).trim(),
        "Error: daemon error: Some entity that we attempted to create already exists — project 'alpha' already exists"
    );

    let missing_show =
        run_foundry(client_home.path(), &client_registry, &addr, &["registry", "show", "ghost"]);
    assert_eq!(
        stderr_string(&missing_show).trim(),
        "Error: daemon error: Some requested entity was not found — project 'ghost' not found"
    );

    let invalid_stack = run_foundry(
        client_home.path(),
        &client_registry,
        &addr,
        &[
            "registry",
            "add",
            "--name",
            "beta",
            "--path",
            "/srv/beta",
            "--stack",
            "cobol",
            "--agent",
            "claude",
            "--repo",
            "daemon/beta",
        ],
    );
    assert_eq!(
        stderr_string(&invalid_stack).trim(),
        "Error: daemon error: Client specified an invalid argument — invalid stack 'cobol'"
    );

    let conflicting_install = run_foundry(
        client_home.path(),
        &client_registry,
        &addr,
        &[
            "registry",
            "add",
            "--name",
            "beta",
            "--path",
            "/srv/beta",
            "--stack",
            "rust",
            "--agent",
            "claude",
            "--repo",
            "daemon/beta",
            "--install-command",
            "./install.sh",
            "--install-brew",
            "foundry",
        ],
    );
    assert_eq!(
        stderr_string(&conflicting_install).trim(),
        "Error: daemon error: Client specified an invalid argument — provide at most one of install_command or install_brew"
    );

    let missing_remove =
        run_foundry(client_home.path(), &client_registry, &addr, &["registry", "remove", "ghost"]);
    assert_eq!(
        stderr_string(&missing_remove).trim(),
        "Error: daemon error: Some requested entity was not found — project 'ghost' not found"
    );

    let missing_edit = run_foundry(
        client_home.path(),
        &client_registry,
        &addr,
        &["registry", "edit", "ghost", "--branch", "develop"],
    );
    assert_eq!(
        stderr_string(&missing_edit).trim(),
        "Error: daemon error: Some requested entity was not found — project 'ghost' not found"
    );

    assert_registry_file_absent(
        &client_registry,
        "typed online daemon errors must not create a client-side registry file",
    );
    assert_daemon_project_fields(daemon_registry_file.path(), &seeded);
}

#[tokio::test(flavor = "multi_thread")]
async fn online_cli_persist_failures_surface_internal_error_and_leave_daemon_registry_byte_identical()
 {
    let seeded = online_added_project();
    let seeded_registry = Registry {
        version: 2,
        projects: vec![seeded.clone()],
    };
    let (tempdir, daemon_registry_path, registry_before) =
        make_persist_failure_registry_fixture(&seeded_registry);
    let (service, _tmp_traces) =
        make_service_with_registry(seeded_registry, daemon_registry_path.clone());
    let addr = start_server(service).await;

    let client_home = tempfile::tempdir().expect("client home");
    let client_registry = missing_client_registry_path(client_home.path());

    let add_output = run_foundry(
        client_home.path(),
        &client_registry,
        &addr,
        &[
            "registry",
            "add",
            "--name",
            "beta",
            "--path",
            "/srv/beta",
            "--stack",
            "rust",
            "--agent",
            "claude",
            "--repo",
            "daemon/beta",
        ],
    );
    assert_eq!(
        stderr_string(&add_output).trim(),
        "Error: daemon error: Internal error — failed to persist registry state"
    );

    let edit_output = run_foundry(
        client_home.path(),
        &client_registry,
        &addr,
        &["registry", "edit", "alpha", "--repo", "daemon/alpha-edited"],
    );
    assert_eq!(
        stderr_string(&edit_output).trim(),
        "Error: daemon error: Internal error — failed to persist registry state"
    );

    let remove_output =
        run_foundry(client_home.path(), &client_registry, &addr, &["registry", "remove", "alpha"]);
    assert_eq!(
        stderr_string(&remove_output).trim(),
        "Error: daemon error: Internal error — failed to persist registry state"
    );

    let show_output =
        run_online_registry_show(client_home.path(), &client_registry, &addr, "alpha");
    assert_show_displays_exact_fields(&show_output, &seeded);
    let list_output =
        run_foundry(client_home.path(), &client_registry, &addr, &["registry", "list"]);
    assert_command_succeeded(&list_output);
    assert_stdout_contains(
        &list_output,
        "alpha",
        "daemon state must remain readable after persistence failures",
    );

    assert_registry_file_absent(
        &client_registry,
        "persistence failures must not create a client-side registry file",
    );
    assert_daemon_project_fields(&daemon_registry_path, &seeded);
    assert_eq!(
        std::fs::read(&daemon_registry_path).expect("read daemon registry after failed mutations"),
        registry_before,
        "failed online mutations must leave the daemon-owned registry bytes unchanged"
    );
    let mut permissions = std::fs::metadata(&daemon_registry_path)
        .expect("stat registry file")
        .permissions();
    permissions.set_mode(0o644);
    std::fs::set_permissions(&daemon_registry_path, permissions)
        .expect("restore registry permissions");
    drop(tempdir);
}

#[test]
fn offline_remove_mutates_direct_registry_file_via_cli() {
    let client_home = tempfile::tempdir().expect("client home");
    let registry_path = seed_offline_registry(client_home.path());

    let output = run_foundry(
        client_home.path(),
        &registry_path,
        DUMMY_ADDR,
        &["--offline", "registry", "remove", "offline-seeded"],
    );
    assert_command_succeeded(&output);

    let registry = Registry::load(&registry_path).expect("load registry after offline remove");
    assert!(
        registry.projects.is_empty(),
        "offline remove must delete the exact project from the direct file store"
    );
}
