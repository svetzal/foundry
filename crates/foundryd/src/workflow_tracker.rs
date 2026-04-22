use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use chrono::{DateTime, Utc};

/// Snapshot of an active workflow being processed in the background.
#[derive(Debug, Clone)]
pub struct ActiveWorkflow {
    pub event_id: String,
    pub event_type: String,
    pub project: String,
    pub trace_id: String,
    pub started_at: DateTime<Utc>,
}

/// Tracks workflows that are currently being processed by background tasks.
///
/// Thread-safe via `RwLock`; callers never hold the lock across await points.
pub struct WorkflowTracker {
    active: RwLock<HashMap<String, ActiveWorkflow>>,
}

impl WorkflowTracker {
    pub fn new() -> Self {
        Self {
            active: RwLock::new(HashMap::new()),
        }
    }

    /// Register a workflow as active.
    pub fn insert(&self, workflow: ActiveWorkflow) {
        let mut map = self.active.write().expect("workflow tracker lock poisoned");
        map.insert(workflow.event_id.clone(), workflow);
    }

    /// Remove a workflow when processing completes (or panics).
    pub fn remove(&self, event_id: &str) -> Option<ActiveWorkflow> {
        let mut map = self.active.write().expect("workflow tracker lock poisoned");
        map.remove(event_id)
    }

    /// Return a snapshot of all active workflows.
    pub fn list(&self) -> Vec<ActiveWorkflow> {
        let map = self.active.read().expect("workflow tracker lock poisoned");
        map.values().cloned().collect()
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
        }
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
