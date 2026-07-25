use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};
use tokio::task::JoinHandle;

/// Snapshot of an active workflow being processed in the background.
#[derive(Debug, Clone)]
pub struct ActiveWorkflow {
    pub event_id: String,
    pub event_type: String,
    pub project: String,
    pub trace_id: String,
    pub started_at: DateTime<Utc>,
    /// Campaign this workflow serves, when its root event names one.
    ///
    /// A whole multi-cycle campaign runs inside one spawned task — the engine
    /// re-queues each `CampaignAdvanceRequested` inline rather than spawning
    /// again — so this is what lets `foundry campaign cancel --now` find the
    /// single task to abort.
    pub campaign: Option<String>,
}

struct Inner {
    active: HashMap<String, ActiveWorkflow>,
    /// Join handles for the spawned tasks, keyed by the same event id.
    ///
    /// Kept beside `active` rather than inside `ActiveWorkflow` because
    /// `JoinHandle` is not `Clone` and `list()` clones; keeping them under the
    /// same lock means there is no lock ordering to reason about.
    handles: HashMap<String, JoinHandle<()>>,
}

/// Tracks workflows that are currently being processed by background tasks.
///
/// Thread-safe via `RwLock`; callers never hold the lock across await points.
pub struct WorkflowTracker {
    inner: RwLock<Inner>,
}

impl Default for WorkflowTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkflowTracker {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(Inner {
                active: HashMap::new(),
                handles: HashMap::new(),
            }),
        }
    }

    /// Best-effort: this guard protects a pure in-memory "what's running now"
    /// cache with no cross-field invariant; recovering from poison keeps
    /// tracking workflows instead of taking down the daemon.
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Inner> {
        self.inner.write().unwrap_or_else(|e| {
            tracing::warn!("workflow tracker lock poisoned; recovering write guard");
            e.into_inner()
        })
    }

    /// Register a workflow as active.
    pub fn insert(&self, workflow: ActiveWorkflow) {
        self.write().active.insert(workflow.event_id.clone(), workflow);
    }

    /// Record the join handle of the task processing `event_id`.
    ///
    /// A no-op when the entry has already gone — a workflow can finish, and its
    /// `WorkflowGuard` remove it, before the spawner gets back here. Dropping
    /// the handle in that case detaches the (already finished) task rather than
    /// leaving an entry nothing will ever clean up.
    pub fn attach_handle(&self, event_id: &str, handle: JoinHandle<()>) {
        let mut inner = self.write();
        if inner.active.contains_key(event_id) {
            inner.handles.insert(event_id.to_string(), handle);
        }
    }

    /// Remove a workflow when processing completes (or panics).
    pub fn remove(&self, event_id: &str) -> Option<ActiveWorkflow> {
        let mut inner = self.write();
        inner.handles.remove(event_id);
        inner.active.remove(event_id)
    }

    /// Return a snapshot of all active workflows.
    pub fn list(&self) -> Vec<ActiveWorkflow> {
        // Best-effort: see `write`.
        let inner = self.inner.read().unwrap_or_else(|e| {
            tracing::warn!("workflow tracker lock poisoned; recovering read guard to list");
            e.into_inner()
        });
        inner.active.values().cloned().collect()
    }

    /// Abort every in-flight workflow serving `campaign` and wait for each to
    /// unwind. Returns what was aborted.
    ///
    /// **The await is load-bearing.** `abort()` only schedules cancellation; it
    /// does not wait for the task's future to be dropped. The aborted task is
    /// very likely holding the campaign store's advisory file lock — the
    /// formation block holds it across the whole agent call — and the caller's
    /// very next act is to take that same lock. Only awaiting the handle
    /// proves the future was dropped and therefore that `CampaignStoreGuard`'s
    /// `Drop` has released the flock. Without it the caller can block on a lock
    /// nobody has got around to releasing.
    ///
    /// Dropping a task also drops the `tokio::process::Child` it owns, and
    /// those are spawned with `kill_on_drop`, which is what actually kills the
    /// running agent.
    pub async fn abort_campaign(&self, campaign: &str) -> Vec<ActiveWorkflow> {
        let (aborted, handles) = {
            let mut inner = self.write();
            let ids: Vec<String> = inner
                .active
                .values()
                .filter(|w| w.campaign.as_deref() == Some(campaign))
                .map(|w| w.event_id.clone())
                .collect();

            let handles: Vec<JoinHandle<()>> =
                ids.iter().filter_map(|id| inner.handles.remove(id)).collect();
            let aborted: Vec<ActiveWorkflow> =
                ids.iter().filter_map(|id| inner.active.remove(id)).collect();
            (aborted, handles)
        };
        // The guard is released above, before the awaits below, and must stay
        // that way: the aborted task's `WorkflowGuard::drop` calls `remove`,
        // which needs this same lock. Rust enforces it — `RwLockWriteGuard` is
        // `!Send`, so holding one across an `.await` here will not compile —
        // but the reason is worth stating for whoever refactors this next.
        for handle in handles {
            handle.abort();
            // A task that had already finished returns `Ok(())`; one we just
            // aborted returns a cancellation error. Neither is actionable —
            // what matters is that awaiting proves it is no longer running.
            let _ = handle.await;
        }
        aborted
    }
}

/// RAII guard that removes a workflow from the tracker on drop.
///
/// Move this into a `tokio::spawn` task to guarantee cleanup even if the
/// future panics.
pub struct WorkflowGuard {
    tracker: Arc<WorkflowTracker>,
    event_id: String,
}

impl WorkflowGuard {
    pub fn new(tracker: Arc<WorkflowTracker>, event_id: String) -> Self {
        Self { tracker, event_id }
    }
}

impl Drop for WorkflowGuard {
    fn drop(&mut self) {
        self.tracker.remove(&self.event_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_workflow(id: &str) -> ActiveWorkflow {
        ActiveWorkflow {
            event_id: id.to_string(),
            event_type: "test_event".to_string(),
            project: "test-project".to_string(),
            trace_id: format!("trc_{id}"),
            started_at: Utc::now(),
            campaign: None,
        }
    }

    fn campaign_workflow(id: &str, campaign: &str) -> ActiveWorkflow {
        ActiveWorkflow {
            campaign: Some(campaign.to_string()),
            ..sample_workflow(id)
        }
    }

    /// Register a workflow whose task parks forever, so tests can abort it.
    fn spawn_parked(tracker: &Arc<WorkflowTracker>, id: &str, campaign: &str) {
        tracker.insert(campaign_workflow(id, campaign));
        let guard = WorkflowGuard::new(Arc::clone(tracker), id.to_string());
        let handle = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        tracker.attach_handle(id, handle);
    }

    #[tokio::test]
    async fn abort_campaign_stops_only_the_named_campaign() {
        let tracker = Arc::new(WorkflowTracker::new());
        spawn_parked(&tracker, "evt_1", "alpha");
        spawn_parked(&tracker, "evt_2", "beta");
        tracker.insert(sample_workflow("evt_3"));

        let aborted = tracker.abort_campaign("alpha").await;

        assert_eq!(aborted.len(), 1);
        assert_eq!(aborted[0].event_id, "evt_1");
        let remaining: Vec<String> = tracker.list().into_iter().map(|w| w.event_id).collect();
        assert!(remaining.contains(&"evt_2".to_string()));
        assert!(remaining.contains(&"evt_3".to_string()));
        assert!(!remaining.contains(&"evt_1".to_string()));
    }

    #[tokio::test]
    async fn abort_campaign_with_no_match_is_a_no_op() {
        let tracker = Arc::new(WorkflowTracker::new());
        tracker.insert(campaign_workflow("evt_1", "alpha"));

        assert!(tracker.abort_campaign("nonexistent").await.is_empty());
        assert_eq!(tracker.list().len(), 1);
    }

    /// The regression guard for the deadlock this design exists to avoid.
    ///
    /// `cancel` aborts the workflow and then immediately takes the campaign
    /// store's lock, which the aborted task was holding. If `abort_campaign`
    /// returned without awaiting the handle, the guard's `Drop` might not have
    /// run yet and the caller would block on a lock nobody is releasing. Here
    /// a `tokio::sync::Mutex` stands in for the store lock: `try_lock` must
    /// succeed the instant `abort_campaign` returns.
    #[tokio::test]
    async fn abort_campaign_waits_for_the_aborted_task_to_release_its_locks() {
        let tracker = Arc::new(WorkflowTracker::new());
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        let (holding_tx, holding_rx) = tokio::sync::oneshot::channel();

        tracker.insert(campaign_workflow("evt_1", "alpha"));
        let task_lock = Arc::clone(&lock);
        let guard = WorkflowGuard::new(Arc::clone(&tracker), "evt_1".to_string());
        let handle = tokio::spawn(async move {
            let _guard = guard;
            let _held = task_lock.lock().await;
            holding_tx.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        tracker.attach_handle("evt_1", handle);

        // Only abort once the task genuinely holds the lock.
        holding_rx.await.unwrap();
        assert!(lock.try_lock().is_err(), "precondition: the task holds the lock");

        tracker.abort_campaign("alpha").await;

        assert!(
            lock.try_lock().is_ok(),
            "abort_campaign returned before the aborted task released its lock"
        );
        assert!(tracker.list().is_empty());
    }

    #[tokio::test]
    async fn attach_handle_for_an_already_finished_workflow_leaves_nothing_stale() {
        let tracker = Arc::new(WorkflowTracker::new());
        let handle = tokio::spawn(async {});
        handle.await.unwrap();

        // The workflow was never inserted (or already removed itself).
        tracker.attach_handle("evt_gone", tokio::spawn(async {}));

        assert!(tracker.list().is_empty());
        assert!(tracker.abort_campaign("anything").await.is_empty());
    }

    #[test]
    fn insert_and_list() {
        let tracker = WorkflowTracker::new();
        tracker.insert(sample_workflow("evt_1"));
        let active = tracker.list();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].event_id, "evt_1");
    }

    #[test]
    fn remove_returns_workflow() {
        let tracker = WorkflowTracker::new();
        tracker.insert(sample_workflow("evt_1"));

        let removed = tracker.remove("evt_1");
        assert!(removed.is_some());
        assert_eq!(removed.unwrap().event_id, "evt_1");
        assert!(tracker.list().is_empty());
    }

    #[test]
    fn remove_missing_returns_none() {
        let tracker = WorkflowTracker::new();
        assert!(tracker.remove("evt_unknown").is_none());
    }

    #[test]
    fn multiple_workflows_tracked() {
        let tracker = WorkflowTracker::new();
        tracker.insert(sample_workflow("evt_1"));
        tracker.insert(sample_workflow("evt_2"));
        tracker.insert(sample_workflow("evt_3"));

        assert_eq!(tracker.list().len(), 3);

        tracker.remove("evt_2");
        let remaining = tracker.list();
        assert_eq!(remaining.len(), 2);
        assert!(remaining.iter().all(|w| w.event_id != "evt_2"));
    }

    #[test]
    fn guard_removes_on_drop() {
        let tracker = Arc::new(WorkflowTracker::new());
        tracker.insert(sample_workflow("evt_1"));

        {
            let _guard = WorkflowGuard::new(Arc::clone(&tracker), "evt_1".to_string());
            assert_eq!(tracker.list().len(), 1);
        } // guard drops here

        assert!(tracker.list().is_empty());
    }

    #[tokio::test]
    async fn concurrent_insert_and_list() {
        let tracker = Arc::new(WorkflowTracker::new());
        let mut handles = vec![];

        for i in 0..10 {
            let t = Arc::clone(&tracker);
            handles.push(tokio::spawn(async move {
                t.insert(sample_workflow(&format!("evt_{i}")));
                t.list();
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        assert_eq!(tracker.list().len(), 10);
    }
}
