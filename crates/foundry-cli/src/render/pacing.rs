//! Pure rendering for the pacing stage — the `foundry pacing` views.
//!
//! Everything here takes an already-fetched [`PacingStatus`] and returns a
//! `String`. Nothing sorts: the daemon's `GetPacing` (and the same selection
//! offline) already lists running items oldest start first and waiting items
//! in the scheduler's priority order.

use std::fmt::Write as _;

use serde::Serialize;

use crate::proto::{PacingItem, PacingStatus};

/// The whole stage on one screen: the limits in force, what runs where,
/// what waits and why, and which lanes are paused.
#[must_use]
pub fn show(status: &PacingStatus) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Limits: {} of {} running on this host; one item per repository",
        status.running, status.max_running
    );
    let _ = writeln!(out, "Paused lanes: {}", paused_lanes(status));
    let _ = writeln!(out);
    section(&mut out, "Running", &status.running_items);
    section(&mut out, "Waiting", &status.waiting_items);
    out
}

/// The paused lanes as text, or `none`.
fn paused_lanes(status: &PacingStatus) -> String {
    if status.paused_lanes.is_empty() {
        "none".to_string()
    } else {
        status.paused_lanes.join(", ")
    }
}

/// Column widths for one rendered section, computed per section so a short
/// block does not inherit the padding of a wide one.
struct Widths {
    id: usize,
    project: usize,
    repository: usize,
    kind: usize,
    lane: usize,
    state: usize,
    since: usize,
}

impl Widths {
    fn of(items: &[PacingItem]) -> Self {
        let mut widths = Self {
            id: 0,
            project: 0,
            repository: 0,
            kind: 0,
            lane: 0,
            state: 0,
            since: 0,
        };
        for item in items {
            widths.id = widths.id.max(item.id.len());
            widths.project = widths.project.max(item.project.len());
            widths.repository = widths.repository.max(item.repository.len());
            widths.kind = widths.kind.max(item.kind.len());
            widths.lane = widths.lane.max(item.lane.len());
            widths.state = widths.state.max(item.state.len());
            widths.since = widths.since.max(item.since.len());
        }
        widths
    }
}

/// One item as a single line: id, project, the repository it occupies or
/// waits for, kind, lane, state, the timestamp that placed it there, then
/// what it occupies or why it waits.
fn item_line(item: &PacingItem, widths: &Widths) -> String {
    format!(
        "  {id:<id_w$}  {project:<project_w$}  {repository:<repository_w$}  {kind:<kind_w$}  {lane:<lane_w$}  {state:<state_w$}  {since:<since_w$}  {reason}",
        id = item.id,
        project = item.project,
        repository = item.repository,
        kind = item.kind,
        lane = item.lane,
        state = item.state,
        since = item.since,
        reason = item.reason,
        id_w = widths.id,
        project_w = widths.project,
        repository_w = widths.repository,
        kind_w = widths.kind,
        lane_w = widths.lane,
        state_w = widths.state,
        since_w = widths.since,
    )
}

/// One headed block of items, or `(none)` when the group is empty.
fn section(out: &mut String, heading: &str, items: &[PacingItem]) {
    let _ = writeln!(out, "{heading}");
    if items.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        let widths = Widths::of(items);
        for item in items {
            let _ = writeln!(out, "{}", item_line(item, &widths));
        }
    }
    let _ = writeln!(out);
}

/// JSON projection of one pacing item: the wire record's keys, unchanged.
#[derive(Serialize)]
struct JsonItem<'a> {
    id: &'a str,
    project: &'a str,
    repository: &'a str,
    kind: &'a str,
    lane: &'a str,
    state: &'a str,
    reason: &'a str,
    since: &'a str,
}

impl<'a> From<&'a PacingItem> for JsonItem<'a> {
    fn from(item: &'a PacingItem) -> Self {
        Self {
            id: &item.id,
            project: &item.project,
            repository: &item.repository,
            kind: &item.kind,
            lane: &item.lane,
            state: &item.state,
            reason: &item.reason,
            since: &item.since,
        }
    }
}

/// JSON projection of the stage: the same fields the human form shows.
#[derive(Serialize)]
struct JsonStatus<'a> {
    max_running: u64,
    running: u64,
    paused_lanes: &'a [String],
    running_items: Vec<JsonItem<'a>>,
    waiting_items: Vec<JsonItem<'a>>,
}

/// `pacing show --json`: one object carrying the limits, the counts, the
/// paused lanes and both item lists.
#[must_use]
pub fn show_json(status: &PacingStatus) -> String {
    let projected = JsonStatus {
        max_running: status.max_running,
        running: status.running,
        paused_lanes: &status.paused_lanes,
        running_items: status.running_items.iter().map(JsonItem::from).collect(),
        waiting_items: status.waiting_items.iter().map(JsonItem::from).collect(),
    };
    #[allow(
        clippy::expect_used,
        reason = "JsonStatus is strings, integers and vectors of strings, which are infallibly serializable"
    )]
    let mut out =
        serde_json::to_string_pretty(&projected).expect("JsonStatus is infallibly serializable");
    out.push('\n');
    out
}

/// Confirmation of a pause or resume: the lanes paused afterwards.
#[must_use]
pub fn lanes_notice(verb: &str, status: &PacingStatus) -> String {
    format!("{verb}; paused lanes: {}\n", paused_lanes(status))
}

/// One line for an item that settled while `pacing drain` waited.
#[must_use]
pub fn drained_line(id: &str, state: &str, reason: &str) -> String {
    format!("settled {id}: {state} — {reason}\n")
}

/// The lines `pacing drain` prints when it gives up: what still runs.
#[must_use]
pub fn still_running(items: &[PacingItem]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "still running after the timeout:");
    for item in items {
        let _ = writeln!(out, "  {}  {}  {}  {}", item.id, item.project, item.kind, item.reason);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, state: &str, reason: &str) -> PacingItem {
        PacingItem {
            id: id.to_string(),
            project: "alpha".to_string(),
            repository: "owner/alpha".to_string(),
            kind: "task".to_string(),
            lane: "interactive".to_string(),
            state: state.to_string(),
            reason: reason.to_string(),
            since: "2026-10-10T12:00:00+00:00".to_string(),
        }
    }

    fn status() -> PacingStatus {
        PacingStatus {
            max_running: 2,
            running: 1,
            paused_lanes: vec!["campaign".to_string()],
            running_items: vec![item("wi_run", "running", "running")],
            waiting_items: vec![
                item("wi_wait", "queued", "repository busy: wi_run"),
                item("wi_held", "held", "held by operator"),
            ],
        }
    }

    #[test]
    fn show_prints_limits_pauses_and_both_groups_with_reasons() {
        let out = show(&status());
        assert!(out.contains("Limits: 1 of 2 running on this host"), "got:\n{out}");
        assert!(out.contains("Paused lanes: campaign"), "got:\n{out}");
        assert!(out.contains("Running\n  wi_run"), "got:\n{out}");
        assert!(out.contains("owner/alpha"), "the repository is named:\n{out}");
        assert!(
            out.contains("wi_wait") && out.contains("repository busy: wi_run"),
            "got:\n{out}"
        );
        assert!(out.contains("wi_held") && out.contains("held by operator"), "got:\n{out}");
    }

    #[test]
    fn show_with_nothing_paused_or_waiting_says_so() {
        let empty = PacingStatus {
            max_running: 2,
            running: 0,
            paused_lanes: vec![],
            running_items: vec![],
            waiting_items: vec![],
        };
        let out = show(&empty);
        assert!(out.contains("Paused lanes: none"), "got:\n{out}");
        assert_eq!(out.matches("  (none)").count(), 2, "got:\n{out}");
    }

    #[test]
    fn json_carries_the_same_fields_as_the_human_form() {
        let parsed: serde_json::Value = serde_json::from_str(&show_json(&status())).unwrap();
        assert_eq!(parsed["max_running"], 2);
        assert_eq!(parsed["running"], 1);
        assert_eq!(parsed["paused_lanes"], serde_json::json!(["campaign"]));
        assert_eq!(parsed["running_items"][0]["id"], "wi_run");
        assert_eq!(parsed["running_items"][0]["repository"], "owner/alpha");
        assert_eq!(parsed["waiting_items"][0]["reason"], "repository busy: wi_run");
        assert_eq!(parsed["waiting_items"][1]["state"], "held");
    }

    #[test]
    fn drain_lines_name_what_settled_and_what_still_runs() {
        assert_eq!(drained_line("wi_a", "landed", "done"), "settled wi_a: landed — done\n");
        let out = still_running(&[item("wi_run", "running", "running")]);
        assert!(out.starts_with("still running after the timeout:\n"));
        assert!(out.contains("wi_run  alpha  task  running"));
        assert_eq!(lanes_notice("paused", &status()), "paused; paused lanes: campaign\n");
    }
}
