//! Binary-level proof of the `foundryd` single-instance guard and its
//! informational flags.
//!
//! The 2026-09-30 incident: `foundryd --version`, run beside a live daemon,
//! started a second daemon whose restart sweep settled a still-running task
//! `failed` before the process failed to bind and exited. These tests run the
//! real binary against a temporary Foundry home holding a running ledger item,
//! an interrupted maintenance cycle in the event log and a campaign store, and
//! check every byte of that home before and after.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;
use foundry_engine::event_writer::EventWriter;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{
    WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
};
use foundryd::proto::{StatusRequest, foundry_client::FoundryClient};
use fs2::FileExt;
use tempfile::TempDir;

const CYCLE_TRACE: &str = "c0ffee00c0ffee00c0ffee00c0ffee00";

/// A Foundry home in the shape the incident found: one task still running,
/// one maintenance cycle with no completion, and a campaign store.
struct Home {
    dir: TempDir,
    item_id: String,
}

impl Home {
    /// A machine where Foundry has never run: no `~/.foundry` at all.
    fn fresh() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("empty-path")).unwrap();
        Self {
            dir,
            item_id: String::new(),
        }
    }

    fn new() -> Self {
        let Self { dir, .. } = Self::fresh();
        let foundry = dir.path().join(".foundry");
        std::fs::create_dir_all(&foundry).unwrap();

        let mut store = WorkItemStore::default();
        let item = WorkItem::dispatched(
            WorkItemSpec {
                project: "alpha".to_string(),
                objective: "Add a --quiet flag.".to_string(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "foundry task".to_string(),
                trace_id: Some("a".repeat(32)),
            },
            Utc::now(),
        );
        let item_id = item.id.clone();
        store.upsert(item);
        store.save(&foundry.join("work-items.json")).unwrap();

        let cycle = Event::new(
            EventType::MaintenanceCycleStarted,
            "system".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some(CYCLE_TRACE.to_string()));
        EventWriter::new(foundry.join("events")).write(&cycle).unwrap();

        std::fs::write(
            foundry.join("campaigns.json"),
            "{\n  \"version\": 1,\n  \"campaigns\": []\n}\n",
        )
        .unwrap();

        Self { dir, item_id }
    }

    fn foundry(&self) -> PathBuf {
        self.dir.path().join(".foundry")
    }

    fn lock_path(&self) -> PathBuf {
        self.foundry().join("foundryd.lock")
    }

    /// Every path under the home, with file contents, so "changed nothing"
    /// covers files created as well as files rewritten.
    fn snapshot(&self) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut out = BTreeMap::new();
        walk(self.dir.path(), &mut out);
        out
    }

    fn command(&self, listen_addr: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_foundryd"));
        // No agent CLI or git on PATH: nothing a start does may reach out.
        let empty_path = self.dir.path().join("empty-path");
        command
            .env("HOME", self.dir.path())
            .env("XDG_CONFIG_HOME", self.dir.path().join(".config"))
            .env("PATH", &empty_path)
            .env("FOUNDRYD_LISTEN_ADDR", listen_addr)
            .env_remove("FOUNDRYD_LOCK_PATH")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null());
        for (key, _) in std::env::vars() {
            if key.starts_with("FOUNDRY_") {
                command.env_remove(key);
            }
        }
        command
    }
}

fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.insert(path.clone(), None);
            walk(&path, out);
        } else {
            out.insert(path.clone(), Some(std::fs::read(&path).unwrap()));
        }
    }
}

fn free_listen_addr() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}

/// Run to exit, failing the test rather than hanging if the process starts
/// serving instead of exiting.
fn run_to_exit(mut command: Command) -> Output {
    let mut child = command.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            // Best-effort: the assertion below is the failure being reported.
            let _ = child.kill();
            panic!("foundryd did not exit; it started serving instead");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// Hold the instance lock the way a live daemon does, with its pid recorded.
fn hold_lock(path: &Path, pid: u32) -> File {
    let file = File::create(path).unwrap();
    FileExt::lock_exclusive(&file).unwrap();
    std::fs::write(path, format!("{pid}\n")).unwrap();
    file
}

fn stderr_line(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    assert_eq!(stderr.lines().count(), 1, "one clear line on stderr, got: {stderr:?}");
    stderr
}

#[test]
fn a_second_start_while_the_lock_is_held_exits_non_zero_and_changes_nothing() {
    let home = Home::new();
    let _live = hold_lock(&home.lock_path(), 1_519_730);
    let before = home.snapshot();

    let output = run_to_exit(home.command(&free_listen_addr()));

    assert!(!output.status.success(), "a refused start must exit non-zero");
    let line = stderr_line(&output);
    assert!(line.contains("already running"), "{line}");
    assert!(line.contains("pid 1519730"), "names the running instance: {line}");
    assert_eq!(
        home.snapshot(),
        before,
        "the ledger, event log and campaign store are byte-identical"
    );
}

#[test]
fn a_second_start_on_a_bound_address_exits_non_zero_and_changes_nothing() {
    let home = Home::new();
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let before = home.snapshot();

    let output = run_to_exit(home.command(&taken.local_addr().unwrap().to_string()));

    assert!(!output.status.success());
    let line = stderr_line(&output);
    assert!(line.contains("address already in use"), "{line}");
    assert_eq!(home.snapshot(), before, "an address clash writes nothing, not even the lock");
}

/// The incident exactly: a live daemon holds both the lock and the address.
#[test]
fn a_second_start_beside_a_live_daemon_names_its_pid() {
    let home = Home::new();
    let _live = hold_lock(&home.lock_path(), 1_519_730);
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let before = home.snapshot();

    let output = run_to_exit(home.command(&taken.local_addr().unwrap().to_string()));

    assert!(!output.status.success());
    let line = stderr_line(&output);
    assert!(line.contains("foundryd pid 1519730 is running"), "{line}");
    assert_eq!(home.snapshot(), before);
}

#[test]
fn version_and_help_exit_zero_and_change_nothing_beside_a_live_daemon() {
    let home = Home::new();
    let _live = hold_lock(&home.lock_path(), 1_519_730);
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = taken.local_addr().unwrap().to_string();
    let before = home.snapshot();

    for flag in ["--version", "-V", "--help", "-h"] {
        let mut command = home.command(&addr);
        command.arg(flag);
        let output = run_to_exit(command);

        assert!(output.status.success(), "{flag} must exit 0: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("foundryd"), "{flag} prints: {stdout}");
        assert_eq!(home.snapshot(), before, "{flag} changed the Foundry home");
    }

    let mut version = home.command(&addr);
    version.arg("--version");
    let stdout = String::from_utf8(run_to_exit(version).stdout).unwrap();
    assert_eq!(stdout.trim(), format!("foundryd {}", env!("CARGO_PKG_VERSION")));
}

#[test]
fn version_on_a_fresh_machine_creates_no_foundry_home() {
    let home = Home::fresh();

    let mut command = home.command(&free_listen_addr());
    command.arg("--version");
    let output = run_to_exit(command);

    assert!(output.status.success());
    assert!(!home.foundry().exists(), "--version must not create ~/.foundry");
}

#[test]
fn unknown_flags_are_a_usage_error_and_change_nothing() {
    let home = Home::new();
    let before = home.snapshot();

    for arg in ["--verison", "--foreground", "start"] {
        let mut command = home.command(&free_listen_addr());
        command.arg(arg);
        let output = run_to_exit(command);

        assert!(!output.status.success(), "{arg} must be rejected");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("Usage"), "{arg} prints usage: {stderr}");
        assert_eq!(home.snapshot(), before, "{arg} changed the Foundry home");
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        // Best-effort: the daemon is torn down with the test either way.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn logged_events(events_dir: &Path) -> Vec<serde_json::Value> {
    let Ok(entries) = std::fs::read_dir(events_dir) else {
        return Vec::new();
    };
    entries
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .flat_map(|content| {
            content
                .lines()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

fn count(events: &[serde_json::Value], pred: impl Fn(&serde_json::Value) -> bool) -> usize {
    events.iter().filter(|e| pred(e)).count()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_normal_start_runs_each_recovery_sweep_exactly_once() {
    let home = Home::new();
    let addr = free_listen_addr();
    let child = home.command(&addr).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let pid = child.id();
    let _child = ChildGuard(child);

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(mut client) = FoundryClient::connect(format!("http://{addr}")).await
            && client
                .status(StatusRequest {
                    workflow_id: String::new(),
                })
                .await
                .is_ok()
        {
            break;
        }
        assert!(Instant::now() < deadline, "foundryd never started serving");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let events_dir = home.foundry().join("events");
    let is_cycle_completion = |e: &serde_json::Value| {
        e["event_type"] == "maintenance_cycle_completed" && e["trace_id"] == CYCLE_TRACE
    };
    let is_item_settled = |e: &serde_json::Value| {
        e["event_type"] == "work_item_settled" && e["payload"]["item_id"] == home.item_id.as_str()
    };
    while count(&logged_events(&events_dir), is_cycle_completion) == 0 {
        assert!(Instant::now() < deadline, "the interrupted cycle was never closed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // Room for a duplicate sweep to show up if one were going to.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let events = logged_events(&events_dir);
    assert_eq!(count(&events, is_item_settled), 1, "the restart sweep ran exactly once");
    assert_eq!(count(&events, is_cycle_completion), 1, "cycle recovery ran exactly once");

    let ledger = WorkItemStore::load(&home.foundry().join("work-items.json")).unwrap();
    assert_eq!(ledger.items[0].state, WorkItemState::Failed);
    assert_eq!(ledger.items[0].reason, "daemon restarted");

    let recorded = std::fs::read_to_string(home.lock_path()).unwrap();
    assert_eq!(recorded.trim(), pid.to_string(), "the running daemon records its pid");
}
