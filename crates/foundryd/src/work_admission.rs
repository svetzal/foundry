//! One durable admission boundary for engineering workflow roots.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::Utc;
use foundry_engine::admission::{Admission, ResultFuture, WorkAdmission};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::pacing::PacingPaths;
use foundry_sdk::payload::{ExecutionRequestedPayload, WorkItemEventPayload};
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

    fn store(&self) -> anyhow::Result<WorkItemStore> {
        Ok(WorkItemStore::load(&self.path)?)
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
            let lane = if event.event_type == EventType::CampaignAdvanceRequested {
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
        Ok(Some(item))
    }
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
            let independent = event.event_type == EventType::CampaignAdvanceRequested
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
        let Some(mut item) = self.planned(event)? else {
            return Ok(Admission::Execute);
        };
        let _gate = ledger_write_gate()
            .lock()
            .map_err(|_| anyhow::anyhow!("ledger write gate poisoned"))?;
        let mut store = self.store()?;
        if let Some(existing) = store.items.iter().find(|item| {
            item.pending_root.as_ref().is_some_and(|root| root.id == event.id)
                || item
                    .started_root
                    .as_ref()
                    .is_some_and(|root| root.causation_id.as_deref() == Some(&event.id))
        }) {
            return Ok(Admission::Deferred {
                id: existing.id.clone(),
                submitted: None,
            });
        }
        store.upsert(item.clone());
        item.reason = queued_reason(&store, &item.id, &self.registry, &self.pacing);
        store.upsert(item.clone());
        let submitted = lifecycle(EventType::WorkItemSubmitted, &item)?;
        self.writer.write(&submitted)?;
        store.save(&self.path)?;
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
                if item.state == WorkItemState::Cancelled
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
            let events = if item.is_running() {
                if root.event_type == EventType::ExecutionRequested
                    && WorkflowType::from_payload(&root.payload) == WorkflowType::Task
                    && root.throttle.permits_mutation()
                {
                    item.settle_failed(
                        "task traversal ended without typed settlement; inspect its trace",
                        Utc::now(),
                    );
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
            store.save(&self.path)?;
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

fn workflow_succeeded(root: &Event, result: &ProcessResult) -> bool {
    let terminal = match root.event_type {
        EventType::ProjectRunStarted if root.gather_id.is_some() => {
            Some(EventType::ProjectRunCompleted)
        }
        EventType::ProjectMaintenanceRequested => Some(EventType::ProjectMaintenanceCompleted),
        EventType::ProjectIterationRequested => Some(EventType::ProjectIterationCompleted),
        EventType::ReleaseRequested => Some(EventType::ReleaseCompleted),
        EventType::CampaignAdvanceRequested => Some(EventType::CampaignAdvanceCompleted),
        EventType::ExecutionRequested
            if WorkflowType::from_payload(&root.payload) == WorkflowType::Task =>
        {
            Some(EventType::TaskRunCompleted)
        }
        _ => None,
    };
    terminal.is_none_or(|kind| result.events.iter().any(|event| event.event_type == kind))
        && result.is_success()
        && result.block_executions.iter().all(|block| block.success)
        && !result.events.iter().any(|event| {
            matches!(
                event.event_type,
                EventType::ProjectRunCompleted
                    | EventType::ProjectIterationCompleted
                    | EventType::ProjectMaintenanceCompleted
                    | EventType::ReleaseCompleted
                    | EventType::RemediationCompleted
                    | EventType::TaskRunCompleted
            ) && event.payload_bool("success") == Some(false)
        })
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
        assert!(h.items().is_empty());
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
            assert!(crate::service::work_ledger::settle_running(&mut store).is_empty());
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
        assert!(snapshot.running.is_empty());
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
        assert!(item.reserved_projects.is_empty());
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
}
