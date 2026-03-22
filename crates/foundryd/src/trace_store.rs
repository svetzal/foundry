use std::collections::HashMap;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use crate::engine::ProcessResult;

/// A stored trace entry with expiry tracking.
struct TraceEntry {
    result: ProcessResult,
    stored_at: Instant,
}

/// In-memory store for completed event chain results, queryable by root `event_id`.
pub struct TraceStore {
    entries: RwLock<HashMap<String, TraceEntry>>,
    ttl: Duration,
}

impl TraceStore {
    /// Create a new trace store with the given TTL for entries.
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
        }
    }

    /// Store a completed process result keyed by root `event_id`.
    pub fn insert(&self, event_id: String, result: ProcessResult) {
        let mut entries = self.entries.write().expect("trace store lock poisoned");
        // Evict expired entries opportunistically
        let now = Instant::now();
        entries.retain(|_, entry| now.duration_since(entry.stored_at) < self.ttl);
        entries.insert(
            event_id,
            TraceEntry {
                result,
                stored_at: now,
            },
        );
    }

    /// Retrieve a stored trace by root `event_id`, if it exists and hasn't expired.
    pub fn get(&self, event_id: &str) -> Option<ProcessResult> {
        let entries = self.entries.read().expect("trace store lock poisoned");
        entries.get(event_id).and_then(|entry| {
            if Instant::now().duration_since(entry.stored_at) < self.ttl {
                Some(entry.result.clone())
            } else {
                None
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::BlockExecution;
    use foundry_core::event::{Event, EventType};
    use foundry_core::throttle::Throttle;

    fn sample_result() -> ProcessResult {
        let event = Event::new(
            EventType::GreetRequested,
            "test".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        ProcessResult {
            events: vec![event],
            block_executions: vec![BlockExecution {
                block_name: "TestBlock".to_string(),
                trigger_event_id: "evt_abc".to_string(),
                success: true,
                summary: "did stuff".to_string(),
                emitted_event_ids: vec![],
                duration_ms: 0,
            }],
            total_duration_ms: 0,
        }
    }

    #[test]
    fn insert_and_retrieve() {
        let store = TraceStore::new(Duration::from_secs(60));
        store.insert("evt_123".to_string(), sample_result());
        let result = store.get("evt_123");
        assert!(result.is_some());
        assert_eq!(result.unwrap().events.len(), 1);
    }

    #[test]
    fn unknown_id_returns_none() {
        let store = TraceStore::new(Duration::from_secs(60));
        assert!(store.get("evt_unknown").is_none());
    }

    #[test]
    fn expired_entries_return_none() {
        let store = TraceStore::new(Duration::from_millis(0));
        store.insert("evt_old".to_string(), sample_result());
        // Entry should be expired immediately
        assert!(store.get("evt_old").is_none());
    }
}
