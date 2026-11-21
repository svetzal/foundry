//! State written by the chrono build loads unchanged and saves back
//! byte-for-byte.
//!
//! `tests/fixtures/chrono-era/` holds one file per persisted store, in the
//! shape the production daemon has on disk: the event JSONL, the work-item
//! ledger, campaigns (with initiatives and findings), the registry, sentinels,
//! pacing limits and pause state, and two trace files. The structure was
//! copied from production records and every value replaced with synthetic
//! data. The chrono build (Foundry 0.59.0) then loaded and saved each file
//! with the store's own `load`/`save`, so the bytes are what chrono wrote.
//! Timestamps cover every form that build produced: nanosecond, microsecond
//! and millisecond fractions, whole seconds, `Z` from serde and `+00:00` or
//! `-05:00` strings inside payloads.
//!
//! Each test loads a fixture with the jiff build, saves it the way the daemon
//! does, and requires the exact bytes back. Other readers (ops-visualizer and
//! scripts) parse these files, so no field may change format.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use foundry_sdk::campaign::CampaignStore;
use foundry_sdk::event::Event;
use foundry_sdk::pacing::{Limits, PauseState};
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::time::UtcDateTime;
use foundry_sdk::trace::ProcessResult;
use foundry_sdk::work_item::WorkItemStore;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/chrono-era")
        .join(name)
}

fn chrono_bytes(name: &str) -> String {
    std::fs::read_to_string(fixture(name)).expect("fixture is readable")
}

/// Save with `save` into a temp dir and return what landed on disk.
fn saved(name: &str, save: impl FnOnce(&Path)) -> String {
    let dir = tempfile::tempdir().expect("temp dir");
    let path = dir.path().join(name);
    save(&path);
    std::fs::read_to_string(path).expect("saved file is readable")
}

fn instant(text: &str) -> UtcDateTime {
    UtcDateTime::parse_from_rfc3339(text).expect("valid timestamp")
}

#[test]
fn work_item_ledger_round_trips_byte_for_byte() {
    let store = WorkItemStore::load(&fixture("work-items.json")).expect("chrono ledger loads");
    let written = saved("work-items.json", |path| store.save(path).expect("save"));
    assert_eq!(written, chrono_bytes("work-items.json"));

    // Whole-second and millisecond values keep their exact instants.
    let first = &store.items[0];
    assert_eq!(first.submitted_at, instant("2026-01-03T04:05:06Z"));
    assert_eq!(first.started_at, Some(instant("2026-01-03T04:05:07.120Z")));
    assert_eq!(
        first.settled_at.map(|at| at.to_rfc3339()),
        Some("2026-01-05T03:05:47.123456789+00:00".to_string()),
        "nanosecond fraction survives"
    );
}

#[test]
fn campaign_store_round_trips_byte_for_byte() {
    let store = CampaignStore::load(&fixture("campaigns.json")).expect("chrono campaigns load");
    assert!(!store.campaigns.is_empty() && !store.initiatives.is_empty());
    assert!(!store.findings.is_empty());
    let written = saved("campaigns.json", |path| store.save(path).expect("save"));
    assert_eq!(written, chrono_bytes("campaigns.json"));
}

#[test]
fn registry_round_trips_byte_for_byte() {
    let registry = Registry::load(&fixture("registry.json")).expect("chrono registry loads");
    let written = saved("registry.json", |path| registry.save(path).expect("save"));
    assert_eq!(written, chrono_bytes("registry.json"));
}

#[test]
fn sentinel_store_round_trips_byte_for_byte() {
    let store = SentinelStore::load(&fixture("sentinels.json")).expect("chrono sentinels load");
    let written = saved("sentinels.json", |path| store.save(path).expect("save"));
    assert_eq!(written, chrono_bytes("sentinels.json"));
}

#[test]
fn pacing_files_round_trip_byte_for_byte() {
    let state = PauseState::load(&fixture("pacing-state.json")).expect("pause state loads");
    let written = saved("pacing-state.json", |path| state.save(path).expect("save"));
    assert_eq!(written, chrono_bytes("pacing-state.json"));

    let limits = Limits::load(&fixture("pacing.json")).expect("limits load");
    assert_eq!(serde_json::to_string_pretty(&limits).unwrap(), chrono_bytes("pacing.json"));
}

#[test]
fn event_jsonl_round_trips_byte_for_byte() {
    // EventWriter writes `serde_json::to_string(event)` plus a newline.
    let chrono_text = chrono_bytes("events.jsonl");
    let mut written = String::new();
    for line in chrono_text.lines() {
        let event: Event = serde_json::from_str(line).expect("chrono event parses");
        written.push_str(&serde_json::to_string(&event).unwrap());
        written.push('\n');
    }
    assert_eq!(written, chrono_text);

    let shapes: Vec<&str> = chrono_text
        .lines()
        .filter_map(|line| line.split("\"occurred_at\":\"").nth(1))
        .filter_map(|rest| rest.split('"').next())
        .collect();
    assert!(shapes.iter().any(|at| at.len() == "2026-01-01T00:00:00.123456789Z".len()));
    assert!(shapes.iter().any(|at| at.len() == "2026-01-01T00:00:00.123456Z".len()));
    assert!(shapes.contains(&"2026-01-03T04:05:06Z"));
}

#[test]
fn trace_files_round_trip_byte_for_byte() {
    // TraceWriter writes `serde_json::to_string_pretty(result)`.
    for name in ["trace-0.json", "trace-1.json"] {
        let chrono_text = chrono_bytes(name);
        let trace: ProcessResult = serde_json::from_str(&chrono_text).expect("chrono trace parses");
        assert_eq!(serde_json::to_string_pretty(&trace).unwrap(), chrono_text, "{name}");
    }
}
