use std::path::Path;
use std::sync::{Arc, RwLock};

use tokio::sync::Notify;
use tonic::{Request, Response, Status};

use foundry_sdk::sentinel::{
    Schedule as SentinelSchedule, SentinelEntry, SentinelMutationError, SentinelStore,
};
use foundry_sdk::throttle::Throttle;

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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use tempfile::NamedTempFile;
    use tokio::sync::Notify;
    use tonic::Request;

    use foundry_sdk::sentinel::{SentinelMutationError, SentinelStore};

    use crate::proto::{SentinelDisableRequest, SentinelEnableRequest};

    use super::{disable, enable, sentinel_error_to_status, sentinel_to_proto};

    #[test]
    fn sentinel_error_to_status_not_found_maps_correctly() {
        let err = SentinelMutationError::NotFound("my-sentinel".to_string());
        let status = sentinel_error_to_status(err);
        assert_eq!(status.code(), tonic::Code::NotFound);
        assert!(status.message().contains("my-sentinel"));
    }

    #[test]
    fn sentinel_to_proto_maps_fields_from_nightly_maintenance() {
        let store = SentinelStore::default_seed();
        let entry = store.find_sentinel("nightly-maintenance").unwrap();
        let proto = sentinel_to_proto(entry);

        assert_eq!(proto.name, "nightly-maintenance");
        assert_eq!(proto.cron, "0 2 * * *");
        assert_eq!(proto.emit_event_type, "maintenance_cycle_started");
        assert_eq!(proto.emit_project, "system");
        assert_eq!(proto.emit_throttle, 0); // Full → 0
        assert!(proto.enabled);
    }

    #[test]
    fn enable_flips_disabled_sentinel_and_persists_to_file() {
        let mut store = SentinelStore::default_seed();
        store.sentinels[0].enabled = false;
        let sentinels = Arc::new(RwLock::new(store));
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let reload = Arc::new(Notify::new());

        let resp = enable(
            &sentinels,
            &path,
            &reload,
            Request::new(SentinelEnableRequest {
                name: "nightly-maintenance".to_string(),
            }),
        )
        .expect("enable should succeed");

        let proto = resp.into_inner().sentinel.expect("sentinel echoed");
        assert!(proto.enabled);
        assert_eq!(proto.name, "nightly-maintenance");

        let on_disk = SentinelStore::load(&path).expect("persisted file must be loadable");
        assert!(on_disk.sentinels[0].enabled);
    }

    #[test]
    fn disable_flips_sentinel_and_persists_to_file() {
        let sentinels = Arc::new(RwLock::new(SentinelStore::default_seed()));
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let reload = Arc::new(Notify::new());

        let resp = disable(
            &sentinels,
            &path,
            &reload,
            Request::new(SentinelDisableRequest {
                name: "nightly-maintenance".to_string(),
            }),
        )
        .expect("disable should succeed");

        let proto = resp.into_inner().sentinel.expect("sentinel echoed");
        assert!(!proto.enabled);

        let on_disk = SentinelStore::load(&path).expect("persisted file must be loadable");
        assert!(!on_disk.sentinels[0].enabled);
    }

    #[test]
    fn enable_unknown_sentinel_returns_not_found() {
        let sentinels = Arc::new(RwLock::new(SentinelStore::default_seed()));
        let tmp = NamedTempFile::new().unwrap();
        let reload = Arc::new(Notify::new());

        let err = enable(
            &sentinels,
            tmp.path(),
            &reload,
            Request::new(SentinelEnableRequest {
                name: "ghost".to_string(),
            }),
        )
        .unwrap_err();

        assert_eq!(err.code(), tonic::Code::NotFound);
    }

    #[test]
    fn disable_unknown_sentinel_returns_not_found() {
        let sentinels = Arc::new(RwLock::new(SentinelStore::default_seed()));
        let tmp = NamedTempFile::new().unwrap();
        let reload = Arc::new(Notify::new());

        let err = disable(
            &sentinels,
            tmp.path(),
            &reload,
            Request::new(SentinelDisableRequest {
                name: "ghost".to_string(),
            }),
        )
        .unwrap_err();

        assert_eq!(err.code(), tonic::Code::NotFound);
    }
}
