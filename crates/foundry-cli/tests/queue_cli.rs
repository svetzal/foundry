//! Integration tests for `foundry queue` CLI behavior.
//!
//! These stand up a real `FoundryService` over tonic against a temporary
//! work-item ledger, so the online path is exercised end to end through the
//! `ListWorkItems` / `GetWorkItem` / `ListWorkItemEvents` RPCs rather than
//! against a stub. The daemon's events directory is written by the real
//! `EventWriter`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;
use foundry_engine::event_writer::EventWriter;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::WorkItemEventPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::throttle::Throttle;
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

/// Build a service over `work_items_path`, keeping its trace files and its
/// events directory (`state_dir/events`) under `state_dir`.
fn make_service(
    work_items_path: std::path::PathBuf,
    state_dir: &std::path::Path,
) -> FoundryService {
    let (event_tx, _rx) = broadcast::channel(64);
    let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));
    let traces = state_dir.join("traces");
    let trace_writer =
        Arc::new(TraceWriter::new(traces.to_str().expect("trace dir must be UTF-8")));
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
        events_dir: state_dir.join("events"),
        campaigns_path: std::path::PathBuf::new(),
        registry_path: std::path::PathBuf::new(),
        sentinels: Arc::new(RwLock::new(SentinelStore {
            version: 1,
            sentinels: vec![],
        })),
        sentinels_path: std::path::PathBuf::new(),
        scheduler_reload: Arc::new(Notify::new()),
    };

    FoundryService::new(ctx, stores)
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

/// One `work_item_*` event for the seeded item `id`, as it stood in `state`.
fn lifecycle_event(id: &str, event_type: EventType, state: WorkItemState, second: i64) -> Event {
    let subject = seeded_ledger()
        .items
        .into_iter()
        .find(|item| item.id == id)
        .expect("the id is seeded");
    let mut snapshot = subject.clone();
    snapshot.state = state;
    snapshot.reason = format!("{} for {id}", state.tag());
    let mut event = Event::new(
        event_type,
        subject.project.clone(),
        Throttle::Full,
        Event::serialize_payload(&WorkItemEventPayload::from_item(&snapshot))
            .expect("payload serializes"),
    );
    // A fixed id, so every call describes the very same logged event.
    event.id = format!("evt_{id}_{}", event.event_type.as_str());
    event.occurred_at = at(second);
    event.recorded_at = at(second);
    event.trace_id = subject.trace_id;
    event
}

/// The seeded `work_item_*` events, in the order the log is written. Every
/// seeded item shares one trace, so only the payload item id tells them apart.
fn seeded_events() -> Vec<Event> {
    vec![
        lifecycle_event(
            "wi_preserved",
            EventType::WorkItemSettled,
            WorkItemState::Preserved,
            5_000,
        ),
        lifecycle_event("wi_running", EventType::WorkItemStarted, WorkItemState::Running, 1_001),
        lifecycle_event(
            "wi_preserved",
            EventType::WorkItemSubmitted,
            WorkItemState::Submitted,
            1_000,
        ),
        lifecycle_event("wi_preserved", EventType::WorkItemStarted, WorkItemState::Running, 1_002),
    ]
}

/// The seeded events of `wi_preserved`, in chronological order.
fn preserved_events_in_order() -> Vec<Event> {
    let events = seeded_events();
    vec![events[2].clone(), events[3].clone(), events[0].clone()]
}

/// Write `events` into `events_dir` through the real `EventWriter`.
fn write_events(events_dir: &std::path::Path, events: &[Event]) {
    let writer = EventWriter::new(events_dir);
    for event in events {
        writer.write(event).expect("EventWriter writes the event");
    }
}

/// Stand up a daemon over a ledger seeded with one item in every state and an
/// events directory holding the seeded `work_item_*` events.
async fn daemon_over_seeded_ledger() -> (String, NamedTempFile, TempDir) {
    let ledger = NamedTempFile::new().expect("tempfile for the daemon ledger");
    seeded_ledger().save(ledger.path()).expect("seed the daemon ledger");
    let state = tempfile::tempdir().expect("tempdir for daemon state");
    write_events(&state.path().join("events"), &seeded_events());
    let service = make_service(ledger.path().to_path_buf(), state.path());
    let addr = start_server(service).await;
    (addr, ledger, state)
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

/// Run the CLI with an explicit client-side events directory.
fn run_foundry_with_events(
    home: &std::path::Path,
    work_items_path: &std::path::Path,
    events_dir: &std::path::Path,
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
        .env("FOUNDRY_EVENTS_DIR", events_dir)
        .output()
        .expect("run foundry binary")
}

/// The `Events:` block of `queue show` output: every line after the heading.
fn event_lines(out: &str) -> Vec<String> {
    let start = out
        .find("\nEvents:\n")
        .unwrap_or_else(|| panic!("no Events heading in:\n{out}"));
    out[start + "\nEvents:\n".len()..].lines().map(ToString::to_string).collect()
}

/// Assert each line carries the matching event's id and type, in order.
fn assert_event_lines(out: &str, expected: &[Event]) {
    let lines = event_lines(out);
    assert_eq!(lines.len(), expected.len(), "one line per event in:\n{out}");
    for (line, event) in lines.iter().zip(expected) {
        assert!(line.contains(&event.id), "'{}' missing from '{line}'", event.id);
        assert!(line.contains(&event.event_type.as_str()), "type missing from '{line}'");
    }
}

/// The `(id, event_type)` pairs of a `--json` `events` array, in order.
fn json_event_ids_and_types(parsed: &serde_json::Value) -> Vec<(String, String)> {
    parsed["events"]
        .as_array()
        .expect("an events array")
        .iter()
        .map(|event| {
            (
                event["id"].as_str().expect("id").to_string(),
                event["event_type"].as_str().expect("event_type").to_string(),
            )
        })
        .collect()
}

fn ids_and_types(events: &[Event]) -> Vec<(String, String)> {
    events
        .iter()
        .map(|event| (event.id.clone(), event.event_type.as_str()))
        .collect()
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

// ── item events (queue show) ──────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
async fn online_queue_show_lists_the_items_own_events_in_order() {
    let (addr, ledger, _state) = daemon_over_seeded_ledger().await;
    let home = tempfile::tempdir().expect("client home");

    let output = run_foundry(home.path(), ledger.path(), &addr, &["queue", "show", "wi_preserved"]);
    assert_command_succeeded(&output);
    let out = stdout_string(&output);

    assert!(out.starts_with("Id:               wi_preserved\n"), "record first:\n{out}");
    assert_event_lines(&out, &preserved_events_in_order());
    let other = &seeded_events()[1];
    assert!(
        !out.contains(&other.id),
        "another item's event on the same trace leaked:\n{out}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn online_queue_show_of_an_item_with_no_events_says_so() {
    let (addr, ledger, _state) = daemon_over_seeded_ledger().await;
    let home = tempfile::tempdir().expect("client home");

    let output = run_foundry(home.path(), ledger.path(), &addr, &["queue", "show", "wi_landed"]);
    assert_command_succeeded(&output);
    assert_eq!(event_lines(&stdout_string(&output)), vec!["  (no events)".to_string()]);
}

#[tokio::test(flavor = "multi_thread")]
async fn online_queue_show_json_adds_events_beside_the_unchanged_record_keys() {
    let (addr, ledger, _state) = daemon_over_seeded_ledger().await;
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

    let mut keys: Vec<&str> =
        parsed.as_object().expect("object").keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected_keys = vec![
        "events",
        "id",
        "kind",
        "lane",
        "objective",
        "origin",
        "preservation_ref",
        "project",
        "reason",
        "settled_at",
        "started_at",
        "state",
        "submitted_at",
        "trace_id",
        "verdict",
        "worktree",
        "worktree_removed",
    ];
    expected_keys.sort_unstable();
    assert_eq!(keys, expected_keys);
    assert_eq!(parsed["id"], serde_json::json!("wi_preserved"));
    assert_eq!(parsed["verdict"], serde_json::json!("remainder"));
    assert_eq!(parsed["worktree_removed"], serde_json::json!(false));
    assert_eq!(json_event_ids_and_types(&parsed), ids_and_types(&preserved_events_in_order()));
    assert_eq!(parsed["events"][2]["state"], serde_json::json!("preserved"));
}

#[tokio::test(flavor = "multi_thread")]
async fn online_queue_show_leaves_absent_client_ledger_and_events_paths_absent() {
    let (addr, _ledger, _state) = daemon_over_seeded_ledger().await;
    let home = tempfile::tempdir().expect("client home");
    let client_ledger = client_ledger_path(home.path());
    let client_events = home.path().join(".foundry/events");

    for args in [
        vec!["queue", "show", "wi_preserved"],
        vec!["queue", "show", "wi_preserved", "--json"],
    ] {
        let output =
            run_foundry_with_events(home.path(), &client_ledger, &client_events, &addr, &args);
        assert_command_succeeded(&output);
        assert!(
            stdout_string(&output).contains(&preserved_events_in_order()[0].id),
            "the daemon's events must be rendered"
        );
        assert!(!client_ledger.exists(), "{args:?} must leave the client ledger absent");
        assert!(!client_events.exists(), "{args:?} must leave the client events dir absent");
    }
}

#[test]
fn offline_queue_show_lists_the_same_events_in_the_same_order_from_files_alone() {
    let home = tempfile::tempdir().expect("client home");
    let ledger = NamedTempFile::new().expect("tempfile for the offline ledger");
    seeded_ledger().save(ledger.path()).expect("seed the offline ledger");
    let events_dir = home.path().join("events");
    write_events(&events_dir, &seeded_events());

    let human = run_foundry_with_events(
        home.path(),
        ledger.path(),
        &events_dir,
        DUMMY_ADDR,
        &["--offline", "queue", "show", "wi_preserved"],
    );
    assert_command_succeeded(&human);
    assert_event_lines(&stdout_string(&human), &preserved_events_in_order());

    let json = run_foundry_with_events(
        home.path(),
        ledger.path(),
        &events_dir,
        DUMMY_ADDR,
        &["--offline", "queue", "show", "wi_preserved", "--json"],
    );
    assert_command_succeeded(&json);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout_string(&json)).expect("--json output must parse");
    assert_eq!(json_event_ids_and_types(&parsed), ids_and_types(&preserved_events_in_order()));
}

#[test]
fn offline_queue_show_with_a_missing_events_dir_prints_no_events_and_creates_nothing() {
    let home = tempfile::tempdir().expect("client home");
    let ledger = NamedTempFile::new().expect("tempfile for the offline ledger");
    seeded_ledger().save(ledger.path()).expect("seed the offline ledger");
    let events_dir = home.path().join("absent-events");

    let output = run_foundry_with_events(
        home.path(),
        ledger.path(),
        &events_dir,
        DUMMY_ADDR,
        &["--offline", "queue", "show", "wi_preserved"],
    );
    assert_command_succeeded(&output);
    assert_eq!(event_lines(&stdout_string(&output)), vec!["  (no events)".to_string()]);
    assert!(!events_dir.exists(), "a read must not create the events directory");
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
