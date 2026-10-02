//! CLI handlers for the `foundry queue` subcommands — reads of the work-item
//! ledger.
//!
//! The three read commands are read-only. Owner controls require the daemon. The default online path is
//! daemon-authoritative: it renders `ListWorkItems` / `GetWorkItem` directly
//! and never reads, creates or mutates the client-side ledger file, so an
//! absent `FOUNDRY_WORK_ITEMS_PATH` stays absent. `--offline` is an explicit
//! recovery mode that reads `work-items.json` directly when `foundryd` is not
//! running.
//!
//! `queue show` prints the record followed by the item's own `work_item_*`
//! events, one line each. Online, the events come from `ListWorkItemEvents`;
//! `--offline` reads `FOUNDRY_EVENTS_DIR` directly through
//! [`foundry_sdk::work_item_events::read_work_item_events`], the same selection
//! the daemon applies — exact payload `item_id`, every monthly file whatever
//! its age, chronological order — so the two differ only in transport. The
//! online path never reads the client-side ledger or events files.

use std::path::Path;

use anyhow::{Context as _, Result};
use foundry_sdk::work_item::{WorkItem, WorkItemState, WorkItemStore};
use foundry_sdk::work_item_events::{WorkItemEventRecord, read_work_item_events};

use crate::daemon::{connect_daemon_required, status_to_anyhow};
use crate::proto::{
    GetWorkItemRequest, ListWorkItemEventsRequest, ListWorkItemsRequest, WorkItem as ProtoWorkItem,
    WorkItemEvent as ProtoWorkItemEvent,
};
use crate::render;

/// Which view a `foundry queue` invocation renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// All four groups on one screen.
    Overview,
    /// Only the items that still need a person.
    Open,
}

/// Render a fetched list in the requested view and output form.
///
/// The human and JSON forms render from the same fetched slice, so they can
/// never disagree about which items the daemon returned.
fn render_list(items: &[ProtoWorkItem], view: View, json: bool) -> String {
    match (view, json) {
        (View::Overview, false) => render::queue::queue_overview(items),
        (View::Overview, true) => render::queue::items_json(items),
        (View::Open, false) => render::queue::open_only(items),
        (View::Open, true) => render::queue::open_json(items),
    }
}

/// List the ledger in `view`.
pub async fn list(
    work_items_path: &Path,
    addr: &str,
    offline: bool,
    view: View,
    json: bool,
) -> Result<()> {
    let items = if offline {
        load_offline(work_items_path)?
    } else {
        fetch_online(addr, view).await?
    };

    print!("{}", render_list(&items, view, json));
    Ok(())
}

/// Fetch every ledger record from the daemon, in the RPC's documented order.
async fn fetch_online(addr: &str, view: View) -> Result<Vec<ProtoWorkItem>> {
    let mut client = connect_daemon_required(addr, &offline_hint(view_suffix(view))).await?;
    let response = client
        .list_work_items(ListWorkItemsRequest {
            project: String::new(),
            state: String::new(),
        })
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    Ok(response.items)
}

/// Show one item's full durable record, followed by its `work_item_*` events.
///
/// The human and JSON forms render from the same fetched record and events.
pub async fn show(
    work_items_path: &Path,
    events_dir: &Path,
    addr: &str,
    offline: bool,
    id: &str,
    json: bool,
) -> Result<()> {
    let (item, events) = if offline {
        load_one_offline(work_items_path, events_dir, id)?
    } else {
        fetch_one_online(addr, id).await?
    };

    if json {
        print!("{}", render::queue::item_with_events_json(&item, &events));
    } else {
        print!("{}", render::queue::item_with_events_detail(&item, &events));
    }
    Ok(())
}

/// Read one record and its events straight from the client-side files.
fn load_one_offline(
    work_items_path: &Path,
    events_dir: &Path,
    id: &str,
) -> Result<(ProtoWorkItem, Vec<ProtoWorkItemEvent>)> {
    let item = load_offline(work_items_path)?
        .into_iter()
        .find(|item| item.id == id)
        .with_context(|| format!("work item '{id}' not found"))?;
    let events = read_work_item_events(events_dir, id)
        .with_context(|| format!("could not read the events of work item '{id}'"))?
        .iter()
        .map(event_to_proto)
        .collect();
    Ok((item, events))
}

/// Fetch one record and its events from the daemon, surfacing `NOT_FOUND` as
/// the id-not-found error rather than as an empty record.
async fn fetch_one_online(
    addr: &str,
    id: &str,
) -> Result<(ProtoWorkItem, Vec<ProtoWorkItemEvent>)> {
    let not_found = |status: tonic::Status| {
        if status.code() == tonic::Code::NotFound {
            anyhow::anyhow!("work item '{id}' not found")
        } else {
            status_to_anyhow(status)
        }
    };
    let mut client = connect_daemon_required(addr, &offline_hint(&format!("show {id}"))).await?;
    let response = client
        .get_work_item(GetWorkItemRequest { id: id.to_string() })
        .await
        .map_err(not_found)?
        .into_inner();
    let item = response
        .item
        .with_context(|| format!("daemon returned no record for work item '{id}'"))?;

    let events = client
        .list_work_item_events(ListWorkItemEventsRequest { id: id.to_string() })
        .await
        .map_err(not_found)?
        .into_inner()
        .events;
    Ok((item, events))
}

/// The offline recovery command matching a failed online invocation.
fn offline_hint(command_suffix: &str) -> String {
    if command_suffix.is_empty() {
        "foundry queue --offline".to_string()
    } else {
        format!("foundry queue {command_suffix} --offline")
    }
}

/// The `foundry queue …` suffix a list view is invoked as.
fn view_suffix(view: View) -> &'static str {
    match view {
        View::Overview => "",
        View::Open => "open",
    }
}

/// Read the ledger file directly and put it in the order `ListWorkItems`
/// documents.
fn load_offline(work_items_path: &Path) -> Result<Vec<ProtoWorkItem>> {
    let store = WorkItemStore::load(work_items_path).with_context(|| {
        format!("could not read the work-item ledger at {}", work_items_path.display())
    })?;
    Ok(ordered(&store).iter().map(item_to_proto).collect())
}

/// Which ordering group an item's state puts it in.
///
/// Mirrors the group order the `ListWorkItems` contract documents so `--offline`
/// differs from the online path only in transport, never in reading order.
fn order_group(state: WorkItemState) -> u8 {
    match state {
        WorkItemState::Running => 0,
        WorkItemState::Submitted | WorkItemState::Queued => 1,
        WorkItemState::Preserved | WorkItemState::NeedsDecision | WorkItemState::Failed => 2,
        WorkItemState::Landed | WorkItemState::Cancelled => 3,
    }
}

/// The sort key for one item, as a tuple ordered exactly as the RPC contract
/// describes: group, then the timestamp that group reads on (negated for the
/// descending groups), then id ascending to break ties.
fn sort_key(item: &WorkItem) -> (u8, i64, &str) {
    let stamp = match item.state {
        WorkItemState::Running => item.started_at.map_or(0, |at| at.timestamp_micros()),
        WorkItemState::Submitted | WorkItemState::Queued => item.submitted_at.timestamp_micros(),
        WorkItemState::Preserved
        | WorkItemState::NeedsDecision
        | WorkItemState::Failed
        | WorkItemState::Landed
        | WorkItemState::Cancelled => -item.settled_at.map_or(0, |at| at.timestamp_micros()),
    };
    (order_group(item.state), stamp, item.id.as_str())
}

/// Every item in the store, in the RPC's deterministic order.
fn ordered(store: &WorkItemStore) -> Vec<WorkItem> {
    let mut items = store.items.clone();
    items.sort_by(|left, right| sort_key(left).cmp(&sort_key(right)));
    items
}

/// Wire form of one ledger record, matching what the daemon would have sent.
fn item_to_proto(item: &WorkItem) -> ProtoWorkItem {
    let disposition = item.disposition.as_ref();
    ProtoWorkItem {
        id: item.id.clone(),
        resumes: item.resumes.clone(),
        project: item.project.clone(),
        objective: item.objective.clone(),
        kind: item.kind.tag().to_string(),
        lane: item.lane.tag().to_string(),
        origin: item.origin.clone(),
        submitted_at: item.submitted_at.to_rfc3339(),
        started_at: item.started_at.map(|at| at.to_rfc3339()),
        settled_at: item.settled_at.map(|at| at.to_rfc3339()),
        state: item.state.tag().to_string(),
        reason: item.reason.clone(),
        trace_id: item.trace_id.clone(),
        verdict: disposition.and_then(|d| d.verdict.clone()),
        landed_commit: disposition.and_then(|d| d.landed_commit.clone()),
        preservation_ref: disposition.and_then(|d| d.preservation_ref.clone()),
        worktree: disposition.and_then(|d| d.worktree.clone()),
        worktree_removed: disposition.and_then(|d| d.worktree_removed),
        operator_action: item.operator_action.as_ref().map(|action| {
            crate::proto::WorkItemOperatorAction {
                command: action.command.clone(),
                origin: action.origin.clone(),
                previous_state: action.previous_state.tag().to_string(),
                previous_reason: action.previous_reason.clone(),
                previous_settled_at: action.previous_settled_at.map(|at| at.to_rfc3339()),
            }
        }),
    }
}

/// Wire form of one `work_item_*` event, matching what the daemon would have
/// sent.
fn event_to_proto(record: &WorkItemEventRecord) -> ProtoWorkItemEvent {
    ProtoWorkItemEvent {
        id: record.event_id.clone(),
        event_type: record.event_type.as_str(),
        occurred_at: record.occurred_at.to_rfc3339(),
        state: record.payload.state.tag().to_string(),
        reason: record.payload.reason.clone(),
        trace_id: record.trace_id.clone(),
    }
}

/// Ask the daemon to settle exactly one item; owner controls have no offline path.
pub async fn cancel_item(
    addr: &str,
    offline: bool,
    id: &str,
    reason: Option<&str>,
    origin: Option<&str>,
) -> Result<()> {
    anyhow::ensure!(!offline, "queue close/cancel require foundryd; --offline is not supported");
    let mut client = crate::daemon::connect_daemon_online(addr).await?;
    let operator_origin = crate::origin::local_operator_origin(origin);
    let item = if let Some(reason) = reason {
        client
            .close_work_item(crate::proto::CloseWorkItemRequest {
                id: id.to_string(),
                reason: reason.to_string(),
                operator_origin,
            })
            .await
            .map_err(status_to_anyhow)?
            .into_inner()
            .item
    } else {
        client
            .cancel_work_item(crate::proto::CancelWorkItemRequest {
                id: id.to_string(),
                operator_origin,
            })
            .await
            .map_err(status_to_anyhow)?
            .into_inner()
            .item
    }
    .context("daemon returned no cancelled work item")?;
    print!("{}", render::queue::cancellation_notice(&item));
    Ok(())
}

/// Resume preserved work via the daemon; never read or write client stores.
pub async fn resume_item(addr: &str, offline: bool, id: &str, origin: Option<&str>) -> Result<()> {
    anyhow::ensure!(!offline, "queue resume requires foundryd; --offline is not supported");
    let mut client = crate::daemon::connect_daemon_online(addr).await?;
    let item = client
        .resume_work_item(crate::proto::ResumeWorkItemRequest {
            id: id.to_string(),
            operator_origin: crate::origin::local_operator_origin(origin),
        })
        .await
        .map_err(status_to_anyhow)?
        .into_inner()
        .item
        .context("daemon returned no resumed work item")?;
    print!("{}", render::queue::item_detail(&item));
    Ok(())
}

/// Run the canonical reconciler on the daemon and print its invocation-specific report.
pub async fn reconcile(addr: &str, offline: bool) -> Result<()> {
    anyhow::ensure!(
        !offline,
        "foundry queue reconcile requires the daemon; --offline is not supported"
    );
    let mut client = crate::daemon::connect_daemon_online(addr).await?;
    let response = client
        .reconcile_work(crate::proto::ReconcileWorkRequest {})
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    print!("{}", response.markdown);
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use foundry_sdk::work_item::{
        WorkDisposition, WorkItemKind, WorkItemSpec, WorkItemStore, WorkLane,
    };
    use tempfile::NamedTempFile;

    use super::*;

    fn at(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("in-range timestamp")
    }

    fn item(id: &str, project: &str, state: WorkItemState, settled: i64) -> WorkItem {
        let mut item = WorkItem::submitted(
            WorkItemSpec {
                project: project.to_string(),
                objective: "do the thing".to_string(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "cli".to_string(),
                trace_id: None,
            },
            at(1_000),
        );
        item.id = id.to_string();
        item.state = state;
        item.reason = format!("{} reason", state.tag());
        item.started_at = Some(at(1_001));
        if state != WorkItemState::Running
            && state != WorkItemState::Submitted
            && state != WorkItemState::Queued
        {
            item.settled_at = Some(at(settled));
        }
        item
    }

    fn store(items: Vec<WorkItem>) -> WorkItemStore {
        WorkItemStore { version: 1, items }
    }

    fn ids(items: &[ProtoWorkItem]) -> Vec<&str> {
        items.iter().map(|item| item.id.as_str()).collect()
    }

    // ── offline ordering ──────────────────────────────────────────────────────

    #[test]
    fn offline_ordering_matches_the_documented_group_order() {
        let store = store(vec![
            item("wi_landed", "alpha", WorkItemState::Landed, 2_000),
            item("wi_failed", "alpha", WorkItemState::Failed, 2_000),
            item("wi_queued", "alpha", WorkItemState::Queued, 0),
            item("wi_running", "alpha", WorkItemState::Running, 0),
        ]);
        let ordered = ordered(&store);
        let order: Vec<&str> = ordered.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(order, vec!["wi_running", "wi_queued", "wi_failed", "wi_landed"]);
    }

    #[test]
    fn offline_ordering_puts_the_newest_settled_item_first() {
        let store = store(vec![
            item("wi_old", "alpha", WorkItemState::Landed, 2_000),
            item("wi_new", "alpha", WorkItemState::Landed, 3_000),
        ]);
        let order: Vec<String> = ordered(&store).iter().map(|item| item.id.clone()).collect();
        assert_eq!(order, vec!["wi_new", "wi_old"]);
    }

    #[test]
    fn offline_ordering_breaks_equal_timestamps_by_id_ascending() {
        let store = store(vec![
            item("wi_b", "alpha", WorkItemState::Landed, 2_000),
            item("wi_a", "alpha", WorkItemState::Landed, 2_000),
        ]);
        let order: Vec<String> = ordered(&store).iter().map(|item| item.id.clone()).collect();
        assert_eq!(order, vec!["wi_a", "wi_b"]);
    }

    #[test]
    fn every_state_lands_in_exactly_one_offline_ordering_group() {
        assert_eq!(order_group(WorkItemState::Running), 0);
        assert_eq!(order_group(WorkItemState::Submitted), 1);
        assert_eq!(order_group(WorkItemState::Queued), 1);
        assert_eq!(order_group(WorkItemState::Preserved), 2);
        assert_eq!(order_group(WorkItemState::NeedsDecision), 2);
        assert_eq!(order_group(WorkItemState::Failed), 2);
        assert_eq!(order_group(WorkItemState::Landed), 3);
        assert_eq!(order_group(WorkItemState::Cancelled), 3);
    }

    // ── offline loading ───────────────────────────────────────────────────────

    #[test]
    fn offline_load_of_a_missing_path_is_an_empty_ledger() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("nope/work-items.json");
        let items = load_offline(&missing).expect("a missing ledger is empty, not a fault");
        assert!(items.is_empty());
    }

    #[test]
    fn offline_overview_of_a_missing_path_renders_four_empty_groups() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rendered = render_list(
            &load_offline(&dir.path().join("absent.json")).expect("empty ledger"),
            View::Overview,
            false,
        );
        assert_eq!(rendered.matches("  (none)").count(), 4, "got:\n{rendered}");
    }

    #[test]
    fn offline_load_of_a_malformed_ledger_is_an_error_naming_the_parse_failure() {
        let tmp = NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), b"{ not json").expect("write malformed ledger");

        let err = load_offline(tmp.path()).expect_err("a malformed ledger must fail");
        let chain = format!("{err:#}");
        assert!(chain.contains("could not read the work-item ledger"), "got: {chain}");
        assert!(chain.contains("malformed JSON"), "the parse failure must be named: {chain}");
    }

    #[test]
    fn offline_load_converts_every_settlement_field_to_its_wire_form() {
        let mut settled = item("wi_preserved", "alpha", WorkItemState::Preserved, 2_000);
        settled.trace_id = Some("a".repeat(32));
        settled.disposition = Some(WorkDisposition {
            task_branch: None,
            branch_cleanup: Vec::new(),
            verdict: Some("remainder".to_string()),
            landed_commit: None,
            preservation_ref: Some("foundry/wip".to_string()),
            worktree: Some("/tmp/wt".to_string()),
            worktree_removed: Some(false),
        });

        let tmp = NamedTempFile::new().expect("tempfile");
        store(vec![settled]).save(tmp.path()).expect("save ledger");

        let items = load_offline(tmp.path()).expect("load ledger");
        let wire = items.first().expect("one item");
        assert_eq!(wire.verdict.as_deref(), Some("remainder"));
        assert_eq!(wire.preservation_ref.as_deref(), Some("foundry/wip"));
        assert_eq!(wire.worktree.as_deref(), Some("/tmp/wt"));
        assert_eq!(wire.worktree_removed, Some(false));
        assert_eq!(wire.landed_commit, None, "an unrecorded field stays absent");
        assert_eq!(wire.kind, "task");
        assert_eq!(wire.lane, "interactive");
    }

    #[test]
    fn offline_load_leaves_an_unsettled_item_with_no_settlement_fields() {
        let tmp = NamedTempFile::new().expect("tempfile");
        store(vec![item("wi_running", "alpha", WorkItemState::Running, 0)])
            .save(tmp.path())
            .expect("save ledger");

        let items = load_offline(tmp.path()).expect("load ledger");
        let wire = items.first().expect("one item");
        assert_eq!(wire.settled_at, None);
        assert_eq!(wire.verdict, None);
        assert_eq!(wire.worktree_removed, None);
    }

    // ── offline command paths ─────────────────────────────────────────────────

    #[tokio::test]
    async fn offline_list_never_contacts_the_daemon() {
        let tmp = NamedTempFile::new().expect("tempfile");
        store(vec![item("wi_running", "alpha", WorkItemState::Running, 0)])
            .save(tmp.path())
            .expect("save ledger");

        // An unroutable address: reaching for it would fail the test.
        list(tmp.path(), "http://127.0.0.1:0", true, View::Overview, false)
            .await
            .expect("offline overview should succeed");
        list(tmp.path(), "http://127.0.0.1:0", true, View::Open, true)
            .await
            .expect("offline open --json should succeed");
    }

    #[tokio::test]
    async fn offline_show_finds_a_seeded_id_and_rejects_an_unknown_one() {
        let tmp = NamedTempFile::new().expect("tempfile");
        let dir = tempfile::tempdir().expect("tempdir");
        let events_dir = dir.path().join("events");
        store(vec![item("wi_running", "alpha", WorkItemState::Running, 0)])
            .save(tmp.path())
            .expect("save ledger");

        show(tmp.path(), &events_dir, "http://127.0.0.1:0", true, "wi_running", false)
            .await
            .expect("offline show should succeed");

        let err = show(tmp.path(), &events_dir, "http://127.0.0.1:0", true, "wi_missing", false)
            .await
            .expect_err("an unknown id must fail");
        assert!(err.to_string().contains("work item 'wi_missing' not found"), "got: {err}");
    }

    #[test]
    fn offline_show_of_an_unreadable_events_log_is_an_error_not_no_events() {
        let tmp = NamedTempFile::new().expect("tempfile");
        store(vec![item("wi_running", "alpha", WorkItemState::Running, 0)])
            .save(tmp.path())
            .expect("save ledger");
        // The ledger file itself stands where the events directory should be,
        // so the events directory cannot be listed.
        let err = load_one_offline(tmp.path(), tmp.path(), "wi_running")
            .expect_err("an unreadable events log must fail");
        assert!(
            format!("{err:#}").contains("could not read the events of work item 'wi_running'"),
            "got: {err:#}"
        );
    }

    #[test]
    fn offline_show_of_a_missing_events_dir_is_the_record_with_no_events() {
        let tmp = NamedTempFile::new().expect("tempfile");
        store(vec![item("wi_running", "alpha", WorkItemState::Running, 0)])
            .save(tmp.path())
            .expect("save ledger");
        let dir = tempfile::tempdir().expect("tempdir");
        let (record, events) =
            load_one_offline(tmp.path(), &dir.path().join("absent"), "wi_running")
                .expect("a missing events dir is empty, not a fault");
        assert_eq!(record.id, "wi_running");
        assert!(events.is_empty());
    }

    // ── view plumbing ─────────────────────────────────────────────────────────

    #[test]
    fn the_open_view_renders_only_open_ids_in_both_output_forms() {
        let items: Vec<ProtoWorkItem> = ordered(&store(vec![
            item("wi_running", "alpha", WorkItemState::Running, 0),
            item("wi_failed", "beta", WorkItemState::Failed, 2_000),
            item("wi_landed", "beta", WorkItemState::Landed, 2_000),
        ]))
        .iter()
        .map(item_to_proto)
        .collect();
        assert_eq!(ids(&items), vec!["wi_running", "wi_failed", "wi_landed"]);

        for json in [false, true] {
            let rendered = render_list(&items, View::Open, json);
            assert!(rendered.contains("wi_failed"), "got:\n{rendered}");
            assert!(!rendered.contains("wi_running"), "got:\n{rendered}");
            assert!(!rendered.contains("wi_landed"), "got:\n{rendered}");
        }
    }

    #[test]
    fn the_overview_and_its_json_form_carry_the_same_ids() {
        let items: Vec<ProtoWorkItem> = ordered(&store(vec![
            item("wi_running", "alpha", WorkItemState::Running, 0),
            item("wi_failed", "beta", WorkItemState::Failed, 2_000),
        ]))
        .iter()
        .map(item_to_proto)
        .collect();

        let human = render_list(&items, View::Overview, false);
        let json = render_list(&items, View::Overview, true);
        for id in ["wi_running", "wi_failed"] {
            assert!(human.contains(id), "human form missing {id}");
            assert!(json.contains(id), "JSON form missing {id}");
        }
    }

    // ── offline hints ─────────────────────────────────────────────────────────

    #[test]
    fn each_command_names_its_own_offline_recovery_form() {
        assert_eq!(offline_hint(view_suffix(View::Overview)), "foundry queue --offline");
        assert_eq!(offline_hint(view_suffix(View::Open)), "foundry queue open --offline");
        assert_eq!(offline_hint("show wi_abc"), "foundry queue show wi_abc --offline");
    }
}
