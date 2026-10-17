//! One durable admission boundary for engineering workflow roots.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::Utc;
use foundry_engine::admission::{Admission, ResultFuture, WorkAdmission};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::pacing::PacingPaths;
use foundry_sdk::payload::{
    ExecutionRequestedPayload, InitiativeSteeredPayload, SteeringSettlement, WorkItemEventPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::trace::{BlockExecution, ProcessResult};
use foundry_sdk::work_item::{
    WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane, ledger_write_gate,
};
use foundry_sdk::workflow::WorkflowType;
use tokio::sync::{Notify, broadcast};

use foundry_blocks::blocks::{ADMITTED_WORK_ITEM_KEY, item_from_dispatch, queued_reason};

/// Admission policy sharing the daemon's existing ledger and pacing scheduler.
pub struct DurableWorkAdmission {
    path: PathBuf,
    campaigns_path: PathBuf,
    registry: Arc<RwLock<Registry>>,
    pacing: PacingPaths,
    changed: Arc<Notify>,
    event_tx: broadcast::Sender<Event>,
    writer: Arc<foundry_engine::event_writer::EventWriter>,
    result_writer: Arc<foundry_blocks::trace_writer::TraceWriter>,
    supply_chain_remediation: bool,
    persistence_failures: Mutex<HashMap<String, String>>,
}

impl DurableWorkAdmission {
    pub fn new(
        path: PathBuf,
        registry: Arc<RwLock<Registry>>,
        pacing: PacingPaths,
        event_tx: broadcast::Sender<Event>,
        writer: Arc<foundry_engine::event_writer::EventWriter>,
        result_writer: Arc<foundry_blocks::trace_writer::TraceWriter>,
    ) -> Self {
        Self {
            campaigns_path: path
                .parent()
                .unwrap_or_else(|| std::path::Path::new("."))
                .join("campaigns.json"),
            path,
            registry,
            pacing,
            changed: Arc::new(Notify::new()),
            event_tx,
            writer,
            result_writer,
            supply_chain_remediation: foundry_blocks::blocks::supply_chain_remediation_enabled(),
            persistence_failures: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn with_campaigns_path(mut self, path: PathBuf) -> Self {
        self.campaigns_path = path;
        self
    }

    fn store(&self) -> anyhow::Result<WorkItemStore> {
        Ok(WorkItemStore::load(&self.path)?)
    }

    /// The steer that settling `root` requests, if any. `ledger` already
    /// records `root`'s own work item as settled.
    ///
    /// An accounted member cycle result and a standalone initiative task
    /// always wake steering: the agent judges their outcome. Any other
    /// successful member advance (one that escalated, stood down, finished its
    /// member or, when a steer issued it, dispatched a cycle) wakes it only
    /// when it moved its member on and the precheck finds another member to
    /// advance. See [`Self::advance_left_work_to_steer`].
    fn automatic_steer_after(
        &self,
        root: &Event,
        ledger: &WorkItemStore,
    ) -> anyhow::Result<Option<Event>> {
        let advance = (root.event_type == EventType::CampaignAdvanceRequested)
            .then(|| root.parse_payload::<foundry_sdk::payload::CampaignAdvanceRequestedPayload>())
            .and_then(Result::ok);
        let accounted_member = advance.as_ref().is_some_and(|payload| {
            payload.run_event_id.as_deref().is_some_and(|id| !id.trim().is_empty())
                && payload.run_result.is_some()
        });
        let standalone_task = root.event_type == EventType::ExecutionRequested
            && WorkflowType::from_payload(&root.payload) == WorkflowType::Task
            && root.payload_str("campaign").is_none();
        if advance.is_none() && !standalone_task {
            return Ok(None);
        }
        let Some(name) = root.payload_str("initiative").or_else(|| {
            root.source
                .as_ref()
                .filter(|source| {
                    source.kind == foundry_sdk::work_source::WorkSourceKind::Initiative
                })
                .map(|source| source.reference.as_str())
        }) else {
            return Ok(None);
        };
        // CampaignStore saves by atomic rename. Reading here does not take its
        // flock or invert the campaign-store-before-ledger mutation order.
        let mut store = foundry_sdk::campaign::CampaignStore::load(&self.campaigns_path)?;
        let enabled = store.find_initiative(name).is_some_and(|initiative| {
            initiative.steering_enabled
                && !matches!(
                    initiative.status,
                    foundry_sdk::initiative::InitiativeStatus::Completed
                        | foundry_sdk::initiative::InitiativeStatus::Cancelled
                )
        });
        if !enabled {
            return Ok(None);
        }
        if let Some(advance) = advance.as_ref().filter(|_| !accounted_member) {
            // This completion releases the advance's admission reservation
            // after the ledger is saved. Judge eligibility as if it had, or
            // the settled member would still hold its slot.
            if let Some(id) = root.payload_str(ADMITTED_WORK_ITEM_KEY) {
                store.release_member_reservation(name, id)?;
            }
            let steer_issued = root.payload_str("steering_action_id").is_some();
            if !self.advance_left_work_to_steer(
                &store,
                name,
                &advance.campaign,
                steer_issued,
                ledger,
            )? {
                return Ok(None);
            }
        }
        let trigger = root.causation_id.as_deref().unwrap_or(&root.id);
        let payload =
            Event::serialize_payload(&foundry_sdk::payload::InitiativeAdvanceRequestedPayload {
                initiative: name.into(),
                member: None,
                operator_origin: None,
                trigger_id: Some(trigger.into()),
                follow_up_of: None,
            })?;
        let mut event = Event::new(
            EventType::InitiativeAdvanceRequested,
            "system".into(),
            root.throttle,
            payload,
        )
        .with_source(Some(foundry_sdk::work_source::WorkSource::initiative(name, 0)))
        .with_trace_id(root.trace_id.clone())
        .with_span_ids(Some(foundry_sdk::event::mint_span_id()), root.span_id.clone());
        event.id = format!("evt_{trigger}_initiative_advance");
        Ok(Some(event))
    }

    /// Whether `member`'s settled advance moved it on while another member of
    /// `initiative` could advance.
    ///
    /// The member must no longer be eligible itself: it escalated, finished,
    /// stopped, became blocked or, for an advance a steer issued, started a
    /// cycle. Its dispatched objective and its queued cycle item both hold its
    /// slot, so a steer requested here fills the next free slot, and the chain
    /// ends when no slot or no eligible member remains. A formation another
    /// path asked for that started a cycle waits for that cycle's result. An
    /// advance that left its member eligible changed nothing, and steering
    /// again could choose the same advance without end.
    fn advance_left_work_to_steer(
        &self,
        store: &foundry_sdk::campaign::CampaignStore,
        initiative: &str,
        member: &str,
        steer_issued: bool,
        ledger: &WorkItemStore,
    ) -> anyhow::Result<bool> {
        let Some(campaign) = store.find(member) else {
            return Ok(false);
        };
        if campaign.has_cycle_in_flight() && !steer_issued {
            return Ok(false);
        }
        let registry = foundry_sdk::error::read_lock(&self.registry, "registry")?;
        let eligible = foundry_blocks::blocks::steering_eligible_members(
            store, initiative, ledger, &registry,
        )?;
        Ok(!eligible.is_empty() && !eligible.iter().any(|eligible| eligible == member))
    }

    fn validate_external_context(
        &self,
        event: &Event,
    ) -> anyhow::Result<Option<foundry_sdk::initiative::SteeringExternalContext>> {
        let external_context = event
            .payload
            .get("steering_external_context")
            .filter(|value| !value.is_null())
            .map(|value| {
                serde_json::from_value::<foundry_sdk::initiative::SteeringExternalContext>(
                    value.clone(),
                )
            })
            .transpose()?;
        if event.payload_str("steering_action_id").is_some()
            && !recorded_root(&self.store()?, &event.id)
            && let Some(context) = external_context.as_ref()
        {
            let registry = foundry_sdk::error::read_lock(&self.registry, "registry")?.clone();
            anyhow::ensure!(
                foundry_blocks::blocks::validate_steering_external_context(context, &registry)?,
                "initiative action context changed before final admission"
            );
        }
        Ok(external_context)
    }

    fn queue_steering_root(
        &self,
        store: &mut WorkItemStore,
        root: Event,
        events: &mut Vec<Event>,
    ) -> anyhow::Result<()> {
        let existing = store.items.iter_mut().find(|item| {
            (item.occupies_slot() || item.state.is_waiting())
                && item.started_root.as_ref().or(item.pending_root.as_ref()).is_some_and(|event| {
                    event.event_type == EventType::InitiativeAdvanceRequested
                        && event.payload_str("initiative") == root.payload_str("initiative")
                })
        });
        if let Some(existing) = existing {
            if existing.occupies_slot() && distinct_steering_trigger(existing, &root) {
                existing.coalesced_root = Some(root);
            }
        } else if !recorded_root(store, &root.id)
            && let Some(mut next) = self.planned(&root)?
        {
            store.upsert(next.clone());
            next.reason = queued_reason(store, &next.id, &self.registry, &self.pacing);
            let submitted = lifecycle(EventType::WorkItemSubmitted, &next)?;
            store.upsert(next);
            self.writer.write(&submitted)?;
            events.push(submitted);
        }
        Ok(())
    }

    fn stopped(&self, item: &WorkItem) -> anyhow::Result<ProcessResult> {
        let _gate = ledger_write_gate()
            .lock()
            .map_err(|_| anyhow::anyhow!("ledger write gate poisoned"))?;
        let result_key = format!("stopped_{}", item.id);
        if let Some(result) = self.result_writer.read(&result_key) {
            return Ok(result);
        }
        let result = stopped_result(item);
        for event in &result.events {
            self.writer.write(event)?;
            let _sent = self.event_tx.send(event.clone());
        }
        self.result_writer.write(&result_key, &result)?;
        Ok(result)
    }

    /// The durable item `event` becomes, or `None` for a control fact.
    fn planned(&self, event: &Event) -> anyhow::Result<Option<WorkItem>> {
        if control_event(&event.event_type) {
            return Ok(None);
        }
        let registry = foundry_sdk::error::read_lock(&self.registry, "registry")?;
        let system_work = event.project == "system";
        anyhow::ensure!(
            system_work || registry.find_project(&event.project).is_some(),
            "project is not registered: {}",
            event.project
        );
        let mut item = if event.event_type == EventType::ExecutionRequested
            && WorkflowType::from_payload(&event.payload) == WorkflowType::Task
        {
            let payload: ExecutionRequestedPayload = event.parse_payload()?;
            anyhow::ensure!(!payload.prompt.is_empty(), "task objective is empty");
            item_from_dispatch(event, &payload)
        } else {
            let kind = match event.event_type {
                EventType::ProjectRunStarted | EventType::ProjectMaintenanceRequested => {
                    WorkItemKind::Maintenance
                }
                EventType::ReleaseRequested => WorkItemKind::Release,
                EventType::VulnerabilityDetected
                | EventType::MainBranchAudited
                | EventType::PipelineChecked => WorkItemKind::Remediation,
                _ => WorkItemKind::Workflow,
            };
            let lane = if matches!(
                event.event_type,
                EventType::CampaignAdvanceRequested | EventType::InitiativeAdvanceRequested
            ) {
                WorkLane::Campaign
            } else if event.gather_id.is_some() || system_work {
                WorkLane::Maintenance
            } else {
                WorkLane::Interactive
            };
            WorkItem::queued(
                WorkItemSpec {
                    project: event.project.clone(),
                    objective: format!("{} for {}", event.event_type, event.project),
                    kind,
                    lane,
                    origin: event.event_type.to_string(),
                    trace_id: event.trace_id.clone(),
                },
                event.clone(),
                Utc::now(),
            )
            .with_source(event.source.clone())
        };
        if self.supply_chain_remediation
            && matches!(
                event.event_type,
                EventType::SupplyChainScanStarted | EventType::SupplyChainScanned
            )
        {
            // This formation audits and can repair many projects sequentially.
            // Reserve those repositories through the same admission rules.
            item.reserved_projects =
                registry.active_projects().iter().map(|p| p.name.clone()).collect();
        }
        if event.event_type == EventType::InitiativeAdvanceRequested
            && let Some(name) = event.payload_str("initiative")
        {
            // Integration preparation fast-forwards every registered trunk and
            // gates one snapshot of them all, so a steer reserves their
            // repositories through normal pacing before any Git work. The
            // reservation is optional: pacing settles it when the steer
            // starts, taking every trunk if all are free and none otherwise.
            // A required reservation would hold the steer behind a member's
            // whole cycle, so the initiative could never run a second member.
            // A steer that starts without the trunks skips integration.
            let campaigns = foundry_sdk::campaign::CampaignStore::load(&self.campaigns_path)?;
            if let Some(initiative) = campaigns.find_initiative(name)
                && (initiative.completion_proposal.is_some()
                    || initiative.done_evidence.iter().any(|evidence| {
                        matches!(evidence, foundry_sdk::initiative::DoneEvidence::Gate { .. })
                    }))
            {
                item.reserved_projects.clone_from(&initiative.projects);
                item.reservation_if_free = true;
            }
        }
        Ok(Some(item))
    }
}

fn acknowledge_action(
    guard: &mut Option<foundry_sdk::campaign::CampaignStoreGuard>,
    event: &Event,
    item_id: &str,
) -> anyhow::Result<()> {
    if let (Some(guard), Some(action), Some(initiative)) = (
        guard.as_mut(),
        event.payload_str("steering_action_id"),
        event.payload_str("initiative"),
    ) {
        guard.store.acknowledge_steering_action(initiative, action, Some(item_id))?;
        guard.save()?;
    }
    Ok(())
}

fn recorded_root(store: &WorkItemStore, id: &str) -> bool {
    store.items.iter().any(|item| {
        item.pending_root.as_ref().is_some_and(|root| root.id == id)
            || item
                .started_root
                .as_ref()
                .is_some_and(|root| root.id == id || root.causation_id.as_deref() == Some(id))
    })
}

fn distinct_steering_trigger(item: &WorkItem, event: &Event) -> bool {
    let same_trigger = |root: &Event| {
        root.id == event.id
            || (event.payload_str("trigger_id").is_some()
                && root.payload_str("trigger_id") == event.payload_str("trigger_id"))
    };
    !item.started_root.as_ref().is_some_and(same_trigger)
        && !item.coalesced_root.as_ref().is_some_and(same_trigger)
}

/// Control facts and system coordination do not execute project work.
fn control_event(kind: &EventType) -> bool {
    matches!(
        kind,
        EventType::MaintenanceCycleStarted
            | EventType::MaintenanceCycleCompleted
            | EventType::WorkItemSubmitted
            | EventType::WorkItemStarted
            | EventType::WorkItemSettled
            | EventType::WorkItemCancelled
            | EventType::WorkItemHeld
            | EventType::WorkItemReleased
            | EventType::PacingPaused
            | EventType::PacingResumed
            | EventType::AgentSessionStarted
            | EventType::AgentSessionEnded
            | EventType::InitiativeAdded
            | EventType::InitiativeDecided
            | EventType::InitiativeCompleted
            | EventType::InitiativeCancelled
            | EventType::InitiativeSteered
            | EventType::InitiativeOwnerActionRecorded
            | EventType::InitiativeActionRequested
            | EventType::FindingRecorded
            | EventType::FindingConsumed
            | EventType::CampaignCancelled
            | EventType::CampaignCompleted
            | EventType::CampaignEscalated
            | EventType::WorkReconcileStarted
            | EventType::WorkReconcileCompleted
    )
}

fn lifecycle(kind: EventType, item: &WorkItem) -> anyhow::Result<Event> {
    Ok(Event::new(
        kind,
        item.project.clone(),
        foundry_sdk::throttle::Throttle::Full,
        Event::serialize_payload(&WorkItemEventPayload::from_item(item))?,
    )
    .with_trace_id(item.trace_id.clone())
    .with_source(item.source.clone()))
}

impl WorkAdmission for DurableWorkAdmission {
    fn wake_handle(&self) -> Arc<Notify> {
        self.changed.clone()
    }
    fn failure_events(&self, root: &Event, reason: &str) -> Vec<Event> {
        if let (Some(action), Some(initiative)) =
            (root.payload_str("steering_action_id"), root.payload_str("initiative"))
        {
            let discard = (|| -> anyhow::Result<()> {
                let mut guard =
                    foundry_sdk::campaign::CampaignStore::lock_exclusive(&self.campaigns_path)?;
                let _gate = ledger_write_gate()
                    .lock()
                    .map_err(|_| anyhow::anyhow!("ledger gate poisoned"))?;
                let store = self.store()?;
                if !recorded_root(&store, &root.id) {
                    guard.store.discard_steering_action(initiative, action)?;
                    guard.save()?;
                }
                Ok(())
            })();
            if let Err(error) = discard {
                tracing::error!(%error, action, "could not release rejected initiative action; recovery retains its receipt");
            }
        }
        vec![stopped_event(root, reason)]
    }

    fn owner(&self, root: &Event) -> anyhow::Result<Option<String>> {
        let Some(id) = root.payload_str(ADMITTED_WORK_ITEM_KEY) else {
            return Ok(None);
        };
        let store = self.store()?;
        let item = store
            .find(id)
            .ok_or_else(|| anyhow::anyhow!("unrecorded execution owner {id}"))?;
        anyhow::ensure!(
            item.state == WorkItemState::Running && item.started_root.as_ref() == Some(root),
            "root does not match the scheduler's durable started evidence for {id}"
        );
        Ok(Some(id.to_string()))
    }

    fn prepare(&self, event: &Event, owner: Option<&str>) -> anyhow::Result<Admission> {
        if let Some(id) = owner {
            let independent = event.event_type == EventType::InitiativeAdvanceRequested
                || event.event_type == EventType::CampaignAdvanceRequested
                || event.event_type == EventType::ExecutionRequested;
            let store = self.store()?;
            let item = store
                .find(id)
                .ok_or_else(|| anyhow::anyhow!("execution owner disappeared: {id}"))?;
            let own_root = item.started_root.as_ref().is_some_and(|root| root.id == event.id);
            if own_root
                || (!independent
                    && (item.project == event.project
                        || item.reserved_projects.contains(&event.project)))
            {
                return Ok(Admission::Execute);
            }
        }
        let external_context = self.validate_external_context(event)?;
        // An initiative action reserves under the campaign store before taking
        // the ledger gate. A durable queued child is the receipt, even if the
        // later campaign-store save fails and startup must acknowledge it again.
        let mut action_guard = if event.payload_str("steering_action_id").is_some() {
            Some(foundry_sdk::campaign::CampaignStore::lock_exclusive(&self.campaigns_path)?)
        } else {
            None
        };
        let _gate = ledger_write_gate()
            .lock()
            .map_err(|_| anyhow::anyhow!("ledger write gate poisoned"))?;
        let mut store = self.store()?;
        let Some(mut item) = self.planned(event)? else {
            return Ok(Admission::Execute);
        };
        if let Some(existing) = store.items.iter().find(|item| {
            item.pending_root.as_ref().is_some_and(|root| root.id == event.id)
                || item
                    .started_root
                    .as_ref()
                    .is_some_and(|root| root.causation_id.as_deref() == Some(&event.id))
        }) {
            acknowledge_action(&mut action_guard, event, &existing.id)?;
            return Ok(Admission::Deferred {
                id: existing.id.clone(),
                submitted: None,
            });
        }
        validate_action_admission(action_guard.as_ref(), event, &store, external_context.as_ref())?;
        if event.event_type == EventType::InitiativeAdvanceRequested
            && let Some(existing) = store.items.iter_mut().find(|item| {
                (item.state.is_waiting() || item.occupies_slot())
                    && item.pending_root.as_ref().or(item.started_root.as_ref()).is_some_and(
                        |root| {
                            root.event_type == EventType::InitiativeAdvanceRequested
                                && root.payload_str("initiative") == event.payload_str("initiative")
                        },
                    )
            })
        {
            let id = existing.id.clone();
            if existing.occupies_slot() && distinct_steering_trigger(existing, event) {
                existing.coalesced_root = Some(event.clone());
                store.save(&self.path)?;
            }
            return Ok(Admission::Deferred {
                id,
                submitted: None,
            });
        }
        store.upsert(item.clone());
        item.reason = queued_reason(&store, &item.id, &self.registry, &self.pacing);
        store.upsert(item.clone());
        let submitted = lifecycle(EventType::WorkItemSubmitted, &item)?;
        self.writer.write(&submitted)?;
        store.save(&self.path)?;
        acknowledge_action(&mut action_guard, event, &item.id)?;
        Ok(Admission::Deferred {
            id: item.id.clone(),
            submitted: Some(Box::new(submitted)),
        })
    }

    fn wait(&self, id: &str) -> ResultFuture<'_> {
        let id = id.to_string();
        let mut events = self.event_tx.subscribe();
        Box::pin(async move {
            loop {
                let notified = self.changed.notified();
                if let Some(reason) = self
                    .persistence_failures
                    .lock()
                    .map_err(|_| anyhow::anyhow!("persistence failure notification poisoned"))?
                    .get(&id)
                    .cloned()
                {
                    anyhow::bail!("{reason}");
                }
                let store = self.store()?;
                let item = store
                    .find(&id)
                    .ok_or_else(|| anyhow::anyhow!("deferred item disappeared: {id}"))?;
                // A traversal that ran to completion and then settled
                // `cancelled` (a steer whose decision a guard refused) returns
                // its own result. An aborted traversal has none and stops below.
                if item.state == WorkItemState::Cancelled
                    && item.execution_completed
                    && let Some(result) = item
                        .started_root
                        .as_ref()
                        .and_then(|root| self.result_writer.read(&root.id))
                {
                    return Ok(result);
                }
                if (item.state == WorkItemState::Cancelled && !item.occupies_slot())
                    || item.reason == "daemon restarted"
                    || (item.started_at.is_none() && !item.state.is_waiting())
                {
                    return self.stopped(item);
                }
                if item.execution_completed {
                    if item.reason.starts_with("settlement persistence failed:") {
                        return self.stopped(item);
                    }
                    if let Some(result) = item
                        .started_root
                        .as_ref()
                        .and_then(|root| self.result_writer.read(&root.id))
                    {
                        return Ok(result);
                    }
                    return self.stopped(item);
                }
                tokio::select! {
                    () = notified => {},
                    _ = events.recv() => {},
                    () = tokio::time::sleep(Duration::from_secs(15)) => {},
                }
            }
        })
    }

    fn complete(&self, root: &Event, result: &ProcessResult) -> anyhow::Result<Vec<Event>> {
        let Some(id) = root.payload_str(ADMITTED_WORK_ITEM_KEY) else {
            return Ok(vec![]);
        };
        let initiative_name = root.payload_str("initiative").or_else(|| {
            root.source
                .as_ref()
                .filter(|source| {
                    source.kind == foundry_sdk::work_source::WorkSourceKind::Initiative
                })
                .map(|source| source.reference.as_str())
        });
        // Completion releases admission reservations even for passive owner dispatches.
        // Match the same campaign-store-before-ledger order as final admission.
        let mut initiative_guard = initiative_name
            .map(|_| foundry_sdk::campaign::CampaignStore::lock_exclusive(&self.campaigns_path))
            .transpose()?;
        let _gate = ledger_write_gate()
            .lock()
            .map_err(|_| anyhow::anyhow!("ledger write gate poisoned"))?;
        let completion = (|| -> anyhow::Result<Vec<Event>> {
            let mut store = self.store()?;
            let item = store
                .find_mut(id)
                .ok_or_else(|| anyhow::anyhow!("execution owner disappeared: {id}"))?;
            self.result_writer.write(&root.id, result)?;
            item.execution_completed = true;
            let mut events = if item.is_running() {
                if root.event_type == EventType::ExecutionRequested
                    && WorkflowType::from_payload(&root.payload) == WorkflowType::Task
                    && root.throttle.permits_mutation()
                {
                    item.settle_failed(
                        "task traversal ended without typed settlement; inspect its trace",
                        Utc::now(),
                    );
                } else if let Some(settlement) = steering_settlement(root, result) {
                    match settlement {
                        SteeringSettlement::Landed(reason) => {
                            item.settle_landed(&reason, Utc::now());
                        }
                        SteeringSettlement::Cancelled(reason) => {
                            item.settle_cancelled(&reason, item.disposition.clone(), Utc::now());
                        }
                    }
                } else if workflow_succeeded(root, result) {
                    let reason = if root.throttle.permits_mutation() {
                        "workflow completed"
                    } else {
                        "dry-run completed; mutations simulated"
                    };
                    item.settle_landed(reason, Utc::now());
                } else {
                    item.settle_failed("workflow failed; inspect its trace", Utc::now());
                }
                let event = lifecycle(EventType::WorkItemSettled, item)?;
                store.save(&self.path)?;
                self.writer.write(&event)?;
                vec![event]
            } else {
                vec![]
            };
            if (root.event_type != EventType::CampaignAdvanceRequested
                || workflow_succeeded(root, result))
                && let Some(next_root) = self.automatic_steer_after(root, &store)?
            {
                self.queue_steering_root(&mut store, next_root, &mut events)?;
            }
            let rerun = store.find_mut(id).and_then(|item| item.coalesced_root.take());
            if let Some(rerun) = rerun {
                self.queue_steering_root(&mut store, rerun, &mut events)?;
            }
            store.save(&self.path)?;
            if let (Some(guard), Some(name)) = (initiative_guard.as_mut(), initiative_name)
                && guard.store.find_initiative(name).is_some()
            {
                guard.store.release_member_reservation(name, id)?;
                guard.save()?;
            }
            Ok(events)
        })();
        if let Err(error) = &completion {
            let reason = format!("settlement persistence failed: {error}");
            // Deliver an I/O fault to current waiters even when that same fault
            // prevents updating durable state. Recovery settles the open slot.
            self.persistence_failures
                .lock()
                .map_err(|_| anyhow::anyhow!("persistence failure notification poisoned"))?
                .insert(id.to_string(), reason.clone());
            let failed_save = (|| -> anyhow::Result<()> {
                let mut failed_store = self.store()?;
                let failed = failed_store
                    .find_mut(id)
                    .ok_or_else(|| anyhow::anyhow!("execution owner disappeared: {id}"))?;
                failed.execution_completed = true;
                failed.settle_failed(&reason, Utc::now());
                failed_store.save(&self.path)?;
                Ok(())
            })();
            if let Err(save_error) = failed_save {
                tracing::error!(%save_error, item_id = id, "could not record failed completion; durable slot remains open");
            }
        }
        self.changed.notify_waiters();
        completion
    }
}

fn validate_action_admission(
    guard: Option<&foundry_sdk::campaign::CampaignStoreGuard>,
    event: &Event,
    store: &WorkItemStore,
    external_context: Option<&foundry_sdk::initiative::SteeringExternalContext>,
) -> anyhow::Result<()> {
    if let (Some(guard), Some(action), Some(initiative)) =
        (guard, event.payload_str("steering_action_id"), event.payload_str("initiative"))
    {
        let active = guard.store.ledger_active_members(store);
        let pending = guard
            .store
            .find_initiative(initiative)
            .and_then(|initiative| {
                initiative.pending_actions.iter().find(|pending| pending.id == action)
            })
            .ok_or_else(|| {
                anyhow::anyhow!("initiative action receipt disappeared before admission")
            })?;
        anyhow::ensure!(
            external_context.is_some(),
            "initiative work action requires captured external evidence"
        );
        anyhow::ensure!(
            pending.external_context.as_ref() == external_context,
            "initiative child evidence differs from the persisted action receipt"
        );
        guard.store.enforce_steering_action(initiative, action, &active)?;
        anyhow::ensure!(
            !store.items.iter().any(|item| item.occupies_slot()
                && !owns_assessment_reservation(item, pending)
                && (item.project == event.project
                    || item.reserved_projects.contains(&event.project))),
            "initiative action cannot dispatch into a busy project: {}",
            event.project
        );
    }
    Ok(())
}

fn owns_assessment_reservation(
    item: &WorkItem,
    action: &foundry_sdk::initiative::DurableSteeringAction,
) -> bool {
    item.started_root.as_ref().is_some_and(|root| {
        root.id == action.id
            && root.event_type == EventType::InitiativeAdvanceRequested
            && root.payload_str("initiative") == Some(action.initiative.as_str())
    })
}

/// The event that ends `root`'s workflow and carries its verdict, if it has one.
fn terminal_kind(root: &Event) -> Option<EventType> {
    match root.event_type {
        EventType::ProjectRunStarted if root.gather_id.is_some() => {
            Some(EventType::ProjectRunCompleted)
        }
        EventType::ProjectMaintenanceRequested => Some(EventType::ProjectMaintenanceCompleted),
        EventType::ProjectIterationRequested => Some(EventType::ProjectIterationCompleted),
        EventType::ReleaseRequested => Some(EventType::ReleaseCompleted),
        EventType::CampaignAdvanceRequested => Some(EventType::CampaignAdvanceCompleted),
        EventType::InitiativeAdvanceRequested => Some(EventType::InitiativeSteered),
        EventType::ExecutionRequested
            if WorkflowType::from_payload(&root.payload) == WorkflowType::Task =>
        {
            Some(EventType::TaskRunCompleted)
        }
        _ => None,
    }
}

/// An outcome event that reports `success: false`.
fn reports_failure(event: &Event) -> bool {
    matches!(
        event.event_type,
        EventType::ProjectRunCompleted
            | EventType::ProjectIterationCompleted
            | EventType::ProjectMaintenanceCompleted
            | EventType::ReleaseCompleted
            | EventType::RemediationCompleted
            | EventType::TaskRunCompleted
    ) && event.payload_bool("success") == Some(false)
}

/// Whether `root`'s workflow succeeded.
///
/// A workflow with a terminal event is judged by its last terminal: it must
/// exist and not report failure. Foundry retries failed steps (a failed gate
/// run asks for another attempt), so a failed block *before* that terminal is
/// a retry the run recovered from, not a failure of the run. A block that
/// fails *after* it (for example the local install that follows a completed
/// maintenance run), or an outcome event reporting failure after it, still
/// fails the workflow. A workflow without a terminal keeps the strict rule:
/// every block succeeded and no outcome event reported failure.
fn workflow_succeeded(root: &Event, result: &ProcessResult) -> bool {
    let Some(kind) = terminal_kind(root) else {
        return result.is_success()
            && result.block_executions.iter().all(|block| block.success)
            && !result.events.iter().any(reports_failure);
    };
    let Some(last) = result.events.iter().rposition(|event| event.event_type == kind) else {
        return false;
    };
    let terminal = &result.events[last];
    if terminal.payload_bool("success") == Some(false) {
        return false;
    }
    let after_terminal = result
        .block_executions
        .iter()
        .rposition(|block| block.emitted_event_ids.contains(&terminal.id))
        .map_or(&result.block_executions[..0], |at| &result.block_executions[at + 1..]);
    after_terminal.iter().all(|block| block.success)
        && !result.events[last + 1..].iter().any(reports_failure)
}

/// The settlement a steer's own outcome decides, when its workflow succeeded.
///
/// A wait, a decision already in force and a skipped assessment land with
/// their reason. A decision a guard refused settles `cancelled`: it took no
/// effect and owes nobody anything, so it must not appear among the failures
/// that need a person. `None` keeps the ordinary judgement.
fn steering_settlement(root: &Event, result: &ProcessResult) -> Option<SteeringSettlement> {
    if root.event_type != EventType::InitiativeAdvanceRequested || !workflow_succeeded(root, result)
    {
        return None;
    }
    result
        .events
        .iter()
        .rfind(|event| event.event_type == EventType::InitiativeSteered)?
        .parse_payload::<InitiativeSteeredPayload>()
        .ok()?
        .settlement()
}

fn stopped_result(item: &WorkItem) -> ProcessResult {
    let root = item.pending_root.as_ref().or(item.started_root.as_ref());
    let mut execution = BlockExecution::new(
        "Queued work",
        root.map_or("", |e| e.id.as_str()),
        0,
        serde_json::json!({}),
    );
    execution.summary.clone_from(&item.reason);
    let mut events = vec![];
    if let Some(root) = root {
        let mut terminal = stopped_event(root, &item.reason);
        terminal.id = format!("evt_stopped_{}", item.id);
        events.push(terminal);
    }
    ProcessResult {
        events,
        block_executions: vec![execution],
        total_duration_ms: 0,
    }
}

fn stopped_event(root: &Event, reason: &str) -> Event {
    let kind = match root.event_type {
        EventType::ProjectRunStarted => EventType::ProjectRunCompleted,
        EventType::ProjectMaintenanceRequested => EventType::ProjectMaintenanceCompleted,
        EventType::ProjectIterationRequested => EventType::ProjectIterationCompleted,
        EventType::ReleaseRequested => EventType::ReleaseCompleted,
        EventType::PipelineCheckRequested
        | EventType::MainBranchAudited
        | EventType::PipelineChecked
        | EventType::VulnerabilityDetected => EventType::RemediationCompleted,
        EventType::ExecutionRequested
            if WorkflowType::from_payload(&root.payload) == WorkflowType::Task =>
        {
            EventType::TaskRunCompleted
        }
        EventType::ValidationRequested => EventType::ValidationCompleted,
        EventType::DriftAssessmentRequested => EventType::DriftAssessmentCompleted,
        EventType::CampaignAdvanceRequested => EventType::CampaignAdvanceCompleted,
        EventType::InitiativeAdvanceRequested => EventType::InitiativeSteered,
        EventType::SupplyChainScanStarted => EventType::SupplyChainScanCompleted,
        _ => EventType::Custom("workflow_refused".to_string()),
    };
    let mut payload = root.payload.clone();
    if !payload.is_object() {
        payload = serde_json::json!({});
    }
    for (key, value) in serde_json::json!({
        "project": root.project, "success": false, "landed": false,
        "summary": reason, "reason": reason, "root_event_id": root.id,
        "verdict": "runner_error", "detail": reason,
        "decision": "escalate", "cycles_completed": 0, "cycles_landed": 0,
        "workflow": WorkflowType::from_payload(&root.payload).as_str(),
        "candidate_count": 0, "high_value_count": 0, "candidates": [],
    })
    .as_object()
    .into_iter()
    .flatten()
    {
        payload[key] = value.clone();
    }
    Event::new(kind, root.project.clone(), root.throttle, payload)
        .with_trace_id(root.trace_id.clone())
        .with_gather_id(root.gather_id.clone())
        .with_span_ids(root.span_id.clone(), root.parent_span_id.clone())
        .with_causation_id(Some(root.id.clone()))
        .with_source(root.source.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_engine::engine::Engine;
    use foundry_engine::event_writer::EventWriter;
    use foundry_sdk::registry::{ActionFlags, ProjectEntry, Stack};
    use foundry_sdk::task_block::{BlockFuture, BlockKind, TaskBlock, TaskBlockResult};
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::work_source::WorkSource;
    use std::sync::Mutex;
    use tokio::task::JoinHandle;

    struct Runner(Arc<Mutex<Vec<Event>>>);
    impl TaskBlock for Runner {
        fn dry_run_events(&self, event: &Event) -> Vec<Event> {
            vec![Event::new(
                EventType::ReleaseCompleted,
                event.project.clone(),
                event.throttle,
                serde_json::json!({"success": true, "dry_run": true}),
            )]
        }
        fn name(&self) -> &'static str {
            "Synthetic engineering work"
        }
        fn kind(&self) -> BlockKind {
            BlockKind::Mutator
        }
        fn sinks_on(&self) -> &[EventType] {
            &[
                EventType::ProjectRunStarted,
                EventType::ProjectMaintenanceRequested,
                EventType::ProjectIterationRequested,
                EventType::ReleaseRequested,
                EventType::CampaignAdvanceRequested,
                EventType::InitiativeAdvanceRequested,
                EventType::SupplyChainScanStarted,
                EventType::PipelineCheckRequested,
                EventType::MainBranchAudited,
                EventType::CommitDigestStarted,
                EventType::OpsDigestStarted,
                EventType::CommitsObserved,
                EventType::OpsObserved,
            ]
        }
        fn execute(&self, event: &Event) -> BlockFuture<'_> {
            self.0.lock().expect("effects").push(event.clone());
            let terminal = match event.event_type {
                EventType::ProjectRunStarted => EventType::ProjectRunCompleted,
                EventType::ProjectMaintenanceRequested => EventType::ProjectMaintenanceCompleted,
                EventType::ProjectIterationRequested => EventType::ProjectIterationCompleted,
                EventType::ReleaseRequested => EventType::ReleaseCompleted,
                EventType::CampaignAdvanceRequested => EventType::CampaignAdvanceCompleted,
                EventType::InitiativeAdvanceRequested => EventType::InitiativeSteered,
                EventType::PipelineCheckRequested | EventType::MainBranchAudited => {
                    EventType::RemediationCompleted
                }
                _ => EventType::SupplyChainScanCompleted,
            };
            let completed = Event::new(
                terminal,
                event.project.clone(),
                event.throttle,
                serde_json::json!({"success": true}),
            );
            Box::pin(
                async move { Ok(TaskBlockResult::success("done".to_string(), vec![completed])) },
            )
        }
    }

    struct ObserverRunner(Arc<Mutex<Vec<Event>>>);
    impl TaskBlock for ObserverRunner {
        fn name(&self) -> &'static str {
            "Synthetic reasoning observer"
        }
        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }
        fn sinks_on(&self) -> &[EventType] {
            &[EventType::OpsObserved]
        }
        fn execute(&self, event: &Event) -> BlockFuture<'_> {
            self.0.lock().expect("observer calls").push(event.clone());
            let terminal = Event::new(
                EventType::OpsDigestCompleted,
                event.project.clone(),
                event.throttle,
                serde_json::json!({"success": true}),
            );
            Box::pin(async move { Ok(TaskBlockResult::success("reasoned", vec![terminal])) })
        }
    }

    struct Continuation {
        from: EventType,
        to: EventType,
    }

    struct WaitingRoot {
        entered: Arc<Notify>,
        finish: Arc<Notify>,
    }
    impl TaskBlock for WaitingRoot {
        fn name(&self) -> &'static str {
            "Wait for root completion"
        }
        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }
        fn sinks_on(&self) -> &[EventType] {
            &[EventType::ProjectRunStarted]
        }
        fn execute(&self, _event: &Event) -> BlockFuture<'_> {
            Box::pin(async move {
                self.entered.notify_one();
                self.finish.notified().await;
                Ok(TaskBlockResult::success("finished cleanup", vec![]))
            })
        }
    }
    impl TaskBlock for Continuation {
        fn name(&self) -> &'static str {
            "Synthetic continuation"
        }
        fn kind(&self) -> BlockKind {
            BlockKind::Mutator
        }
        fn sinks_on(&self) -> &[EventType] {
            std::slice::from_ref(&self.from)
        }
        fn execute(&self, event: &Event) -> BlockFuture<'_> {
            let payload = if self.to == EventType::ExecutionRequested {
                serde_json::json!({"project": event.project, "workflow": "task", "prompt": "derived work"})
            } else {
                serde_json::json!({})
            };
            let child = Event::new(self.to.clone(), event.project.clone(), event.throttle, payload);
            Box::pin(async move { Ok(TaskBlockResult::success("continue", vec![child])) })
        }
    }

    struct Harness {
        dir: tempfile::TempDir,
        policy: Arc<DurableWorkAdmission>,
        engine: Arc<Engine>,
        scheduler: crate::pacing::PacingScheduler,
        effects: Arc<Mutex<Vec<Event>>>,
    }
    impl Harness {
        fn new(admission: bool) -> Self {
            Self::with_blocks(admission, vec![])
        }
        fn with_blocks(admission: bool, blocks: Vec<Box<dyn TaskBlock>>) -> Self {
            let dir = tempfile::tempdir().expect("tempdir");
            let registry = Arc::new(RwLock::new(Registry {
                version: 2,
                projects: ["alpha", "beta"]
                    .into_iter()
                    .map(|name| ProjectEntry {
                        name: name.to_string(),
                        path: dir.path().join(name).display().to_string(),
                        stack: Stack::Rust,
                        agent: String::new(),
                        repo: String::new(),
                        branch: "main".to_string(),
                        skip: None,
                        notes: None,
                        actions: ActionFlags::default(),
                        install: None,
                        installs_skill: None,
                        timeout_secs: None,
                        audit_exceptions: vec![],
                        update_policy: None,
                    })
                    .collect(),
            }));
            let pacing = PacingPaths {
                limits: dir.path().join("pacing.json"),
                state: dir.path().join("paused.json"),
            };
            let (event_tx, _) = broadcast::channel(128);
            let writer = Arc::new(EventWriter::new(dir.path().join("events")));
            let mut policy = DurableWorkAdmission::new(
                dir.path().join("work.json"),
                registry.clone(),
                pacing.clone(),
                event_tx.clone(),
                writer.clone(),
                Arc::new(foundry_blocks::trace_writer::TraceWriter::new(
                    dir.path().join("traces").to_str().expect("utf8"),
                )),
            );
            policy.supply_chain_remediation = true;
            let policy = Arc::new(policy);
            let mut engine =
                Engine::new().with_event_writer(writer).with_event_broadcaster(event_tx.clone());
            if admission {
                engine = engine.with_work_admission(policy.clone());
            }
            let effects = Arc::new(Mutex::new(vec![]));
            for block in blocks {
                engine.register(block);
            }
            engine.register(Box::new(Runner(effects.clone())));
            engine
                .register(Box::new(crate::orchestrator::FanOutMaintenance::new(registry.clone())));
            let engine = Arc::new(engine);
            let ctx = crate::service::RuntimeContext {
                engine: engine.clone(),
                registry,
                event_tx,
                trace_store: Arc::new(crate::trace_store::TraceStore::new(Duration::from_secs(60))),
                trace_writer: Arc::new(foundry_blocks::trace_writer::TraceWriter::new(
                    dir.path().join("traces").to_str().expect("utf8"),
                )),
                workflow_tracker: Arc::new(crate::workflow_tracker::WorkflowTracker::new()),
            };
            let dispatch_engine = engine.clone();
            let scheduler = crate::pacing::PacingScheduler::new(
                ctx,
                policy.path.clone(),
                pacing,
                policy.changed.clone(),
            )
            .with_dispatch(Arc::new(move |event| {
                let engine = dispatch_engine.clone();
                tokio::spawn(async move {
                    engine.process(event).await;
                });
            }));
            Self {
                dir,
                policy,
                engine,
                scheduler,
                effects,
            }
        }
        fn items(&self) -> Vec<WorkItem> {
            self.policy.store().expect("ledger").items
        }
        fn seed_running(&self, project: &str) -> String {
            let item = WorkItem::dispatched(
                WorkItemSpec {
                    project: project.to_string(),
                    objective: "campaign".to_string(),
                    kind: WorkItemKind::CampaignCycle,
                    lane: WorkLane::Campaign,
                    origin: "test".to_string(),
                    trace_id: Some("b".repeat(32)),
                },
                Utc::now(),
            );
            let id = item.id.clone();
            let _gate = ledger_write_gate().lock().expect("gate");
            let mut store = self.policy.store().expect("load");
            store.upsert(item);
            store.save(&self.policy.path).expect("save");
            id
        }
        fn settle(&self, id: &str) {
            let _gate = ledger_write_gate().lock().expect("gate");
            let mut store = self.policy.store().expect("load");
            store.find_mut(id).expect("item").settle_landed("done", Utc::now());
            store.save(&self.policy.path).expect("save");
        }
        fn submit(&self, kind: EventType, project: &str) -> JoinHandle<ProcessResult> {
            let engine = self.engine.clone();
            let event = root(kind, project);
            tokio::spawn(async move { engine.process(event).await })
        }
        async fn admitted(&self, count: usize) {
            tokio::time::timeout(Duration::from_secs(2), async {
                while self.items().iter().filter(|i| i.is_queued()).count() < count {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("expected all roots durably queued before execution");
        }
    }
    fn root(kind: EventType, project: &str) -> Event {
        Event::new(kind, project.to_string(), Throttle::Full, serde_json::json!({}))
            .with_trace_id(Some("a".repeat(32)))
            .with_span_ids(Some("c".repeat(16)), None)
            .with_source(Some(WorkSource::sentinel("nightly-maintenance")))
    }

    #[tokio::test]
    async fn cancelled_started_root_keeps_its_join_and_slot_until_traversal_returns() {
        let entered = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        let h = Harness::with_blocks(
            true,
            vec![Box::new(WaitingRoot {
                entered: entered.clone(),
                finish: finish.clone(),
            })],
        );
        let pending = h.submit(EventType::ProjectRunStarted, "alpha");
        h.admitted(1).await;
        h.scheduler.tick().await;
        tokio::time::timeout(Duration::from_secs(2), entered.notified())
            .await
            .expect("root started");
        let owner_id = h.items().remove(0).id;
        {
            let _gate = ledger_write_gate().lock().expect("gate");
            let mut store = h.policy.store().expect("store");
            store.find_mut(&owner_id).expect("owner").settle_cancelled(
                "obligation cancelled during cleanup",
                None,
                Utc::now(),
            );
            store.save(&h.policy.path).expect("save");
        }
        let join = tokio::spawn({
            let engine = h.engine.clone();
            let id = owner_id.clone();
            async move { engine.join_work(&id).await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(30), async {
                while !join.is_finished() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .is_err(),
            "cancelled root must not report traversal completion early"
        );
        let next = h.submit(EventType::ReleaseRequested, "alpha");
        h.admitted(1).await;
        h.scheduler.tick().await;
        assert!(
            h.items()
                .iter()
                .find(|item| item.id == owner_id)
                .expect("owner")
                .occupies_slot()
        );
        assert!(h.effects.lock().expect("effects").is_empty(), "same repository still owned");
        finish.notify_one();
        tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("root finished")
            .expect("process join");
        tokio::time::timeout(Duration::from_secs(2), join)
            .await
            .expect("wait finished")
            .expect("wait join")
            .expect("result");
        let owner = h.items().into_iter().find(|item| item.id == owner_id).expect("owner");
        assert!(owner.execution_completed);
        assert!(!owner.occupies_slot());
        assert_eq!(owner.state, WorkItemState::Cancelled);
        h.scheduler.tick().await;
        tokio::time::timeout(Duration::from_secs(2), next)
            .await
            .expect("next ran")
            .expect("next join");
    }

    #[tokio::test]
    async fn nightly_stages_every_project_and_waits_for_campaign_repository_before_gather() {
        let h = Harness::new(true);
        let campaign = h.seed_running("alpha");
        let cycle = h.submit(EventType::MaintenanceCycleStarted, "system");
        h.admitted(2).await;
        assert!(h.effects.lock().expect("effects").is_empty());
        assert!(!cycle.is_finished(), "queued children keep gather outstanding");
        h.scheduler.tick().await;
        tokio::task::yield_now().await;
        assert!(h.items().iter().any(|i| i.project == "alpha" && i.is_queued()));
        h.settle(&campaign);
        h.scheduler.tick().await;
        let result = tokio::time::timeout(Duration::from_secs(2), cycle)
            .await
            .expect("cycle complete")
            .expect("join");
        let gathered = result
            .events
            .iter()
            .find(|e| e.event_type == EventType::MaintenanceCycleCompleted)
            .expect("gather");
        assert_eq!(gathered.payload["arrived"], 2);
        assert!(gathered.payload.get("missing").is_none());
        assert!(result.is_success());
        for effect in h.effects.lock().expect("effects").iter() {
            assert_eq!(effect.source, Some(WorkSource::sentinel("nightly-maintenance")));
            assert_eq!(effect.trace_id.as_deref(), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
            assert!(effect.gather_id.is_some());
        }
    }

    #[tokio::test]
    async fn manual_roots_share_repository_exclusion_and_host_cap_with_campaigns() {
        for kind in [
            EventType::ProjectMaintenanceRequested,
            EventType::ProjectIterationRequested,
            EventType::ReleaseRequested,
            EventType::PipelineCheckRequested,
            EventType::MainBranchAudited,
            EventType::CampaignAdvanceRequested,
        ] {
            let h = Harness::new(true);
            let campaign = h.seed_running("alpha");
            let pending = h.submit(kind.clone(), "alpha");
            h.admitted(1).await;
            h.scheduler.tick().await;
            assert!(h.effects.lock().expect("effects").is_empty(), "{kind}");
            assert!(h.items().iter().any(|i| i.reason.starts_with("repository busy:")));
            h.settle(&campaign);
            h.scheduler.tick().await;
            let result = tokio::time::timeout(Duration::from_secs(2), pending)
                .await
                .expect("completed")
                .expect("join");
            assert!(result.is_success(), "{kind}");
        }
        let h = Harness::new(true);
        std::fs::write(&h.policy.pacing.limits, r#"{"max_running":1}"#).expect("limits");
        let campaign = h.seed_running("alpha");
        let pending = h.submit(EventType::ReleaseRequested, "beta");
        h.admitted(1).await;
        h.scheduler.tick().await;
        assert!(h.effects.lock().expect("effects").is_empty());
        assert!(h.items().iter().any(|i| i.reason == "host at capacity 1/1"));
        h.settle(&campaign);
        h.scheduler.tick().await;
        tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("completed")
            .expect("join");
    }

    #[tokio::test]
    async fn supply_chain_waits_for_every_repository_it_can_repair() {
        let h = Harness::new(true);
        let campaign = h.seed_running("beta");
        let pending = h.submit(EventType::SupplyChainScanStarted, "system");
        h.admitted(1).await;
        let queued = h.items().into_iter().find(WorkItem::is_queued).expect("scan queued");
        assert_eq!(queued.reserved_projects, ["alpha", "beta"]);
        h.scheduler.tick().await;
        assert!(h.effects.lock().expect("effects").is_empty());
        h.settle(&campaign);
        h.scheduler.tick().await;
        tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("completed")
            .expect("join");
    }

    #[tokio::test]
    async fn forged_admission_marker_and_unwritable_event_log_cannot_execute() {
        let h = Harness::new(true);
        let mut forged = root(EventType::ReleaseRequested, "alpha");
        forged.payload[ADMITTED_WORK_ITEM_KEY] = "wi_fake".into();
        let result = h.engine.process(forged).await;
        assert!(!result.is_success());
        assert!(h.effects.lock().expect("effects").is_empty());
        std::fs::write(h.dir.path().join("events-blocked"), "file").expect("block writer");
        let policy = DurableWorkAdmission::new(
            h.policy.path.clone(),
            h.policy.registry.clone(),
            h.policy.pacing.clone(),
            h.policy.event_tx.clone(),
            Arc::new(EventWriter::new(h.dir.path().join("events-blocked"))),
            h.policy.result_writer.clone(),
        );
        assert!(policy.prepare(&root(EventType::ReleaseRequested, "alpha"), None).is_err());
        assert!(h.items().is_empty(), "expected `h.items()` to be empty: {:?}", h.items());
    }
    #[tokio::test]
    async fn digest_agents_wait_for_the_same_host_capacity() {
        for kind in [
            EventType::CommitDigestStarted,
            EventType::OpsDigestStarted,
            EventType::CommitsObserved,
            EventType::OpsObserved,
        ] {
            let h = Harness::new(true);
            std::fs::write(&h.policy.pacing.limits, r#"{"max_running":1}"#).expect("limits");
            let campaign = h.seed_running("alpha");
            let pending = h.submit(kind.clone(), "system");
            h.admitted(1).await;
            h.scheduler.tick().await;
            assert!(h.effects.lock().expect("effects").is_empty(), "{kind}");
            assert!(h.items().iter().any(|i| i.reason == "host at capacity 1/1"));
            h.settle(&campaign);
            h.scheduler.tick().await;
            tokio::time::timeout(Duration::from_secs(2), pending)
                .await
                .expect("digest complete")
                .expect("join");
            assert_eq!(h.effects.lock().expect("effects").len(), 1, "{kind}");
        }
    }
    #[tokio::test]
    async fn nested_release_keeps_one_owner_and_campaign_child_waits_until_formation_finishes() {
        let h = Harness::with_blocks(
            true,
            vec![Box::new(Continuation {
                from: EventType::PipelineCheckRequested,
                to: EventType::ReleaseRequested,
            })],
        );
        std::fs::write(&h.policy.pacing.limits, r#"{"max_running":1}"#).expect("limits");
        let pending = h.submit(EventType::PipelineCheckRequested, "alpha");
        h.admitted(1).await;
        h.scheduler.tick().await;
        let result = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("no nested deadlock")
            .expect("join");
        assert!(result.is_success());
        assert_eq!(h.items().len(), 1);
        assert_eq!(h.effects.lock().expect("effects").len(), 2);

        let h = Harness::with_blocks(
            true,
            vec![Box::new(Continuation {
                from: EventType::CampaignAdvanceRequested,
                to: EventType::ExecutionRequested,
            })],
        );
        std::fs::write(&h.policy.pacing.limits, r#"{"max_running":1}"#).expect("limits");
        let pending = h.submit(EventType::CampaignAdvanceRequested, "alpha");
        h.admitted(1).await;
        h.scheduler.tick().await;
        tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("formation releases slot")
            .expect("join");
        let items = h.items();
        assert_eq!(items.len(), 2);
        assert!(items.iter().any(|item| item.kind == WorkItemKind::Task && item.is_queued()));
        assert!(items.iter().all(|item| !item.occupies_slot()));
        assert_eq!(h.scheduler.tick().await.started.len(), 1);
    }

    #[tokio::test]
    async fn held_maintenance_survives_restart_and_cancellation_publishes_one_failed_terminal() {
        let h = Harness::new(true);
        let pending = h.submit(EventType::MaintenanceCycleStarted, "system");
        h.admitted(2).await;
        let id = h.items().into_iter().find(|item| item.project == "alpha").expect("alpha").id;
        {
            let _gate = ledger_write_gate().lock().expect("gate");
            let mut store = h.policy.store().expect("store");
            store.find_mut(&id).expect("item").hold();
            assert!(
                crate::service::work_ledger::settle_running(&mut store).is_empty(),
                "expected `crate::service::work_ledger::settle_running(&mut store)` to be empty: {:?}",
                crate::service::work_ledger::settle_running(&mut store)
            );
            store.save(&h.policy.path).expect("save");
        }
        h.scheduler.tick().await;
        assert!(!pending.is_finished(), "held child keeps cycle open");
        let mut watcher = h.policy.event_tx.subscribe();
        {
            let _gate = ledger_write_gate().lock().expect("gate");
            let mut store = h.policy.store().expect("store");
            store.find_mut(&id).expect("item").settle_cancelled(
                "operator stopped maintenance",
                None,
                Utc::now(),
            );
            store.save(&h.policy.path).expect("save");
        }
        h.policy.changed.notify_waiters();
        let result = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("cycle terminal")
            .expect("join");
        assert!(!result.is_success());
        let gathered = result
            .events
            .iter()
            .find(|event| event.event_type == EventType::MaintenanceCycleCompleted)
            .expect("gather");
        assert_eq!(gathered.payload["arrived"], 2);
        assert_eq!(
            gathered.payload["children"]
                .as_array()
                .expect("children")
                .iter()
                .filter(|child| child["success"] == false)
                .count(),
            1
        );
        let stopped = result
            .events
            .iter()
            .find(|event| {
                event.project == "alpha" && event.event_type == EventType::ProjectRunCompleted
            })
            .expect("typed terminal");
        let mut delivered = vec![];
        while let Ok(event) = watcher.try_recv() {
            delivered.push(event);
        }
        assert_eq!(delivered.iter().filter(|event| event.id == stopped.id).count(), 1);
        let again = h.engine.join_work(&id).await.expect("rejoin cancelled");
        assert_eq!(again.events[0].id, stopped.id);
        assert!(watcher.try_recv().is_err(), "joining cannot publish terminal twice");
        let log = foundry_blocks::blocks::triage_core::read_events_jsonl(
            &h.dir.path().join("events"),
            Utc::now() - chrono::Duration::days(1),
        );
        assert_eq!(log.iter().filter(|event| event.id == stopped.id).count(), 1);
    }

    #[tokio::test]
    async fn completed_roots_rejoin_from_disk_and_lifecycle_facts_are_unique() {
        let h = Harness::new(true);
        let pending = h.submit(EventType::ReleaseRequested, "alpha");
        h.admitted(1).await;
        h.scheduler.tick().await;
        tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("done")
            .expect("join");
        let item = h.items().remove(0);
        let policy = Arc::new(DurableWorkAdmission::new(
            h.policy.path.clone(),
            h.policy.registry.clone(),
            h.policy.pacing.clone(),
            h.policy.event_tx.clone(),
            h.policy.writer.clone(),
            h.policy.result_writer.clone(),
        ));
        let recovered = Engine::new()
            .with_work_admission(policy)
            .join_work(&item.id)
            .await
            .expect("durable join");
        assert!(
            recovered
                .events
                .iter()
                .any(|event| event.event_type == EventType::ReleaseCompleted)
        );
        let log = foundry_blocks::blocks::triage_core::read_events_jsonl(
            &h.dir.path().join("events"),
            Utc::now() - chrono::Duration::days(1),
        );
        for kind in [
            EventType::WorkItemSubmitted,
            EventType::WorkItemStarted,
            EventType::WorkItemSettled,
        ] {
            assert_eq!(log.iter().filter(|event| event.event_type == kind).count(), 1, "{kind}");
        }

        // A result written before a failed settlement save can survive a
        // restart. The durable restart failure must win over that old result.
        let mut store = WorkItemStore::load(&h.policy.path).expect("store");
        store
            .find_mut(&item.id)
            .expect("item")
            .settle_failed("daemon restarted", Utc::now());
        store.save(&h.policy.path).expect("save restart");
        let failed = h.engine.join_work(&item.id).await.expect("join stopped root");
        assert!(!failed.is_success());
        assert_eq!(failed.events[0].event_type, EventType::ReleaseCompleted);
        assert_eq!(failed.events[0].payload_bool("success"), Some(false));
    }

    #[tokio::test]
    async fn dry_run_observers_wait_for_capacity_and_mutators_remain_simulated() {
        let calls = Arc::new(Mutex::new(vec![]));
        let h = Harness::with_blocks(true, vec![Box::new(ObserverRunner(calls.clone()))]);
        std::fs::write(&h.policy.pacing.limits, r#"{"max_running":1}"#).expect("limits");
        let campaign = h.seed_running("alpha");
        let mut event = root(EventType::OpsObserved, "system");
        event.throttle = Throttle::DryRun;
        let engine = h.engine.clone();
        let pending = tokio::spawn(async move { engine.process(event).await });
        h.admitted(1).await;
        h.scheduler.tick().await;
        assert!(calls.lock().expect("observer calls").is_empty());
        assert!(h.effects.lock().expect("mutations").is_empty());
        h.settle(&campaign);
        h.scheduler.tick().await;
        let result = tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("dry-run completed")
            .expect("join");
        assert!(result.is_success());
        assert_eq!(calls.lock().expect("observer calls").len(), 1);
        assert!(h.effects.lock().expect("mutations").is_empty());
        let dry_item = h
            .items()
            .into_iter()
            .find(|item| item.project == "system")
            .expect("dry-run record");
        assert_eq!(dry_item.reason, "dry-run completed; mutations simulated");
    }

    #[tokio::test]
    async fn a_failed_started_append_dispatches_nothing_and_releases_capacity() {
        let h = Harness::new(true);
        let Admission::Deferred { id, .. } = h
            .policy
            .prepare(&root(EventType::ReleaseRequested, "alpha"), None)
            .expect("admit")
        else {
            panic!("queued");
        };
        std::fs::remove_dir_all(h.dir.path().join("events")).expect("remove log");
        std::fs::write(h.dir.path().join("events"), "blocked").expect("block log");
        h.scheduler.tick().await;
        let failed = h.items().into_iter().find(|item| item.id == id).expect("item");
        assert_eq!(failed.state, WorkItemState::Failed);
        assert!(failed.execution_completed);
        assert!(!failed.occupies_slot());
        assert!(h.effects.lock().expect("effects").is_empty());
        let snapshot = foundry_sdk::pacing::snapshot(
            &h.items(),
            &foundry_sdk::pacing::Limits::default(),
            &foundry_sdk::pacing::PauseState::default(),
            &str::to_string,
        );
        assert!(
            snapshot.running.is_empty(),
            "expected `snapshot.running` to be empty: {:?}",
            snapshot.running
        );
        std::fs::remove_file(h.dir.path().join("events")).expect("restore log");
        let pending = h.submit(EventType::ProjectMaintenanceRequested, "alpha");
        h.admitted(1).await;
        h.scheduler.tick().await;
        tokio::time::timeout(Duration::from_secs(2), pending)
            .await
            .expect("next work can complete")
            .expect("join");
        assert_eq!(h.effects.lock().expect("effects").len(), 1);
    }

    #[tokio::test]
    async fn cancelled_release_and_iteration_publish_their_own_typed_failure() {
        for (request, terminal) in [
            (EventType::ReleaseRequested, EventType::ReleaseCompleted),
            (EventType::ProjectIterationRequested, EventType::ProjectIterationCompleted),
            (EventType::ProjectMaintenanceRequested, EventType::ProjectMaintenanceCompleted),
        ] {
            let h = Harness::new(true);
            let Admission::Deferred { id, .. } =
                h.policy.prepare(&root(request, "alpha"), None).expect("admit")
            else {
                panic!("queued");
            };
            {
                let _gate = ledger_write_gate().lock().expect("gate");
                let mut store = h.policy.store().expect("store");
                store.find_mut(&id).expect("item").settle_cancelled(
                    "operator stopped it",
                    None,
                    Utc::now(),
                );
                store.save(&h.policy.path).expect("save");
            }
            let mut watcher = h.policy.event_tx.subscribe();
            let result = h.engine.join_work(&id).await.expect("cancelled result");
            let event = watcher.try_recv().expect("watcher terminal");
            assert_eq!(event.event_type, terminal);
            assert_eq!(event.payload_bool("success"), Some(false));
            assert!(!result.is_success());
            assert!(h.effects.lock().expect("effects").is_empty());
        }
    }

    /// An initiative over alpha and beta whose done evidence is a gate, so
    /// its steers integrate.
    fn gated_initiative(h: &Harness) {
        let mut campaigns = foundry_sdk::campaign::CampaignStore::default();
        let initiative = serde_json::from_value(serde_json::json!({
            "name":"shared", "outcome":"The trunks agree", "projects":["alpha","beta"],
            "authorized_by":"Stacey", "members":[],
            "done_evidence":[{"kind":"gate","command":"true","required":true}]
        }))
        .unwrap();
        campaigns.add_initiative(initiative, &["alpha".into(), "beta".into()]).unwrap();
        campaigns.save(&h.policy.campaigns_path).unwrap();
    }

    /// A member cycle on `project`, queued with a root the scheduler can start.
    fn seed_waiting(h: &Harness, project: &str) -> String {
        let item = WorkItem::queued(
            WorkItemSpec {
                project: project.to_string(),
                objective: "campaign".to_string(),
                kind: WorkItemKind::CampaignCycle,
                lane: WorkLane::Campaign,
                origin: "test".to_string(),
                trace_id: Some("b".repeat(32)),
            },
            root(EventType::CampaignAdvanceRequested, project),
            Utc::now(),
        );
        let id = item.id.clone();
        let _gate = ledger_write_gate().lock().expect("gate");
        let mut store = h.policy.store().expect("load");
        store.upsert(item);
        store.save(&h.policy.path).expect("save");
        id
    }

    /// Admit a gated steer and return its queued item.
    fn admit_steer(h: &Harness) -> WorkItem {
        let Admission::Deferred { id, .. } =
            h.policy.prepare(&steer_root("member-settled"), None).unwrap()
        else {
            panic!("expected a queued steer");
        };
        h.items().into_iter().find(|item| item.id == id).unwrap()
    }

    #[test]
    fn a_gated_steer_is_admitted_reserving_every_trunk_if_free_whatever_the_ledger_holds() {
        let admitted = |seed: &dyn Fn(&Harness)| {
            let h = Harness::new(true);
            gated_initiative(&h);
            seed(&h);
            admit_steer(&h)
        };
        for (steer, ledger) in [
            (admitted(&|_| {}), "a quiet ledger"),
            (
                admitted(&|h| {
                    h.seed_running("alpha");
                }),
                "a running member",
            ),
            (
                admitted(&|h| {
                    seed_waiting(h, "beta");
                }),
                "a queued member",
            ),
        ] {
            assert_eq!(steer.reserved_projects, ["alpha", "beta"], "{ledger}");
            assert!(steer.reservation_if_free, "pacing settles the reservation: {ledger}");
        }
    }

    #[tokio::test]
    async fn a_steer_that_starts_on_a_quiet_ledger_holds_every_trunk() {
        let h = Harness::new(true);
        gated_initiative(&h);
        let steer = admit_steer(&h);
        h.scheduler.tick().await;
        let started = h.items().into_iter().find(|item| item.id == steer.id).unwrap();
        assert!(started.started_at.is_some());
        // The integration guard reads exactly this record.
        assert_eq!(started.reserved_projects, ["alpha", "beta"]);
        assert!(!started.reservation_if_free, "the reservation is held, no longer optional");
    }

    /// A member cycle that keeps its trunk until the test ends.
    struct HoldingMember;
    impl TaskBlock for HoldingMember {
        fn name(&self) -> &'static str {
            "Synthetic member cycle that never finishes"
        }
        fn kind(&self) -> BlockKind {
            BlockKind::Observer
        }
        fn sinks_on(&self) -> &[EventType] {
            &[EventType::CampaignAdvanceRequested]
        }
        fn execute(&self, _event: &Event) -> BlockFuture<'_> {
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn a_steer_kept_waiting_on_its_own_key_starts_beside_a_member_that_started_meanwhile() {
        let h = Harness::with_blocks(true, vec![Box::new(HoldingMember)]);
        std::fs::write(h.dir.path().join("pacing.json"), r#"{"max_running":4}"#).unwrap();
        gated_initiative(&h);
        // Other system work holds the key every steer starts on.
        let scan = h.seed_running("system");
        let steer = admit_steer(&h).id;
        let member = seed_waiting(&h, "alpha");
        h.scheduler.tick().await;
        let item = |id: &str| h.items().into_iter().find(|item| item.id == id).unwrap();
        assert_eq!(item(&steer).reason, foundry_sdk::pacing::repository_busy(&scan));
        assert!(item(&member).occupies_slot(), "the member takes its trunk meanwhile");

        h.settle(&scan);
        h.scheduler.tick().await;
        let started = item(&steer);
        assert!(started.started_at.is_some(), "the steer starts beside the member: {started:?}");
        assert!(started.reserved_projects.is_empty(), "it holds no initiative trunk");
        assert!(!started.reservation_if_free);
        assert!(item(&member).occupies_slot(), "the member still runs");
    }

    /// Steers like the real block: the first pass advances beta and requests
    /// one follow-up, the follow-up does nothing. The Runner settles both.
    struct AdvancingSteer;
    impl TaskBlock for AdvancingSteer {
        fn name(&self) -> &'static str {
            "Synthetic steer that advances beta"
        }
        fn kind(&self) -> BlockKind {
            BlockKind::Mutator
        }
        fn sinks_on(&self) -> &[EventType] {
            &[EventType::InitiativeAdvanceRequested]
        }
        fn execute(&self, event: &Event) -> BlockFuture<'_> {
            let children = if event.payload.get("follow_up_of").is_some() {
                vec![]
            } else {
                let mut follow_up = Event::new(
                    EventType::InitiativeAdvanceRequested,
                    "system".into(),
                    event.throttle,
                    serde_json::json!({
                        "initiative":"shared", "trigger_id":event.id,
                        "follow_up_of":{"steer_id":event.id, "step":"stage"}
                    }),
                )
                .with_source(Some(WorkSource::initiative("shared", 0)));
                follow_up.id = format!("evt_{}_follow_up", event.id);
                let beta = Event::new(
                    EventType::CampaignAdvanceRequested,
                    "beta".into(),
                    event.throttle,
                    serde_json::json!({"campaign":"beta-member"}),
                );
                vec![follow_up, beta]
            };
            Box::pin(async move { Ok(TaskBlockResult::success("steered", children)) })
        }
    }

    #[tokio::test]
    async fn a_steer_its_follow_up_and_the_next_member_all_start_while_a_member_cycle_runs() {
        let h = Harness::with_blocks(true, vec![Box::new(AdvancingSteer)]);
        std::fs::write(h.dir.path().join("pacing.json"), r#"{"max_running":4}"#).unwrap();
        gated_initiative(&h);
        let alpha = h.seed_running("alpha");
        let Admission::Deferred { id: steer, .. } =
            h.policy.prepare(&steer_root("member-settled"), None).unwrap()
        else {
            panic!("expected a queued steer");
        };
        let started = |kind: EventType, matches: &dyn Fn(&Event) -> bool| {
            h.effects
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.event_type == kind && matches(event))
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                h.scheduler.tick().await;
                let steered = h.effects.lock().unwrap().iter().find_map(|event| {
                    (event.event_type == EventType::InitiativeAdvanceRequested
                        && event.payload.get("follow_up_of").is_none())
                    .then(|| event.id.clone())
                });
                if let Some(steered) = steered
                    // The scheduler starts a fresh root caused by the admitted one.
                    && started(EventType::InitiativeAdvanceRequested, &|event| {
                        event.causation_id.as_deref() == Some(&format!("evt_{steered}_follow_up"))
                    })
                    && started(EventType::CampaignAdvanceRequested, &|event| {
                        event.project == "beta"
                    })
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the steer, its follow-up and beta's cycle start while alpha's cycle runs");
        let items = h.items();
        let item = |id: &str| items.iter().find(|item| item.id == id).unwrap();
        assert!(item(&alpha).is_running(), "alpha's cycle never settled");
        let reserved = &item(&steer).reserved_projects;
        assert!(reserved.is_empty(), "{reserved:?}");
        let follow_up = items
            .iter()
            .find(|item| {
                item.started_root.as_ref().is_some_and(|root| {
                    root.event_type == EventType::InitiativeAdvanceRequested
                        && root.payload.get("follow_up_of").is_some()
                })
            })
            .expect("the follow-up steer has its own started item");
        assert_eq!(follow_up.project, "system");
        assert!(follow_up.reserved_projects.is_empty(), "{:?}", follow_up.reserved_projects);
        assert!(items.iter().any(|item| item.project == "beta" && item.started_at.is_some()));
    }

    #[test]
    fn audit_only_scans_use_capacity_without_repository_reservations() {
        let h = Harness::new(true);
        let mut policy = DurableWorkAdmission::new(
            h.policy.path.clone(),
            h.policy.registry.clone(),
            h.policy.pacing.clone(),
            h.policy.event_tx.clone(),
            h.policy.writer.clone(),
            h.policy.result_writer.clone(),
        );
        policy.supply_chain_remediation = false;
        let item = policy
            .planned(&root(EventType::SupplyChainScanStarted, "system"))
            .expect("plan")
            .expect("item");
        assert_eq!(item.kind, WorkItemKind::Workflow);
        assert!(
            item.reserved_projects.is_empty(),
            "expected `item.reserved_projects` to be empty: {:?}",
            item.reserved_projects
        );
    }
    #[tokio::test]
    async fn unregistered_digest_roots_are_refused_before_observers_even_when_host_is_full() {
        let calls = Arc::new(Mutex::new(vec![]));
        let h = Harness::with_blocks(true, vec![Box::new(ObserverRunner(calls.clone()))]);
        std::fs::write(&h.policy.pacing.limits, r#"{"max_running":1}"#).expect("limits");
        h.seed_running("alpha");
        let mut request = root(EventType::OpsObserved, "not-registered");
        request.payload = serde_json::json!({"proceed": true, "new_event_count": 1, "events": []});
        let result = h.engine.process(request).await;
        assert!(!result.is_success());
        assert!(calls.lock().expect("observer calls").is_empty());
        assert!(h.effects.lock().expect("effects").is_empty());
        assert_eq!(h.items().len(), 1, "unknown work was refused, not admitted");
        assert!(
            result
                .events
                .iter()
                .any(|event| event.event_type == EventType::Custom("workflow_refused".into()))
        );
        let log = foundry_blocks::blocks::triage_core::read_events_jsonl(
            &h.dir.path().join("events"),
            Utc::now() - chrono::Duration::days(1),
        );
        assert!(
            log.iter()
                .any(|event| event.event_type == EventType::Custom("workflow_refused".into()))
        );
    }

    fn block(name: &str, success: bool, emitted: &[&Event]) -> BlockExecution {
        let mut block = BlockExecution::new(name, "evt_trigger", 1, serde_json::json!({}));
        block.success = success;
        block.emitted_event_ids = emitted.iter().map(|event| event.id.clone()).collect();
        block
    }

    fn nightly_run(success: bool) -> (Event, Event) {
        let request = root(EventType::ProjectRunStarted, "alpha").with_gather_id(Some("g".into()));
        let completed = Event::new(
            EventType::ProjectRunCompleted,
            "alpha".into(),
            request.throttle,
            serde_json::json!({"success": success}),
        );
        (request, completed)
    }

    #[test]
    fn a_nightly_run_that_recovered_on_a_retry_succeeded() {
        let (request, completed) = nightly_run(true);
        let result = ProcessResult {
            events: vec![completed.clone()],
            block_executions: vec![
                block("Run Verify Gates", false, &[]),
                block("Route Gate Result", false, &[]),
                block("Run Verify Gates", true, &[]),
                block("Complete Project Run", true, &[&completed]),
            ],
            total_duration_ms: 0,
        };
        assert!(
            workflow_succeeded(&request, &result),
            "a retried gate failure is not the run's verdict"
        );
    }

    #[test]
    fn a_failure_after_the_terminal_still_fails_the_run() {
        let (request, completed) = nightly_run(true);
        let result = ProcessResult {
            events: vec![completed.clone()],
            block_executions: vec![
                block("Complete Project Run", true, &[&completed]),
                block("Local Install", false, &[]),
            ],
            total_duration_ms: 0,
        };
        assert!(
            !workflow_succeeded(&request, &result),
            "an install failing after completion fails the run"
        );
    }

    #[test]
    fn a_nightly_run_is_judged_by_its_terminal() {
        let (request, failed) = nightly_run(false);
        let result = ProcessResult {
            events: vec![failed.clone()],
            block_executions: vec![block("Complete Project Run", true, &[&failed])],
            total_duration_ms: 0,
        };
        assert!(
            !workflow_succeeded(&request, &result),
            "a terminal reporting failure fails the run"
        );
        let missing = ProcessResult {
            events: vec![],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        assert!(
            !workflow_succeeded(&request, &missing),
            "no terminal means no verdict to land on"
        );
    }

    #[test]
    fn task_completion_requires_a_terminal_and_generic_completion_cannot_invent_a_landing() {
        let h = Harness::new(true);
        let mut request = root(EventType::ExecutionRequested, "alpha");
        request.payload =
            serde_json::json!({"project": "alpha", "workflow": "task", "prompt": "Do work"});
        let Admission::Deferred { id, .. } = h.policy.prepare(&request, None).expect("admission")
        else {
            panic!("task must queue");
        };
        let mut result = ProcessResult {
            events: vec![],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        assert!(!workflow_succeeded(&request, &result), "a missing task terminal cannot land");
        result.events.push(Event::new(
            EventType::TaskRunCompleted,
            "alpha".into(),
            request.throttle,
            serde_json::json!({"success": false}),
        ));
        assert!(!workflow_succeeded(&request, &result), "an explicit task failure cannot land");
        result.events[0].payload["success"] = serde_json::json!(true);
        assert!(workflow_succeeded(&request, &result));
        let mut store = WorkItemStore::load(&h.policy.path).expect("store");
        store.find_mut(&id).expect("task").start(Utc::now());
        store.save(&h.policy.path).expect("save");
        request.payload[ADMITTED_WORK_ITEM_KEY] = serde_json::json!(id);
        h.policy.complete(&request, &result).expect("completion");
        let item = h.items().remove(0);
        assert_eq!(item.state, WorkItemState::Failed);
        assert!(item.execution_completed);
        assert!(item.reason.contains("without typed settlement"));

        // Dry-run typed settlement is itself a simulated mutator. It leaves
        // the real queue item for the admission boundary to close honestly.
        let mut request = root(EventType::ExecutionRequested, "beta");
        request.throttle = Throttle::DryRun;
        request.payload =
            serde_json::json!({"project": "beta", "workflow": "task", "prompt": "Simulate work"});
        let Admission::Deferred { id, .. } =
            h.policy.prepare(&request, None).expect("dry-run admission")
        else {
            panic!("dry-run task must queue");
        };
        let mut store = WorkItemStore::load(&h.policy.path).expect("store");
        store.find_mut(&id).expect("dry-run task").start(Utc::now());
        store.save(&h.policy.path).expect("save");
        request.payload[ADMITTED_WORK_ITEM_KEY] = serde_json::json!(id);
        h.policy.complete(&request, &result).expect("dry-run completion");
        let item = h.items().remove(1);
        assert_eq!(item.state, WorkItemState::Landed);
        assert_eq!(item.reason, "dry-run completed; mutations simulated");
    }
    fn steer_root(trigger: &str) -> Event {
        let mut event = root(EventType::InitiativeAdvanceRequested, "system");
        event.payload = serde_json::json!({"initiative":"shared","trigger_id":trigger});
        event.source = Some(WorkSource::initiative("shared", 0));
        event
    }

    fn start_admitted(h: &Harness, event: &Event) -> (String, Event) {
        let Admission::Deferred { id, .. } = h.policy.prepare(event, None).expect("admit") else {
            panic!("expected queued")
        };
        let mut started = event.clone();
        started.payload[ADMITTED_WORK_ITEM_KEY] = serde_json::json!(id);
        let _gate = ledger_write_gate().lock().expect("gate");
        let mut store = h.policy.store().expect("store");
        let item = store.find_mut(&id).expect("owner");
        item.start(Utc::now());
        item.started_root = Some(started.clone());
        store.save(&h.policy.path).expect("persist started root");
        (id, started)
    }

    #[test]
    fn steering_dedup_retains_one_distinct_fact_and_queues_it_after_completion() {
        let h = Harness::new(true);
        let initial = steer_root("first-fact");
        let (id, started) = start_admitted(&h, &initial);
        let duplicate = steer_root("first-fact");
        h.policy.prepare(&duplicate, None).expect("dedup same fact");
        assert!(h.items()[0].coalesced_root.is_none());
        let changed = steer_root("new-fact");
        h.policy.prepare(&changed, None).expect("coalesce changed fact");
        h.policy.prepare(&changed, None).expect("retry changed fact");
        let before = h.items();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].id, id);
        assert_eq!(before[0].coalesced_root.as_ref().unwrap().id, changed.id);
        let result = ProcessResult {
            events: vec![Event::new(
                EventType::InitiativeSteered,
                "system".into(),
                Throttle::Full,
                serde_json::json!({"success":true}),
            )],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        h.policy.complete(&started, &result).expect("complete and queue changed packet");
        let after = h.items();
        assert_eq!(after.len(), 2);
        assert_eq!(after.iter().filter(|item| item.is_queued()).count(), 1);
        assert!(after.iter().all(|item| item.coalesced_root.is_none()));
        h.policy.complete(&started, &result).expect("completion retry");
        assert_eq!(h.items().len(), 2);
    }

    fn steered(outcome: foundry_sdk::payload::SteeringOutcome, detail: &str) -> ProcessResult {
        use foundry_sdk::payload::SteeringOutcome;
        let quiet = matches!(
            outcome,
            SteeringOutcome::Waited | SteeringOutcome::NoOp | SteeringOutcome::Skipped
        );
        let fact = InitiativeSteeredPayload {
            success: outcome != SteeringOutcome::Failed,
            outcome,
            error: (!quiet).then(|| detail.to_owned()),
            initiative: "shared".into(),
            steer_id: "steer".into(),
            prompt: String::new(),
            decision: None,
            provider: None,
            action: None,
            reason: quiet.then(|| detail.to_owned()),
            eligible_members: vec![],
            follow_up_of: None,
        };
        ProcessResult {
            events: vec![Event::new(
                EventType::InitiativeSteered,
                "system".into(),
                Throttle::Full,
                serde_json::to_value(&fact).unwrap(),
            )],
            block_executions: vec![],
            total_duration_ms: 0,
        }
    }

    #[tokio::test]
    async fn a_rejected_steer_settles_cancelled_with_the_guard_reason_and_owes_nothing() {
        use foundry_sdk::payload::SteeringOutcome;
        let h = Harness::new(true);
        let (id, started) = start_admitted(&h, &steer_root("held-member"));
        let result =
            steered(SteeringOutcome::Rejected, "member 'two' is held: Owner reviews the API");

        h.policy.complete(&started, &result).expect("completion");

        let item = h.items().into_iter().find(|item| item.id == id).unwrap();
        assert_eq!(item.state, WorkItemState::Cancelled);
        assert_eq!(item.reason, "steering rejected: member 'two' is held: Owner reviews the API");
        assert!(!item.state.is_open(), "a rejection is not an obligation for a person");
        let waited = h.policy.wait(&id).await.expect("the traversal's own result");
        assert_eq!(waited.events, result.events, "no synthetic stop event replaces the fact");
    }

    #[test]
    fn a_waited_skipped_or_no_op_steer_lands_with_its_reason() {
        use foundry_sdk::payload::SteeringOutcome;
        for (outcome, expected) in [
            (SteeringOutcome::Waited, "steering waited: both members are running"),
            (SteeringOutcome::Skipped, "steering skipped: both members are running"),
            (
                SteeringOutcome::NoOp,
                "steering decision already in force: both members are running",
            ),
        ] {
            let h = Harness::new(true);
            let (id, started) = start_admitted(&h, &steer_root("settled"));
            h.policy
                .complete(&started, &steered(outcome, "both members are running"))
                .expect("completion");
            let item = h.items().into_iter().find(|item| item.id == id).unwrap();
            assert_eq!(item.state, WorkItemState::Landed, "{outcome:?}");
            assert_eq!(item.reason, expected);
        }
    }

    #[test]
    fn a_failed_steer_still_fails() {
        use foundry_sdk::payload::SteeringOutcome;
        let h = Harness::new(true);
        let (id, started) = start_admitted(&h, &steer_root("crash"));
        h.policy
            .complete(&started, &steered(SteeringOutcome::Failed, "steering agent failed: crash"))
            .expect("completion");
        let item = h.items().into_iter().find(|item| item.id == id).unwrap();
        assert_eq!(item.state, WorkItemState::Failed);
        assert!(item.state.is_open());
    }

    #[test]
    fn a_follow_up_request_during_its_steer_queues_once_after_it_with_its_cause() {
        let h = Harness::new(true);
        let (id, started) = start_admitted(&h, &steer_root("settled-fact"));
        let mut follow_up = steer_root(&started.id);
        follow_up.payload["follow_up_of"] =
            serde_json::json!({"steer_id": started.id, "step": "relay"});
        h.policy.prepare(&follow_up, None).expect("coalesce the follow-up");
        h.policy.prepare(&follow_up, None).expect("a repeat is the same request");
        assert_eq!(h.items().len(), 1, "no second assessment is admitted while one runs");

        h.policy
            .complete(&started, &steered(foundry_sdk::payload::SteeringOutcome::Applied, ""))
            .expect("completion");

        let queued: Vec<_> = h.items().into_iter().filter(|item| item.id != id).collect();
        assert_eq!(queued.len(), 1);
        assert!(queued[0].is_queued());
        let root = queued[0].pending_root.as_ref().unwrap();
        assert_eq!(root.payload["follow_up_of"]["step"], "relay");
        assert_eq!(root.payload["follow_up_of"]["steer_id"], started.id.as_str());
    }

    #[test]
    fn coalesced_steering_fact_survives_restart_and_failed_completion_save() {
        let h = Harness::new(true);
        let (_, started) = start_admitted(&h, &steer_root("first"));
        let changed = steer_root("changed");
        h.policy.prepare(&changed, None).expect("persist dirty root");
        let before = std::fs::read(&h.policy.path).unwrap();
        std::fs::create_dir(h.policy.path.with_extension("json.tmp")).unwrap();
        let result = ProcessResult {
            events: vec![],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        assert!(h.policy.complete(&started, &result).is_err());
        assert_eq!(std::fs::read(&h.policy.path).unwrap(), before);
        std::fs::remove_dir(h.policy.path.with_extension("json.tmp")).unwrap();
        let mut store = h.policy.store().unwrap();
        crate::service::work_ledger::settle_running(&mut store);
        store.save(&h.policy.path).unwrap();
        assert_eq!(store.items.iter().filter(|item| item.is_queued()).count(), 1);
        assert!(
            store
                .items
                .iter()
                .any(|item| item.pending_root.as_ref().is_some_and(|root| root.id == changed.id))
        );
        crate::service::work_ledger::settle_running(&mut store);
        assert_eq!(store.items.len(), 2);
    }

    #[test]
    fn action_ack_save_failure_retries_the_same_durable_child_without_double_spend() {
        let h = Harness::new(true);
        let path = h.policy.campaigns_path.clone();
        let mut store = foundry_sdk::campaign::CampaignStore::default();
        let initiative = serde_json::from_value(serde_json::json!({"name":"shared","outcome":"A working contract","projects":["alpha"],
            "authorized_by":"Stacey","members":[],"done_evidence":[{"kind":"review","statement":"The contract works"}]})).unwrap();
        store.add_initiative(initiative, &["alpha".into()]).unwrap();
        let action = store
            .commit_owner_action(
                "shared",
                "owner_dispatch",
                foundry_sdk::initiative::SteeringDecision::Dispatch {
                    project: "alpha".into(),
                    objective: "Implement the contract".into(),
                    member: None,
                },
                "Owner requested the task".into(),
            )
            .unwrap();
        let action = store
            .bind_external_context("shared", &action.id, synthetic_external_context())
            .unwrap();
        store.save(&path).unwrap();
        let mut child = root(EventType::ExecutionRequested, "alpha");
        child.id = "evt_owner_dispatch_dispatch".into();
        child.payload = serde_json::json!({"project":"alpha","workflow":"task","prompt":"Implement the contract",
            "initiative":"shared","steering_action_id":action.id,"steering_external_context":action.external_context});
        child.source = Some(WorkSource::initiative("shared", 0));
        std::fs::create_dir(path.with_extension("json.tmp")).unwrap();
        assert!(h.policy.prepare(&child, None).is_err());
        assert_eq!(h.items().len(), 1);
        assert_eq!(
            foundry_sdk::campaign::CampaignStore::load(&path)
                .unwrap()
                .find_initiative("shared")
                .unwrap()
                .pending_actions
                .len(),
            1
        );
        std::fs::remove_dir(path.with_extension("json.tmp")).unwrap();
        h.policy.prepare(&child, None).expect("acknowledge same queued child");
        assert_eq!(h.items().len(), 1);
        let store = foundry_sdk::campaign::CampaignStore::load(&path).unwrap();
        let initiative = store.find_initiative("shared").unwrap();
        assert!(
            initiative.pending_actions.is_empty(),
            "expected `initiative.pending_actions` to be empty: {:?}",
            initiative.pending_actions
        );
        assert_eq!(initiative.task_dispatches.len(), 1);
        assert_eq!(initiative.work_admissions.len(), 1);
        assert_eq!(store.shared_cycles_used("shared").unwrap(), 1);
    }

    fn assessment_work_child(h: &Harness, campaign: bool) -> (String, Event) {
        use foundry_sdk::initiative::SteeringDecision;
        let path = &h.policy.campaigns_path;
        let mut store = foundry_sdk::campaign::CampaignStore::default();
        store.add(serde_json::from_value(serde_json::json!({
            "name":"member","project":"alpha","mission":"Align the contract",
            "authorized_by":"Stacey","done_evidence":[{"kind":"review","statement":"Aligned"}]
        })).unwrap()).unwrap();
        store
            .add_initiative(
                serde_json::from_value(serde_json::json!({
                    "name":"shared","outcome":"The trunks agree","projects":["alpha","beta"],
                    "authorized_by":"Stacey","members":[{"kind":"campaign","name":"member"}],
                    "done_evidence":[{"kind":"gate","command":"true","required":true}]
                }))
                .unwrap(),
                &["alpha".into(), "beta".into()],
            )
            .unwrap();
        store.save(path).unwrap();
        let (owner, assessment) = start_admitted(h, &steer_root("assessment"));
        let mut store = foundry_sdk::campaign::CampaignStore::load(path).unwrap();
        let decision = if campaign {
            SteeringDecision::Advance {
                member: "member".into(),
                project: None,
            }
        } else {
            SteeringDecision::Dispatch {
                project: "alpha".into(),
                objective: "Align contract".into(),
                member: None,
            }
        };
        let action = store
            .commit_owner_action("shared", &assessment.id, decision, "One assessed step".into())
            .unwrap();
        let action = store
            .bind_external_context("shared", &action.id, synthetic_external_context())
            .unwrap();
        store.save(path).unwrap();
        let mut child = root(
            if campaign {
                EventType::CampaignAdvanceRequested
            } else {
                EventType::ExecutionRequested
            },
            "alpha",
        );
        child.id = format!("evt_{}_child", action.id);
        child.payload = serde_json::json!({"project":"alpha","workflow":"task","prompt":"Align contract","campaign":if campaign {Some("member")} else {None},
            "initiative":"shared","steering_action_id":action.id,"steering_external_context":action.external_context});
        child.source = Some(WorkSource::initiative("shared", 0));
        (owner, child)
    }

    #[test]
    fn assessed_work_admits_beneath_its_own_reservation_once() {
        for campaign in [false, true] {
            let h = Harness::new(true);
            let (owner, child) = assessment_work_child(&h, campaign);
            assert_eq!(
                h.items().iter().find(|item| item.id == owner).unwrap().reserved_projects,
                ["alpha", "beta"]
            );
            let Admission::Deferred { id, .. } = h.policy.prepare(&child, Some(&owner)).unwrap()
            else {
                panic!("child must be queued")
            };
            let Admission::Deferred {
                id: retry,
                submitted,
            } = h.policy.prepare(&child, Some(&owner)).unwrap()
            else {
                panic!("retry must use its durable child")
            };
            assert_eq!(retry, id);
            assert!(submitted.is_none());
            assert_eq!(h.items().len(), 2);
            let store =
                foundry_sdk::campaign::CampaignStore::load(&h.policy.campaigns_path).unwrap();
            let initiative = store.find_initiative("shared").unwrap();
            assert!(
                initiative.pending_actions.is_empty(),
                "expected `initiative.pending_actions` to be empty: {:?}",
                initiative.pending_actions
            );
            assert_eq!(initiative.work_admissions.len(), 1);
            assert_eq!(initiative.task_dispatches.len(), usize::from(!campaign));
        }
    }

    #[test]
    fn assessed_work_still_rejects_unrelated_execution_and_cleanup_owners() {
        for cleanup in [false, true] {
            let h = Harness::new(true);
            let (owner, child) = assessment_work_child(&h, true);
            let other = h.seed_running("alpha");
            if cleanup {
                let _gate = ledger_write_gate().lock().unwrap();
                let mut ledger = h.policy.store().unwrap();
                let item = ledger.find_mut(&other).unwrap();
                item.execution_completed = false;
                item.started_root = Some(root(EventType::ProjectRunStarted, "alpha"));
                item.state = WorkItemState::Landed;
                assert!(item.occupies_slot(), "settled work retains its cleanup owner");
                ledger.save(&h.policy.path).unwrap();
            }
            let error = h.policy.prepare(&child, Some(&owner)).err().unwrap();
            assert!(error.to_string().contains("busy project"));
            assert_eq!(h.items().len(), 2);
            let store =
                foundry_sdk::campaign::CampaignStore::load(&h.policy.campaigns_path).unwrap();
            assert_eq!(store.find_initiative("shared").unwrap().pending_actions.len(), 1);
        }
    }

    #[test]
    fn assessment_reservation_identity_cannot_hide_another_initiative_or_root_kind() {
        let h = Harness::new(true);
        let (owner, child) = assessment_work_child(&h, true);
        let store = foundry_sdk::campaign::CampaignStore::load(&h.policy.campaigns_path).unwrap();
        let action = &store.find_initiative("shared").unwrap().pending_actions[0];
        let mut item = h.items().into_iter().find(|item| item.id == owner).unwrap();
        assert!(owns_assessment_reservation(&item, action));
        item.started_root.as_mut().unwrap().payload["initiative"] = serde_json::json!("other");
        assert!(!owns_assessment_reservation(&item, action));
        item.started_root.as_mut().unwrap().payload["initiative"] = serde_json::json!("shared");
        item.started_root.as_mut().unwrap().event_type = child.event_type;
        assert!(!owns_assessment_reservation(&item, action));
    }
    #[test]
    fn passive_owner_dispatch_releases_reservations_after_each_cleanup() {
        let h = Harness::new(true);
        let path = h.policy.campaigns_path.clone();
        let mut store = foundry_sdk::campaign::CampaignStore::default();
        let initiative = serde_json::from_value(serde_json::json!({"name":"shared","outcome":"A working contract","projects":["alpha"],
            "authorized_by":"Stacey","members":[],"budget":{"max_cycles":6,"max_active_members":2,"max_open_questions":3},
            "done_evidence":[{"kind":"review","statement":"The contract works"}]})).unwrap();
        store.add_initiative(initiative, &["alpha".into()]).unwrap();
        store.save(&path).unwrap();
        for number in 0..3 {
            let mut guard = foundry_sdk::campaign::CampaignStore::lock_exclusive(&path).unwrap();
            let action = guard
                .store
                .commit_owner_action(
                    "shared",
                    &format!("task_{number}"),
                    foundry_sdk::initiative::SteeringDecision::Dispatch {
                        project: "alpha".into(),
                        objective: format!("Task {number}"),
                        member: None,
                    },
                    "Owner requested the task".into(),
                )
                .unwrap();
            let action = guard
                .store
                .bind_external_context("shared", &action.id, synthetic_external_context())
                .unwrap();
            guard.save().unwrap();
            drop(guard);
            let mut child = root(EventType::ExecutionRequested, "alpha");
            child.id = format!("evt_{}_dispatch", action.id);
            child.payload = serde_json::json!({"project":"alpha","workflow":"task","prompt":"Implement the contract",
                "initiative":"shared","steering_action_id":action.id,"steering_external_context":action.external_context});
            child.source = Some(WorkSource::initiative("shared", 0));
            let (_, started) = start_admitted(&h, &child);
            h.policy
                .complete(
                    &started,
                    &ProcessResult {
                        events: vec![],
                        block_executions: vec![],
                        total_duration_ms: 0,
                    },
                )
                .unwrap();
            let store = foundry_sdk::campaign::CampaignStore::load(&path).unwrap();
            let initiative = store.find_initiative("shared").unwrap();
            assert!(!initiative.steering_enabled);
            assert!(
                initiative.reservations.is_empty(),
                "expected `initiative.reservations` to be empty: {:?}",
                initiative.reservations
            );
            assert_eq!(initiative.work_admissions.len(), number + 1);
        }
        assert_eq!(h.items().len(), 3, "passive cleanup does not schedule a steer");
    }
    fn synthetic_external_context() -> foundry_sdk::initiative::SteeringExternalContext {
        foundry_sdk::initiative::SteeringExternalContext {
            repositories: vec![],
            derive_source: None,
            intent: None,
            brief: None,
            brief_content: None,
        }
    }
    #[test]
    fn changed_git_evidence_refuses_and_discards_a_child_before_ledger_admission() {
        let h = Harness::new(true);
        let registry = h.policy.registry.read().unwrap().clone();
        let project = registry.find_project("alpha").unwrap();
        let repo = std::path::Path::new(&project.path);
        std::fs::create_dir(repo).unwrap();
        let git = |args: &[&str]| {
            let output =
                std::process::Command::new("git").current_dir(repo).args(args).output().unwrap();
            assert!(
                output.status.success(),
                "git failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        git(&["init", "-b", "main"]);
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "commit",
            "--allow-empty",
            "-m",
            "Fixture",
        ]);
        let mut context = synthetic_external_context();
        context.repositories.push(foundry_sdk::initiative::SteeringRepositoryIdentity {
            project: "alpha".into(),
            root: repo.canonicalize().unwrap().display().to_string(),
            registry: serde_json::to_string(project).unwrap(),
            trunk: git(&["rev-parse", "refs/heads/main"]),
            head: git(&["rev-parse", "HEAD"]),
            status: git(&["status", "--porcelain", "--untracked-files=all"]),
            diff: git(&["diff", "HEAD", "--"]),
        });
        let path = h.policy.campaigns_path.clone();
        let mut store = foundry_sdk::campaign::CampaignStore::default();
        store.add_initiative(serde_json::from_value(serde_json::json!({"name":"shared","outcome":"Contract works","projects":["alpha"],"authorized_by":"Stacey","members":[],
            "done_evidence":[{"kind":"review","statement":"It works"}]})).unwrap(),&["alpha".into()]).unwrap();
        let action = store
            .commit_owner_action(
                "shared",
                "changed_context",
                foundry_sdk::initiative::SteeringDecision::Dispatch {
                    project: "alpha".into(),
                    objective: "Write contract".into(),
                    member: None,
                },
                "Owner dispatch".into(),
            )
            .unwrap();
        let action = store.bind_external_context("shared", &action.id, context).unwrap();
        store.save(&path).unwrap();
        std::fs::write(repo.join("new-evidence.txt"), "The repository changed").unwrap();
        let mut child = root(EventType::ExecutionRequested, "alpha");
        child.id = "evt_changed_context_dispatch".into();
        child.payload = serde_json::json!({"project":"alpha","workflow":"task","prompt":"Write contract","initiative":"shared","steering_action_id":action.id,"steering_external_context":action.external_context});
        let error = h.policy.prepare(&child, None).err().unwrap();
        assert!(error.to_string().contains("context changed"));
        assert!(h.items().is_empty(), "expected `h.items()` to be empty: {:?}", h.items());
        h.policy.failure_events(&child, &error.to_string());
        let store = foundry_sdk::campaign::CampaignStore::load(&path).unwrap();
        let initiative = store.find_initiative("shared").unwrap();
        assert!(
            initiative.pending_actions.is_empty(),
            "expected `initiative.pending_actions` to be empty: {:?}",
            initiative.pending_actions
        );
        assert!(
            initiative.reservations.is_empty(),
            "expected `initiative.reservations` to be empty: {:?}",
            initiative.reservations
        );
        assert!(
            initiative.work_admissions.is_empty(),
            "expected `initiative.work_admissions` to be empty: {:?}",
            initiative.work_admissions
        );
    }
    #[test]
    fn formation_cleanup_waits_for_cycle_result_accounting_before_steering() {
        let h = Harness::new(true);
        let mut campaigns = foundry_sdk::campaign::CampaignStore::default();
        campaigns.add_initiative(serde_json::from_value(serde_json::json!({"name":"shared","outcome":"A working contract","projects":["alpha"],"authorized_by":"Stacey","members":[],
            "done_evidence":[{"kind":"review","statement":"The contract works"}]})).unwrap(),&["alpha".into()]).unwrap();
        campaigns.enable_steering("shared").unwrap();
        campaigns.save(&h.policy.campaigns_path).unwrap();
        let mut formation = root(EventType::CampaignAdvanceRequested, "alpha");
        formation.payload =
            Event::serialize_payload(&foundry_sdk::payload::CampaignAdvanceRequestedPayload {
                campaign: "member".into(),
                run_event_id: None,
                run_result: None,
                operator_origin: None,
                providers: foundry_sdk::gateway::StageProviders::default(),
            })
            .unwrap();
        formation.payload["initiative"] = serde_json::json!("shared");
        formation.source = Some(WorkSource::initiative("shared", 0));
        let (_, started) = start_admitted(&h, &formation);
        let result = ProcessResult {
            events: vec![root(EventType::CampaignAdvanceCompleted, "alpha")],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        h.policy.complete(&started, &result).unwrap();
        assert!(
            h.items().iter().all(|item| !item.is_queued()),
            "initial formation must wait for its dispatched cycle"
        );
        let mut accounting = root(EventType::CampaignAdvanceRequested, "alpha");
        accounting.payload = Event::serialize_payload(&foundry_sdk::payload::CampaignAdvanceRequestedPayload {
            campaign:"member".into(),run_event_id:Some("completed_cycle".into()),
            run_result:Some(serde_json::from_value(serde_json::json!({"project":"alpha","success":true,"landed":true,"summary":"Contract landed","verdict":"complete"})).unwrap()),
            operator_origin:None,providers:foundry_sdk::gateway::StageProviders::default(),
        }).unwrap();
        accounting.payload["initiative"] = serde_json::json!("shared");
        accounting.source = Some(WorkSource::initiative("shared", 1));
        let mut failed_accounting = accounting.clone();
        failed_accounting.id = "failed_result_accounting".into();
        let (_, failed_started) = start_admitted(&h, &failed_accounting);
        let mut failed_terminal = root(EventType::CampaignAdvanceCompleted, "alpha");
        failed_terminal.payload["success"] = serde_json::json!(false);
        h.policy
            .complete(
                &failed_started,
                &ProcessResult {
                    events: vec![failed_terminal],
                    block_executions: vec![],
                    total_duration_ms: 0,
                },
            )
            .unwrap();
        assert!(
            h.items().iter().all(|item| !item.is_queued()),
            "failed result accounting must not schedule work from unrecorded counters"
        );
        let (_, started) = start_admitted(&h, &accounting);
        h.policy.complete(&started, &result).unwrap();
        h.policy.complete(&started, &result).unwrap();
        let items = h.items();
        let queued = items.iter().filter(|item| item.is_queued()).collect::<Vec<_>>();
        assert_eq!(queued.len(), 1, "one result-accounting cleanup schedules one steer");
        let pending = queued[0].pending_root.as_ref().unwrap();
        assert_eq!(pending.event_type, EventType::InitiativeAdvanceRequested);
        assert_eq!(pending.payload_str("trigger_id"), Some(accounting.id.as_str()));
    }

    /// Two port members of a steered initiative, `alpha-port` on alpha and
    /// `beta-port` on beta. `setup` changes the store before it is saved.
    fn port_initiative(h: &Harness, setup: impl FnOnce(&mut foundry_sdk::campaign::CampaignStore)) {
        let mut campaigns = foundry_sdk::campaign::CampaignStore::default();
        for (name, project) in [("alpha-port", "alpha"), ("beta-port", "beta")] {
            let campaign = serde_json::from_value(serde_json::json!({
                "name": name, "project": project, "mission": "Port the library",
                "authorized_by": "Stacey",
                "done_evidence": [{"kind": "review", "statement": "The port works"}]
            }))
            .unwrap();
            campaigns.add(campaign).unwrap();
        }
        let initiative = serde_json::from_value(serde_json::json!({
            "name": "shared", "outcome": "Every port works", "projects": ["alpha", "beta"],
            "authorized_by": "Stacey",
            "members": [
                {"kind": "campaign", "name": "alpha-port"},
                {"kind": "campaign", "name": "beta-port"}
            ],
            "done_evidence": [{"kind": "review", "statement": "Every port works"}]
        }))
        .unwrap();
        campaigns.add_initiative(initiative, &["alpha".into(), "beta".into()]).unwrap();
        campaigns.enable_steering("shared").unwrap();
        setup(&mut campaigns);
        campaigns.save(&h.policy.campaigns_path).unwrap();
    }

    /// Escalate `member` as its advance does: the owner question applies to
    /// it alone, and the initiative becomes `escalated`.
    fn escalate(store: &mut foundry_sdk::campaign::CampaignStore, member: &str) {
        store.find_mut(member).unwrap().status = foundry_sdk::campaign::CampaignStatus::Escalated;
        store.refresh_questions("shared").unwrap();
    }

    /// An advance a steer issued for `member`: it carries no cycle result.
    fn steered_advance(member: &str, project: &str) -> Event {
        let mut advance = root(EventType::CampaignAdvanceRequested, project);
        advance.payload =
            Event::serialize_payload(&foundry_sdk::payload::CampaignAdvanceRequestedPayload {
                campaign: member.into(),
                run_event_id: None,
                run_result: None,
                operator_origin: None,
                providers: foundry_sdk::gateway::StageProviders::default(),
            })
            .unwrap();
        advance.payload["initiative"] = serde_json::json!("shared");
        advance.source = Some(WorkSource::initiative("shared", 1));
        advance
    }

    /// A settled advance that started no cycle, ending with `terminal` if any.
    fn advance_settled(project: &str, success: bool, terminal: Option<EventType>) -> ProcessResult {
        let mut completed = root(EventType::CampaignAdvanceCompleted, project);
        completed.payload = serde_json::json!({"success": success});
        ProcessResult {
            events: std::iter::once(completed)
                .chain(terminal.map(|kind| root(kind, project)))
                .collect(),
            block_executions: vec![],
            total_duration_ms: 0,
        }
    }

    fn queued_steers(h: &Harness) -> Vec<Event> {
        h.items()
            .into_iter()
            .filter(WorkItem::is_queued)
            .filter_map(|item| item.pending_root)
            .filter(|root| root.event_type == EventType::InitiativeAdvanceRequested)
            .collect()
    }

    /// Steers like the real block's eligibility: it advances the first member
    /// the real precheck lists, over the harness's own ledger and store.
    struct SteerFirstEligible(Arc<std::sync::OnceLock<Arc<DurableWorkAdmission>>>);
    impl TaskBlock for SteerFirstEligible {
        fn name(&self) -> &'static str {
            "Synthetic steer that advances the first eligible member"
        }
        fn kind(&self) -> BlockKind {
            BlockKind::Mutator
        }
        fn sinks_on(&self) -> &[EventType] {
            &[EventType::InitiativeAdvanceRequested]
        }
        fn execute(&self, event: &Event) -> BlockFuture<'_> {
            let policy = self.0.get().expect("harness policy");
            let store =
                foundry_sdk::campaign::CampaignStore::load(&policy.campaigns_path).expect("store");
            let registry = policy.registry.read().expect("registry").clone();
            let eligible = foundry_blocks::blocks::steering_eligible_members(
                &store,
                "shared",
                &policy.store().expect("ledger"),
                &registry,
            )
            .expect("precheck");
            let children = eligible
                .first()
                .map(|member| {
                    Event::new(
                        EventType::CampaignAdvanceRequested,
                        store.find(member).expect("member campaign").project.clone(),
                        event.throttle,
                        serde_json::json!({"campaign": member, "initiative": "shared"}),
                    )
                })
                .into_iter()
                .collect();
            Box::pin(async move { Ok(TaskBlockResult::success("steered", children)) })
        }
    }

    #[tokio::test]
    async fn an_escalated_steered_advance_wakes_steering_to_advance_another_member() {
        let policy = Arc::new(std::sync::OnceLock::new());
        let h = Harness::with_blocks(true, vec![Box::new(SteerFirstEligible(policy.clone()))]);
        policy.set(h.policy.clone()).ok().expect("one policy");
        std::fs::write(h.dir.path().join("pacing.json"), r#"{"max_running":4}"#).unwrap();
        port_initiative(&h, |store| escalate(store, "alpha-port"));
        let campaigns = foundry_sdk::campaign::CampaignStore::load(&h.policy.campaigns_path)
            .unwrap()
            .initiative_view("shared")
            .unwrap();
        assert_eq!(campaigns.status, foundry_sdk::initiative::InitiativeStatus::Escalated);
        assert_eq!(campaigns.open_questions.len(), 1);
        assert_eq!(campaigns.open_questions[0].members, ["alpha-port"]);
        let advance = steered_advance("alpha-port", "alpha");
        let (_, started) = start_admitted(&h, &advance);

        h.policy
            .complete(&started, &advance_settled("alpha", true, Some(EventType::CampaignEscalated)))
            .unwrap();

        let steers = queued_steers(&h);
        assert_eq!(steers.len(), 1, "the settled advance requests one steer");
        assert_eq!(steers[0].payload_str("trigger_id"), Some(advance.id.as_str()));
        let started = |kind: EventType, matches: &dyn Fn(&Event) -> bool| {
            h.effects
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.event_type == kind && matches(event))
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                h.scheduler.tick().await;
                if started(EventType::InitiativeAdvanceRequested, &|event| {
                    event.causation_id.as_deref() == Some(steers[0].id.as_str())
                }) && started(EventType::CampaignAdvanceRequested, &|event| {
                    event.project == "beta" && event.payload_str("campaign") == Some("beta-port")
                }) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the steer starts and advances beta-port, the member without a question");
    }

    #[test]
    fn a_settled_advance_frees_its_own_reservation_before_eligibility_is_judged() {
        let h = Harness::new(true);
        port_initiative(&h, |store| {
            store.find_initiative_mut("shared").unwrap().budget.max_active_members = 1;
            escalate(store, "alpha-port");
        });
        let advance = steered_advance("alpha-port", "alpha");
        let (id, started) = start_admitted(&h, &advance);
        // Admission moved the steer's slot reservation onto the advance's item.
        let mut campaigns =
            foundry_sdk::campaign::CampaignStore::load(&h.policy.campaigns_path).unwrap();
        campaigns.find_initiative_mut("shared").unwrap().reservations.push(
            foundry_sdk::initiative::MemberReservation {
                member: "alpha-port".into(),
                operation_id: id,
                kind: foundry_sdk::initiative::ReservationKind::Formation,
            },
        );
        campaigns.save(&h.policy.campaigns_path).unwrap();

        h.policy
            .complete(&started, &advance_settled("alpha", true, Some(EventType::CampaignEscalated)))
            .unwrap();

        assert_eq!(queued_steers(&h).len(), 1, "the one slot is free for beta-port");
        let campaigns =
            foundry_sdk::campaign::CampaignStore::load(&h.policy.campaigns_path).unwrap();
        let reservations = &campaigns.find_initiative("shared").unwrap().reservations;
        assert!(reservations.is_empty(), "{reservations:?}");
    }

    /// Dispatch a cycle for `member` as its formation does: the campaign
    /// records the objective, and the cycle waits queued in the ledger.
    fn dispatch_cycle(h: &Harness, member: &str, project: &str) {
        let mut campaigns =
            foundry_sdk::campaign::CampaignStore::load(&h.policy.campaigns_path).unwrap();
        let campaign = campaigns.find_mut(member).unwrap();
        campaign.status = foundry_sdk::campaign::CampaignStatus::Active;
        campaign.cycles_completed += 1;
        campaign.record_dispatched_objective("Port the next module".into());
        campaigns.save(&h.policy.campaigns_path).unwrap();
        let mut cycle = root(EventType::ExecutionRequested, project);
        cycle.payload = serde_json::json!({
            "project": project, "workflow": "task", "prompt": "Port the next module",
            "campaign": member, "initiative": "shared"
        });
        let id = seed_waiting(h, project);
        let _gate = ledger_write_gate().lock().expect("gate");
        let mut store = h.policy.store().expect("load");
        store.find_mut(&id).unwrap().pending_root = Some(cycle);
        store.save(&h.policy.path).expect("save");
    }

    /// A steer-issued advance already running. Its action receipt is not
    /// modelled, so it starts without final action admission.
    fn start_steer_issued_advance(h: &Harness, member: &str, project: &str) -> Event {
        let mut advance = steered_advance(member, project);
        advance.payload["steering_action_id"] = serde_json::json!("evt_earlier_steer");
        let mut item = WorkItem::dispatched(
            WorkItemSpec {
                project: project.into(),
                objective: "advance".into(),
                kind: WorkItemKind::Workflow,
                lane: WorkLane::Campaign,
                origin: EventType::CampaignAdvanceRequested.to_string(),
                trace_id: None,
            },
            Utc::now(),
        );
        advance.payload[ADMITTED_WORK_ITEM_KEY] = serde_json::json!(item.id);
        item.started_root = Some(advance.clone());
        item.source.clone_from(&advance.source);
        let _gate = ledger_write_gate().lock().expect("gate");
        let mut store = h.policy.store().expect("load");
        store.upsert(item);
        store.save(&h.policy.path).expect("save");
        advance
    }

    #[tokio::test]
    async fn a_steered_advance_that_dispatched_a_cycle_wakes_steering_to_fill_the_next_slot() {
        let policy = Arc::new(std::sync::OnceLock::new());
        let h = Harness::with_blocks(true, vec![Box::new(SteerFirstEligible(policy.clone()))]);
        policy.set(h.policy.clone()).ok().expect("one policy");
        std::fs::write(h.dir.path().join("pacing.json"), r#"{"max_running":4}"#).unwrap();
        port_initiative(&h, |_| {});
        let advance = start_steer_issued_advance(&h, "alpha-port", "alpha");
        dispatch_cycle(&h, "alpha-port", "alpha");

        h.policy.complete(&advance, &advance_settled("alpha", true, None)).unwrap();

        let steers = queued_steers(&h);
        assert_eq!(steers.len(), 1, "a free slot remains for beta-port");
        let campaigns =
            foundry_sdk::campaign::CampaignStore::load(&h.policy.campaigns_path).unwrap();
        let registry = h.policy.registry.read().unwrap().clone();
        assert_eq!(
            foundry_blocks::blocks::steering_eligible_members(
                &campaigns,
                "shared",
                &h.policy.store().unwrap(),
                &registry,
            )
            .unwrap(),
            ["beta-port"],
            "alpha-port's queued cycle holds its slot"
        );
        let advanced = |member: &str| {
            h.effects.lock().unwrap().iter().any(|event| {
                event.event_type == EventType::CampaignAdvanceRequested
                    && event.payload_str("campaign") == Some(member)
            })
        };
        tokio::time::timeout(Duration::from_secs(5), async {
            while !advanced("beta-port") {
                h.scheduler.tick().await;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the steer advances beta-port beside alpha-port's cycle");
        assert!(!advanced("alpha-port"), "the steer never re-advances alpha-port");
    }

    #[test]
    fn a_dispatching_advance_that_fills_the_last_slot_requests_no_steer() {
        let h = Harness::new(true);
        port_initiative(&h, |store| {
            store.find_initiative_mut("shared").unwrap().budget.max_active_members = 1;
        });
        let advance = start_steer_issued_advance(&h, "alpha-port", "alpha");
        dispatch_cycle(&h, "alpha-port", "alpha");

        h.policy.complete(&advance, &advance_settled("alpha", true, None)).unwrap();

        assert!(queued_steers(&h).is_empty(), "alpha-port's cycle holds the only slot");
    }

    #[test]
    fn an_escalated_advance_requests_no_steer_when_no_other_member_can_advance() {
        type Setup = fn(&mut foundry_sdk::campaign::CampaignStore);
        let cases: [(&str, Setup); 2] = [
            ("held", |store| {
                store.hold_member("shared", "beta-port", "Owner reviews the port").unwrap();
            }),
            ("completed", |store| {
                store.find_mut("beta-port").unwrap().status =
                    foundry_sdk::campaign::CampaignStatus::Completed;
            }),
        ];
        for (case, setup) in cases {
            let h = Harness::new(true);
            port_initiative(&h, |store| {
                setup(store);
                escalate(store, "alpha-port");
            });
            let (_, started) = start_admitted(&h, &steered_advance("alpha-port", "alpha"));

            h.policy
                .complete(
                    &started,
                    &advance_settled("alpha", true, Some(EventType::CampaignEscalated)),
                )
                .unwrap();

            assert!(queued_steers(&h).is_empty(), "beta-port {case}: nothing could advance");
        }
    }

    #[test]
    fn a_stood_down_advance_requests_a_steer_while_another_member_can_advance() {
        let h = Harness::new(true);
        port_initiative(&h, |store| {
            store.find_mut("alpha-port").unwrap().status =
                foundry_sdk::campaign::CampaignStatus::Paused;
        });
        let advance = steered_advance("alpha-port", "alpha");
        let (_, started) = start_admitted(&h, &advance);

        h.policy.complete(&started, &advance_settled("alpha", true, None)).unwrap();

        let steers = queued_steers(&h);
        assert_eq!(steers.len(), 1);
        assert_eq!(steers[0].payload_str("trigger_id"), Some(advance.id.as_str()));
    }

    #[test]
    fn a_failed_advance_or_one_that_left_its_member_eligible_requests_no_steer() {
        let h = Harness::new(true);
        port_initiative(&h, |store| escalate(store, "alpha-port"));
        let (_, failed) = start_admitted(&h, &steered_advance("alpha-port", "alpha"));
        h.policy.complete(&failed, &advance_settled("alpha", false, None)).unwrap();
        assert!(queued_steers(&h).is_empty(), "a broken member must not loop");

        // beta-port is still eligible: its advance changed nothing, and
        // steering again could choose the same advance without end.
        let (_, unchanged) = start_admitted(&h, &steered_advance("beta-port", "beta"));
        h.policy.complete(&unchanged, &advance_settled("beta", true, None)).unwrap();
        let queued = queued_steers(&h);
        assert!(queued.is_empty(), "{queued:?}");
    }
}
