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
use foundry_sdk::work_source::{WorkSource, WorkSourceKind};

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

/// An exact source to select on, as `--source <kind>:<ref>` names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFilter {
    /// The source kind.
    pub kind: WorkSourceKind,
    /// The reference: a campaign or sentinel name, a host, or a parent item id.
    pub reference: String,
}

impl SourceFilter {
    /// Parse `<kind>:<ref>`. The kind is one of the closed set of tags and the
    /// ref is nonblank; anything else is the operator's mistake, named.
    pub fn parse(text: &str) -> Result<Self> {
        let (kind, reference) = text.split_once(':').with_context(|| {
            format!("--source takes <kind>:<ref>, e.g. campaign:tidy-cli; got '{text}'")
        })?;
        let kind = WorkSourceKind::from_tag(kind).with_context(|| {
            format!("unknown work-source kind '{kind}'; expected one of {}", known_source_kinds())
        })?;
        anyhow::ensure!(
            !reference.trim().is_empty(),
            "--source names a ref beside its kind, as <kind>:<ref>; got '{text}'"
        );
        Ok(Self {
            kind,
            reference: reference.to_string(),
        })
    }

    /// The exact cycles of the campaign `name`.
    #[must_use]
    pub fn campaign(name: &str) -> Self {
        Self {
            kind: WorkSourceKind::Campaign,
            reference: name.to_string(),
        }
    }

    /// Whether a wire record's recorded source is this one. A record with no
    /// source never matches.
    fn matches(&self, item: &ProtoWorkItem) -> bool {
        item.source
            .as_ref()
            .is_some_and(|source| source.kind == self.kind.tag() && source.r#ref == self.reference)
    }

    /// The `--source <kind>:<ref>` text that names this filter.
    fn flag_text(&self) -> String {
        format!("--source {}:{}", self.kind.tag(), self.reference)
    }
}

/// The source kinds an operator may name, for error messages.
fn known_source_kinds() -> String {
    WorkSourceKind::ALL.iter().map(|kind| kind.tag()).collect::<Vec<_>>().join(", ")
}

/// List the ledger in `view`, optionally only the items from one source.
pub async fn list(
    work_items_path: &Path,
    addr: &str,
    offline: bool,
    view: View,
    json: bool,
    source: Option<&str>,
) -> Result<()> {
    let source = source.map(SourceFilter::parse).transpose()?;
    let items = if offline {
        filter_by_source(load_offline(work_items_path)?, source.as_ref())
    } else {
        fetch_online(addr, view, source.as_ref()).await?
    };

    print!("{}", render_list(&items, view, json));
    Ok(())
}

/// Only the items whose recorded source is `source`, in the order given;
/// every item when there is no filter.
///
/// The offline counterpart of the daemon's source filter, so `--offline`
/// differs from the online path only in transport.
pub(crate) fn filter_by_source(
    items: Vec<ProtoWorkItem>,
    source: Option<&SourceFilter>,
) -> Vec<ProtoWorkItem> {
    match source {
        Some(filter) => items.into_iter().filter(|item| filter.matches(item)).collect(),
        None => items,
    }
}

/// Fetch the ledger records from the daemon, in the RPC's documented order,
/// optionally selected by source.
async fn fetch_online(
    addr: &str,
    view: View,
    source: Option<&SourceFilter>,
) -> Result<Vec<ProtoWorkItem>> {
    let suffix = match source {
        Some(filter) => format!("{} {}", view_suffix(view), filter.flag_text()),
        None => view_suffix(view).to_string(),
    };
    let mut client = connect_daemon_required(addr, &offline_hint(suffix.trim())).await?;
    let response = client.list_work_items(list_request(source)).await.map_err(status_to_anyhow)?;
    Ok(response.into_inner().items)
}

/// The `ListWorkItems` request selecting every project and state, and
/// `source` when given.
pub(crate) fn list_request(source: Option<&SourceFilter>) -> ListWorkItemsRequest {
    ListWorkItemsRequest {
        project: String::new(),
        state: String::new(),
        source_kind: source.map(|filter| filter.kind.tag().to_string()).unwrap_or_default(),
        source_ref: source.map(|filter| filter.reference.clone()).unwrap_or_default(),
    }
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
pub(crate) fn load_offline(work_items_path: &Path) -> Result<Vec<ProtoWorkItem>> {
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
        WorkItemState::Submitted | WorkItemState::Queued | WorkItemState::Held => 1,
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
        WorkItemState::Submitted | WorkItemState::Queued | WorkItemState::Held => {
            item.submitted_at.timestamp_micros()
        }
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
        source: item.source.as_ref().map(source_to_proto),
        depends_on: item.depends_on.clone(),
        not_before: item.not_before.map(|at| at.to_rfc3339()),
    }
}

/// Wire form of a typed source, matching what the daemon would have sent.
fn source_to_proto(source: &WorkSource) -> crate::proto::WorkSource {
    crate::proto::WorkSource {
        kind: source.kind.tag().to_string(),
        r#ref: source.reference.clone(),
        cycle: source.cycle,
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
///
/// `after` and `not_before` are the owner's pacing constraints for the new
/// item: the ids it waits on, and the earliest time (RFC 3339, or a duration
/// from now such as `2h`) the scheduler may start it.
pub async fn resume_item(
    addr: &str,
    offline: bool,
    id: &str,
    origin: Option<&str>,
    after: &[String],
    not_before: Option<&str>,
) -> Result<()> {
    anyhow::ensure!(!offline, "queue resume requires foundryd; --offline is not supported");
    let not_before = not_before
        .map(|text| crate::commands::parse_not_before(text, chrono::Utc::now()))
        .transpose()?;
    let mut client = crate::daemon::connect_daemon_online(addr).await?;
    let item = client
        .resume_work_item(crate::proto::ResumeWorkItemRequest {
            id: id.to_string(),
            operator_origin: crate::origin::local_operator_origin(origin),
            depends_on: after.to_vec(),
            not_before: not_before.map(|at| at.to_rfc3339()).unwrap_or_default(),
        })
        .await
        .map_err(status_to_anyhow)?
        .into_inner()
        .item
        .context("daemon returned no resumed work item")?;
    print!("{}", render::queue::item_detail(&item));
    Ok(())
}

/// Take a queued item out of the scheduler's hands, or give it back
/// (`release` also returns an item the scheduler moved to `needs_decision`
/// for a dependency). Daemon only; no offline path.
pub async fn hold_or_release_item(
    addr: &str,
    offline: bool,
    id: &str,
    origin: Option<&str>,
    release: bool,
) -> Result<()> {
    anyhow::ensure!(!offline, "queue hold/release require foundryd; --offline is not supported");
    let mut client = crate::daemon::connect_daemon_online(addr).await?;
    let operator_origin = crate::origin::local_operator_origin(origin);
    let item = if release {
        client
            .release_work_item(crate::proto::ReleaseWorkItemRequest {
                id: id.to_string(),
                operator_origin,
            })
            .await
            .map_err(status_to_anyhow)?
            .into_inner()
            .item
    } else {
        client
            .hold_work_item(crate::proto::HoldWorkItemRequest {
                id: id.to_string(),
                operator_origin,
            })
            .await
            .map_err(status_to_anyhow)?
            .into_inner()
            .item
    }
    .context("daemon returned no work item")?;
    print!("{}", render::queue::transition_notice(&item));
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
        assert_eq!(order_group(WorkItemState::Held), 1);
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
        list(tmp.path(), "http://127.0.0.1:0", true, View::Overview, false, None)
            .await
            .expect("offline overview should succeed");
        list(tmp.path(), "http://127.0.0.1:0", true, View::Open, true, None)
            .await
            .expect("offline open --json should succeed");
        list(
            tmp.path(),
            "http://127.0.0.1:0",
            true,
            View::Overview,
            false,
            Some("operator:desk"),
        )
        .await
        .expect("offline overview with a source filter should succeed");
    }

    // ── source filter ─────────────────────────────────────────────────────────

    #[test]
    fn a_source_filter_parses_kind_colon_ref_and_names_what_it_rejects() {
        assert_eq!(
            SourceFilter::parse("campaign:tidy-cli").expect("valid"),
            SourceFilter::campaign("tidy-cli")
        );
        assert_eq!(
            SourceFilter::parse("work_item:wi_abc").expect("valid").kind,
            WorkSourceKind::WorkItem
        );
        let no_colon = SourceFilter::parse("campaign").expect_err("no ref");
        assert!(no_colon.to_string().contains("<kind>:<ref>"), "got: {no_colon}");
        let unknown = SourceFilter::parse("dashboard:x").expect_err("unknown kind");
        assert!(
            unknown.to_string().contains("campaign, sentinel, operator, work_item"),
            "got: {unknown}"
        );
        assert!(SourceFilter::parse("sentinel:").is_err(), "a blank ref names nothing");
    }

    #[test]
    fn a_list_request_carries_the_filter_or_leaves_both_fields_empty() {
        let bare = list_request(None);
        assert_eq!((bare.source_kind.as_str(), bare.source_ref.as_str()), ("", ""));
        let filtered = list_request(Some(&SourceFilter::campaign("tidy-cli")));
        assert_eq!(
            (filtered.source_kind.as_str(), filtered.source_ref.as_str()),
            ("campaign", "tidy-cli")
        );
    }

    #[test]
    fn offline_source_filtering_keeps_only_that_source_in_the_given_order() {
        let mut cycle_two = item("wi_c2", "alpha", WorkItemState::Running, 0);
        cycle_two.source = Some(WorkSource::campaign("tidy-cli", 2));
        let mut cycle_one = item("wi_c1", "alpha", WorkItemState::Landed, 2_000);
        cycle_one.source = Some(WorkSource::campaign("tidy-cli", 1));
        let mut nightly = item("wi_nightly", "alpha", WorkItemState::Running, 0);
        nightly.source = Some(WorkSource::sentinel("nightly-maintenance"));
        let unsourced = item("wi_old", "alpha", WorkItemState::Running, 0);
        let items: Vec<ProtoWorkItem> =
            ordered(&store(vec![cycle_one, cycle_two, nightly, unsourced]))
                .iter()
                .map(item_to_proto)
                .collect();

        let cycles = filter_by_source(items.clone(), Some(&SourceFilter::campaign("tidy-cli")));
        assert_eq!(ids(&cycles), vec!["wi_c2", "wi_c1"]);
        assert_eq!(cycles[1].source.as_ref().map(|s| s.cycle), Some(Some(1)));

        assert_eq!(filter_by_source(items.clone(), None).len(), 4, "no filter, every item");
        assert!(
            filter_by_source(items, Some(&SourceFilter::campaign("tidy"))).is_empty(),
            "the ref is an exact match, not a prefix"
        );
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
        assert_eq!(
            offline_hint(&SourceFilter::campaign("tidy-cli").flag_text()),
            "foundry queue --source campaign:tidy-cli --offline"
        );
    }
}
