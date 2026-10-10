use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use tokio::sync::{Notify, broadcast};
use tonic::{Request, Response, Status};
use tracing::Instrument;

use foundry_sdk::event::Event;
use foundry_sdk::pacing::PacingPaths;
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;

use crate::proto::{
    AddCampaignRequest, AddCampaignResponse, AdvanceCampaignRequest, AdvanceCampaignResponse,
    CampaignReportResponse, CancelCampaignRequest, CancelCampaignResponse, CancelWorkItemRequest,
    CancelWorkItemResponse, CloseWorkItemRequest, CloseWorkItemResponse, CompleteCampaignRequest,
    CompleteCampaignResponse, DecideCampaignRequest, DecideCampaignResponse, EmitRequest,
    EmitResponse, GetCampaignRequest, GetCampaignResponse, GetPacingRequest, GetPacingResponse,
    GetWorkItemRequest, GetWorkItemResponse, HistoryRequest, HistoryResponse, HoldWorkItemRequest,
    HoldWorkItemResponse, ListCampaignsRequest, ListCampaignsResponse, ListWorkItemEventsRequest,
    ListWorkItemEventsResponse, ListWorkItemsRequest, ListWorkItemsResponse, PauseCampaignRequest,
    PauseCampaignResponse, PausePacingRequest, PausePacingResponse, RegistryAddRequest,
    RegistryAddResponse, RegistryEditRequest, RegistryEditResponse, RegistryListRequest,
    RegistryListResponse, RegistryRemoveRequest, RegistryRemoveResponse, RegistryShowRequest,
    RegistryShowResponse, ReleaseWorkItemRequest, ReleaseWorkItemResponse, ResumeCampaignRequest,
    ResumeCampaignResponse, ResumePacingRequest, ResumePacingResponse, SentinelDisableRequest,
    SentinelDisableResponse, SentinelEnableRequest, SentinelEnableResponse, SentinelListRequest,
    SentinelListResponse, SentinelShowRequest, SentinelShowResponse, SpanRequest, SpanResponse,
    StatusRequest, StatusResponse, TraceRequest, TraceResponse, WatchRequest, WatchResponse,
    foundry_server::Foundry,
};
use crate::trace_store::TraceStore;
use crate::workflow_tracker::{ActiveWorkflow, WorkflowTracker};
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;

mod campaign_ops;
pub(crate) mod eventing_ops;
mod pacing_ops;
mod recovery;
mod registry_ops;
mod sentinel_ops;
mod session_recovery;
mod tracing_ops;
mod work_item_ops;
mod work_ledger;

/// The Arc cluster shared between `spawn_workflow`, `spawn_scheduler`, and
/// `FoundryService`. Grouping them here eliminates recurring long positional
/// argument lists across all three call sites.
#[derive(Clone)]
pub struct RuntimeContext {
    pub engine: Arc<Engine>,
    pub trace_store: Arc<TraceStore>,
    pub workflow_tracker: Arc<WorkflowTracker>,
    pub trace_writer: Arc<TraceWriter>,
    pub event_tx: broadcast::Sender<Event>,
    pub registry: Arc<RwLock<Registry>>,
}

/// Store-level configuration for `FoundryService` that is not part of the
/// runtime event-processing cluster.
pub struct StoreConfig {
    pub campaigns_path: PathBuf,
    /// The durable work-item ledger the read RPCs load on every call.
    pub work_items_path: PathBuf,
    /// The durable event log (`YYYY-MM.jsonl` files) `ListWorkItemEvents`
    /// reads one item's `work_item_*` events from on every call.
    pub events_dir: PathBuf,
    pub registry_path: PathBuf,
    pub sentinels: Arc<RwLock<SentinelStore>>,
    pub sentinels_path: PathBuf,
    pub scheduler_reload: Arc<Notify>,
}

pub struct FoundryService {
    campaigns_path: PathBuf,
    work_items_path: PathBuf,
    events_dir: PathBuf,
    ctx: RuntimeContext,
    registry_path: PathBuf,
    sentinels: Arc<RwLock<SentinelStore>>,
    sentinels_path: PathBuf,
    scheduler_reload: Arc<Notify>,
    /// The pacing limits and lane-pause files the pacing RPCs and the owner
    /// controls read and write.
    pacing: PacingPaths,
    /// Poked after an admission whose ledger write follows its own
    /// announcement, so the scheduler ticks at once (see `crate::pacing`).
    pacing_wake: Arc<Notify>,
}

impl FoundryService {
    /// Campaign mutations can wait for a formation's cross-process flock.
    /// Keep their entire lock/read/save operation off Tokio's worker threads.
    async fn campaign_operation<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&std::path::Path, &RuntimeContext) -> Result<T, Status> + Send + 'static,
    ) -> Result<T, Status> {
        let path = self.campaigns_path.clone();
        let ctx = self.ctx.clone();
        campaign_ops::blocking(move || operation(&path, &ctx)).await
    }

    /// A service over `stores`, reading the pacing files the environment
    /// names (see [`PacingPaths::from_env`]) until [`Self::with_pacing`]
    /// says otherwise.
    pub fn new(ctx: RuntimeContext, stores: StoreConfig) -> Self {
        Self {
            campaigns_path: stores.campaigns_path,
            work_items_path: stores.work_items_path,
            events_dir: stores.events_dir,
            ctx,
            registry_path: stores.registry_path,
            sentinels: stores.sentinels,
            sentinels_path: stores.sentinels_path,
            scheduler_reload: stores.scheduler_reload,
            pacing: PacingPaths::from_env(),
            pacing_wake: Arc::new(Notify::new()),
        }
    }

    /// Read and write the pacing files at `pacing`, and poke `wake` (the
    /// handle given to [`spawn_pacing_scheduler`]) after an admission the
    /// scheduler would otherwise learn of only on its next interval.
    #[must_use]
    pub fn with_pacing(mut self, pacing: PacingPaths, wake: Arc<Notify>) -> Self {
        self.pacing = pacing;
        self.pacing_wake = wake;
        self
    }
}

/// Run the pacing scheduler over the ledger at `work_items_path` on the
/// tokio runtime: queued items start through `spawn_workflow` when the rules
/// in `foundry_sdk::pacing` allow, and `wake` ticks it early. Call once at
/// start, after the restart sweeps and before the daemon serves traffic.
pub fn spawn_pacing_scheduler(
    ctx: &RuntimeContext,
    work_items_path: PathBuf,
    pacing: PacingPaths,
    wake: Arc<Notify>,
) {
    tokio::spawn(
        crate::pacing::PacingScheduler::new(ctx.clone(), work_items_path, pacing, wake).run(),
    );
}

/// Record a root event as an active workflow, so `foundry status` shows it.
pub(crate) fn track_workflow(event: &Event, tracker: &WorkflowTracker) {
    // Read the campaign generically off the root payload rather than matching
    // on event type: every campaign root event names it under the same key, so
    // this stays correct as new campaign roots are added.
    let campaign = event
        .payload
        .get("campaign")
        .and_then(serde_json::Value::as_str)
        .map(ToString::to_string);
    tracker.insert(ActiveWorkflow {
        event_id: event.id.clone(),
        event_type: event.event_type.to_string(),
        project: event.project.clone(),
        trace_id: event.trace_id.clone().unwrap_or_default(),
        started_at: chrono::Utc::now(),
        campaign,
    });
}

/// Settle every work item the previous process was stopped during, before the
/// daemon dispatches anything new. Awaited rather than spawned for exactly
/// that reason.
pub async fn settle_running_work_items_on_start(ctx: &RuntimeContext, path: &std::path::Path) {
    work_ledger::settle_running_items_on_start(ctx, path).await;
}

/// End every agent session the previous process was stopped during, before
/// the daemon dispatches anything new (see `session_recovery`). Awaited rather
/// than spawned so no new session can start first.
pub async fn end_interrupted_agent_sessions_on_start(
    ctx: &RuntimeContext,
    events_dir: &std::path::Path,
) {
    session_recovery::end_interrupted_sessions(ctx, events_dir).await;
}

/// Close, in the background, any maintenance cycle foundryd was stopped in
/// the middle of (see `recovery`). Call once at start, before the scheduler
/// can fire a new cycle.
pub fn spawn_interrupted_cycle_recovery(ctx: &RuntimeContext, events_dir: std::path::PathBuf) {
    let ctx = ctx.clone();
    tokio::spawn(async move {
        recovery::recover_interrupted_cycles(&ctx, &events_dir).await;
    });
}

/// Track the event in the workflow registry and spawn `run_workflow` on the
/// tokio runtime. Used by both the gRPC `emit()` handler and the in-process
/// scheduler so every root event flows through the same trace/audit machinery.
pub(crate) fn spawn_workflow(event: Event, ctx: &RuntimeContext) {
    let event_id = event.id.clone();
    // Insert before spawning: the spawned task's `WorkflowGuard` removes this
    // entry on drop, and a fast workflow could otherwise finish and remove it
    // before it was ever recorded.
    track_workflow(&event, &ctx.workflow_tracker);

    let span = tracing::info_span!(
        "process",
        event_id = %event_id,
        event_type = %event.event_type,
        project = %event.project,
    );

    let handle = tokio::spawn(
        eventing_ops::run_workflow(
            event,
            Arc::clone(&ctx.engine),
            Arc::clone(&ctx.trace_store),
            Arc::clone(&ctx.workflow_tracker),
            Arc::clone(&ctx.trace_writer),
            ctx.event_tx.clone(),
            Arc::clone(&ctx.registry),
        )
        .instrument(span),
    );
    // Retaining the handle is what makes `foundry campaign cancel --now`
    // possible: a whole campaign runs inside this one task, so aborting it
    // stops the loop and drops the running agent's `Child`.
    ctx.workflow_tracker.attach_handle(&event_id, handle);
}

#[tonic::async_trait]
impl Foundry for FoundryService {
    async fn emit(&self, request: Request<EmitRequest>) -> Result<Response<EmitResponse>, Status> {
        eventing_ops::emit_rpc(&self.ctx, &self.work_items_path, request)
    }

    async fn status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        Ok(eventing_ops::status_rpc(&self.ctx.workflow_tracker, request))
    }

    async fn history(
        &self,
        request: Request<HistoryRequest>,
    ) -> Result<Response<HistoryResponse>, Status> {
        Ok(tracing_ops::history_rpc(&self.ctx.trace_store, request))
    }

    type WatchStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<WatchResponse, Status>> + Send>>;

    async fn watch(
        &self,
        request: Request<WatchRequest>,
    ) -> Result<Response<Self::WatchStream>, Status> {
        Ok(eventing_ops::watch_rpc(&self.ctx.event_tx, request))
    }

    async fn registry_add(
        &self,
        request: Request<RegistryAddRequest>,
    ) -> Result<Response<RegistryAddResponse>, Status> {
        registry_ops::add(&self.ctx.registry, &self.registry_path, request)
    }

    async fn registry_list(
        &self,
        request: Request<RegistryListRequest>,
    ) -> Result<Response<RegistryListResponse>, Status> {
        registry_ops::list(&self.ctx.registry, request)
    }

    async fn registry_show(
        &self,
        request: Request<RegistryShowRequest>,
    ) -> Result<Response<RegistryShowResponse>, Status> {
        registry_ops::show(&self.ctx.registry, request)
    }

    async fn registry_remove(
        &self,
        request: Request<RegistryRemoveRequest>,
    ) -> Result<Response<RegistryRemoveResponse>, Status> {
        registry_ops::remove(&self.ctx.registry, &self.registry_path, request)
    }

    async fn registry_edit(
        &self,
        request: Request<RegistryEditRequest>,
    ) -> Result<Response<RegistryEditResponse>, Status> {
        registry_ops::edit(&self.ctx.registry, &self.registry_path, request)
    }

    async fn add_campaign(
        &self,
        request: Request<AddCampaignRequest>,
    ) -> Result<Response<AddCampaignResponse>, Status> {
        self.campaign_operation(move |path, ctx| campaign_ops::add(path, &ctx.registry, request))
            .await
    }

    async fn list_campaigns(
        &self,
        request: Request<ListCampaignsRequest>,
    ) -> Result<Response<ListCampaignsResponse>, Status> {
        campaign_ops::list(&self.campaigns_path, request)
    }

    async fn get_campaign(
        &self,
        request: Request<GetCampaignRequest>,
    ) -> Result<Response<GetCampaignResponse>, Status> {
        campaign_ops::get(&self.campaigns_path, request)
    }

    async fn get_campaign_report(
        &self,
        request: Request<GetCampaignRequest>,
    ) -> Result<Response<CampaignReportResponse>, Status> {
        let name = request.into_inner().name;
        let campaigns = self.campaigns_path.clone();
        let events = self.events_dir.clone();
        let report = tokio::task::spawn_blocking(move || {
            let store = foundry_sdk::campaign::CampaignStore::load(&campaigns)
                .map_err(|e| Status::internal(e.to_string()))?;
            let campaign = store
                .find(&name)
                .ok_or_else(|| Status::not_found(format!("campaign '{name}' not found")))?;
            foundry_sdk::campaign::report::read_report(campaign, &events)
                .map_err(|e| Status::internal(e.to_string()))
        })
        .await
        .map_err(|e| Status::internal(e.to_string()))??;
        Ok(Response::new(CampaignReportResponse {
            report_json: serde_json::to_string(&report)
                .map_err(|e| Status::internal(e.to_string()))?,
        }))
    }

    async fn pause_campaign(
        &self,
        request: Request<PauseCampaignRequest>,
    ) -> Result<Response<PauseCampaignResponse>, Status> {
        self.campaign_operation(move |path, _ctx| campaign_ops::pause(path, request))
            .await
    }

    async fn resume_campaign(
        &self,
        request: Request<ResumeCampaignRequest>,
    ) -> Result<Response<ResumeCampaignResponse>, Status> {
        self.campaign_operation(move |path, _ctx| campaign_ops::resume(path, request))
            .await
    }

    async fn decide_campaign(
        &self,
        request: Request<DecideCampaignRequest>,
    ) -> Result<Response<DecideCampaignResponse>, Status> {
        self.campaign_operation(move |path, _ctx| campaign_ops::decide(path, request))
            .await
    }

    async fn complete_campaign(
        &self,
        request: Request<CompleteCampaignRequest>,
    ) -> Result<Response<CompleteCampaignResponse>, Status> {
        self.campaign_operation(move |path, ctx| campaign_ops::complete(path, ctx, request))
            .await
    }

    async fn cancel_campaign(
        &self,
        request: Request<CancelCampaignRequest>,
    ) -> Result<Response<CancelCampaignResponse>, Status> {
        campaign_ops::cancel(&self.campaigns_path, &self.work_items_path, &self.ctx, request).await
    }

    async fn close_work_item(
        &self,
        request: Request<CloseWorkItemRequest>,
    ) -> Result<Response<CloseWorkItemResponse>, Status> {
        let request = request.into_inner();
        let item = work_item_ops::cancel_item(
            &self.work_items_path,
            &self.ctx,
            request.id,
            request.reason,
            request.operator_origin,
            true,
        )
        .await?;
        Ok(Response::new(CloseWorkItemResponse { item: Some(item) }))
    }

    async fn resume_work_item(
        &self,
        request: Request<crate::proto::ResumeWorkItemRequest>,
    ) -> Result<Response<crate::proto::ResumeWorkItemResponse>, Status> {
        let request = request.into_inner();
        let hints =
            work_item_ops::ScheduleHints::from_wire(request.depends_on, &request.not_before)?;
        let item = work_item_ops::resume_item(
            &self.work_items_path,
            &self.ctx,
            &self.pacing,
            request.id,
            request.operator_origin,
            hints,
        )
        .await?;
        // The child's `work_item_submitted` was announced before the ledger
        // save that queued it, so the scheduler is told again now.
        self.pacing_wake.notify_one();
        Ok(Response::new(crate::proto::ResumeWorkItemResponse { item: Some(item) }))
    }

    async fn hold_work_item(
        &self,
        request: Request<HoldWorkItemRequest>,
    ) -> Result<Response<HoldWorkItemResponse>, Status> {
        let request = request.into_inner();
        let item = work_item_ops::hold_item(
            &self.work_items_path,
            &self.ctx,
            request.id,
            request.operator_origin,
        )
        .await?;
        Ok(Response::new(HoldWorkItemResponse { item: Some(item) }))
    }

    async fn release_work_item(
        &self,
        request: Request<ReleaseWorkItemRequest>,
    ) -> Result<Response<ReleaseWorkItemResponse>, Status> {
        let request = request.into_inner();
        let item = work_item_ops::release_item(
            &self.work_items_path,
            &self.ctx,
            &self.pacing,
            request.id,
            request.operator_origin,
        )
        .await?;
        Ok(Response::new(ReleaseWorkItemResponse { item: Some(item) }))
    }

    async fn get_pacing(
        &self,
        request: Request<GetPacingRequest>,
    ) -> Result<Response<GetPacingResponse>, Status> {
        pacing_ops::get(&self.work_items_path, &self.pacing, &self.ctx, request)
    }

    async fn pause_pacing(
        &self,
        request: Request<PausePacingRequest>,
    ) -> Result<Response<PausePacingResponse>, Status> {
        pacing_ops::pause(&self.work_items_path, &self.pacing, &self.ctx, request).await
    }

    async fn resume_pacing(
        &self,
        request: Request<ResumePacingRequest>,
    ) -> Result<Response<ResumePacingResponse>, Status> {
        pacing_ops::resume(&self.work_items_path, &self.pacing, &self.ctx, request).await
    }

    #[tracing::instrument(skip(self, _request))]
    async fn reconcile_work(
        &self,
        _request: Request<crate::proto::ReconcileWorkRequest>,
    ) -> Result<Response<crate::proto::ReconcileWorkResponse>, Status> {
        let event = Event::new(
            foundry_sdk::event::EventType::WorkReconcileStarted,
            "system".into(),
            foundry_sdk::throttle::Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some(foundry_sdk::event::mint_trace_id()));
        track_workflow(&event, &self.ctx.workflow_tracker);
        let result = eventing_ops::run_workflow_result(
            event,
            self.ctx.engine.clone(),
            self.ctx.trace_store.clone(),
            self.ctx.workflow_tracker.clone(),
            self.ctx.trace_writer.clone(),
            self.ctx.event_tx.clone(),
            self.ctx.registry.clone(),
        )
        .await;
        let completion = result
            .events
            .iter()
            .find(|event| event.event_type == foundry_sdk::event::EventType::WorkReconcileCompleted)
            .ok_or_else(|| Status::internal("work reconciliation emitted no completion"))?;
        let report = completion
            .parse_payload::<foundry_sdk::payload::WorkReconcileCompletedPayload>()
            .map_err(|error| Status::internal(error.to_string()))?;
        if !report.success {
            return Err(Status::internal(format!(
                "work reconciliation failed: {}",
                report.errors.join("; ")
            )));
        }
        Ok(Response::new(crate::proto::ReconcileWorkResponse {
            digest_path: report.digest_path.unwrap_or_default(),
            markdown: report.markdown,
            completion_json: completion.payload.to_string(),
        }))
    }

    async fn cancel_work_item(
        &self,
        request: Request<CancelWorkItemRequest>,
    ) -> Result<Response<CancelWorkItemResponse>, Status> {
        let request = request.into_inner();
        let item = work_item_ops::cancel_item(
            &self.work_items_path,
            &self.ctx,
            request.id,
            "cancelled by operator".to_string(),
            request.operator_origin,
            false,
        )
        .await?;
        Ok(Response::new(CancelWorkItemResponse { item: Some(item) }))
    }

    async fn list_work_items(
        &self,
        request: Request<ListWorkItemsRequest>,
    ) -> Result<Response<ListWorkItemsResponse>, Status> {
        work_item_ops::list(&self.work_items_path, request)
    }

    async fn get_work_item(
        &self,
        request: Request<GetWorkItemRequest>,
    ) -> Result<Response<GetWorkItemResponse>, Status> {
        work_item_ops::get(&self.work_items_path, request)
    }

    async fn list_work_item_events(
        &self,
        request: Request<ListWorkItemEventsRequest>,
    ) -> Result<Response<ListWorkItemEventsResponse>, Status> {
        work_item_ops::list_events(&self.work_items_path, &self.events_dir, request).await
    }

    async fn advance_campaign(
        &self,
        request: Request<AdvanceCampaignRequest>,
    ) -> Result<Response<AdvanceCampaignResponse>, Status> {
        self.campaign_operation(move |path, ctx| campaign_ops::advance(path, ctx, request))
            .await
    }

    async fn sentinel_enable(
        &self,
        request: Request<SentinelEnableRequest>,
    ) -> Result<Response<SentinelEnableResponse>, Status> {
        sentinel_ops::enable(&self.sentinels, &self.sentinels_path, &self.scheduler_reload, request)
    }

    async fn sentinel_disable(
        &self,
        request: Request<SentinelDisableRequest>,
    ) -> Result<Response<SentinelDisableResponse>, Status> {
        sentinel_ops::disable(
            &self.sentinels,
            &self.sentinels_path,
            &self.scheduler_reload,
            request,
        )
    }

    async fn sentinel_list(
        &self,
        request: Request<SentinelListRequest>,
    ) -> Result<Response<SentinelListResponse>, Status> {
        sentinel_ops::list(&self.sentinels, request)
    }

    async fn sentinel_show(
        &self,
        request: Request<SentinelShowRequest>,
    ) -> Result<Response<SentinelShowResponse>, Status> {
        sentinel_ops::show(&self.sentinels, request)
    }

    async fn trace(
        &self,
        request: Request<TraceRequest>,
    ) -> Result<Response<TraceResponse>, Status> {
        Ok(tracing_ops::trace_rpc(&self.ctx.trace_store, request))
    }

    async fn span(&self, request: Request<SpanRequest>) -> Result<Response<SpanResponse>, Status> {
        Ok(tracing_ops::span_rpc(&self.ctx.trace_store, request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Duration;

    /// Build a minimal `FoundryService` for testing, returning the service and
    /// a broadcast receiver to observe emitted events.
    fn test_service() -> (FoundryService, broadcast::Receiver<Event>) {
        let (event_tx, rx) = broadcast::channel(64);
        let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));
        let trace_store = Arc::new(TraceStore::new(Duration::from_secs(60)));
        let workflow_tracker = Arc::new(WorkflowTracker::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let trace_writer = Arc::new(TraceWriter::new(tmp.path().to_str().unwrap()));
        let registry = Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let tmp_registry = tempfile::NamedTempFile::new().expect("tempfile");
        let registry_path = tmp_registry.path().to_path_buf();
        let tmp_campaigns = tempfile::NamedTempFile::new().expect("tempfile");
        let campaigns_path = tmp_campaigns.path().to_path_buf();
        let sentinels = Arc::new(RwLock::new(SentinelStore::default_seed()));
        let tmp_sentinels = tempfile::NamedTempFile::new().expect("tempfile");
        let sentinels_path = tmp_sentinels.path().to_path_buf();
        let scheduler_reload = Arc::new(Notify::new());
        let ctx = RuntimeContext {
            engine,
            trace_store,
            workflow_tracker,
            trace_writer,
            event_tx,
            registry,
        };
        let stores = StoreConfig {
            work_items_path: std::path::PathBuf::new(),
            events_dir: std::path::PathBuf::new(),
            campaigns_path,
            registry_path,
            sentinels,
            sentinels_path,
            scheduler_reload,
        };
        let service = FoundryService::new(ctx, stores);
        (service, rx)
    }

    #[tokio::test]
    async fn project_run_broadcasts_completion_event() {
        let (service, mut rx) = test_service();

        let request = Request::new(EmitRequest {
            event_type: "project_run_started".to_string(),
            project: "test-project".to_string(),
            throttle: 2, // dry_run
            payload_json: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: String::new(),
            source: None,
        });

        let response = service.emit(request).await.expect("emit should succeed");
        let root_event_id = response.into_inner().event_id;

        // Collect events from the broadcast channel until we see the completion
        // event or time out.
        let mut saw_root = false;
        let mut saw_completed = false;
        let mut completed_payload = serde_json::Value::Null;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let result = tokio::time::timeout_at(deadline, rx.recv()).await;
            match result {
                Ok(Ok(event)) => {
                    if event.id == root_event_id {
                        saw_root = true;
                    }
                    if event.event_type == foundry_sdk::event::EventType::ProjectRunCompleted {
                        saw_completed = true;
                        completed_payload = event.payload.clone();
                        break;
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }

        assert!(saw_root, "root event should be broadcast");
        assert!(saw_completed, "ProjectRunCompleted should be broadcast");
        assert_eq!(completed_payload["root_event_id"], root_event_id);
        assert_eq!(completed_payload["success"], true);
    }

    #[tokio::test]
    async fn system_maintenance_cycle_broadcasts_summary_request_with_root_event_id() {
        let (service, mut rx) = test_service();

        let request = Request::new(EmitRequest {
            event_type: "maintenance_cycle_started".to_string(),
            project: "system".to_string(),
            throttle: 2, // dry_run
            payload_json: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: String::new(),
            source: None,
        });

        let response = service.emit(request).await.expect("emit should succeed");
        let root_event_id = response.into_inner().event_id;

        let mut saw_summary = false;
        let mut summary_payload = serde_json::Value::Null;

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let result = tokio::time::timeout_at(deadline, rx.recv()).await;
            match result {
                Ok(Ok(event)) => {
                    if event.event_type
                        == foundry_sdk::event::EventType::MaintenanceSummaryRequested
                        && event.project == "system"
                    {
                        saw_summary = true;
                        summary_payload = event.payload.clone();
                        break;
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }

        assert!(
            saw_summary,
            "the service should broadcast MaintenanceSummaryRequested after a system cycle"
        );
        assert_eq!(
            summary_payload["root_event_id"], root_event_id,
            "the summary request must include root_event_id so the CLI can detect run end"
        );
    }

    #[tokio::test]
    async fn emit_mints_span_id_when_request_omits_it() {
        let (service, mut rx) = test_service();

        let request = Request::new(EmitRequest {
            event_type: "project_run_started".to_string(),
            project: "span-mint-project".to_string(),
            throttle: 2, // dry_run
            payload_json: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: String::new(),
            source: None,
        });

        let response = service.emit(request).await.expect("emit should succeed");
        let root_event_id = response.into_inner().event_id;

        // Read the broadcast stream until we see the root event itself, which
        // carries the minted span_id.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut root_span_id: Option<String> = None;
        loop {
            let result = tokio::time::timeout_at(deadline, rx.recv()).await;
            match result {
                Ok(Ok(event)) => {
                    if event.id == root_event_id {
                        root_span_id = event.span_id.clone();
                        break;
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }

        let span_id = root_span_id.expect("root event must carry a span_id");
        assert!(
            !span_id.is_empty(),
            "Emit must mint a non-empty span_id when the request omits one"
        );
        // span_ids are 16 hex chars (see mint_span_id contract).
        assert_eq!(span_id.len(), 16, "minted span_id must be 16 hex chars");
        assert!(span_id.chars().all(|c| c.is_ascii_hexdigit()), "minted span_id must be hex");
    }

    #[tokio::test]
    async fn span_rpc_returns_events_sharing_requested_span_id() {
        let (service, mut rx) = test_service();

        // 1. Emit a workflow root event with no trace/span set — Emit will mint both.
        let request = Request::new(EmitRequest {
            event_type: "project_run_started".to_string(),
            project: "span-rpc-project".to_string(),
            throttle: 2, // dry_run
            payload_json: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: String::new(),
            source: None,
        });

        let response = service.emit(request).await.expect("emit should succeed");
        let root_event_id = response.into_inner().event_id;

        // Wait for ProjectRunCompleted to confirm the background task has
        // finished inserting the trace into the store.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut saw_completed = false;
        loop {
            let result = tokio::time::timeout_at(deadline, rx.recv()).await;
            match result {
                Ok(Ok(event)) => {
                    if event.event_type == foundry_sdk::event::EventType::ProjectRunCompleted {
                        saw_completed = true;
                        break;
                    }
                }
                Ok(Err(_)) | Err(_) => break,
            }
        }
        assert!(saw_completed, "background processing must complete before Trace lookup");

        // 2. Call Trace and confirm span fields are populated end-to-end.
        let trace_resp = service
            .trace(Request::new(TraceRequest {
                event_id: root_event_id.clone(),
            }))
            .await
            .expect("trace should succeed")
            .into_inner();

        assert!(trace_resp.found, "trace must be found for the root event");
        let root_trace_event = trace_resp
            .events
            .iter()
            .find(|e| e.event_id == root_event_id)
            .expect("root event must appear in trace");
        assert!(!root_trace_event.span_id.is_empty(), "root event's span_id must be populated");
        assert!(!root_trace_event.trace_id.is_empty(), "root event's trace_id must be populated");

        // 3. Pick the workflow span_id from the response.
        let workflow_span_id = root_trace_event.span_id.clone();

        // 4. Call Span and confirm found=true plus every returned event shares that span_id.
        let span_resp = service
            .span(Request::new(SpanRequest {
                span_id: workflow_span_id.clone(),
            }))
            .await
            .expect("span should succeed")
            .into_inner();

        assert!(span_resp.found, "span must be found");
        assert!(!span_resp.events.is_empty(), "span lookup must surface at least the root event");
        for e in &span_resp.events {
            assert_eq!(
                e.span_id, workflow_span_id,
                "every event returned by Span must share the requested span_id"
            );
        }
    }

    #[tokio::test]
    async fn span_rpc_returns_not_found_for_unknown_span() {
        let (service, _rx) = test_service();

        let resp = service
            .span(Request::new(SpanRequest {
                span_id: "deadbeefdeadbeef".to_string(),
            }))
            .await
            .expect("span should succeed")
            .into_inner();

        assert!(!resp.found);
        assert!(resp.events.is_empty());
        assert!(resp.block_executions.is_empty());
        assert_eq!(resp.total_duration_ms, 0);
    }

    #[tokio::test]
    async fn non_maintenance_event_does_not_broadcast_completion() {
        let (service, mut rx) = test_service();

        let request = Request::new(EmitRequest {
            event_type: "greeting_requested".to_string(),
            project: "test-project".to_string(),
            throttle: 0,
            payload_json: String::new(),
            trace_id: String::new(),
            span_id: String::new(),
            parent_span_id: String::new(),
            source: None,
        });

        service.emit(request).await.expect("emit should succeed");

        // Give the background task time to complete.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // Drain all events — none should be MaintenanceCycleCompleted or ProjectRunCompleted.
        let mut saw_completed = false;
        while let Ok(event) = rx.try_recv() {
            if event.event_type == foundry_sdk::event::EventType::MaintenanceCycleCompleted
                || event.event_type == foundry_sdk::event::EventType::ProjectRunCompleted
            {
                saw_completed = true;
            }
        }

        assert!(!saw_completed, "no completion event for non-maintenance runs");
    }

    // -- Registry mutation tests --

    #[tokio::test]
    async fn registry_add_inserts_project_and_saves() {
        let (service, _rx) = test_service();

        let req = Request::new(RegistryAddRequest {
            name: "my-project".to_string(),
            path: "/tmp/my-project".to_string(),
            stack: "rust".to_string(),
            agent: "claude".to_string(),
            repo: "owner/my-project".to_string(),
            branch: "main".to_string(),
            iterate: true,
            maintain: false,
            push: true,
            audit: false,
            release: false,
            install_command: String::new(),
            install_brew: String::new(),
            notes: String::new(),
            timeout_secs: 0,
            update_policy: String::new(),
            installs_skill: String::new(),
        });

        let resp = service.registry_add(req).await.expect("add should succeed");
        let project = resp.into_inner().project.expect("project should be returned");
        assert_eq!(project.name, "my-project");
        assert_eq!(project.stack, "rust");
        assert!(project.iterate);

        // In-memory registry should now have the project.
        let reg = service.ctx.registry.read().unwrap();
        assert_eq!(reg.projects.len(), 1);
        assert_eq!(reg.projects[0].name, "my-project");
    }

    #[tokio::test]
    async fn registry_add_duplicate_returns_already_exists() {
        let (service, _rx) = test_service();

        let make_req = || {
            Request::new(RegistryAddRequest {
                name: "alpha".to_string(),
                path: "/tmp/alpha".to_string(),
                stack: "rust".to_string(),
                agent: String::new(),
                repo: String::new(),
                branch: "main".to_string(),
                iterate: false,
                maintain: false,
                push: false,
                audit: false,
                release: false,
                install_command: String::new(),
                install_brew: String::new(),
                notes: String::new(),
                timeout_secs: 0,
                update_policy: String::new(),
                installs_skill: String::new(),
            })
        };

        service.registry_add(make_req()).await.expect("first add should succeed");
        let err = service.registry_add(make_req()).await.unwrap_err();
        assert_eq!(err.code(), tonic::Code::AlreadyExists);
    }

    #[tokio::test]
    async fn registry_remove_deletes_project() {
        let (service, _rx) = test_service();

        // Add first, then remove.
        service
            .registry_add(Request::new(RegistryAddRequest {
                name: "to-remove".to_string(),
                path: "/tmp/tr".to_string(),
                stack: "rust".to_string(),
                agent: String::new(),
                repo: String::new(),
                branch: "main".to_string(),
                iterate: false,
                maintain: false,
                push: false,
                audit: false,
                release: false,
                install_command: String::new(),
                install_brew: String::new(),
                notes: String::new(),
                timeout_secs: 0,
                update_policy: String::new(),
                installs_skill: String::new(),
            }))
            .await
            .expect("add should succeed");

        service
            .registry_remove(Request::new(RegistryRemoveRequest {
                name: "to-remove".to_string(),
            }))
            .await
            .expect("remove should succeed");

        let reg = service.ctx.registry.read().unwrap();
        assert!(reg.projects.is_empty());
    }

    #[tokio::test]
    async fn registry_remove_not_found_returns_not_found_status() {
        let (service, _rx) = test_service();

        let err = service
            .registry_remove(Request::new(RegistryRemoveRequest {
                name: "nonexistent".to_string(),
            }))
            .await
            .unwrap_err();

        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn registry_edit_updates_project_fields() {
        let (service, _rx) = test_service();

        service
            .registry_add(Request::new(RegistryAddRequest {
                name: "editable".to_string(),
                path: "/tmp/editable".to_string(),
                stack: "rust".to_string(),
                agent: String::new(),
                repo: String::new(),
                branch: "main".to_string(),
                iterate: false,
                maintain: false,
                push: false,
                audit: false,
                release: false,
                install_command: String::new(),
                install_brew: String::new(),
                notes: String::new(),
                timeout_secs: 0,
                update_policy: String::new(),
                installs_skill: String::new(),
            }))
            .await
            .expect("add should succeed");

        let resp = service
            .registry_edit(Request::new(RegistryEditRequest {
                name: "editable".to_string(),
                path: String::new(),
                stack: String::new(),
                agent: "gemini".to_string(),
                repo: String::new(),
                branch: String::new(),
                skip: String::new(),
                clear_skip: false,
                iterate: true,
                clear_iterate: false,
                maintain: false,
                clear_maintain: false,
                push: false,
                clear_push: false,
                audit: false,
                clear_audit: false,
                release: false,
                clear_release: false,
                install_command: String::new(),
                install_brew: String::new(),
                clear_install: false,
                notes: String::new(),
                clear_notes: false,
                timeout_secs: 0,
                clear_timeout: false,
                update_policy: String::new(),
                installs_skill: String::new(),
            }))
            .await
            .expect("edit should succeed");

        let project = resp.into_inner().project.expect("project should be returned");
        assert_eq!(project.agent, "gemini");
        assert!(project.iterate);
    }

    #[tokio::test]
    async fn registry_edit_not_found_returns_not_found_status() {
        let (service, _rx) = test_service();

        let err = service
            .registry_edit(Request::new(RegistryEditRequest {
                name: "ghost".to_string(),
                path: String::new(),
                stack: String::new(),
                agent: String::new(),
                repo: String::new(),
                branch: String::new(),
                skip: String::new(),
                clear_skip: false,
                iterate: false,
                clear_iterate: false,
                maintain: false,
                clear_maintain: false,
                push: false,
                clear_push: false,
                audit: false,
                clear_audit: false,
                release: false,
                clear_release: false,
                install_command: String::new(),
                install_brew: String::new(),
                clear_install: false,
                notes: String::new(),
                clear_notes: false,
                timeout_secs: 0,
                clear_timeout: false,
                update_policy: String::new(),
                installs_skill: String::new(),
            }))
            .await
            .unwrap_err();

        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    // -----------------------------------------------------------------
    // Sentinel RPCs
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn sentinel_disable_flips_in_memory_persists_and_notifies() {
        let (service, _rx) = test_service();
        let path = service.sentinels_path.clone();
        let reload = Arc::clone(&service.scheduler_reload);

        // Pre-arm a waiter so we can confirm the scheduler reload was poked.
        let notified = tokio::spawn(async move { reload.notified().await });

        let response = service
            .sentinel_disable(Request::new(SentinelDisableRequest {
                name: "nightly-maintenance".to_string(),
            }))
            .await
            .expect("disable should succeed")
            .into_inner();

        let proto = response.sentinel.expect("sentinel echoed back");
        assert_eq!(proto.name, "nightly-maintenance");
        assert!(!proto.enabled);

        // In-memory state flipped.
        {
            let store = service.sentinels.read().unwrap();
            assert!(!store.sentinels[0].enabled);
        }

        // Persisted to disk.
        let on_disk = SentinelStore::load(&path).expect("load reads what we just saved");
        assert!(!on_disk.sentinels[0].enabled);

        // Scheduler reload pulse delivered.
        tokio::time::timeout(std::time::Duration::from_millis(100), notified)
            .await
            .expect("reload signal should be delivered")
            .expect("waiter task should finish");
    }

    #[tokio::test]
    async fn sentinel_enable_flips_in_memory_persists_and_notifies() {
        let (service, _rx) = test_service();
        let path = service.sentinels_path.clone();
        let reload = Arc::clone(&service.scheduler_reload);

        // Disable first so re-enable is observable.
        {
            let mut store = service.sentinels.write().unwrap();
            store.sentinels[0].enabled = false;
        }

        let notified = tokio::spawn(async move { reload.notified().await });

        let response = service
            .sentinel_enable(Request::new(SentinelEnableRequest {
                name: "nightly-maintenance".to_string(),
            }))
            .await
            .expect("enable should succeed")
            .into_inner();

        let proto = response.sentinel.expect("sentinel echoed back");
        assert!(proto.enabled);
        assert_eq!(proto.cron, "0 2 * * *");
        assert_eq!(proto.emit_event_type, "maintenance_cycle_started");
        assert_eq!(proto.emit_project, "system");
        assert_eq!(proto.emit_throttle, 0); // full

        let on_disk = SentinelStore::load(&path).expect("load reads what we just saved");
        assert!(on_disk.sentinels[0].enabled);

        tokio::time::timeout(std::time::Duration::from_millis(100), notified)
            .await
            .expect("reload signal should be delivered")
            .expect("waiter task should finish");
    }

    #[tokio::test]
    async fn sentinel_enable_unknown_returns_not_found() {
        let (service, _rx) = test_service();
        let err = service
            .sentinel_enable(Request::new(SentinelEnableRequest {
                name: "ghost".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn sentinel_disable_unknown_returns_not_found() {
        let (service, _rx) = test_service();
        let err = service
            .sentinel_disable(Request::new(SentinelDisableRequest {
                name: "ghost".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn sentinel_list_returns_all_seed_entries() {
        let (service, _rx) = test_service();

        let response = service
            .sentinel_list(Request::new(SentinelListRequest {}))
            .await
            .expect("list should succeed")
            .into_inner();

        assert_eq!(response.sentinels.len(), 5);
        assert_eq!(response.sentinels[0].name, "nightly-maintenance");
        assert_eq!(response.sentinels[0].cron, "0 2 * * *");
        assert_eq!(response.sentinels[0].emit_event_type, "maintenance_cycle_started");
        assert_eq!(response.sentinels[0].emit_project, "system");
        assert_eq!(response.sentinels[0].emit_throttle, 0);
        assert_eq!(response.sentinels[0].emit_payload_json, "{}");
        assert!(response.sentinels[0].enabled);
    }

    #[tokio::test]
    async fn sentinel_show_returns_full_seed_entry() {
        let (service, _rx) = test_service();

        let response = service
            .sentinel_show(Request::new(SentinelShowRequest {
                name: "nightly-maintenance".to_string(),
            }))
            .await
            .expect("show should succeed")
            .into_inner();

        let sentinel = response.sentinel.expect("sentinel echoed");
        assert_eq!(sentinel.name, "nightly-maintenance");
        assert_eq!(sentinel.cron, "0 2 * * *");
        assert_eq!(sentinel.emit_event_type, "maintenance_cycle_started");
        assert_eq!(sentinel.emit_project, "system");
        assert_eq!(sentinel.emit_throttle, 0);
        assert_eq!(sentinel.emit_payload_json, "{}");
        assert!(sentinel.enabled);
    }

    #[cfg(unix)]
    fn make_sentinel_persist_failure_fixture(
        store: &SentinelStore,
    ) -> (tempfile::TempDir, std::path::PathBuf, Vec<u8>) {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let sentinels_path = tempdir.path().join("sentinels.json");
        store.save(&sentinels_path).expect("save seeded sentinels");
        let before = std::fs::read(&sentinels_path).expect("read seeded sentinels");
        let mut file_permissions =
            std::fs::metadata(&sentinels_path).expect("stat sentinel file").permissions();
        file_permissions.set_mode(0o644);
        std::fs::set_permissions(&sentinels_path, file_permissions)
            .expect("set sentinel file writable for direct-write trap");
        let mut dir_permissions = std::fs::metadata(tempdir.path())
            .expect("stat sentinel directory")
            .permissions();
        dir_permissions.set_mode(0o555);
        std::fs::set_permissions(tempdir.path(), dir_permissions)
            .expect("set sentinel directory readonly");
        (tempdir, sentinels_path, before)
    }

    #[cfg(unix)]
    fn restore_sentinel_fixture_permissions(dir: &std::path::Path) {
        let mut permissions =
            std::fs::metadata(dir).expect("stat sentinel directory").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(dir, permissions).expect("restore sentinel directory permissions");
    }

    #[cfg(unix)]
    fn test_service_with_sentinels_path(
        sentinels: SentinelStore,
        sentinels_path: std::path::PathBuf,
    ) -> FoundryService {
        let (event_tx, _rx) = broadcast::channel(64);
        let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));
        let trace_store = Arc::new(TraceStore::new(Duration::from_secs(60)));
        let workflow_tracker = Arc::new(WorkflowTracker::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let trace_writer = Arc::new(TraceWriter::new(tmp.path().to_str().unwrap()));
        let registry = Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let tmp_registry = tempfile::NamedTempFile::new().expect("tempfile");
        let registry_path = tmp_registry.path().to_path_buf();
        let tmp_campaigns = tempfile::NamedTempFile::new().expect("tempfile");
        let campaigns_path = tmp_campaigns.path().to_path_buf();
        let scheduler_reload = Arc::new(Notify::new());
        let ctx = RuntimeContext {
            engine,
            trace_store,
            workflow_tracker,
            trace_writer,
            event_tx,
            registry,
        };
        let stores = StoreConfig {
            work_items_path: std::path::PathBuf::new(),
            events_dir: std::path::PathBuf::new(),
            campaigns_path,
            registry_path,
            sentinels: Arc::new(RwLock::new(sentinels)),
            sentinels_path,
            scheduler_reload,
        };
        FoundryService::new(ctx, stores)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sentinel_enable_failed_persistence_leaves_memory_scheduler_and_disk_unchanged() {
        let mut initial = SentinelStore::default_seed();
        initial.sentinels[0].enabled = false;
        let (tempdir, sentinels_path, before) = make_sentinel_persist_failure_fixture(&initial);
        let service = test_service_with_sentinels_path(initial.clone(), sentinels_path.clone());
        let reload = Arc::clone(&service.scheduler_reload);

        let err = service
            .sentinel_enable(Request::new(SentinelEnableRequest {
                name: "nightly-maintenance".to_string(),
            }))
            .await
            .expect_err("enable should fail when persistence fails");
        assert_eq!(err.code(), tonic::Code::Internal);
        assert!(err.message().contains("failed to save sentinels"));

        let notify_result =
            tokio::time::timeout(std::time::Duration::from_millis(50), reload.notified()).await;
        assert!(notify_result.is_err(), "failed persistence must not notify scheduler");

        let listed = service
            .sentinel_list(Request::new(SentinelListRequest {}))
            .await
            .expect("list should succeed after failed enable")
            .into_inner();
        assert!(!listed.sentinels[0].enabled, "list view must remain on the pre-mutation state");
        {
            let store = service.sentinels.read().unwrap();
            assert!(!store.sentinels[0].enabled, "in-memory state must remain unchanged");
        }
        assert_eq!(
            std::fs::read(&sentinels_path).expect("read sentinel file after failed enable"),
            before,
            "disk bytes must remain byte-identical after failed enable"
        );

        restore_sentinel_fixture_permissions(tempdir.path());
        drop(tempdir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn sentinel_disable_failed_persistence_leaves_memory_scheduler_and_disk_unchanged() {
        let initial = SentinelStore::default_seed();
        let (tempdir, sentinels_path, before) = make_sentinel_persist_failure_fixture(&initial);
        let service = test_service_with_sentinels_path(initial, sentinels_path.clone());
        let reload = Arc::clone(&service.scheduler_reload);

        let err = service
            .sentinel_disable(Request::new(SentinelDisableRequest {
                name: "nightly-maintenance".to_string(),
            }))
            .await
            .expect_err("disable should fail when persistence fails");
        assert_eq!(err.code(), tonic::Code::Internal);
        assert!(err.message().contains("failed to save sentinels"));

        let notify_result =
            tokio::time::timeout(std::time::Duration::from_millis(50), reload.notified()).await;
        assert!(notify_result.is_err(), "failed persistence must not notify scheduler");

        let shown = service
            .sentinel_show(Request::new(SentinelShowRequest {
                name: "nightly-maintenance".to_string(),
            }))
            .await
            .expect("show should succeed after failed disable")
            .into_inner()
            .sentinel
            .expect("sentinel echoed");
        assert!(shown.enabled, "show view must remain on the pre-mutation state");
        {
            let store = service.sentinels.read().unwrap();
            assert!(store.sentinels[0].enabled, "in-memory state must remain unchanged");
        }
        assert_eq!(
            std::fs::read(&sentinels_path).expect("read sentinel file after failed disable"),
            before,
            "disk bytes must remain byte-identical after failed disable"
        );

        restore_sentinel_fixture_permissions(tempdir.path());
        drop(tempdir);
    }

    #[tokio::test]
    async fn sentinel_enable_success_notifies_scheduler_and_updates_list_state() {
        let (service, _rx) = test_service();
        {
            let mut store = service.sentinels.write().unwrap();
            store.sentinels[0].enabled = false;
            store.save(&service.sentinels_path).expect("persist disabled baseline");
        }

        let scheduler_reload = Arc::clone(&service.scheduler_reload);
        let notified = scheduler_reload.notified();
        let response = service
            .sentinel_enable(Request::new(SentinelEnableRequest {
                name: "nightly-maintenance".to_string(),
            }))
            .await
            .expect("enable should succeed")
            .into_inner();

        let sentinel = response.sentinel.expect("sentinel echoed");
        assert!(sentinel.enabled, "response must report enabled state");
        tokio::time::timeout(Duration::from_millis(50), notified)
            .await
            .expect("successful enable must notify scheduler");

        let listed = service
            .sentinel_list(Request::new(SentinelListRequest {}))
            .await
            .expect("list should succeed after enable")
            .into_inner();
        assert!(
            listed.sentinels[0].enabled,
            "list view must observe the committed enabled state"
        );
    }

    #[tokio::test]
    async fn sentinel_disable_success_notifies_scheduler_and_updates_show_state() {
        let (service, _rx) = test_service();

        let scheduler_reload = Arc::clone(&service.scheduler_reload);
        let notified = scheduler_reload.notified();
        let response = service
            .sentinel_disable(Request::new(SentinelDisableRequest {
                name: "nightly-maintenance".to_string(),
            }))
            .await
            .expect("disable should succeed")
            .into_inner();

        let sentinel = response.sentinel.expect("sentinel echoed");
        assert!(!sentinel.enabled, "response must report disabled state");
        tokio::time::timeout(Duration::from_millis(50), notified)
            .await
            .expect("successful disable must notify scheduler");

        let shown = service
            .sentinel_show(Request::new(SentinelShowRequest {
                name: "nightly-maintenance".to_string(),
            }))
            .await
            .expect("show should succeed after disable")
            .into_inner()
            .sentinel
            .expect("sentinel echoed");
        assert!(!shown.enabled, "show view must observe the committed disabled state");
    }

    #[tokio::test]
    async fn concurrent_sentinel_toggles_leave_store_and_disk_consistent() {
        let (service, _rx) = test_service();
        let path = service.sentinels_path.clone();

        let disable = service.sentinel_disable(Request::new(SentinelDisableRequest {
            name: "nightly-maintenance".to_string(),
        }));
        let enable = service.sentinel_enable(Request::new(SentinelEnableRequest {
            name: "nightly-maintenance".to_string(),
        }));
        let (disable_result, enable_result) = tokio::join!(disable, enable);

        let disable_response = disable_result.expect("disable should succeed").into_inner();
        let enable_response = enable_result.expect("enable should succeed").into_inner();
        assert!(
            !disable_response.sentinel.expect("disable sentinel echoed").enabled,
            "disable response should reflect a disabled sentinel"
        );
        assert!(
            enable_response.sentinel.expect("enable sentinel echoed").enabled,
            "enable response should reflect an enabled sentinel"
        );

        let in_memory_enabled = {
            let store = service.sentinels.read().unwrap();
            store
                .find_sentinel("nightly-maintenance")
                .expect("seed sentinel exists")
                .enabled
        };
        let on_disk_enabled = SentinelStore::load(&path)
            .expect("load persisted sentinels")
            .find_sentinel("nightly-maintenance")
            .expect("seed sentinel exists")
            .enabled;
        assert_eq!(
            in_memory_enabled, on_disk_enabled,
            "serialized concurrent toggles must leave disk and memory in the same final state"
        );
    }

    // ── get_campaign service-level tests ─────────────────────────────────────

    /// Build a service backed by a specific campaigns store path.
    fn test_service_with_campaigns_path(campaigns_path: std::path::PathBuf) -> FoundryService {
        let (event_tx, _rx) = broadcast::channel(64);
        let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));
        let trace_store = Arc::new(TraceStore::new(Duration::from_secs(60)));
        let workflow_tracker = Arc::new(WorkflowTracker::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let trace_writer = Arc::new(TraceWriter::new(tmp.path().to_str().unwrap()));
        let registry = Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let tmp_registry = tempfile::NamedTempFile::new().expect("tempfile");
        let registry_path = tmp_registry.path().to_path_buf();
        let sentinels = Arc::new(RwLock::new(SentinelStore::default_seed()));
        let tmp_sentinels = tempfile::NamedTempFile::new().expect("tempfile");
        let sentinels_path = tmp_sentinels.path().to_path_buf();
        let scheduler_reload = Arc::new(Notify::new());
        let ctx = RuntimeContext {
            engine,
            trace_store,
            workflow_tracker,
            trace_writer,
            event_tx,
            registry,
        };
        let stores = StoreConfig {
            work_items_path: std::path::PathBuf::new(),
            events_dir: std::path::PathBuf::new(),
            campaigns_path,
            registry_path,
            sentinels,
            sentinels_path,
            scheduler_reload,
        };
        FoundryService::new(ctx, stores)
    }

    #[tokio::test]
    async fn get_campaign_returns_full_detail_with_gate_and_review_evidence() {
        use foundry_sdk::campaign::{
            Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
        };

        // Write an on-disk campaign store with one campaign carrying both a
        // Gate and a Review done-evidence entry.
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let campaign = Campaign {
            name: "service-campaign".to_string(),
            project: "service-project".to_string(),
            mission: "Prove the service detail path works end-to-end.".to_string(),
            intent_refs: vec!["intent.alpha".to_string(), "intent.beta".to_string()],
            context_paths: vec!["docs/service.md".to_string()],
            done_evidence: vec![
                DoneEvidence::Gate {
                    command: "cargo test --workspace".to_string(),
                    required: true,
                    artifacts: vec!["tests/campaign_detail.rs".to_string()],
                },
                DoneEvidence::Review {
                    statement: "Human reviewer signed off.".to_string(),
                },
            ],
            budget: CampaignBudget {
                max_cycles: 10,
                ..Default::default()
            },
            escalation: vec!["Escalate to team lead.".to_string()],
            status: CampaignStatus::Active,
            cycles_completed: 4,
            cycles_landed: 3,
            authorized_by: Some("bob".to_string()),
            agent_provider: Some("opus".to_string()),
            last_run_event_id: Some("evt-service-42".to_string()),
            owner_decisions: vec![],
            pending_run_result: None,
            objective_history: vec![],
            writable_repositories: vec![],
        };
        let store = CampaignStore {
            version: 1,
            campaigns: vec![campaign],
        };
        store.save(tmp.path()).expect("save store");

        let service = test_service_with_campaigns_path(tmp.path().to_path_buf());
        let response = service
            .get_campaign(Request::new(GetCampaignRequest {
                name: "service-campaign".to_string(),
            }))
            .await
            .expect("get_campaign should succeed");
        let detail = response.into_inner().campaign.expect("campaign present");

        assert_eq!(detail.name, "service-campaign");
        assert_eq!(detail.project, "service-project");
        assert_eq!(detail.mission, "Prove the service detail path works end-to-end.");
        assert_eq!(detail.status, "active");
        assert_eq!(detail.cycles_completed, 4);
        assert_eq!(detail.cycles_landed, 3);
        assert_eq!(detail.max_cycles, 10);
        assert_eq!(detail.authorized_by, "bob");
        assert_eq!(detail.agent_provider, "opus");
        assert_eq!(detail.last_run_event_id, "evt-service-42");
        assert_eq!(detail.intent_refs, vec!["intent.alpha", "intent.beta"]);
        assert_eq!(detail.context_paths, vec!["docs/service.md"]);
        assert_eq!(detail.escalation, vec!["Escalate to team lead."]);
        assert_eq!(detail.done_evidence.len(), 2);

        // Gate: assert command AND required flag.
        let gate = &detail.done_evidence[0];
        assert_eq!(gate.kind, "gate");
        assert_eq!(gate.command, "cargo test --workspace");
        assert!(gate.required, "gate.required must be true");
        assert_eq!(gate.artifacts, vec!["tests/campaign_detail.rs"]);

        // Review: assert statement.
        let review = &detail.done_evidence[1];
        assert_eq!(review.kind, "review");
        assert_eq!(review.statement, "Human reviewer signed off.");
    }

    #[tokio::test]
    async fn get_campaign_returns_not_found_for_absent_name_in_non_empty_store() {
        use foundry_sdk::campaign::{
            Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence,
        };

        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let store = CampaignStore {
            version: 1,
            campaigns: vec![Campaign {
                name: "present".to_string(),
                project: "p".to_string(),
                mission: "m".to_string(),
                intent_refs: vec![],
                context_paths: vec![],
                done_evidence: vec![DoneEvidence::Review {
                    statement: "ok".to_string(),
                }],
                budget: CampaignBudget {
                    max_cycles: 3,
                    ..Default::default()
                },
                escalation: vec![],
                status: CampaignStatus::Staged,
                cycles_completed: 0,
                cycles_landed: 0,
                authorized_by: None,
                agent_provider: None,
                last_run_event_id: None,
                owner_decisions: vec![],
                pending_run_result: None,
                objective_history: vec![],
                writable_repositories: vec![],
            }],
        };
        store.save(tmp.path()).expect("save");

        let service = test_service_with_campaigns_path(tmp.path().to_path_buf());
        let err = service
            .get_campaign(Request::new(GetCampaignRequest {
                name: "absent".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn get_campaign_returns_failed_precondition_on_malformed_store() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), b"}{bad json}").expect("write");

        let service = test_service_with_campaigns_path(tmp.path().to_path_buf());
        let err = service
            .get_campaign(Request::new(GetCampaignRequest {
                name: "any".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn get_campaign_returns_internal_on_unreadable_store() {
        let tmp = tempfile::tempdir().expect("tempdir");
        // Pass the directory path — reading a directory as a file produces an Io error.
        let service = test_service_with_campaigns_path(tmp.path().to_path_buf());
        let err = service
            .get_campaign(Request::new(GetCampaignRequest {
                name: "any".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::Internal);
    }
    // ── work-item ledger read RPCs ───────────────────────────────────────────

    /// Build a service backed by a specific work-item ledger path.
    ///
    /// Everything else is a throwaway: these tests exercise only the two read
    /// RPCs, and they must observe the ledger through the service rather than
    /// through `WorkItemStore` directly.
    fn test_service_with_work_items_path(work_items_path: std::path::PathBuf) -> FoundryService {
        let (event_tx, _rx) = broadcast::channel(64);
        let engine = Arc::new(Engine::new().with_event_broadcaster(event_tx.clone()));
        let trace_store = Arc::new(TraceStore::new(Duration::from_secs(60)));
        let workflow_tracker = Arc::new(WorkflowTracker::new());
        let tmp = tempfile::tempdir().expect("tempdir");
        let trace_writer = Arc::new(TraceWriter::new(tmp.path().to_str().unwrap()));
        let registry = Arc::new(RwLock::new(Registry {
            version: 2,
            projects: vec![],
        }));
        let tmp_registry = tempfile::NamedTempFile::new().expect("tempfile");
        let registry_path = tmp_registry.path().to_path_buf();
        let tmp_campaigns = tempfile::NamedTempFile::new().expect("tempfile");
        let campaigns_path = tmp_campaigns.path().to_path_buf();
        let sentinels = Arc::new(RwLock::new(SentinelStore::default_seed()));
        let tmp_sentinels = tempfile::NamedTempFile::new().expect("tempfile");
        let sentinels_path = tmp_sentinels.path().to_path_buf();
        let scheduler_reload = Arc::new(Notify::new());
        let ctx = RuntimeContext {
            engine,
            trace_store,
            workflow_tracker,
            trace_writer,
            event_tx,
            registry,
        };
        let stores = StoreConfig {
            campaigns_path,
            work_items_path,
            events_dir: std::path::PathBuf::new(),
            registry_path,
            sentinels,
            sentinels_path,
            scheduler_reload,
        };
        FoundryService::new(ctx, stores)
    }

    fn work_item_at(seconds: i64) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from_timestamp(seconds, 0).expect("in-range timestamp")
    }

    /// One ledger record with every field set explicitly, so a test can choose
    /// timestamps that disagree with insertion order.
    fn ledger_item(
        id: &str,
        project: &str,
        state: foundry_sdk::work_item::WorkItemState,
        submitted: i64,
        started: Option<i64>,
        settled: Option<i64>,
    ) -> foundry_sdk::work_item::WorkItem {
        foundry_sdk::work_item::WorkItem {
            id: id.to_string(),
            project: project.to_string(),
            objective: format!("objective for {id}"),
            kind: foundry_sdk::work_item::WorkItemKind::Task,
            lane: foundry_sdk::work_item::WorkLane::Interactive,
            origin: "service test".to_string(),
            source: None,
            submitted_at: work_item_at(submitted),
            started_at: started.map(work_item_at),
            settled_at: settled.map(work_item_at),
            state,
            reason: format!("reason for {id}"),
            trace_id: None,
            disposition: None,
            operator_action: None,
            resumes: None,
            depends_on: Vec::new(),
            not_before: None,
            pending_root: None,
        }
    }

    /// Write a ledger file directly, so the service is the only reader.
    fn write_ledger(path: &std::path::Path, items: Vec<foundry_sdk::work_item::WorkItem>) {
        let store = foundry_sdk::work_item::WorkItemStore {
            version: foundry_sdk::work_item::WORK_ITEM_STORE_VERSION,
            items,
        };
        std::fs::write(path, serde_json::to_string_pretty(&store).expect("serialize"))
            .expect("write");
    }

    async fn listed_ids(service: &FoundryService, project: &str, state: &str) -> Vec<String> {
        service
            .list_work_items(Request::new(ListWorkItemsRequest {
                project: project.to_string(),
                state: state.to_string(),
                source_kind: String::new(),
                source_ref: String::new(),
            }))
            .await
            .expect("list_work_items should succeed")
            .into_inner()
            .items
            .into_iter()
            .map(|item| item.id)
            .collect()
    }

    /// Eight items spanning every state, with timestamps chosen so that
    /// insertion order, `submitted_at` order and `settled_at` order all disagree:
    /// the file is written newest-submitted-first, running items were started
    /// in the reverse of their submission, and the settled items' `settled_at`
    /// order is the opposite of their `submitted_at` order.
    fn scrambled_ledger() -> Vec<foundry_sdk::work_item::WorkItem> {
        use foundry_sdk::work_item::WorkItemState as S;
        vec![
            // Insertion order deliberately mixes groups.
            ledger_item("wi_landed_old", "alpha", S::Landed, 800, Some(810), Some(820)),
            ledger_item("wi_running_late", "alpha", S::Running, 100, Some(700), None),
            ledger_item("wi_failed", "alpha", S::Failed, 700, Some(710), Some(990)),
            ledger_item("wi_queued", "alpha", S::Queued, 600, None, None),
            ledger_item("wi_cancelled", "alpha", S::Cancelled, 200, Some(210), Some(980)),
            ledger_item("wi_running_early", "alpha", S::Running, 900, Some(300), None),
            ledger_item("wi_preserved", "alpha", S::Preserved, 400, Some(410), Some(950)),
            ledger_item("wi_submitted", "alpha", S::Submitted, 50, None, None),
            ledger_item("wi_needs", "alpha", S::NeedsDecision, 300, Some(310), Some(970)),
        ]
    }

    #[tokio::test]
    async fn list_work_items_orders_by_group_then_timestamp_regardless_of_store_order() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write_ledger(tmp.path(), scrambled_ledger());
        let service = test_service_with_work_items_path(tmp.path().to_path_buf());

        assert_eq!(
            listed_ids(&service, "", "").await,
            vec![
                // running, started_at ascending (300 then 700) — note this is
                // the reverse of both insertion and submitted_at order.
                "wi_running_early",
                "wi_running_late",
                // submitted/queued, submitted_at ascending (50 then 600).
                "wi_submitted",
                "wi_queued",
                // open, settled_at descending (990, 970, 950).
                "wi_failed",
                "wi_needs",
                "wi_preserved",
                // terminal, settled_at descending (980 then 820).
                "wi_cancelled",
                "wi_landed_old",
            ],
        );
    }

    #[tokio::test]
    async fn list_work_items_breaks_equal_timestamps_by_id_ascending() {
        use foundry_sdk::work_item::WorkItemState as S;
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write_ledger(
            tmp.path(),
            vec![
                ledger_item("wi_zeta", "alpha", S::Preserved, 10, Some(20), Some(30)),
                ledger_item("wi_alpha", "alpha", S::Preserved, 11, Some(21), Some(30)),
            ],
        );
        let service = test_service_with_work_items_path(tmp.path().to_path_buf());
        assert_eq!(listed_ids(&service, "", "").await, vec!["wi_alpha", "wi_zeta"]);
    }

    #[tokio::test]
    async fn list_work_items_project_filter_matches_exactly_and_not_by_prefix() {
        use foundry_sdk::work_item::WorkItemState as S;
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write_ledger(
            tmp.path(),
            vec![
                ledger_item("wi_on_alpha", "alpha", S::Running, 10, Some(20), None),
                ledger_item("wi_on_alpha_2", "alpha-2", S::Running, 11, Some(21), None),
            ],
        );
        let service = test_service_with_work_items_path(tmp.path().to_path_buf());
        assert_eq!(listed_ids(&service, "alpha", "").await, vec!["wi_on_alpha"]);
        assert_eq!(listed_ids(&service, "alpha-2", "").await, vec!["wi_on_alpha_2"]);
    }

    #[tokio::test]
    async fn list_work_items_state_filter_preserves_the_unfiltered_relative_order() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let mut items = scrambled_ledger();
        items.push(ledger_item(
            "wi_preserved_newer",
            "alpha",
            foundry_sdk::work_item::WorkItemState::Preserved,
            500,
            Some(510),
            Some(960),
        ));
        write_ledger(tmp.path(), items);
        let service = test_service_with_work_items_path(tmp.path().to_path_buf());

        let all = listed_ids(&service, "", "").await;
        let preserved = listed_ids(&service, "", "preserved").await;
        assert_eq!(preserved, vec!["wi_preserved_newer", "wi_preserved"]);
        let relative: Vec<String> = all.into_iter().filter(|id| preserved.contains(id)).collect();
        assert_eq!(relative, preserved, "filtering must not reorder");
    }

    #[tokio::test]
    async fn list_work_items_applies_project_and_state_filters_together() {
        use foundry_sdk::work_item::WorkItemState as S;
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write_ledger(
            tmp.path(),
            vec![
                ledger_item("wi_a_failed", "alpha", S::Failed, 10, Some(20), Some(30)),
                ledger_item("wi_a_landed", "alpha", S::Landed, 11, Some(21), Some(31)),
                ledger_item("wi_b_failed", "beta", S::Failed, 12, Some(22), Some(32)),
            ],
        );
        let service = test_service_with_work_items_path(tmp.path().to_path_buf());
        assert_eq!(listed_ids(&service, "alpha", "failed").await, vec!["wi_a_failed"]);
    }

    #[tokio::test]
    async fn list_work_items_rejects_an_unknown_state_tag() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write_ledger(tmp.path(), scrambled_ledger());
        let service = test_service_with_work_items_path(tmp.path().to_path_buf());
        let err = service
            .list_work_items(Request::new(ListWorkItemsRequest {
                project: String::new(),
                state: "in_progress".to_string(),
                source_kind: String::new(),
                source_ref: String::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn a_missing_ledger_lists_nothing_and_finds_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let service = test_service_with_work_items_path(dir.path().join("absent.json"));
        assert!(listed_ids(&service, "", "").await.is_empty());
        let err = service
            .get_work_item(Request::new(GetWorkItemRequest {
                id: "wi_anything".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn an_empty_ledger_lists_nothing_and_finds_nothing() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), br#"{"version":1,"items":[]}"#).expect("write");
        let service = test_service_with_work_items_path(tmp.path().to_path_buf());
        assert!(listed_ids(&service, "", "").await.is_empty());
        let err = service
            .get_work_item(Request::new(GetWorkItemRequest {
                id: "wi_anything".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn an_absent_id_in_a_non_empty_ledger_is_not_found() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write_ledger(tmp.path(), scrambled_ledger());
        let service = test_service_with_work_items_path(tmp.path().to_path_buf());
        let err = service
            .get_work_item(Request::new(GetWorkItemRequest {
                id: "wi_not_here".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[tokio::test]
    async fn a_malformed_ledger_is_a_failed_precondition_on_both_read_rpcs() {
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        std::fs::write(tmp.path(), b"}{not json}").expect("write");
        let service = test_service_with_work_items_path(tmp.path().to_path_buf());
        let list_err = service
            .list_work_items(Request::new(ListWorkItemsRequest {
                project: String::new(),
                state: String::new(),
                source_kind: String::new(),
                source_ref: String::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(list_err.code(), tonic::Code::FailedPrecondition);
        let get_err = service
            .get_work_item(Request::new(GetWorkItemRequest {
                id: "wi_any".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(get_err.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn an_unreadable_ledger_path_is_internal_on_both_read_rpcs() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A directory exists but cannot be read as a file — an Io fault.
        let service = test_service_with_work_items_path(dir.path().to_path_buf());
        let list_err = service
            .list_work_items(Request::new(ListWorkItemsRequest {
                project: String::new(),
                state: String::new(),
                source_kind: String::new(),
                source_ref: String::new(),
            }))
            .await
            .unwrap_err();
        assert_eq!(list_err.code(), tonic::Code::Internal);
        let get_err = service
            .get_work_item(Request::new(GetWorkItemRequest {
                id: "wi_any".to_string(),
            }))
            .await
            .unwrap_err();
        assert_eq!(get_err.code(), tonic::Code::Internal);
    }

    #[tokio::test]
    async fn the_ledger_is_loaded_at_request_time_and_never_cached() {
        use foundry_sdk::work_item::WorkItemState as S;
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write_ledger(
            tmp.path(),
            vec![ledger_item(
                "wi_first",
                "alpha",
                S::Running,
                10,
                Some(20),
                None,
            )],
        );
        let service = test_service_with_work_items_path(tmp.path().to_path_buf());
        assert_eq!(listed_ids(&service, "", "").await, vec!["wi_first"]);

        // Another writer adds an item behind the service's back.
        write_ledger(
            tmp.path(),
            vec![
                ledger_item("wi_first", "alpha", S::Running, 10, Some(20), None),
                ledger_item("wi_second", "alpha", S::Running, 11, Some(21), None),
            ],
        );
        assert_eq!(
            listed_ids(&service, "", "").await,
            vec!["wi_first", "wi_second"],
            "the same instance must see the new item, proving nothing is cached"
        );
        let found = service
            .get_work_item(Request::new(GetWorkItemRequest {
                id: "wi_second".to_string(),
            }))
            .await
            .expect("get_work_item should find the newly written item");
        assert_eq!(found.into_inner().item.expect("item").id, "wi_second");
    }

    #[tokio::test]
    async fn get_work_item_reports_every_durable_field_of_a_settled_item() {
        use foundry_sdk::work_item::{WorkDisposition, WorkItemKind, WorkItemState, WorkLane};
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let settled = foundry_sdk::work_item::WorkItem {
            id: "wi_settled".to_string(),
            project: "alpha".to_string(),
            objective: "Add a --quiet flag.".to_string(),
            kind: WorkItemKind::CampaignCycle,
            lane: WorkLane::Campaign,
            origin: "campaign tidy-cli cycle 3".to_string(),
            source: None,
            submitted_at: work_item_at(1_000),
            started_at: Some(work_item_at(1_100)),
            settled_at: Some(work_item_at(1_200)),
            state: WorkItemState::Preserved,
            reason: "review found a remainder".to_string(),
            trace_id: Some("a".repeat(32)),
            operator_action: None,
            resumes: None,
            depends_on: Vec::new(),
            not_before: None,
            pending_root: None,
            disposition: Some(WorkDisposition {
                task_branch: None,
                branch_cleanup: Vec::new(),
                verdict: Some("remainder".to_string()),
                landed_commit: None,
                preservation_ref: Some("foundry/task/tidy-cli-3".to_string()),
                worktree: Some("/tmp/worktrees/tidy-cli-3".to_string()),
                worktree_removed: Some(false),
            }),
        };
        write_ledger(tmp.path(), vec![settled.clone()]);
        let service = test_service_with_work_items_path(tmp.path().to_path_buf());

        let item = service
            .get_work_item(Request::new(GetWorkItemRequest {
                id: "wi_settled".to_string(),
            }))
            .await
            .expect("get_work_item should succeed")
            .into_inner()
            .item
            .expect("item present");

        assert_eq!(item.id, settled.id);
        assert_eq!(item.project, "alpha");
        assert_eq!(item.objective, "Add a --quiet flag.");
        assert_eq!(item.kind, "campaign_cycle");
        assert_eq!(item.lane, "campaign");
        assert_eq!(item.origin, "campaign tidy-cli cycle 3");
        assert_eq!(item.submitted_at, work_item_at(1_000).to_rfc3339());
        assert_eq!(item.started_at.as_deref(), Some(work_item_at(1_100).to_rfc3339().as_str()));
        assert_eq!(item.settled_at.as_deref(), Some(work_item_at(1_200).to_rfc3339().as_str()));
        assert_eq!(item.state, "preserved");
        assert_eq!(item.reason, "review found a remainder");
        assert_eq!(item.trace_id.as_deref(), Some("a".repeat(32).as_str()));
        assert_eq!(item.verdict.as_deref(), Some("remainder"));
        assert_eq!(item.landed_commit, None);
        assert_eq!(item.preservation_ref.as_deref(), Some("foundry/task/tidy-cli-3"));
        assert_eq!(item.worktree.as_deref(), Some("/tmp/worktrees/tidy-cli-3"));
        assert_eq!(
            item.worktree_removed,
            Some(false),
            "a recorded false must be distinguishable from 'not recorded'"
        );
    }

    #[tokio::test]
    async fn get_work_item_reports_a_landed_commit_and_an_absent_preservation_ref() {
        use foundry_sdk::work_item::{WorkDisposition, WorkItemState};
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        let mut landed = ledger_item(
            "wi_landed",
            "alpha",
            WorkItemState::Landed,
            2_000,
            Some(2_100),
            Some(2_200),
        );
        landed.disposition = Some(WorkDisposition {
            task_branch: None,
            branch_cleanup: Vec::new(),
            verdict: Some("complete".to_string()),
            landed_commit: Some("deadbeef".to_string()),
            preservation_ref: None,
            worktree: Some("/tmp/worktrees/landed".to_string()),
            worktree_removed: Some(true),
        });
        write_ledger(tmp.path(), vec![landed]);
        let service = test_service_with_work_items_path(tmp.path().to_path_buf());

        let item = service
            .get_work_item(Request::new(GetWorkItemRequest {
                id: "wi_landed".to_string(),
            }))
            .await
            .expect("get_work_item should succeed")
            .into_inner()
            .item
            .expect("item present");
        assert_eq!(item.verdict.as_deref(), Some("complete"));
        assert_eq!(item.landed_commit.as_deref(), Some("deadbeef"));
        assert_eq!(item.preservation_ref, None);
        assert_eq!(item.worktree_removed, Some(true));
    }

    #[tokio::test]
    async fn get_work_item_reports_an_unsettled_item_with_absent_settlement_fields() {
        use foundry_sdk::work_item::WorkItemState;
        let tmp = tempfile::NamedTempFile::new().expect("tempfile");
        write_ledger(
            tmp.path(),
            vec![ledger_item(
                "wi_running",
                "alpha",
                WorkItemState::Running,
                3_000,
                Some(3_100),
                None,
            )],
        );
        let service = test_service_with_work_items_path(tmp.path().to_path_buf());

        let item = service
            .get_work_item(Request::new(GetWorkItemRequest {
                id: "wi_running".to_string(),
            }))
            .await
            .expect("get_work_item should succeed")
            .into_inner()
            .item
            .expect("item present");
        assert_eq!(item.state, "running");
        assert_eq!(item.started_at.as_deref(), Some(work_item_at(3_100).to_rfc3339().as_str()));
        assert_eq!(item.settled_at, None, "an unsettled item reports no settled_at");
        assert_eq!(item.trace_id, None);
        assert_eq!(item.verdict, None);
        assert_eq!(item.landed_commit, None);
        assert_eq!(item.preservation_ref, None);
        assert_eq!(item.worktree, None);
        assert_eq!(item.worktree_removed, None, "not recorded must be absent, not false");
    }
}
