use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use tokio::sync::{Notify, broadcast};
use tonic::{Request, Response, Status};
use tracing::Instrument;

use foundry_sdk::event::Event;
use foundry_sdk::registry::Registry;
use foundry_sdk::sentinel::SentinelStore;

use crate::proto::{
    AddCampaignRequest, AddCampaignResponse, AdvanceCampaignRequest, AdvanceCampaignResponse,
    CompleteCampaignRequest, CompleteCampaignResponse, DecideCampaignRequest,
    DecideCampaignResponse, EmitRequest, EmitResponse, GetCampaignRequest, GetCampaignResponse,
    ListCampaignsRequest, ListCampaignsResponse, PauseCampaignRequest, PauseCampaignResponse,
    RegistryAddRequest, RegistryAddResponse, RegistryEditRequest, RegistryEditResponse,
    RegistryListRequest, RegistryListResponse, RegistryRemoveRequest, RegistryRemoveResponse,
    RegistryShowRequest, RegistryShowResponse, ResumeCampaignRequest, ResumeCampaignResponse,
    SentinelDisableRequest, SentinelDisableResponse, SentinelEnableRequest, SentinelEnableResponse,
    SpanRequest, SpanResponse, StatusRequest, StatusResponse, TraceRequest, TraceResponse,
    WatchRequest, WatchResponse, foundry_server::Foundry,
};
use crate::trace_store::TraceStore;
use crate::workflow_tracker::{ActiveWorkflow, WorkflowTracker};
use foundry_blocks::trace_writer::TraceWriter;
use foundry_engine::engine::Engine;

mod campaign_ops;
mod eventing_ops;
mod registry_ops;
mod sentinel_ops;
mod tracing_ops;

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
    pub registry_path: PathBuf,
    pub sentinels: Arc<RwLock<SentinelStore>>,
    pub sentinels_path: PathBuf,
    pub scheduler_reload: Arc<Notify>,
}

pub struct FoundryService {
    campaigns_path: PathBuf,
    ctx: RuntimeContext,
    registry_path: PathBuf,
    sentinels: Arc<RwLock<SentinelStore>>,
    sentinels_path: PathBuf,
    scheduler_reload: Arc<Notify>,
}

impl FoundryService {
    pub fn new(ctx: RuntimeContext, stores: StoreConfig) -> Self {
        Self {
            campaigns_path: stores.campaigns_path,
            ctx,
            registry_path: stores.registry_path,
            sentinels: stores.sentinels,
            sentinels_path: stores.sentinels_path,
            scheduler_reload: stores.scheduler_reload,
        }
    }
}

/// Track the event in the workflow registry and spawn `run_workflow` on the
/// tokio runtime. Used by both the gRPC `emit()` handler and the in-process
/// scheduler so every root event flows through the same trace/audit machinery.
pub(crate) fn spawn_workflow(event: Event, ctx: &RuntimeContext) {
    let event_id = event.id.clone();
    let trace_id = event.trace_id.clone().unwrap_or_default();

    ctx.workflow_tracker.insert(ActiveWorkflow {
        event_id: event_id.clone(),
        event_type: event.event_type.to_string(),
        project: event.project.clone(),
        trace_id,
        started_at: chrono::Utc::now(),
    });

    let span = tracing::info_span!(
        "process",
        event_id = %event_id,
        event_type = %event.event_type,
        project = %event.project,
    );

    tokio::spawn(
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
}

#[tonic::async_trait]
impl Foundry for FoundryService {
    async fn emit(&self, request: Request<EmitRequest>) -> Result<Response<EmitResponse>, Status> {
        eventing_ops::emit_rpc(&self.ctx, request)
    }

    async fn status(
        &self,
        request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        Ok(eventing_ops::status_rpc(&self.ctx.workflow_tracker, request))
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
        Ok(registry_ops::list(&self.ctx.registry, request))
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
        campaign_ops::add(&self.campaigns_path, &self.ctx.registry, request)
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

    async fn pause_campaign(
        &self,
        request: Request<PauseCampaignRequest>,
    ) -> Result<Response<PauseCampaignResponse>, Status> {
        campaign_ops::pause(&self.campaigns_path, request)
    }

    async fn resume_campaign(
        &self,
        request: Request<ResumeCampaignRequest>,
    ) -> Result<Response<ResumeCampaignResponse>, Status> {
        campaign_ops::resume(&self.campaigns_path, request)
    }

    async fn decide_campaign(
        &self,
        request: Request<DecideCampaignRequest>,
    ) -> Result<Response<DecideCampaignResponse>, Status> {
        campaign_ops::decide(&self.campaigns_path, request)
    }

    async fn complete_campaign(
        &self,
        request: Request<CompleteCampaignRequest>,
    ) -> Result<Response<CompleteCampaignResponse>, Status> {
        campaign_ops::complete(&self.campaigns_path, &self.ctx, request)
    }

    async fn advance_campaign(
        &self,
        request: Request<AdvanceCampaignRequest>,
    ) -> Result<Response<AdvanceCampaignResponse>, Status> {
        campaign_ops::advance(&self.campaigns_path, &self.ctx, request)
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
            budget: CampaignBudget { max_cycles: 10 },
            escalation: vec!["Escalate to team lead.".to_string()],
            status: CampaignStatus::Active,
            cycles_completed: 4,
            cycles_landed: 3,
            authorized_by: Some("bob".to_string()),
            agent_provider: Some("opus".to_string()),
            last_run_event_id: Some("evt-service-42".to_string()),
            owner_decisions: vec![],
            pending_run_result: None,
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
                budget: CampaignBudget { max_cycles: 3 },
                escalation: vec![],
                status: CampaignStatus::Staged,
                cycles_completed: 0,
                cycles_landed: 0,
                authorized_by: None,
                agent_provider: None,
                last_run_event_id: None,
                owner_decisions: vec![],
                pending_run_result: None,
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
}
