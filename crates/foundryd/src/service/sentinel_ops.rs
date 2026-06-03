use std::path::Path;
use std::sync::{Arc, RwLock};

use tokio::sync::Notify;
use tonic::{Request, Response, Status};

use foundry_core::sentinel::{
    Schedule as SentinelSchedule, SentinelEntry, SentinelMutationError, SentinelStore,
};
use foundry_core::throttle::Throttle;

use crate::proto::{
    Sentinel as ProtoSentinel, SentinelDisableRequest, SentinelDisableResponse,
    SentinelEnableRequest, SentinelEnableResponse,
};

fn sentinel_error_to_status(err: SentinelMutationError) -> Status {
    match err {
        SentinelMutationError::NotFound(name) => {
            Status::not_found(format!("sentinel '{name}' not found"))
        }
    }
}

pub(super) fn sentinel_to_proto(entry: &SentinelEntry) -> ProtoSentinel {
    let SentinelSchedule::Cron(cron) = &entry.schedule;
    ProtoSentinel {
        name: entry.name.clone(),
        cron: cron.clone(),
        emit_event_type: entry.emit.event_type.as_str(),
        emit_project: entry.emit.project.clone(),
        emit_throttle: match entry.emit.throttle {
            Throttle::Full => 0,
            Throttle::DryRun => 1,
        },
        emit_payload_json: entry.emit.payload.to_string(),
        enabled: entry.enabled,
    }
}

pub(super) fn enable(
    sentinels: &Arc<RwLock<SentinelStore>>,
    sentinels_path: &Path,
    scheduler_reload: &Arc<Notify>,
    request: Request<SentinelEnableRequest>,
) -> Result<Response<SentinelEnableResponse>, Status> {
    let req = request.into_inner();

    let entry_proto = {
        let mut store = sentinels.write().expect("sentinel store lock poisoned");
        let entry = store.enable(&req.name).map_err(sentinel_error_to_status)?;
        let proto = sentinel_to_proto(entry);
        store
            .save(sentinels_path)
            .map_err(|e| Status::internal(format!("failed to save sentinels: {e}")))?;
        proto
    };

    // Wake the scheduler so the new state is armed immediately.
    scheduler_reload.notify_one();

    tracing::info!(sentinel = %req.name, "sentinel_enable: sentinel enabled");

    Ok(Response::new(SentinelEnableResponse {
        sentinel: Some(entry_proto),
    }))
}

pub(super) fn disable(
    sentinels: &Arc<RwLock<SentinelStore>>,
    sentinels_path: &Path,
    scheduler_reload: &Arc<Notify>,
    request: Request<SentinelDisableRequest>,
) -> Result<Response<SentinelDisableResponse>, Status> {
    let req = request.into_inner();

    let entry_proto = {
        let mut store = sentinels.write().expect("sentinel store lock poisoned");
        let entry = store.disable(&req.name).map_err(sentinel_error_to_status)?;
        let proto = sentinel_to_proto(entry);
        store
            .save(sentinels_path)
            .map_err(|e| Status::internal(format!("failed to save sentinels: {e}")))?;
        proto
    };

    // Wake the scheduler so any pending firing is cancelled.
    scheduler_reload.notify_one();

    tracing::info!(sentinel = %req.name, "sentinel_disable: sentinel disabled");

    Ok(Response::new(SentinelDisableResponse {
        sentinel: Some(entry_proto),
    }))
}
