//! CLI handlers for the `foundry pacing` subcommands.
//!
//! `show` is a read: online it renders `GetPacing` directly, and `--offline`
//! reads the ledger, the pacing files and the registry through the same SDK
//! selection the daemon applies, so the two differ only in transport. `pause`
//! and `resume` mutate daemon-owned state through typed gRPC and have no
//! offline path. `drain` pauses every lane, then watches settlements until
//! nothing is running.

use std::collections::BTreeMap;

use anyhow::{Context as _, Result};
use foundry_sdk::pacing::{self, LaneSelection, PacingPaths, PauseState, Snapshot, SnapshotItem};
use foundry_sdk::registry::Registry;
use foundry_sdk::work_item::WorkItemStore;

use crate::daemon::{connect_daemon_online, connect_daemon_required, status_to_anyhow};
use crate::proto::{
    GetPacingRequest, PacingItem, PacingStatus, PausePacingRequest, ResumePacingRequest,
    WatchRequest, foundry_client::FoundryClient,
};
use crate::render;

/// Show the stage: online through `GetPacing`, offline from the files.
pub async fn show(addr: &str, offline: bool, json: bool) -> Result<()> {
    let status = if offline {
        load_offline(
            &foundry_sdk::paths::work_items_path(),
            &PacingPaths::from_env(),
            &foundry_sdk::paths::registry_path(),
        )?
    } else {
        fetch_online(addr).await?
    };
    if json {
        print!("{}", render::pacing::show_json(&status));
    } else {
        print!("{}", render::pacing::show(&status));
    }
    Ok(())
}

async fn fetch_online(addr: &str) -> Result<PacingStatus> {
    let mut client = connect_daemon_required(addr, "foundry pacing show --offline").await?;
    client
        .get_pacing(GetPacingRequest {})
        .await
        .map_err(status_to_anyhow)?
        .into_inner()
        .pacing
        .context("daemon returned no pacing status")
}

/// Read the stage straight from the client-side files, through the same
/// selection the daemon applies.
pub(crate) fn load_offline(
    work_items_path: &std::path::Path,
    pacing: &PacingPaths,
    registry_path: &std::path::Path,
) -> Result<PacingStatus> {
    let store = WorkItemStore::load(work_items_path).with_context(|| {
        format!("could not read the work-item ledger at {}", work_items_path.display())
    })?;
    let limits = pacing::Limits::load(&pacing.limits).with_context(|| {
        format!("could not read the pacing limits at {}", pacing.limits.display())
    })?;
    let pauses = PauseState::load(&pacing.state).with_context(|| {
        format!("could not read the pacing state at {}", pacing.state.display())
    })?;
    let registry = match Registry::load(registry_path) {
        Ok(registry) => registry,
        Err(foundry_sdk::error::StoreError::NotFound { .. }) => Registry {
            version: 2,
            projects: vec![],
        },
        Err(error) => {
            return Err(error).with_context(|| {
                format!("could not read the registry at {}", registry_path.display())
            });
        }
    };
    let repository_of = |project: &str| pacing::repository_key(&registry, project);
    Ok(snapshot_to_proto(&pacing::snapshot(
        &store.items,
        &limits,
        &pauses,
        &repository_of,
    )))
}

/// Wire form of one snapshot item, matching what the daemon would have sent.
fn item_to_proto(item: &SnapshotItem) -> PacingItem {
    PacingItem {
        id: item.id.clone(),
        project: item.project.clone(),
        repository: item.repository.clone(),
        kind: item.kind.tag().to_string(),
        lane: item.lane.tag().to_string(),
        state: item.state.tag().to_string(),
        reason: item.reason.clone(),
        since: item.since.map(|at| at.to_rfc3339()).unwrap_or_default(),
    }
}

/// Wire form of the stage, matching what the daemon would have sent.
fn snapshot_to_proto(snapshot: &Snapshot) -> PacingStatus {
    PacingStatus {
        max_running: snapshot.max_running as u64,
        running: snapshot.running.len() as u64,
        paused_lanes: snapshot.paused.iter().map(|lane| lane.tag().to_string()).collect(),
        running_items: snapshot.running.iter().map(item_to_proto).collect(),
        waiting_items: snapshot.waiting.iter().map(item_to_proto).collect(),
    }
}

/// The lanes a `--lane` value names, validated here so a typo is refused
/// before the daemon is asked.
fn lanes_for(lane: &str) -> Result<Vec<String>> {
    let selection = LaneSelection::parse(lane.trim()).with_context(|| {
        format!("--lane takes interactive, campaign, maintenance or all; got '{lane}'")
    })?;
    Ok(selection.lanes().iter().map(|lane| lane.tag().to_string()).collect())
}

/// Pause new starts in `lane`.
pub async fn pause(addr: &str, offline: bool, lane: &str, origin: Option<&str>) -> Result<()> {
    anyhow::ensure!(!offline, "pacing pause requires foundryd; --offline is not supported");
    let lanes = lanes_for(lane)?;
    let mut client = connect_daemon_online(addr).await?;
    let status = pause_lanes(&mut client, lanes, origin).await?;
    print!("{}", render::pacing::lanes_notice("paused", &status));
    Ok(())
}

async fn pause_lanes(
    client: &mut FoundryClient<tonic::transport::Channel>,
    lanes: Vec<String>,
    origin: Option<&str>,
) -> Result<PacingStatus> {
    client
        .pause_pacing(PausePacingRequest {
            lanes,
            operator_origin: crate::origin::local_operator_origin(origin),
        })
        .await
        .map_err(status_to_anyhow)?
        .into_inner()
        .pacing
        .context("daemon returned no pacing status")
}

/// Allow new starts in `lane` again.
pub async fn resume(addr: &str, offline: bool, lane: &str, origin: Option<&str>) -> Result<()> {
    anyhow::ensure!(!offline, "pacing resume requires foundryd; --offline is not supported");
    let lanes = lanes_for(lane)?;
    let mut client = connect_daemon_online(addr).await?;
    let status = client
        .resume_pacing(ResumePacingRequest {
            lanes,
            operator_origin: crate::origin::local_operator_origin(origin),
        })
        .await
        .map_err(status_to_anyhow)?
        .into_inner()
        .pacing
        .context("daemon returned no pacing status")?;
    print!("{}", render::pacing::lanes_notice("resumed", &status));
    Ok(())
}

/// What `drain` still waits on: the running items by id, kept current from
/// the settlement events the watch stream delivers.
#[derive(Debug, Default)]
pub(crate) struct Drain {
    running: BTreeMap<String, PacingItem>,
}

impl Drain {
    /// Start from the daemon's running list.
    pub(crate) fn new(running: &[PacingItem]) -> Self {
        Self {
            running: running.iter().map(|item| (item.id.clone(), item.clone())).collect(),
        }
    }

    /// Whether nothing is running any more.
    pub(crate) fn is_idle(&self) -> bool {
        self.running.is_empty()
    }

    /// The items still running, in id order.
    pub(crate) fn remaining(&self) -> Vec<PacingItem> {
        self.running.values().cloned().collect()
    }

    /// Note one watched event. A `work_item_settled` or `work_item_cancelled`
    /// for an item still running takes it off the list and returns the line
    /// to print; anything else returns `None`.
    pub(crate) fn observe(&mut self, event_type: &str, payload_json: &str) -> Option<String> {
        if event_type != "work_item_settled" && event_type != "work_item_cancelled" {
            return None;
        }
        let payload: serde_json::Value = serde_json::from_str(payload_json).ok()?;
        let id = payload.get("item_id")?.as_str()?;
        self.running.remove(id)?;
        let state = payload.get("state").and_then(serde_json::Value::as_str).unwrap_or("settled");
        let reason = payload.get("reason").and_then(serde_json::Value::as_str).unwrap_or("");
        Some(render::pacing::drained_line(id, state, reason))
    }

    /// A running item the stream never settles is checked against a fresh
    /// `GetPacing`: anything no longer running there is gone.
    pub(crate) fn reconcile(&mut self, running: &[PacingItem]) {
        let still: Vec<String> = running.iter().map(|item| item.id.clone()).collect();
        self.running.retain(|id, _| still.contains(id));
    }
}

/// Pause every lane, then wait until nothing is running, printing each item
/// as it settles. Exits 0 once idle; with `--timeout`, exits non-zero naming
/// what still runs.
pub async fn drain(
    addr: &str,
    offline: bool,
    timeout: Option<&str>,
    origin: Option<&str>,
) -> Result<()> {
    anyhow::ensure!(!offline, "pacing drain requires foundryd; --offline is not supported");
    let deadline = timeout
        .map(|text| {
            let now = chrono::Utc::now();
            let at = crate::commands::parse_not_before(text, now)?;
            (at - now).to_std().context("--timeout must be a duration ahead, such as 30m")
        })
        .transpose()?
        .map(|wait| tokio::time::Instant::now() + wait);

    // Subscribe before pausing so no settlement slips between the list and
    // the stream.
    let mut watch_client = connect_daemon_online(addr).await?;
    let mut stream = watch_client
        .watch(WatchRequest {
            project: String::new(),
        })
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    let mut client = connect_daemon_online(addr).await?;
    let status = pause_lanes(&mut client, lanes_for("all")?, origin).await?;
    print!("{}", render::pacing::lanes_notice("paused every lane", &status));
    let mut drain = Drain::new(&status.running_items);
    if drain.is_idle() {
        println!("idle: nothing is running");
        return Ok(());
    }
    println!("waiting for {} running item(s) to settle", drain.remaining().len());

    while !drain.is_idle() {
        let next = stream.message();
        let message = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, next).await {
                Ok(message) => message,
                Err(_elapsed) => {
                    let fresh = fetch_online(addr).await?;
                    drain.reconcile(&fresh.running_items);
                    if drain.is_idle() {
                        break;
                    }
                    print!("{}", render::pacing::still_running(&drain.remaining()));
                    std::process::exit(1);
                }
            },
            None => next.await,
        };
        let Some(event) = message.map_err(status_to_anyhow)? else {
            // The stream closed under us; the daemon is the authority on
            // what still runs.
            let fresh = fetch_online(addr).await?;
            drain.reconcile(&fresh.running_items);
            if !drain.is_idle() {
                anyhow::bail!("the watch stream closed with items still running");
            }
            break;
        };
        if let Some(line) = drain.observe(&event.event_type, &event.payload_json) {
            print!("{line}");
        }
    }
    println!("idle: nothing is running");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(id: &str) -> PacingItem {
        PacingItem {
            id: id.to_string(),
            project: "alpha".to_string(),
            repository: "alpha".to_string(),
            kind: "task".to_string(),
            lane: "interactive".to_string(),
            state: "running".to_string(),
            reason: "running".to_string(),
            since: String::new(),
        }
    }

    #[test]
    fn drain_takes_items_off_the_list_as_their_settlements_arrive() {
        let mut drain = Drain::new(&[running("wi_a"), running("wi_b")]);
        assert!(!drain.is_idle());
        assert!(drain.observe("task_run_completed", "{}").is_none(), "not a settlement");
        assert!(
            drain
                .observe("work_item_settled", r#"{"item_id":"wi_other","state":"landed"}"#)
                .is_none(),
            "a settlement of something not running prints nothing"
        );
        let line = drain
            .observe("work_item_settled", r#"{"item_id":"wi_a","state":"landed","reason":"done"}"#)
            .expect("wi_a settled");
        assert_eq!(line, "settled wi_a: landed — done\n");
        assert_eq!(drain.remaining().len(), 1);
        let line = drain
            .observe(
                "work_item_cancelled",
                r#"{"item_id":"wi_b","state":"cancelled","reason":"stopped"}"#,
            )
            .expect("wi_b cancelled");
        assert_eq!(line, "settled wi_b: cancelled — stopped\n");
        assert!(drain.is_idle());
    }

    #[test]
    fn drain_reconciles_against_a_fresh_running_list() {
        let mut drain = Drain::new(&[running("wi_a"), running("wi_b")]);
        drain.reconcile(&[running("wi_b")]);
        assert_eq!(
            drain.remaining().iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            vec!["wi_b"]
        );
    }

    #[test]
    fn a_lane_value_is_validated_before_the_daemon_is_asked() {
        assert_eq!(lanes_for("all").unwrap(), vec!["interactive", "campaign", "maintenance"]);
        assert_eq!(lanes_for("campaign").unwrap(), vec!["campaign"]);
        let err = lanes_for("nightly").unwrap_err();
        assert!(err.to_string().contains("--lane takes"), "{err}");
    }

    #[test]
    fn offline_show_reads_the_files_and_keys_repositories_from_the_registry() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = dir.path().join("work-items.json");
        let pacing = PacingPaths {
            limits: dir.path().join("pacing.json"),
            state: dir.path().join("pacing-state.json"),
        };
        std::fs::write(&pacing.limits, r#"{"max_running": 3}"#).unwrap();
        let mut pauses = PauseState::default();
        pauses.pause(&[foundry_sdk::work_item::WorkLane::Campaign]);
        pauses.save(&pacing.state).unwrap();
        let root = foundry_sdk::event::Event::new(
            foundry_sdk::event::EventType::ExecutionRequested,
            "alpha".to_string(),
            foundry_sdk::throttle::Throttle::Full,
            serde_json::json!({"project": "alpha", "workflow": "task", "prompt": "x"}),
        );
        let mut queued = foundry_sdk::work_item::WorkItem::queued(
            foundry_sdk::work_item::WorkItemSpec {
                project: "alpha".to_string(),
                objective: "x".to_string(),
                kind: foundry_sdk::work_item::WorkItemKind::Task,
                lane: foundry_sdk::work_item::WorkLane::Interactive,
                origin: "test".to_string(),
                trace_id: None,
            },
            root,
            chrono::Utc::now(),
        );
        queued.reason = "ready".to_string();
        let mut running = queued.clone();
        running.id = "wi_running".to_string();
        running.start(chrono::Utc::now());
        WorkItemStore {
            version: 1,
            items: vec![queued.clone(), running],
        }
        .save(&ledger)
        .unwrap();

        let status =
            load_offline(&ledger, &pacing, &dir.path().join("absent-registry.json")).unwrap();
        assert_eq!(status.max_running, 3);
        assert_eq!(status.running, 1);
        assert_eq!(status.paused_lanes, vec!["campaign".to_string()]);
        assert_eq!(status.running_items[0].id, "wi_running");
        assert_eq!(status.running_items[0].repository, "alpha", "no registry: the project name");
        assert_eq!(status.waiting_items[0].id, queued.id);
        assert_eq!(status.waiting_items[0].reason, "ready");
    }
}
