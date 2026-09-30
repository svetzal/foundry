//! Integration tests for `foundry queue` CLI behavior.
//!
//! These stand up a real `FoundryService` over tonic against a temporary
//! work-item ledger, so the online path is exercised end to end through the
//! `ListWorkItems` / `GetWorkItem` RPCs rather than against a stub.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::work_item::{
    WorkDisposition, WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
};
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

/// A port nothing listens on, so the online path must fail rather than connect.
const DUMMY_ADDR: &str = "http://127.0.0.1:9";

fn at(seconds: i64) -> DateTime<Utc> {
    DateTime::from_timestamp(seconds, 0).expect("in-range timestamp")
}

fn item(
    id: &str,
    project: &str,
    kind: WorkItemKind,
    lane: WorkLane,
    state: WorkItemState,
    settled: i64,
) -> WorkItem {
    let mut item = WorkItem::submitted(
        WorkItemSpec {
            project: project.to_string(),
            objective: format!("objective for {id}"),
            kind,
            lane,
            origin: "integration-test".to_string(),
            trace_id: Some("a".repeat(32)),
        },
        at(1_000),
    );
    item.id = id.to_string();
    item.state = state;
    item.reason = format!("reason for {id}");
    item.started_at = Some(at(1_001));
    if !matches!(state, WorkItemState::Running | WorkItemState::Submitted | WorkItemState::Queued) {
        item.settled_at = Some(at(settled));
    }
    item
}

/// One item in every state across two projects, with the preserved item
/// carrying a full settlement record.
fn seeded_ledger() -> WorkItemStore {
    let mut preserved = item(
        "wi_preserved",
        "beta",
        WorkItemKind::MajorUpgrade,
        WorkLane::Maintenance,
        WorkItemState::Preserved,
        5_000,
    );
    preserved.disposition = Some(WorkDisposition {
        verdict: "remainder".to_string().into(),
        landed_commit: None,
        preservation_ref: Some("foundry/majors/serde".to_string()),
        worktree: Some("/tmp/worktrees/beta".to_string()),
        worktree_removed: Some(false),
    });

    WorkItemStore {
        version: 1,
        items: vec![
            item(
                "wi_running",
                "alpha",
                WorkItemKind::Task,
                WorkLane::Interactive,
                WorkItemState::Running,
                0,
            ),
            item(
                "wi_submitted",
                "alpha",
                WorkItemKind::CampaignCycle,
                WorkLane::Campaign,
                WorkItemState::Submitted,
                0,
            ),
            item(
                "wi_queued",
                "beta",
                WorkItemKind::Maintenance,
                WorkLane::Maintenance,
                WorkItemState::Queued,
                0,
            ),
            preserved,
            item(
                "wi_needs",
                "alpha",
                WorkItemKind::Task,
                WorkLane::Interactive,
                WorkItemState::NeedsDecision,
                4_000,
            ),
            item(
                "wi_failed",
                "beta",
                WorkItemKind::Remediation,
                WorkLane::Maintenance,
                WorkItemState::Failed,
                3_000,
            ),
            item(
                "wi_landed",
                "alpha",
                WorkItemKind::Release,
                WorkLane::Maintenance,
                WorkItemState::Landed,
                6_000,
            ),
            item(
                "wi_cancelled",
                "beta",
                WorkItemKind::Task,
                WorkLane::Interactive,
                WorkItemState::Cancelled,
                2_000,
            ),
        ],
    }
}

fn make_service(work_items_path: std::path::PathBuf) -> (FoundryService, TempDir) {
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

    let ctx = RuntimeContext {
        engine,
        trace_store,
        workflow_tracker,
        trace_writer,
        event_tx,
        registry,
    };
    let stores = StoreConfig {
        work_items_path,
        campaigns_path: std::path::PathBuf::new(),
        registry_path: std::path::PathBuf::new(),
        sentinels: Arc::new(RwLock::new(SentinelStore {
            version: 1,
            sentinels: vec![],
        })),
        sentinels_path: std::path::PathBuf::new(),
        scheduler_reload: Arc::new(Notify::new()),
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

/// Stand up a daemon over a ledger seeded with one item in every state.
async fn daemon_over_seeded_ledger() -> (String, NamedTempFile, TempDir) {
    let ledger = NamedTempFile::new().expect("tempfile for the daemon ledger");
    seeded_ledger().save(ledger.path()).expect("seed the daemon ledger");
    let (service, traces) = make_service(ledger.path().to_path_buf());
    let addr = start_server(service).await;
    (addr, ledger, traces)
}

fn run_foundry(
    home: &std::path::Path,
    work_items_path: &std::path::Path,
    addr: &str,
    args: &[&str],
) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_foundry"))
        .arg("--addr")
        .arg(addr)
        .args(args)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("FOUNDRY_WORK_ITEMS_PATH", work_items_path)
        .output()
        .expect("run foundry binary")
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

/// The block of output under `heading`, up to the next blank line.
fn section_after(out: &str, heading: &str) -> String {
    let start = out
        .find(heading)
        .unwrap_or_else(|| panic!("heading '{heading}' missing from:\n{out}"));
    let rest = &out[start + heading.len()..];
    let end = rest.find("\n\n").unwrap_or(rest.len());
    rest[..end].to_string()
}

fn assert_ids_under(out: &str, heading: &str, expected: &[&str]) {
    let section = section_after(out, heading);
    for id in expected {
        assert!(section.contains(id), "'{id}' missing under '{heading}' in:\n{out}");
    }
}

/// The client-side ledger path the online path must never touch.
fn client_ledger_path(home: &std::path::Path) -> std::path::PathBuf {
    home.join(".foundry/work-items.json")
}

fn seed_client_ledger_trap(home: &std::path::Path) -> (std::path::PathBuf, Vec<u8>) {
    let path = client_ledger_path(home);
    std::fs::create_dir_all(path.parent().expect("client ledger path must have a parent"))
        .expect("create client ledger parent");
    let bytes = br"not valid json and must stay untouched".to_vec();
    std::fs::write(&path, &bytes).expect("seed client ledger trap bytes");
    (path, bytes)
}

// ── online reads ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn online_queue_prints_every_seeded_id_under_the_correct_group_heading() {
    let (addr, ledger, _traces) = daemon_over_seeded_ledger().await;
    let home = tempfile::tempdir().expect("client home");

    let output = run_foundry(home.path(), ledger.path(), &addr, &["queue"]);
    assert_command_succeeded(&output);
    let out = stdout_string(&output);

    assert_ids_under(&out, "Running", &["wi_running"]);
    assert_ids_under(&out, "Queued", &["wi_submitted", "wi_queued"]);
    assert_ids_under(&out, "Open — needs a person", &["wi_preserved", "wi_needs", "wi_failed"]);
    assert_ids_under(&out, "Settled (last 20)", &["wi_landed", "wi_cancelled"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn online_queue_json_parses_and_carries_the_same_ids_and_states() {
    let (addr, ledger, _traces) = daemon_over_seeded_ledger().await;
    let home = tempfile::tempdir().expect("client home");

    let output = run_foundry(home.path(), ledger.path(), &addr, &["queue", "--json"]);
    assert_command_succeeded(&output);

    let parsed: serde_json::Value =
        serde_json::from_str(&stdout_string(&output)).expect("--json output must parse");
    let array = parsed.as_array().expect("an array");

    let mut seen: Vec<(String, String)> = array
        .iter()
        .map(|item| {
            (
                item["id"].as_str().expect("id is a string").to_string(),
                item["state"].as_str().expect("state is a string").to_string(),
            )
        })
        .collect();
    seen.sort();

    let mut expected: Vec<(String, String)> = seeded_ledger()
        .items
        .iter()
        .map(|item| (item.id.clone(), item.state.tag().to_string()))
        .collect();
    expected.sort();

    assert_eq!(seen, expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn online_queue_show_prints_the_items_record_and_settlement_fields() {
    let (addr, ledger, _traces) = daemon_over_seeded_ledger().await;
    let home = tempfile::tempdir().expect("client home");

    let output = run_foundry(home.path(), ledger.path(), &addr, &["queue", "show", "wi_preserved"]);
    assert_command_succeeded(&output);
    let out = stdout_string(&output);

    for expected in [
        "wi_preserved",
        "beta",
        "major_upgrade",
        "maintenance",
        "preserved",
        "reason for wi_preserved",
        "remainder",
        "foundry/majors/serde",
        "/tmp/worktrees/beta",
        "Worktree removed: no",
    ] {
        assert!(out.contains(expected), "'{expected}' missing from:\n{out}");
    }
    assert!(!out.contains("Landed commit:"), "an unrecorded field must stay absent:\n{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn online_queue_show_of_an_unsettled_item_omits_every_settlement_field() {
    let (addr, ledger, _traces) = daemon_over_seeded_ledger().await;
    let home = tempfile::tempdir().expect("client home");

    let output = run_foundry(home.path(), ledger.path(), &addr, &["queue", "show", "wi_running"]);
    assert_command_succeeded(&output);
    let out = stdout_string(&output);

    for absent in [
        "Settled:",
        "Verdict:",
        "Landed commit:",
        "Worktree removed:",
    ] {
        assert!(!out.contains(absent), "'{absent}' must be absent from:\n{out}");
    }
    assert!(!out.contains("false"), "no optional may render as 'false':\n{out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn online_queue_show_json_carries_the_settlement_fields_for_one_id() {
    let (addr, ledger, _traces) = daemon_over_seeded_ledger().await;
    let home = tempfile::tempdir().expect("client home");

    let output = run_foundry(
        home.path(),
        ledger.path(),
        &addr,
        &["queue", "show", "wi_preserved", "--json"],
    );
    assert_command_succeeded(&output);

    let parsed: serde_json::Value =
        serde_json::from_str(&stdout_string(&output)).expect("--json output must parse");
    assert_eq!(parsed["id"], serde_json::json!("wi_preserved"));
    assert_eq!(parsed["state"], serde_json::json!("preserved"));
    assert_eq!(parsed["verdict"], serde_json::json!("remainder"));
    assert_eq!(parsed["worktree_removed"], serde_json::json!(false));
    assert!(
        parsed.as_object().expect("object").get("landed_commit").is_none(),
        "an unrecorded field must stay absent: {parsed}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn online_queue_show_of_a_missing_id_fails_with_a_not_found_message() {
    let (addr, ledger, _traces) = daemon_over_seeded_ledger().await;
    let home = tempfile::tempdir().expect("client home");

    let output = run_foundry(home.path(), ledger.path(), &addr, &["queue", "show", "wi_missing"]);

    assert!(!output.status.success(), "an unknown id must exit non-zero");
    let err = stderr_string(&output);
    assert!(err.contains("wi_missing"), "stderr should name the id: {err}");
    assert!(err.contains("not found"), "stderr should say not found: {err}");
    assert!(stdout_string(&output).is_empty(), "no record may be printed for a missing id");
}

#[tokio::test(flavor = "multi_thread")]
async fn online_queue_open_omits_running_and_landed_ids() {
    let (addr, ledger, _traces) = daemon_over_seeded_ledger().await;
    let home = tempfile::tempdir().expect("client home");

    let output = run_foundry(home.path(), ledger.path(), &addr, &["queue", "open"]);
    assert_command_succeeded(&output);
    let out = stdout_string(&output);

    for present in ["wi_preserved", "wi_needs", "wi_failed"] {
        assert!(out.contains(present), "'{present}' must appear in:\n{out}");
    }
    for absent in [
        "wi_running",
        "wi_submitted",
        "wi_queued",
        "wi_landed",
        "wi_cancelled",
    ] {
        assert!(!out.contains(absent), "'{absent}' must not appear in:\n{out}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn online_reads_never_create_the_client_side_ledger() {
    // The daemon's own ledger is held only to keep the temp file alive; the
    // point of this test is the client-side path, which is passed instead.
    let (addr, _ledger, _traces) = daemon_over_seeded_ledger().await;
    let home = tempfile::tempdir().expect("client home");
    let client = client_ledger_path(home.path());

    for args in [
        vec!["queue"],
        vec!["queue", "open"],
        vec!["queue", "show", "wi_running"],
    ] {
        let output = run_foundry(home.path(), &client, &addr, &args);
        assert_command_succeeded(&output);
        assert!(
            !client.exists(),
            "{args:?} must leave the client ledger absent at {}",
            client.display()
        );
    }
}

// ── online with no daemon listening ───────────────────────────────────────────

#[test]
fn online_with_no_daemon_fails_and_names_offline_leaving_an_absent_ledger_absent() {
    let home = tempfile::tempdir().expect("client home");
    let client = client_ledger_path(home.path());

    for (args, hint) in [
        (vec!["queue"], "foundry queue --offline"),
        (vec!["queue", "open"], "foundry queue open --offline"),
        (vec!["queue", "show", "wi_running"], "foundry queue show wi_running --offline"),
    ] {
        assert!(!client.exists(), "precondition: the client ledger starts absent");
        let output = run_foundry(home.path(), &client, DUMMY_ADDR, &args);

        assert!(!output.status.success(), "{args:?} must exit non-zero with no daemon");
        let err = stderr_string(&output);
        assert!(err.contains("--offline"), "{args:?} stderr should mention --offline: {err}");
        assert!(err.contains(hint), "{args:?} stderr should name `{hint}`: {err}");
        assert!(
            !client.exists(),
            "{args:?} must leave the client ledger absent at {}",
            client.display()
        );
    }
}

#[test]
fn online_with_no_daemon_leaves_a_pre_existing_client_ledger_byte_identical() {
    let home = tempfile::tempdir().expect("client home");
    let (client, before) = seed_client_ledger_trap(home.path());

    for args in [
        vec!["queue"],
        vec!["queue", "open"],
        vec!["queue", "show", "wi_running"],
        vec!["queue", "--json"],
    ] {
        let output = run_foundry(home.path(), &client, DUMMY_ADDR, &args);
        assert!(!output.status.success(), "{args:?} must exit non-zero with no daemon");
        let after = std::fs::read(&client).expect("read the client ledger trap");
        assert_eq!(after, before, "{args:?} must leave the client ledger byte-identical");
    }
}

// ── offline recovery ──────────────────────────────────────────────────────────

#[test]
fn offline_queue_renders_the_same_groups_by_id_without_any_daemon() {
    let home = tempfile::tempdir().expect("client home");
    let ledger = NamedTempFile::new().expect("tempfile for the offline ledger");
    seeded_ledger().save(ledger.path()).expect("seed the offline ledger");

    let output = run_foundry(home.path(), ledger.path(), DUMMY_ADDR, &["--offline", "queue"]);
    assert_command_succeeded(&output);
    let out = stdout_string(&output);

    assert_ids_under(&out, "Running", &["wi_running"]);
    assert_ids_under(&out, "Queued", &["wi_submitted", "wi_queued"]);
    assert_ids_under(&out, "Open — needs a person", &["wi_preserved", "wi_needs", "wi_failed"]);
    assert_ids_under(&out, "Settled (last 20)", &["wi_landed", "wi_cancelled"]);
}

#[test]
fn offline_show_and_open_work_without_any_daemon() {
    let home = tempfile::tempdir().expect("client home");
    let ledger = NamedTempFile::new().expect("tempfile for the offline ledger");
    seeded_ledger().save(ledger.path()).expect("seed the offline ledger");

    let show = run_foundry(
        home.path(),
        ledger.path(),
        DUMMY_ADDR,
        &["--offline", "queue", "show", "wi_preserved"],
    );
    assert_command_succeeded(&show);
    assert!(stdout_string(&show).contains("Worktree removed: no"));

    let open = run_foundry(home.path(), ledger.path(), DUMMY_ADDR, &["--offline", "queue", "open"]);
    assert_command_succeeded(&open);
    let out = stdout_string(&open);
    assert!(out.contains("wi_failed"));
    assert!(!out.contains("wi_running"));
}

#[test]
fn offline_over_a_missing_path_renders_empty_groups_and_exits_zero() {
    let home = tempfile::tempdir().expect("client home");
    let missing = home.path().join("absent/work-items.json");

    let output = run_foundry(home.path(), &missing, DUMMY_ADDR, &["--offline", "queue"]);
    assert_command_succeeded(&output);

    let out = stdout_string(&output);
    assert_eq!(out.matches("  (none)").count(), 4, "all four groups empty:\n{out}");
    assert!(!missing.exists(), "a read must not create the ledger");
}

#[test]
fn offline_over_a_malformed_file_exits_non_zero_with_the_parse_error() {
    let home = tempfile::tempdir().expect("client home");
    let ledger = NamedTempFile::new().expect("tempfile for the offline ledger");
    std::fs::write(ledger.path(), b"{ not json").expect("write a malformed ledger");

    let output = run_foundry(home.path(), ledger.path(), DUMMY_ADDR, &["--offline", "queue"]);

    assert!(!output.status.success(), "a malformed ledger must exit non-zero");
    let err = stderr_string(&output);
    assert!(err.contains("could not read the work-item ledger"), "got: {err}");
    assert!(err.contains("malformed JSON"), "the parse error must be named: {err}");
}

// ── help surface ──────────────────────────────────────────────────────────────

#[test]
fn foundry_help_lists_queue_and_names_its_three_forms() {
    let home = tempfile::tempdir().expect("client home");
    let output =
        run_foundry(home.path(), &client_ledger_path(home.path()), DUMMY_ADDR, &["--help"]);
    assert_command_succeeded(&output);

    let out = stdout_string(&output);
    for expected in ["queue", "queue show", "queue open"] {
        assert!(out.contains(expected), "`foundry --help` should name '{expected}':\n{out}");
    }
}

#[test]
fn queue_help_lists_the_three_forms_and_both_flags() {
    let home = tempfile::tempdir().expect("client home");
    let output = run_foundry(
        home.path(),
        &client_ledger_path(home.path()),
        DUMMY_ADDR,
        &["queue", "--help"],
    );
    assert_command_succeeded(&output);

    let out = stdout_string(&output);
    for expected in [
        "queue show",
        "queue open",
        "show",
        "open",
        "--json",
        "--offline",
    ] {
        assert!(
            out.contains(expected),
            "`foundry queue --help` should name '{expected}':\n{out}"
        );
    }
}
