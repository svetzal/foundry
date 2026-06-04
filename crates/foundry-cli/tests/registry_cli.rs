//! Integration tests for the `foundry registry` command offline path.
//!
//! These tests exercise `registry_commands::add`, `edit`, and `remove` with
//! `offline = true`, verifying that the commands write the correct data to the
//! registry JSON file without requiring a running `foundryd` instance.
//!
//! The daemon-side gRPC contract is covered by `crates/foundryd/tests/registry_grpc.rs`.
//! The request-building logic (online path) is exercised indirectly through that
//! integration test; here we focus on the offline code path that is unique to
//! the CLI crate.

use foundry_cli::registry_commands;
use foundry_sdk::registry::{ProjectEdits, ProjectSpec, Registry, Stack};
use tempfile::NamedTempFile;

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
