//! Settles campaign cycles the campaign itself has moved past.
//!
//! A campaign forms each cycle from the previous cycle's preserved ref (or
//! from trunk), so an earlier cycle left `preserved`, `needs_decision` or
//! `failed` is not owed by a person: the campaign carried it, and the
//! campaign's own escalation is what a person sees. Without this block those
//! cycles stay in the open list after the campaign has advanced, completed or
//! been cancelled. The same holds for the campaign's formation runs, which
//! durable admission records as `workflow` items with the campaign's source:
//! one that ended `failed` is the campaign's to retry, not a person's.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use foundry_sdk::campaign::{CampaignStatus, CampaignStore};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{CampaignTerminalPayload, WorkItemEventPayload};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::{WorkItem, WorkItemKind, WorkItemState, WorkItemStore};
use foundry_sdk::work_source::WorkSourceKind;

use super::SimulatedSuccess;
use super::work_ledger::{ledger_lock, load_ledger, save_ledger};

/// Settles open campaign cycles `cancelled` once their campaign has moved
/// past them.
///
/// Mutator. It sinks on the lifecycle events of campaign cycle and formation
/// items (`work_item_submitted`, `work_item_started`, `work_item_settled`) and on
/// the campaign's own `campaign_completed` and `campaign_cancelled`. Under the
/// daemon's durable admission a cycle's `work_item_submitted` is persisted and
/// broadcast but not routed to blocks, so carrying forward happens when the
/// scheduler starts the new cycle (`work_item_started`, routed through the
/// engine), before its agent runs. Each
/// trigger only names a campaign; the decision is taken from the ledger as it
/// stands, so a missed or reordered trigger is made good by the next one:
///
/// - An open cycle is carried forward when a later-submitted cycle of the same
///   campaign has a higher cycle number and was not itself cancelled. An open
///   formation run is carried forward by the first such cycle submitted after
///   it. Only a cycle carries.
/// - Every open cycle and formation run of a completed or cancelled campaign
///   settles with the
///   campaign's terminal reason. A graceful cancellation lets the in-flight
///   cycle finish; that cycle's own `work_item_settled` finds the campaign
///   already terminal in the campaign store and settles it then.
///
/// Items without a recorded campaign identity, other campaigns' items and settled
/// items are never touched. The decision and the write happen in one pass
/// under the ledger write gate, on the ledger as loaded under that gate, so an
/// owner action that landed first always wins. No `operator_action` is
/// recorded: no person did this. Preservation evidence is kept.
pub struct SettleCarriedCampaignWork {
    work_items_path: PathBuf,
    campaigns_path: PathBuf,
}

impl SettleCarriedCampaignWork {
    /// Settle cycles in the ledger at `work_items_path`, reading campaign
    /// status from the store at `campaigns_path`.
    #[must_use]
    pub fn new(work_items_path: PathBuf, campaigns_path: PathBuf) -> Self {
        Self {
            work_items_path,
            campaigns_path,
        }
    }
}

/// The campaign a trigger concerns, and the terminal reason it carries.
#[derive(Debug, PartialEq, Eq)]
struct Sweep {
    campaign: String,
    project: String,
    /// Set when the trigger itself is the campaign's terminal event.
    terminal: Option<String>,
}

fn sweep_for(trigger: &Event) -> Option<Sweep> {
    match trigger.event_type {
        EventType::WorkItemSubmitted | EventType::WorkItemStarted | EventType::WorkItemSettled => {
            let payload = trigger.parse_payload::<WorkItemEventPayload>().ok()?;
            let campaign = payload.campaign.or_else(|| {
                payload
                    .source
                    .as_ref()
                    .filter(|source| source.kind == WorkSourceKind::Campaign)
                    .map(|source| source.reference.clone())
            })?;
            let cycle = payload.kind == WorkItemKind::CampaignCycle
                && payload
                    .campaign_cycle
                    .or_else(|| payload.source.as_ref().and_then(|source| source.cycle))
                    .is_some();
            let formation = payload.kind == WorkItemKind::Workflow;
            (cycle || formation).then_some(Sweep {
                campaign,
                project: payload.project,
                terminal: None,
            })
        }
        EventType::CampaignCompleted | EventType::CampaignCancelled => {
            // `CampaignCancelledPayload` flattens this payload, so both parse.
            let payload = trigger.parse_payload::<CampaignTerminalPayload>().ok()?;
            let outcome = if trigger.event_type == EventType::CampaignCompleted {
                "completed"
            } else {
                "cancelled"
            };
            Some(Sweep {
                terminal: Some(terminal_reason(&payload.campaign, outcome, &payload.reason)),
                campaign: payload.campaign,
                project: payload.project,
            })
        }
        _ => None,
    }
}

/// `campaign <name> <outcome>`, with the recorded reason appended when there
/// is one.
fn terminal_reason(campaign: &str, outcome: &str, recorded: &str) -> String {
    let recorded = recorded.trim();
    if recorded.is_empty() {
        format!("campaign {campaign} {outcome}")
    } else {
        format!("campaign {campaign} {outcome}: {recorded}")
    }
}

/// The terminal reason the campaign store holds for `campaign`, or `None`
/// while it is still live.
///
/// A terminal status never changes back, so a status read before the ledger
/// gate is taken cannot become stale in a way that matters.
fn stored_terminal(path: &Path, campaign: &str) -> Option<String> {
    let store = match CampaignStore::load(path) {
        Ok(store) => store,
        Err(error) => {
            // Best-effort: without the campaign's status, only carrying forward
            // is decided; a terminal sweep waits for the next trigger.
            tracing::warn!(path = %path.display(), %error, "could not read campaigns for cycle settlement");
            return None;
        }
    };
    let found = store.find(campaign)?;
    let (outcome, prefix) = match found.status {
        CampaignStatus::Completed => ("completed", "Completed externally: "),
        CampaignStatus::Cancelled => ("cancelled", "Cancelled externally: "),
        CampaignStatus::Staged
        | CampaignStatus::Active
        | CampaignStatus::Paused
        | CampaignStatus::Escalated => return None,
    };
    let recorded = found
        .owner_decisions
        .iter()
        .rev()
        .find_map(|decision| decision.decision.strip_prefix(prefix))
        .unwrap_or_default();
    Some(terminal_reason(campaign, outcome, recorded))
}

/// What an item is to `campaign` in `project`, when it belongs to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// A `campaign_cycle` item with its cycle number.
    Cycle(u64),
    /// A `workflow` item carrying the campaign's source: durable admission
    /// records the campaign's formation run (`campaign_advance_requested`)
    /// this way. It is never a carrier.
    Formation,
}

fn role_of(item: &WorkItem, campaign: &str, project: &str) -> Option<Role> {
    if let Some(cycle) = cycle_of(item, campaign, project) {
        return Some(Role::Cycle(cycle));
    }
    (item.kind == WorkItemKind::Workflow
        && item.project == project
        && item.campaign_name() == Some(campaign))
    .then_some(Role::Formation)
}

/// The cycle number of `item` when it is a cycle of `campaign` in `project`.
///
/// Only `campaign_cycle` items count, so only a cycle can carry the others.
fn cycle_of(item: &WorkItem, campaign: &str, project: &str) -> Option<u64> {
    (item.kind == WorkItemKind::CampaignCycle
        && item.project == project
        && item.campaign_name() == Some(campaign))
    .then_some(item.campaign_cycle())
    .flatten()
}

/// Settle `cancelled`, in `store`, every open cycle and formation run of
/// `campaign` in `project` that the campaign has moved past, and return the
/// settled items.
///
/// The carrier is the most recently submitted cycle of the campaign that was
/// not itself cancelled. An open cycle with a lower number, submitted no later,
/// and an open formation run submitted before the carrier, settle `carried
/// forward by campaign <name> cycle <n> (<carrier id>)`. When `terminal` is
/// set, every other open cycle and formation run settles with that reason.
fn settle_campaign_cycles(
    store: &mut WorkItemStore,
    campaign: &str,
    project: &str,
    terminal: Option<&str>,
    at: DateTime<Utc>,
) -> Vec<WorkItem> {
    let carrier = store
        .items
        .iter()
        .filter(|item| item.state != WorkItemState::Cancelled)
        .filter_map(|item| Some((cycle_of(item, campaign, project)?, item)))
        .max_by_key(|(cycle, item)| (item.submitted_at, *cycle))
        .map(|(cycle, item)| (cycle, item.submitted_at, item.id.clone()));
    let mut settled = Vec::new();
    for item in &mut store.items {
        let Some(role) = role_of(item, campaign, project) else {
            continue;
        };
        if !item.state.is_open() {
            continue;
        }
        let moved_past = |carried_by: u64, submitted_at: DateTime<Utc>| match role {
            Role::Cycle(cycle) => cycle < carried_by && item.submitted_at <= submitted_at,
            Role::Formation => item.submitted_at < submitted_at,
        };
        let reason = match &carrier {
            Some((carried_by, submitted_at, id)) if moved_past(*carried_by, *submitted_at) => {
                format!("carried forward by campaign {campaign} cycle {carried_by} ({id})")
            }
            _ => match terminal {
                Some(reason) => reason.to_string(),
                None => continue,
            },
        };
        let disposition = item.disposition.clone();
        item.settle_cancelled(&reason, disposition, at);
        settled.push(item.clone());
    }
    settled
}

/// Run one sweep against the ledger at `path`, under the ledger write gate.
fn apply(path: &Path, sweep: &Sweep, terminal: Option<&str>) -> Vec<WorkItem> {
    let Some(_guard) = ledger_lock() else {
        return Vec::new();
    };
    let Some(mut store) = load_ledger(path) else {
        return Vec::new();
    };
    let settled =
        settle_campaign_cycles(&mut store, &sweep.campaign, &sweep.project, terminal, Utc::now());
    if settled.is_empty() || !save_ledger(&store, path) {
        return Vec::new();
    }
    settled
}

impl SimulatedSuccess for SettleCarriedCampaignWork {
    type Outcome = Vec<WorkItem>;

    /// What a sweep settles depends on the live ledger, which a dry run does
    /// not read, so a dry run announces nothing.
    fn simulate(&self, _trigger: &Event) -> Vec<WorkItem> {
        Vec::new()
    }

    /// One `work_item_cancelled` per settled cycle, on that cycle's own trace
    /// and project.
    fn success_events(&self, trigger: &Event, outcome: &Vec<WorkItem>) -> Vec<Event> {
        outcome
            .iter()
            .map(|item| {
                super::event_from_infallible_payload(
                    EventType::WorkItemCancelled,
                    &item.project,
                    trigger.throttle,
                    &WorkItemEventPayload::from_item(item),
                )
                .with_trace_id(item.trace_id.clone())
            })
            .collect()
    }
}

impl TaskBlock for SettleCarriedCampaignWork {
    task_block_meta! {
        name: "Settle Carried Campaign Work",
        kind: Mutator,
        sinks_on: [
            WorkItemSubmitted,
            WorkItemStarted,
            WorkItemSettled,
            CampaignCompleted,
            CampaignCancelled,
        ],
    }

    dry_run_via_simulation!();

    fn accepts(&self, trigger: &Event) -> bool {
        sweep_for(trigger).is_some()
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let Some(sweep) = sweep_for(trigger) else {
            return skip!("not a campaign cycle or campaign terminal event");
        };
        let trigger = trigger.clone();
        let work_items_path = self.work_items_path.clone();
        let campaigns_path = self.campaigns_path.clone();
        Box::pin(async move {
            let settled = tokio::task::spawn_blocking(move || {
                let terminal = sweep
                    .terminal
                    .clone()
                    .or_else(|| stored_terminal(&campaigns_path, &sweep.campaign));
                let settled = apply(&work_items_path, &sweep, terminal.as_deref());
                (sweep, settled)
            })
            .await?;
            let (sweep, settled) = settled;
            let summary = if settled.is_empty() {
                format!("campaign '{}': no open cycle to settle", sweep.campaign)
            } else {
                let ids: Vec<_> = settled.iter().map(|item| item.id.as_str()).collect();
                format!("campaign '{}': settled {} cancelled", sweep.campaign, ids.join(", "))
            };
            Ok(TaskBlockResult::success(summary, self.success_events(&trigger, &settled)))
        })
    }
}

#[cfg(test)]
mod tests;
