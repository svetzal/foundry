//! Pure rendering for the work-item ledger — the `foundry queue` views.
//!
//! Everything here takes already-fetched [`WorkItem`] wire records and returns
//! a `String`. Nothing in this module sorts: the daemon's `ListWorkItems`
//! contract already returns items in the reading order an operator wants, and
//! re-sorting here would silently disagree with it. The only arrangement these
//! functions apply is *grouping by state*, which partitions the daemon order
//! without disturbing the relative order inside any group.

use std::fmt::Write as _;

use foundry_sdk::work_item::WorkItemState;
use serde::Serialize;

use crate::proto::{WorkItem, WorkItemEvent};

/// How many settled items the overview shows.
///
/// The settled group is unbounded on disk and grows forever, so the overview
/// shows only the newest page of it. The daemon returns settled items newest
/// first, so this is a prefix rather than a selection.
pub const SETTLED_SHOWN: usize = 20;

/// Which of the four reading groups an item belongs to.
///
/// This mirrors the ordering groups the `ListWorkItems` contract documents, so
/// grouping the daemon's response by this never reorders anything within a
/// group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    /// An agent is working on it.
    Running,
    /// Admitted but not started.
    Queued,
    /// Settled, but still holding an obligation for a person.
    Open,
    /// Done with.
    Settled,
}

/// The group a serialized state tag puts an item in.
///
/// An unrecognised tag is treated as *open*: a state this build does not know
/// about is exactly the kind of thing a person should look at, so it is
/// surfaced rather than dropped from every group.
fn group_of(state: &str) -> Group {
    match WorkItemState::from_tag(state) {
        Some(WorkItemState::Running) => Group::Running,
        Some(WorkItemState::Submitted | WorkItemState::Queued) => Group::Queued,
        Some(WorkItemState::Landed | WorkItemState::Cancelled) => Group::Settled,
        Some(WorkItemState::Preserved | WorkItemState::NeedsDecision | WorkItemState::Failed)
        | None => Group::Open,
    }
}

/// The timestamp that placed an item in its group.
///
/// One rule per group, derived from the group rather than chosen by the caller,
/// so every view shows the same timestamp for the same item.
fn group_stamp(item: &WorkItem) -> &str {
    match group_of(&item.state) {
        Group::Running => item.started_at.as_deref().unwrap_or("-"),
        Group::Queued => item.submitted_at.as_str(),
        Group::Open | Group::Settled => item.settled_at.as_deref().unwrap_or("-"),
    }
}

/// The four groups of one fetched item list, each in the order it arrived.
#[derive(Debug, Default)]
pub struct Groups<'a> {
    /// Items an agent is working on.
    pub running: Vec<&'a WorkItem>,
    /// Items admitted but not started.
    pub queued: Vec<&'a WorkItem>,
    /// Settled items that still need a person.
    pub open: Vec<&'a WorkItem>,
    /// Terminal items, newest first, capped at [`SETTLED_SHOWN`].
    pub settled: Vec<&'a WorkItem>,
}

/// Partition `items` into the four reading groups, preserving the given order.
///
/// The settled group is truncated to [`SETTLED_SHOWN`]; every other group is
/// complete. Truncation is a prefix of the daemon's newest-first settled
/// order, so it drops the oldest terminal items rather than an arbitrary
/// subset.
#[must_use]
pub fn group(items: &[WorkItem]) -> Groups<'_> {
    let mut groups = Groups::default();
    for item in items {
        match group_of(&item.state) {
            Group::Running => groups.running.push(item),
            Group::Queued => groups.queued.push(item),
            Group::Open => groups.open.push(item),
            Group::Settled => {
                if groups.settled.len() < SETTLED_SHOWN {
                    groups.settled.push(item);
                }
            }
        }
    }
    groups
}

/// Column widths for one rendered section.
///
/// Computed per section so a section of short ids does not inherit the padding
/// of a wide one, and so the reason column always starts at a predictable
/// place within the block a reader is scanning.
struct Widths {
    id: usize,
    project: usize,
    kind: usize,
    lane: usize,
    state: usize,
    stamp: usize,
}

impl Widths {
    fn of(items: &[&WorkItem]) -> Self {
        let mut widths = Self {
            id: 0,
            project: 0,
            kind: 0,
            lane: 0,
            state: 0,
            stamp: 0,
        };
        for item in items {
            widths.id = widths.id.max(item.id.len());
            widths.project = widths.project.max(item.project.len());
            widths.kind = widths.kind.max(item.kind.len());
            widths.lane = widths.lane.max(item.lane.len());
            widths.state = widths.state.max(item.state.len());
            widths.stamp = widths.stamp.max(group_stamp(item).len());
        }
        widths
    }
}

/// One item as a single unwrapped line: id, project, kind, lane, state, the
/// timestamp that placed it in its group, then the one-line reason.
fn item_line(item: &WorkItem, widths: &Widths) -> String {
    format!(
        "  {id:<id_w$}  {project:<project_w$}  {kind:<kind_w$}  {lane:<lane_w$}  {state:<state_w$}  {stamp:<stamp_w$}  {reason}",
        id = item.id,
        project = item.project,
        kind = item.kind,
        lane = item.lane,
        state = item.state,
        stamp = group_stamp(item),
        reason = item.reason,
        id_w = widths.id,
        project_w = widths.project,
        kind_w = widths.kind,
        lane_w = widths.lane,
        state_w = widths.state,
        stamp_w = widths.stamp,
    )
}

/// One headed block of items, or `(none)` when the group is empty.
fn section(out: &mut String, heading: &str, items: &[&WorkItem]) {
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

/// Heading for the running group.
pub const RUNNING_HEADING: &str = "Running";
/// Heading for the queued group.
pub const QUEUED_HEADING: &str = "Queued";
/// Heading for the open group.
pub const OPEN_HEADING: &str = "Open — needs a person";

/// Heading for the settled group, naming the cap it applies.
fn settled_heading() -> String {
    format!("Settled (last {SETTLED_SHOWN})")
}

/// The whole queue on one screen: running, queued, open, then the newest
/// settled items.
#[must_use]
pub fn queue_overview(items: &[WorkItem]) -> String {
    let groups = group(items);
    let mut out = String::new();
    section(&mut out, RUNNING_HEADING, &groups.running);
    section(&mut out, QUEUED_HEADING, &groups.queued);
    section(&mut out, OPEN_HEADING, &groups.open);
    section(&mut out, &settled_heading(), &groups.settled);
    out
}

/// Only the open group — the items that still hold an obligation.
#[must_use]
pub fn open_only(items: &[WorkItem]) -> String {
    let groups = group(items);
    let mut out = String::new();
    section(&mut out, OPEN_HEADING, &groups.open);
    out
}

/// Append one labelled field, skipping it entirely when the value is absent.
///
/// Absent means absent: an optional the ledger never recorded produces no line
/// at all, so a reader never has to tell a recorded empty string from a field
/// that was never set.
fn optional_field(out: &mut String, label: &str, value: Option<&str>) {
    if let Some(value) = value {
        let _ = writeln!(out, "{label:<18}{value}");
    }
}

/// One item's full durable record, one field per line.
///
/// Every settlement field is rendered only when the ledger recorded it. That
/// includes `worktree_removed`, where a recorded `false` and "never recorded"
/// are different facts: the first prints `no`, the second prints nothing.
#[must_use]
pub fn item_detail(item: &WorkItem) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{:<18}{}", "Id:", item.id);
    let _ = writeln!(out, "{:<18}{}", "Project:", item.project);
    let _ = writeln!(out, "{:<18}{}", "Objective:", item.objective);
    let _ = writeln!(out, "{:<18}{}", "Kind:", item.kind);
    let _ = writeln!(out, "{:<18}{}", "Lane:", item.lane);
    let _ = writeln!(out, "{:<18}{}", "Origin:", item.origin);
    let _ = writeln!(out, "{:<18}{}", "State:", item.state);
    let _ = writeln!(out, "{:<18}{}", "Reason:", item.reason);
    let _ = writeln!(out, "{:<18}{}", "Submitted:", item.submitted_at);
    optional_field(&mut out, "Started:", item.started_at.as_deref());
    optional_field(&mut out, "Settled:", item.settled_at.as_deref());
    optional_field(&mut out, "Trace:", item.trace_id.as_deref());
    optional_field(&mut out, "Verdict:", item.verdict.as_deref());
    optional_field(&mut out, "Landed commit:", item.landed_commit.as_deref());
    optional_field(&mut out, "Preservation ref:", item.preservation_ref.as_deref());
    optional_field(&mut out, "Worktree:", item.worktree.as_deref());
    optional_field(
        &mut out,
        "Worktree removed:",
        item.worktree_removed.map(|removed| if removed { "yes" } else { "no" }),
    );
    out
}

/// The heading above an item's events in `queue show`.
pub const EVENTS_HEADING: &str = "Events:";

/// The line printed under [`EVENTS_HEADING`] when the item has no events, so
/// an empty history reads as a stated fact rather than as nothing.
pub const NO_EVENTS_LINE: &str = "  (no events)";

/// One `work_item_*` event as a single line: when, what, the state it left the
/// item in, the event id and why.
#[must_use]
pub fn event_line(event: &WorkItemEvent) -> String {
    format!(
        "  {}  {:<19}  {:<14}  {}  {}",
        event.occurred_at, event.event_type, event.state, event.id, event.reason
    )
}

/// An item's events under their heading, one line each in the order they
/// arrived, or [`NO_EVENTS_LINE`] when there are none.
#[must_use]
pub fn events_section(events: &[WorkItemEvent]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{EVENTS_HEADING}");
    if events.is_empty() {
        let _ = writeln!(out, "{NO_EVENTS_LINE}");
    }
    for event in events {
        let _ = writeln!(out, "{}", event_line(event));
    }
    out
}

/// `queue show`: one item's full durable record, a blank line, then its events.
#[must_use]
pub fn item_with_events_detail(item: &WorkItem, events: &[WorkItemEvent]) -> String {
    format!("{}\n{}", item_detail(item), events_section(events))
}

/// JSON projection of one `work_item_*` event.
#[derive(Serialize)]
struct JsonEvent<'a> {
    id: &'a str,
    event_type: &'a str,
    occurred_at: &'a str,
    state: &'a str,
    reason: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_id: Option<&'a str>,
}

impl<'a> From<&'a WorkItemEvent> for JsonEvent<'a> {
    fn from(event: &'a WorkItemEvent) -> Self {
        Self {
            id: &event.id,
            event_type: &event.event_type,
            occurred_at: &event.occurred_at,
            state: &event.state,
            reason: &event.reason,
            trace_id: event.trace_id.as_deref(),
        }
    }
}

/// One item's record with every existing key unchanged, plus its `events`.
#[derive(Serialize)]
struct JsonItemWithEvents<'a> {
    #[serde(flatten)]
    item: JsonItem<'a>,
    events: Vec<JsonEvent<'a>>,
}

/// JSON projection of one wire record.
///
/// The prost-generated [`WorkItem`] carries no `Serialize`, so the JSON shape
/// is stated once here. Every optional is skipped when unset, which is what
/// makes an absent field distinguishable from a recorded empty string or
/// `false` after a round trip.
#[derive(Serialize)]
struct JsonItem<'a> {
    id: &'a str,
    project: &'a str,
    objective: &'a str,
    kind: &'a str,
    lane: &'a str,
    origin: &'a str,
    submitted_at: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    settled_at: Option<&'a str>,
    state: &'a str,
    reason: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    trace_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    verdict: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    landed_commit: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    preservation_ref: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree_removed: Option<bool>,
}

impl<'a> From<&'a WorkItem> for JsonItem<'a> {
    fn from(item: &'a WorkItem) -> Self {
        Self {
            id: &item.id,
            project: &item.project,
            objective: &item.objective,
            kind: &item.kind,
            lane: &item.lane,
            origin: &item.origin,
            submitted_at: &item.submitted_at,
            started_at: item.started_at.as_deref(),
            settled_at: item.settled_at.as_deref(),
            state: &item.state,
            reason: &item.reason,
            trace_id: item.trace_id.as_deref(),
            verdict: item.verdict.as_deref(),
            landed_commit: item.landed_commit.as_deref(),
            preservation_ref: item.preservation_ref.as_deref(),
            worktree: item.worktree.as_deref(),
            worktree_removed: item.worktree_removed,
        }
    }
}

/// Serialize a borrowed JSON projection.
///
/// [`JsonItem`] is strings, options of strings and one option of `bool`, none
/// of which `serde_json` can fail on, and the failure it would report has no
/// caller-actionable form.
#[allow(
    clippy::expect_used,
    reason = "JsonItem is strings and plain options, which are infallibly serializable"
)]
fn to_pretty<T: Serialize>(value: &T) -> String {
    let mut out = serde_json::to_string_pretty(value).expect("JsonItem is infallibly serializable");
    out.push('\n');
    out
}

/// Every fetched item as a JSON array, in the order it arrived.
///
/// No grouping and no cap: the JSON form is the fetched data, and a consumer
/// that wants groups applies [`group`]'s rules itself.
#[must_use]
pub fn items_json(items: &[WorkItem]) -> String {
    let projected: Vec<JsonItem<'_>> = items.iter().map(JsonItem::from).collect();
    to_pretty(&projected)
}

/// Only the open group as a JSON array, in the order it arrived.
#[must_use]
pub fn open_json(items: &[WorkItem]) -> String {
    let groups = group(items);
    let projected: Vec<JsonItem<'_>> = groups.open.into_iter().map(JsonItem::from).collect();
    to_pretty(&projected)
}

/// `queue show --json`: one item as a JSON object carrying every record key
/// [`items_json`] gives it, plus an `events` array in the order it arrived.
#[must_use]
pub fn item_with_events_json(item: &WorkItem, events: &[WorkItemEvent]) -> String {
    to_pretty(&JsonItemWithEvents {
        item: JsonItem::from(item),
        events: events.iter().map(JsonEvent::from).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal wire record in `state`, with the timestamps that state implies.
    fn item(id: &str, state: &str) -> WorkItem {
        let settled =
            matches!(state, "landed" | "cancelled" | "preserved" | "needs_decision" | "failed");
        WorkItem {
            id: id.to_string(),
            project: "alpha".to_string(),
            objective: "do the thing".to_string(),
            kind: "task".to_string(),
            lane: "interactive".to_string(),
            origin: "cli".to_string(),
            submitted_at: "2026-09-30T01:00:00+00:00".to_string(),
            started_at: Some("2026-09-30T01:00:01+00:00".to_string()),
            settled_at: settled.then(|| "2026-09-30T01:00:02+00:00".to_string()),
            state: state.to_string(),
            reason: format!("{state} reason"),
            trace_id: None,
            verdict: None,
            landed_commit: None,
            preservation_ref: None,
            worktree: None,
            worktree_removed: None,
        }
    }

    fn ids(items: &[&WorkItem]) -> Vec<String> {
        items.iter().map(|item| item.id.clone()).collect()
    }

    /// One item in every state, in the order `ListWorkItems` would return them.
    fn every_state() -> Vec<WorkItem> {
        vec![
            item("wi_running", "running"),
            item("wi_submitted", "submitted"),
            item("wi_queued", "queued"),
            item("wi_preserved", "preserved"),
            item("wi_needs", "needs_decision"),
            item("wi_failed", "failed"),
            item("wi_landed", "landed"),
            item("wi_cancelled", "cancelled"),
        ]
    }

    // ── grouping ──────────────────────────────────────────────────────────────

    #[test]
    fn the_four_groups_hold_exactly_the_expected_ids_in_daemon_order() {
        let items = every_state();
        let groups = group(&items);

        assert_eq!(ids(&groups.running), vec!["wi_running"]);
        assert_eq!(ids(&groups.queued), vec!["wi_submitted", "wi_queued"]);
        assert_eq!(ids(&groups.open), vec!["wi_preserved", "wi_needs", "wi_failed"]);
        assert_eq!(ids(&groups.settled), vec!["wi_landed", "wi_cancelled"]);
    }

    #[test]
    fn grouping_never_reorders_within_a_group() {
        // Deliberately reverse-alphabetical: grouping must not restore order.
        let items = vec![
            item("wi_z", "running"),
            item("wi_a", "running"),
            item("wi_m", "running"),
        ];
        let groups = group(&items);
        assert_eq!(ids(&groups.running), vec!["wi_z", "wi_a", "wi_m"]);
    }

    #[test]
    fn an_unrecognised_state_is_surfaced_as_open_rather_than_dropped() {
        let items = vec![item("wi_strange", "quantum_superposition")];
        let groups = group(&items);
        assert_eq!(ids(&groups.open), vec!["wi_strange"]);
        assert!(groups.running.is_empty());
        assert!(groups.queued.is_empty());
        assert!(groups.settled.is_empty());
    }

    #[test]
    fn twenty_five_terminal_items_yield_only_the_twenty_newest_ids() {
        // Newest first, as the daemon returns settled items.
        let items: Vec<WorkItem> =
            (0..25).map(|n| item(&format!("wi_t{n:02}"), "landed")).collect();
        let groups = group(&items);

        let shown = ids(&groups.settled);
        assert_eq!(shown.len(), SETTLED_SHOWN);
        assert_eq!(shown.first().map(String::as_str), Some("wi_t00"));
        assert_eq!(shown.last().map(String::as_str), Some("wi_t19"));
        for dropped in ["wi_t20", "wi_t21", "wi_t22", "wi_t23", "wi_t24"] {
            assert!(!shown.contains(&dropped.to_string()), "{dropped} must be omitted");
        }
    }

    #[test]
    fn the_settled_cap_never_truncates_the_other_three_groups() {
        let mut items: Vec<WorkItem> =
            (0..25).map(|n| item(&format!("wi_r{n:02}"), "running")).collect();
        items.extend((0..25).map(|n| item(&format!("wi_f{n:02}"), "failed")));
        let groups = group(&items);
        assert_eq!(groups.running.len(), 25);
        assert_eq!(groups.open.len(), 25);
    }

    // ── overview rendering ────────────────────────────────────────────────────

    #[test]
    fn the_overview_prints_every_id_under_its_own_heading_in_order() {
        let items = every_state();
        let out = queue_overview(&items);

        let headings = [
            RUNNING_HEADING.to_string(),
            QUEUED_HEADING.to_string(),
            OPEN_HEADING.to_string(),
            settled_heading(),
        ];
        let mut cursor = 0usize;
        for heading in &headings {
            let at = out[cursor..]
                .find(heading.as_str())
                .unwrap_or_else(|| panic!("heading '{heading}' missing from:\n{out}"));
            cursor += at + heading.len();
        }

        for expected in [
            ("wi_running", RUNNING_HEADING.to_string()),
            ("wi_queued", QUEUED_HEADING.to_string()),
            ("wi_failed", OPEN_HEADING.to_string()),
            ("wi_landed", settled_heading()),
        ] {
            let (id, heading) = expected;
            let start = out.find(heading.as_str()).expect("heading present");
            let rest = &out[start..];
            let id_at = rest.find(id).unwrap_or_else(|| panic!("{id} missing after {heading}"));
            // The id must appear before the next blank-line section break.
            let break_at = rest.find("\n\n").unwrap_or(rest.len());
            assert!(id_at < break_at, "{id} should sit under '{heading}' in:\n{out}");
        }
    }

    #[test]
    fn an_overview_line_carries_id_project_kind_lane_state_stamp_and_reason() {
        let out = queue_overview(&[item("wi_one", "running")]);
        let line = out
            .lines()
            .find(|line| line.contains("wi_one"))
            .expect("the running line must be present");
        for field in [
            "wi_one",
            "alpha",
            "task",
            "interactive",
            "running",
            "2026-09-30T01:00:01+00:00",
            "running reason",
        ] {
            assert!(line.contains(field), "line '{line}' should carry '{field}'");
        }
    }

    #[test]
    fn an_empty_group_renders_as_none_rather_than_a_bare_heading() {
        let out = queue_overview(&[]);
        assert_eq!(out.matches("  (none)").count(), 4, "all four groups empty:\n{out}");
    }

    #[test]
    fn a_running_item_with_no_started_at_shows_a_dash_for_its_stamp() {
        let mut running = item("wi_nostart", "running");
        running.started_at = None;
        let out = queue_overview(&[running]);
        let line = out.lines().find(|line| line.contains("wi_nostart")).expect("line present");
        assert!(line.contains(" - "), "line '{line}' should show a dash stamp");
    }

    // ── queue open ────────────────────────────────────────────────────────────

    #[test]
    fn open_only_carries_the_open_ids_and_none_of_the_running_or_landed_ids() {
        let out = open_only(&every_state());
        for present in ["wi_preserved", "wi_needs", "wi_failed"] {
            assert!(out.contains(present), "{present} must appear in:\n{out}");
        }
        for absent in [
            "wi_running",
            "wi_submitted",
            "wi_queued",
            "wi_landed",
            "wi_cancelled",
        ] {
            assert!(!out.contains(absent), "{absent} must not appear in:\n{out}");
        }
    }

    #[test]
    fn open_only_prints_no_other_heading() {
        let out = open_only(&every_state());
        assert!(out.contains(OPEN_HEADING));
        assert!(!out.contains(RUNNING_HEADING));
        assert!(!out.contains(QUEUED_HEADING));
    }

    // ── detail rendering ──────────────────────────────────────────────────────

    #[test]
    fn detail_of_an_unsettled_item_omits_every_settlement_field() {
        let out = item_detail(&item("wi_running", "running"));

        assert!(out.contains("Id:               wi_running"), "got:\n{out}");
        assert!(out.contains("Project:          alpha"), "got:\n{out}");
        for omitted in [
            "Settled:",
            "Trace:",
            "Verdict:",
            "Landed commit:",
            "Preservation ref:",
            "Worktree:",
            "Worktree removed:",
        ] {
            assert!(!out.contains(omitted), "'{omitted}' must be absent from:\n{out}");
        }
        assert!(!out.contains("false"), "no optional may render as 'false':\n{out}");
        for line in out.lines() {
            assert!(!line.ends_with(':'), "line '{line}' rendered a label with no value");
        }
    }

    #[test]
    fn detail_renders_every_recorded_settlement_field() {
        let mut settled = item("wi_preserved", "preserved");
        settled.trace_id = Some("a".repeat(32));
        settled.verdict = Some("remainder".to_string());
        settled.landed_commit = Some("deadbeef".to_string());
        settled.preservation_ref = Some("foundry/wip".to_string());
        settled.worktree = Some("/tmp/wt".to_string());
        settled.worktree_removed = Some(true);

        let out = item_detail(&settled);
        for field in [
            "Verdict:          remainder",
            "Landed commit:    deadbeef",
            "Preservation ref: foundry/wip",
            "Worktree:         /tmp/wt",
            "Worktree removed: yes",
        ] {
            assert!(out.contains(field), "'{field}' missing from:\n{out}");
        }
        assert!(out.contains(&format!("Trace:            {}", "a".repeat(32))), "got:\n{out}");
    }

    #[test]
    fn detail_prints_a_recorded_false_worktree_removed_as_no() {
        let mut settled = item("wi_landed", "landed");
        settled.worktree = Some("/tmp/wt".to_string());
        settled.worktree_removed = Some(false);

        let out = item_detail(&settled);
        assert!(out.contains("Worktree removed: no"), "got:\n{out}");
        assert!(!out.contains("false"), "a recorded false must read as 'no':\n{out}");
    }

    // ── JSON rendering ────────────────────────────────────────────────────────

    #[test]
    fn items_json_round_trips_ids_and_states_in_the_fetched_order() {
        let items = every_state();
        let parsed: serde_json::Value =
            serde_json::from_str(&items_json(&items)).expect("items_json must parse");
        let array = parsed.as_array().expect("an array");

        assert_eq!(array.len(), items.len());
        for (rendered, source) in array.iter().zip(&items) {
            assert_eq!(rendered["id"], serde_json::json!(source.id));
            assert_eq!(rendered["state"], serde_json::json!(source.state));
            assert_eq!(rendered["project"], serde_json::json!(source.project));
        }
    }

    #[test]
    fn items_json_is_not_capped_or_grouped() {
        let items: Vec<WorkItem> =
            (0..25).map(|n| item(&format!("wi_t{n:02}"), "landed")).collect();
        let parsed: serde_json::Value = serde_json::from_str(&items_json(&items)).expect("parse");
        assert_eq!(parsed.as_array().expect("array").len(), 25);
    }

    #[test]
    fn json_omits_absent_optionals_rather_than_emitting_empty_or_false() {
        let mut running = item("wi_running", "running");
        running.started_at = None;
        let parsed: serde_json::Value = serde_json::from_str(&item_with_events_json(&running, &[]))
            .expect("item_with_events_json must parse");
        let object = parsed.as_object().expect("an object");

        for absent in [
            "started_at",
            "settled_at",
            "trace_id",
            "verdict",
            "landed_commit",
            "preservation_ref",
            "worktree",
            "worktree_removed",
        ] {
            assert!(!object.contains_key(absent), "'{absent}' must be absent from {parsed}");
        }
    }

    #[test]
    fn json_keeps_a_recorded_false_worktree_removed() {
        let mut settled = item("wi_landed", "landed");
        settled.worktree = Some("/tmp/wt".to_string());
        settled.worktree_removed = Some(false);
        let parsed: serde_json::Value =
            serde_json::from_str(&item_with_events_json(&settled, &[])).expect("parse");
        assert_eq!(parsed["worktree_removed"], serde_json::json!(false));
    }

    #[test]
    fn open_json_carries_only_the_open_ids() {
        let parsed: serde_json::Value =
            serde_json::from_str(&open_json(&every_state())).expect("open_json must parse");
        let found: Vec<&str> = parsed
            .as_array()
            .expect("array")
            .iter()
            .map(|item| item["id"].as_str().expect("id is a string"))
            .collect();
        assert_eq!(found, vec!["wi_preserved", "wi_needs", "wi_failed"]);
    }

    #[test]
    fn every_json_document_ends_with_exactly_one_newline() {
        for rendered in [
            items_json(&every_state()),
            open_json(&every_state()),
            item_with_events_json(&item("wi_one", "running"), &[]),
        ] {
            assert!(rendered.ends_with('\n'), "must end with a newline: {rendered}");
            assert!(!rendered.ends_with("\n\n"), "must not end with a blank line");
        }
    }

    // ── operator origin ───────────────────────────────────────────────────────

    #[test]
    fn show_json_emits_the_origin_exactly_as_stored() {
        let mut stored = item("wi_1", "running");
        stored.origin = "foundry task (host workbench: asked by Stacey)".to_string();

        let parsed: serde_json::Value =
            serde_json::from_str(&item_with_events_json(&stored, &[])).expect("valid JSON");

        assert_eq!(parsed["origin"], "foundry task (host workbench: asked by Stacey)");
    }

    #[test]
    fn item_detail_prints_the_origin_exactly_as_stored() {
        let mut stored = item("wi_1", "running");
        stored.origin = "campaign tidy-cli cycle 4 (host workbench: by hand)".to_string();

        assert!(
            item_detail(&stored).contains("campaign tidy-cli cycle 4 (host workbench: by hand)")
        );
    }

    // ── item events ───────────────────────────────────────────────────────────

    fn event(id: &str, event_type: &str, state: &str, second: u32) -> WorkItemEvent {
        WorkItemEvent {
            id: id.to_string(),
            event_type: event_type.to_string(),
            occurred_at: format!("2026-09-30T01:00:{second:02}+00:00"),
            state: state.to_string(),
            reason: format!("{state} because"),
            trace_id: Some("b".repeat(32)),
        }
    }

    fn three_events() -> Vec<WorkItemEvent> {
        vec![
            event("evt_1", "work_item_submitted", "submitted", 1),
            event("evt_2", "work_item_started", "running", 2),
            event("evt_3", "work_item_settled", "landed", 3),
        ]
    }

    #[test]
    fn an_event_line_carries_time_type_state_id_and_reason() {
        assert_eq!(
            event_line(&event("evt_1", "work_item_settled", "landed", 3)),
            "  2026-09-30T01:00:03+00:00  work_item_settled    landed          evt_1  landed because"
        );
    }

    #[test]
    fn the_events_section_prints_one_line_per_event_in_the_order_given() {
        let out = events_section(&three_events());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 4, "heading plus three events:\n{out}");
        assert_eq!(lines[0], EVENTS_HEADING);
        for (line, id) in lines[1..].iter().zip(["evt_1", "evt_2", "evt_3"]) {
            assert!(line.contains(id), "'{id}' missing from '{line}'");
        }
        assert!(!out.contains(NO_EVENTS_LINE), "got:\n{out}");
    }

    #[test]
    fn an_item_with_no_events_says_so_explicitly() {
        assert_eq!(events_section(&[]), format!("{EVENTS_HEADING}\n{NO_EVENTS_LINE}\n"));
    }

    #[test]
    fn show_detail_is_the_record_then_a_blank_line_then_the_events() {
        let record = item("wi_landed", "landed");
        let out = item_with_events_detail(&record, &three_events());
        let expected = format!("{}\n{}", item_detail(&record), events_section(&three_events()));
        assert_eq!(out, expected);
    }

    #[test]
    fn show_json_keeps_every_record_key_unchanged_and_adds_events() {
        let mut record = item("wi_landed", "landed");
        record.worktree = Some("/tmp/wt".to_string());
        record.worktree_removed = Some(false);
        let listed: serde_json::Value =
            serde_json::from_str(&items_json(std::slice::from_ref(&record))).expect("parses");
        let without = &listed[0];
        let with: serde_json::Value =
            serde_json::from_str(&item_with_events_json(&record, &three_events()))
                .expect("item_with_events_json parses");

        let with_object = with.as_object().expect("object");
        for (key, value) in without.as_object().expect("object") {
            assert_eq!(with_object.get(key), Some(value), "key '{key}' changed");
        }
        assert_eq!(with_object.len(), without.as_object().expect("object").len() + 1);

        let events = with["events"].as_array().expect("events array");
        let ids: Vec<&str> = events.iter().map(|e| e["id"].as_str().expect("id")).collect();
        assert_eq!(ids, vec!["evt_1", "evt_2", "evt_3"]);
        assert_eq!(events[2]["event_type"], serde_json::json!("work_item_settled"));
        assert_eq!(events[2]["state"], serde_json::json!("landed"));
        assert_eq!(events[2]["trace_id"], serde_json::json!("b".repeat(32)));
    }

    #[test]
    fn show_json_of_an_item_with_no_events_carries_an_empty_array() {
        let parsed: serde_json::Value =
            serde_json::from_str(&item_with_events_json(&item("wi_one", "running"), &[]))
                .expect("parses");
        assert_eq!(parsed["events"], serde_json::json!([]));
    }
}
