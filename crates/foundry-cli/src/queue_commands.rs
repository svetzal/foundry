//! CLI handlers for the `foundry queue` subcommands — reads of the work-item
//! ledger.
//!
//! The three read commands are read-only. Owner controls require the daemon.
//! The default online path is daemon-authoritative: it renders
//! `ListWorkItems` / `GetWorkItem` directly and never reads, creates or mutates
//! the client-side ledger file, so an absent `FOUNDRY_WORK_ITEMS_PATH` stays
//! absent. `--offline` is an explicit
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
//!
//! The overview and `open` also list every escalated campaign, because an
//! escalated campaign waits on its owner as much as a preserved item does. A
//! campaign escalation is campaign state, not executed work, so it is never
//! written into the ledger: online it comes from `ListCampaigns`, offline from
//! the campaign store and the event log, and it renders as its own row kind
//! (and its own JSON array) beside the ledger items.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context as _, Result};
use foundry_sdk::work_item::{WorkItem, WorkItemState, WorkItemStore};
use foundry_sdk::work_item_events::{WorkItemEventRecord, read_work_item_events};
use foundry_sdk::work_source::{WorkSource, WorkSourceKind};

use crate::daemon::{connect_daemon_required, status_to_anyhow};
use crate::proto::{
    Campaign as ProtoCampaign, GetWorkItemRequest, ListCampaignsRequest, ListWorkItemEventsRequest,
    ListWorkItemsRequest, WorkItem as ProtoWorkItem, WorkItemEvent as ProtoWorkItemEvent,
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
/// The human and JSON forms render from the same fetched items and escalated
/// campaigns, so they can never disagree about what the daemon returned.
fn render_list(
    items: &[ProtoWorkItem],
    escalated: &[ProtoCampaign],
    view: View,
    json: bool,
) -> String {
    match (view, json) {
        (View::Overview, false) => render::queue::queue_overview(items, escalated),
        (View::Overview, true) => render::queue::items_json(items, escalated),
        (View::Open, false) => render::queue::open_only(items, escalated),
        (View::Open, true) => render::queue::open_json(items, escalated),
    }
}

fn render_with_initiatives(
    items: &[ProtoWorkItem],
    campaigns: &[ProtoCampaign],
    initiatives: &[crate::proto::Initiative],
    view: View,
    json: bool,
) -> Result<String> {
    let members = initiatives
        .iter()
        .flat_map(|initiative| initiative.members.iter().map(|member| member.name.as_str()))
        .collect::<Vec<_>>();
    let campaigns = campaigns
        .iter()
        .filter(|campaign| !members.contains(&campaign.name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let items = items
        .iter()
        .filter(|item| {
            let open = matches!(item.state.as_str(), "preserved" | "needs_decision" | "failed");
            !open
                || !item.source.as_ref().is_some_and(|source| {
                    (source.kind == "campaign" && members.contains(&source.r#ref.as_str()))
                        || (source.kind == "initiative"
                            && initiatives.iter().any(|initiative| initiative.name == source.r#ref))
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut output = render_list(&items, &campaigns, view, json);
    if json {
        let mut value: serde_json::Value = serde_json::from_str(&output)?;
        value["escalated_initiatives"] = serde_json::to_value(initiatives)?;
        output = format!("{}\n", serde_json::to_string_pretty(&value)?);
    } else if !initiatives.is_empty() {
        let mut rows = String::new();
        for initiative in initiatives {
            let _ = writeln!(
                rows,
                "initiative {}  escalated  {} open question(s)\n  next: foundry initiative show {}",
                initiative.name,
                initiative.open_questions.len(),
                initiative.name
            );
        }
        let heading = format!("{}\n", render::queue::OPEN_HEADING);
        let section_start =
            output.find(&heading).context("queue output has no open section")? + heading.len();
        let section_end =
            output[section_start..].find("\n\n").context("queue open section has no end")?
                + section_start;
        if output[section_start..section_end] == *"  (none)" {
            output.replace_range(section_start..=section_end, &rows);
        } else {
            output.insert_str(section_end + 1, &rows);
        }
    }
    Ok(output)
}

fn admits_initiatives(source: Option<&SourceFilter>) -> bool {
    source.is_none_or(|filter| filter.kind == WorkSourceKind::Initiative)
}

/// The client-side files `foundry queue --offline` reads.
#[derive(Debug, Clone, Copy)]
pub struct ListPaths<'a> {
    /// The work-item ledger.
    pub work_items: &'a Path,
    /// The campaign store, for escalated campaigns.
    pub campaigns: &'a Path,
    /// The event log, for escalations that predate the campaign record.
    pub events_dir: &'a Path,
}

/// The escalated campaigns among `campaigns`, in the order given, that a
/// `--source` filter admits.
///
/// No filter admits every escalated campaign, `campaign:<name>` admits only
/// that campaign, and every other source kind admits none: a sentinel or an
/// operator never escalates a campaign.
pub(crate) fn select_escalated(
    campaigns: Vec<ProtoCampaign>,
    source: Option<&SourceFilter>,
) -> Vec<ProtoCampaign> {
    let escalated = foundry_sdk::campaign::CampaignStatus::Escalated.to_string();
    campaigns
        .into_iter()
        .filter(|campaign| campaign.status == escalated)
        .filter(|campaign| match source {
            None => true,
            Some(filter) => {
                filter.kind == WorkSourceKind::Campaign && filter.reference == campaign.name
            }
        })
        .collect()
}

/// Whether a `--source` filter can admit any campaign at all, so the list can
/// skip reading campaigns it would discard.
fn admits_campaigns(source: Option<&SourceFilter>) -> bool {
    source.is_none_or(|filter| filter.kind == WorkSourceKind::Campaign)
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

    /// Campaign filters select member identity even when its source is an initiative.
    fn matches(&self, item: &WorkItem) -> bool {
        if self.kind == WorkSourceKind::Campaign {
            return item.campaign_name() == Some(self.reference.as_str());
        }
        item.source
            .as_ref()
            .is_some_and(|source| source.kind == self.kind && source.reference == self.reference)
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

/// List the ledger in `view`, optionally only the items from one source,
/// beside the escalated campaigns the same source admits.
pub async fn list(
    paths: ListPaths<'_>,
    addr: &str,
    offline: bool,
    view: View,
    json: bool,
    source: Option<&str>,
) -> Result<()> {
    let source = source.map(SourceFilter::parse).transpose()?;
    let (items, campaigns, initiatives) = if offline {
        let campaigns = if admits_campaigns(source.as_ref()) {
            load_campaigns_offline(paths.campaigns, paths.events_dir)?
        } else {
            Vec::new()
        };
        (
            load_offline_selected(paths.work_items, source.as_ref())?,
            campaigns,
            if admits_initiatives(source.as_ref()) {
                crate::initiative_commands::list_offline(paths.campaigns)?
            } else {
                vec![]
            },
        )
    } else {
        fetch_online(addr, view, source.as_ref()).await?
    };
    let escalated = select_escalated(campaigns, source.as_ref());

    let initiatives = initiatives
        .into_iter()
        .filter(|initiative| initiative.status == "escalated")
        .filter(|initiative| {
            source.as_ref().is_none_or(|filter| filter.reference == initiative.name)
        })
        .collect::<Vec<_>>();
    print!("{}", render_with_initiatives(&items, &escalated, &initiatives, view, json)?);
    Ok(())
}

/// Read the campaign store directly, recovering pre-record escalation reasons
/// from the event log the way `ListCampaigns` does, in its name order.
fn load_campaigns_offline(campaigns_path: &Path, events_dir: &Path) -> Result<Vec<ProtoCampaign>> {
    let mut store =
        foundry_sdk::campaign::CampaignStore::load(campaigns_path).with_context(|| {
            format!("could not read the campaign store at {}", campaigns_path.display())
        })?;
    foundry_sdk::campaign::escalation::resolve_open_escalations(&mut store.campaigns, events_dir)
        .context("could not read campaign escalations from the event log")?;
    store.campaigns.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(store.campaigns.iter().map(crate::campaign_commands::summary_to_proto).collect())
}

/// Only the items whose recorded source is `source`, in the order given;
/// every item when there is no filter.
///
/// The offline counterpart of the daemon's source filter, so `--offline`
/// differs from the online path only in transport.
pub(crate) fn filter_by_source(
    items: Vec<WorkItem>,
    source: Option<&SourceFilter>,
) -> Vec<WorkItem> {
    match source {
        Some(filter) => items.into_iter().filter(|item| filter.matches(item)).collect(),
        None => items,
    }
}

/// Fetch the ledger records from the daemon, in the RPC's documented order,
/// optionally selected by source, and the campaign inventory when the source
/// can admit a campaign.
async fn fetch_online(
    addr: &str,
    view: View,
    source: Option<&SourceFilter>,
) -> Result<(Vec<ProtoWorkItem>, Vec<ProtoCampaign>, Vec<crate::proto::Initiative>)> {
    let suffix = match source {
        Some(filter) => format!("{} {}", view_suffix(view), filter.flag_text()),
        None => view_suffix(view).to_string(),
    };
    let mut client = connect_daemon_required(addr, &offline_hint(suffix.trim())).await?;
    let response = client.list_work_items(list_request(source)).await.map_err(status_to_anyhow)?;
    let campaigns = if admits_campaigns(source) {
        client
            .list_campaigns(ListCampaignsRequest {
                project: String::new(),
            })
            .await
            .map_err(status_to_anyhow)?
            .into_inner()
            .campaigns
    } else {
        Vec::new()
    };
    let initiatives = if admits_initiatives(source) {
        client
            .list_initiatives(crate::proto::ListInitiativesRequest {})
            .await
            .map_err(status_to_anyhow)?
            .into_inner()
            .initiatives
    } else {
        vec![]
    };
    Ok((response.into_inner().items, campaigns, initiatives))
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
    load_offline_selected(work_items_path, None)
}

/// Select typed records before their member identity is lost in wire conversion.
pub(crate) fn load_offline_selected(
    work_items_path: &Path,
    source: Option<&SourceFilter>,
) -> Result<Vec<ProtoWorkItem>> {
    let store = WorkItemStore::load(work_items_path).with_context(|| {
        format!("could not read the work-item ledger at {}", work_items_path.display())
    })?;
    Ok(filter_by_source(ordered(&store), source).iter().map(item_to_proto).collect())
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
        continues: item.continues.clone(),
        follow_up: item.follow_up.as_ref().map(follow_up_to_proto),
        ci: item.ci.as_ref().map(trunk_ci_to_proto),
    }
}

/// Wire form of a follow-up's obligation, matching what the daemon would have sent.
fn follow_up_to_proto(
    follow_up: &foundry_sdk::work_item::FollowUp,
) -> crate::proto::WorkItemFollowUp {
    let mut wire = crate::proto::WorkItemFollowUp {
        of: follow_up.of.clone(),
        commit: follow_up.commit.clone(),
        obligation: follow_up.obligation.tag().to_string(),
        ..crate::proto::WorkItemFollowUp::default()
    };
    match &follow_up.obligation {
        foundry_sdk::work_item::FollowUpObligation::Remainder { gaps } => {
            wire.gaps.clone_from(gaps);
        }
        foundry_sdk::work_item::FollowUpObligation::TrunkCi {
            failing_jobs,
            log_tail,
            run_url,
        } => {
            wire.failing_jobs.clone_from(failing_jobs);
            wire.log_tail.clone_from(log_tail);
            wire.run_url.clone_from(run_url);
        }
    }
    wire
}

/// Wire form of a trunk CI watch.
fn trunk_ci_to_proto(watch: &foundry_sdk::trunk_ci::TrunkCiWatch) -> crate::proto::WorkItemTrunkCi {
    crate::proto::WorkItemTrunkCi {
        state: watch.state.tag().to_string(),
        commit: watch.commit.clone(),
        branch: watch.branch.clone(),
        run_url: watch.run_url.clone(),
        failing_jobs: watch.failing_jobs.clone(),
        log_tail: watch.log_tail.clone(),
        detail: watch.detail.clone(),
        follow_up: watch.follow_up.clone(),
        requested_at: watch.requested_at.to_rfc3339(),
        concluded_at: watch.concluded_at.map(|at| at.to_rfc3339()),
        repo: watch.repo.clone(),
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

/// Resume preserved work, or an open follow-up, via the daemon; never read or
/// write client stores.
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
pub async fn reconcile(addr: &str, offline: bool, prune_branches: bool) -> Result<()> {
    anyhow::ensure!(
        !offline,
        "foundry queue reconcile requires the daemon; --offline is not supported"
    );
    let mut client = crate::daemon::connect_daemon_online(addr).await?;
    let response = client
        .reconcile_work(crate::proto::ReconcileWorkRequest { prune_branches })
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    print!("{}", response.markdown);
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_escalated_initiative_suppresses_member_campaigns_and_open_items() {
        let initiative = crate::proto::Initiative {
            name: "shared".into(),
            status: "escalated".into(),
            members: vec![crate::proto::InitiativeMember {
                name: "member".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        let campaigns = vec![ProtoCampaign {
            name: "member".into(),
            status: "escalated".into(),
            ..Default::default()
        }];
        let items = vec![ProtoWorkItem {
            id: "wi_member".into(),
            state: "preserved".into(),
            source: Some(crate::proto::WorkSource {
                kind: "initiative".into(),
                r#ref: "shared".into(),
                cycle: Some(1),
            }),
            ..Default::default()
        }];
        let output =
            render_with_initiatives(&items, &campaigns, &[initiative], View::Open, true).unwrap();
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["escalated_initiatives"].as_array().unwrap().len(), 1);
        assert!(
            value["escalated_campaigns"].as_array().unwrap().is_empty(),
            "expected `value[\"escalated_campaigns\"].as_array().unwrap()` to be empty: {:?}",
            value["escalated_campaigns"].as_array().unwrap()
        );
        assert!(
            value["items"].as_array().unwrap().is_empty(),
            "expected `value[\"items\"].as_array().unwrap()` to be empty: {:?}",
            value["items"].as_array().unwrap()
        );
    }

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
        assert!(items.is_empty(), "expected `items` to be empty: {items:?}");
    }

    #[test]
    fn offline_overview_of_a_missing_path_renders_four_empty_groups() {
        let dir = tempfile::tempdir().expect("tempdir");
        let rendered = render_list(
            &load_offline(&dir.path().join("absent.json")).expect("empty ledger"),
            &[],
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

    #[test]
    fn offline_load_carries_a_follow_ups_obligation_and_none_for_other_items() {
        let parent = item("wi_parent", "alpha", WorkItemState::Landed, 5);
        let follow_up = foundry_sdk::work_item::FollowUp {
            of: "wi_parent".to_string(),
            commit: Some("landedcommit".to_string()),
            obligation: foundry_sdk::work_item::FollowUpObligation::Remainder {
                gaps: vec!["one gap".to_string()],
            },
        }
        .into_item(&parent, at(6));
        let tmp = NamedTempFile::new().expect("tempfile");
        store(vec![parent, follow_up]).save(tmp.path()).expect("save ledger");

        let items = load_offline(tmp.path()).expect("load ledger");
        let wire = items.iter().find(|item| item.kind == "follow_up").expect("the follow-up");
        assert_eq!(
            wire.follow_up,
            Some(crate::proto::WorkItemFollowUp {
                of: "wi_parent".to_string(),
                commit: Some("landedcommit".to_string()),
                obligation: "remainder".to_string(),
                gaps: vec!["one gap".to_string()],
                ..crate::proto::WorkItemFollowUp::default()
            })
        );
        let parent = items.iter().find(|item| item.id == "wi_parent").expect("the parent");
        assert_eq!(parent.follow_up, None);
    }

    #[test]
    fn offline_load_carries_a_trunk_ci_watch_and_a_red_follow_up() {
        let mut parent = item("wi_parent", "alpha", WorkItemState::Landed, 5);
        let mut watch =
            foundry_sdk::trunk_ci::TrunkCiWatch::pending("o/alpha", "main", "abcdef0123", at(5));
        watch.state = foundry_sdk::trunk_ci::TrunkCiState::Failed;
        watch.concluded_at = Some(at(6));
        watch.failing_jobs = vec!["lint".to_string()];
        parent.ci = Some(watch);
        let red = foundry_sdk::work_item::FollowUp {
            of: "wi_parent".to_string(),
            commit: Some("abcdef0123".to_string()),
            obligation: foundry_sdk::work_item::FollowUpObligation::TrunkCi {
                failing_jobs: vec!["lint".to_string()],
                log_tail: Some("tail".to_string()),
                run_url: Some("https://ci/run/1".to_string()),
            },
        }
        .into_item(&parent, at(6));
        let tmp = NamedTempFile::new().expect("tempfile");
        store(vec![parent, red]).save(tmp.path()).expect("save ledger");

        let items = load_offline(tmp.path()).expect("load ledger");
        let parent = items.iter().find(|item| item.id == "wi_parent").expect("the parent");
        let ci = parent.ci.as_ref().expect("the watch");
        assert_eq!(
            (ci.state.as_str(), ci.repo.as_str(), ci.branch.as_str(), ci.commit.as_str()),
            ("failed", "o/alpha", "main", "abcdef0123")
        );
        assert_eq!(ci.failing_jobs, vec!["lint"]);
        assert_eq!(ci.concluded_at, Some(at(6).to_rfc3339()));
        let wire = items.iter().find(|item| item.kind == "follow_up").expect("the follow-up");
        assert_eq!(wire.state, "failed");
        assert_eq!(
            wire.follow_up,
            Some(crate::proto::WorkItemFollowUp {
                of: "wi_parent".to_string(),
                commit: Some("abcdef0123".to_string()),
                obligation: "trunk_ci".to_string(),
                failing_jobs: vec!["lint".to_string()],
                log_tail: Some("tail".to_string()),
                run_url: Some("https://ci/run/1".to_string()),
                ..crate::proto::WorkItemFollowUp::default()
            })
        );
        assert_eq!(wire.ci, None);
    }

    // ── offline command paths ─────────────────────────────────────────────────

    #[tokio::test]
    async fn offline_list_never_contacts_the_daemon() {
        let tmp = NamedTempFile::new().expect("tempfile");
        store(vec![item("wi_running", "alpha", WorkItemState::Running, 0)])
            .save(tmp.path())
            .expect("save ledger");

        let dir = tempfile::tempdir().expect("tempdir");
        let campaigns = dir.path().join("campaigns.json");
        let events = dir.path().join("events");
        let paths = ListPaths {
            work_items: tmp.path(),
            campaigns: &campaigns,
            events_dir: &events,
        };
        // An unroutable address: reaching for it would fail the test.
        list(paths, "http://127.0.0.1:0", true, View::Overview, false, None)
            .await
            .expect("offline overview should succeed");
        list(paths, "http://127.0.0.1:0", true, View::Open, true, None)
            .await
            .expect("offline open --json should succeed");
        list(paths, "http://127.0.0.1:0", true, View::Overview, false, Some("operator:desk"))
            .await
            .expect("offline overview with a source filter should succeed");
        assert!(!campaigns.exists(), "an offline read never creates the campaign store");
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
            unknown
                .to_string()
                .contains("campaign, initiative, sentinel, operator, work_item"),
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
        let items = ordered(&store(vec![cycle_one, cycle_two, nightly, unsourced]));

        let cycles = filter_by_source(items.clone(), Some(&SourceFilter::campaign("tidy-cli")));
        assert_eq!(
            cycles.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
            vec!["wi_c2", "wi_c1"]
        );
        assert_eq!(cycles[1].source.as_ref().map(|s| s.cycle), Some(Some(1)));

        assert_eq!(filter_by_source(items.clone(), None).len(), 4, "no filter, every item");
        assert!(
            filter_by_source(items, Some(&SourceFilter::campaign("tidy"))).is_empty(),
            "the ref is an exact match, not a prefix"
        );
    }

    #[test]
    fn offline_campaign_selection_distinguishes_members_of_one_initiative() {
        let tmp = NamedTempFile::new().expect("tempfile");
        let mut alpha_new = item("wi_alpha_new", "project", WorkItemState::Running, 0);
        let mut alpha_old = item("wi_alpha_old", "project", WorkItemState::Landed, 2_000);
        let mut beta = item("wi_beta", "project", WorkItemState::Running, 0);
        for (work, member, cycle) in [
            (&mut alpha_new, "alpha", 2),
            (&mut alpha_old, "alpha", 1),
            (&mut beta, "beta", 1),
        ] {
            work.source = Some(WorkSource::initiative("shared", cycle));
            let root = foundry_sdk::event::Event::new(
                foundry_sdk::event::EventType::CampaignAdvanceRequested,
                "project".into(),
                foundry_sdk::throttle::Throttle::Full,
                serde_json::json!({"campaign": member, "campaign_cycle": cycle}),
            );
            if work.state == WorkItemState::Running {
                work.pending_root = Some(root);
            } else {
                work.started_root = Some(root);
            }
        }
        store(vec![alpha_old, beta, alpha_new]).save(tmp.path()).expect("save ledger");
        let alpha = load_offline_selected(tmp.path(), Some(&SourceFilter::campaign("alpha")))
            .expect("select alpha cycles");
        assert_eq!(ids(&alpha), vec!["wi_alpha_new", "wi_alpha_old"]);
        assert!(alpha.iter().all(|item| item.source.as_ref().unwrap().r#ref == "shared"));
        let beta = load_offline_selected(tmp.path(), Some(&SourceFilter::campaign("beta")))
            .expect("select beta cycles");
        assert_eq!(ids(&beta), vec!["wi_beta"]);
        let initiative = load_offline_selected(
            tmp.path(),
            Some(&SourceFilter::parse("initiative:shared").expect("source filter")),
        )
        .expect("select shared initiative");
        assert_eq!(ids(&initiative), vec!["wi_alpha_new", "wi_beta", "wi_alpha_old"]);
    }

    #[test]
    fn initiative_questions_fill_the_open_section_in_both_human_views() {
        let initiative = crate::proto::Initiative {
            name: "shared".into(),
            status: "escalated".into(),
            ..Default::default()
        };
        for view in [View::Open, View::Overview] {
            let output =
                render_with_initiatives(&[], &[], std::slice::from_ref(&initiative), view, false)
                    .expect("render queue");
            let open_start = output.find(render::queue::OPEN_HEADING).expect("open heading");
            let open_end = output.find("Settled (").unwrap_or(output.len());
            let open = &output[open_start..open_end];
            assert!(open.contains("initiative shared"), "{output}");
            assert!(!open.contains("(none)"), "{output}");
        }
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
        assert!(events.is_empty(), "expected `events` to be empty: {events:?}");
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
            let rendered = render_list(&items, &[], View::Open, json);
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

        let human = render_list(&items, &[], View::Overview, false);
        let json = render_list(&items, &[], View::Overview, true);
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

    // ── escalated campaign selection ──────────────────────────────────────────

    fn wire_campaign(name: &str, status: &str) -> ProtoCampaign {
        ProtoCampaign {
            name: name.to_string(),
            status: status.to_string(),
            ..ProtoCampaign::default()
        }
    }

    fn names(campaigns: &[ProtoCampaign]) -> Vec<&str> {
        campaigns.iter().map(|c| c.name.as_str()).collect()
    }

    #[test]
    fn only_escalated_campaigns_are_selected_and_a_source_filter_narrows_them() {
        let all = || {
            vec![
                wire_campaign("asks", "escalated"),
                wire_campaign("busy", "active"),
                wire_campaign("stuck", "escalated"),
                wire_campaign("resting", "paused"),
            ]
        };
        assert_eq!(names(&select_escalated(all(), None)), ["asks", "stuck"]);
        let stuck = SourceFilter::campaign("stuck");
        assert_eq!(names(&select_escalated(all(), Some(&stuck))), ["stuck"]);
        let busy = SourceFilter::campaign("busy");
        assert!(select_escalated(all(), Some(&busy)).is_empty(), "not escalated");
        for other in ["sentinel:nightly", "operator:desk", "work_item:wi_1"] {
            let filter = SourceFilter::parse(other).expect("valid");
            assert!(select_escalated(all(), Some(&filter)).is_empty(), "{other}");
            assert!(!admits_campaigns(Some(&filter)), "{other} reads no campaigns");
        }
        assert!(admits_campaigns(None) && admits_campaigns(Some(&stuck)));
    }

    #[test]
    fn offline_campaigns_recover_a_pre_record_reason_and_create_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let campaigns_path = dir.path().join("campaigns.json");
        let missing = load_campaigns_offline(&campaigns_path, &dir.path().join("events"))
            .expect("a missing store has no campaigns");
        assert!(missing.is_empty(), "expected `missing` to be empty: {missing:?}");
        assert!(!campaigns_path.exists(), "a read never creates the store");

        let mut legacy: foundry_sdk::campaign::Campaign =
            serde_json::from_value(serde_json::json!({
                "name": "legacy", "project": "p", "mission": "ship", "status": "escalated",
                "done_evidence": [{"kind": "review", "statement": "shipped"}]
            }))
            .expect("campaign");
        legacy.authorized_by = Some("owner".to_string());
        foundry_sdk::campaign::CampaignStore {
            findings: vec![],
            initiatives: vec![],
            version: 1,
            campaigns: vec![legacy],
        }
        .save(&campaigns_path)
        .expect("save store");
        let events = dir.path().join("events");
        std::fs::create_dir_all(&events).expect("events dir");
        std::fs::write(
            events.join("2026-10.jsonl"),
            "{\"id\":\"e1\",\"event_type\":\"campaign_escalated\",\"project\":\"p\",\
             \"occurred_at\":\"2026-10-11T03:00:00Z\",\"recorded_at\":\"2026-10-11T03:00:00Z\",\
             \"throttle\":\"full\",\"payload\":{\"campaign\":\"legacy\",\"project\":\"p\",\
             \"reason\":\"owner decision required\",\"cycles_completed\":2,\"cycles_landed\":2}}\n",
        )
        .expect("write the log");

        let loaded = load_campaigns_offline(&campaigns_path, &events).expect("loads");
        let open = loaded[0].open_escalation.as_ref().expect("recovered");
        assert_eq!(open.reason, "owner decision required");
        assert!(open.from_event_log);
    }
}
