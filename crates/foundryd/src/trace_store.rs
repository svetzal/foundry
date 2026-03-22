use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use foundry_core::trace::ProcessResult;

use crate::trace_writer::TraceWriter;

/// A stored trace entry with expiry tracking.
struct TraceEntry {
    result: ProcessResult,
    stored_at: Instant,
}

/// In-memory store for completed event chain results, queryable by root `event_id`.
///
/// When a `TraceWriter` is attached, memory-miss lookups fall back to disk.
pub struct TraceStore {
    entries: RwLock<HashMap<String, TraceEntry>>,
    ttl: Duration,
    trace_writer: Option<Arc<TraceWriter>>,
}

impl TraceStore {
    /// Create a new trace store with the given TTL for entries and no disk fallback.
    #[cfg(test)]
    pub fn new(ttl: Duration) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
            trace_writer: None,
        }
    }

    /// Create a trace store backed by a `TraceWriter` for disk fallback on
    /// memory misses.
    pub fn with_trace_writer(ttl: Duration, trace_writer: Arc<TraceWriter>) -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl,
            trace_writer: Some(trace_writer),
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

    /// Retrieve a stored trace by root `event_id`.
    ///
    /// Checks memory first.  On a miss, falls back to the attached
    /// `TraceWriter` (if any) to load from disk.
    pub fn get(&self, event_id: &str) -> Option<ProcessResult> {
        // Memory lookup
        {
            let entries = self.entries.read().expect("trace store lock poisoned");
            if let Some(entry) = entries.get(event_id) {
                if Instant::now().duration_since(entry.stored_at) < self.ttl {
                    return Some(entry.result.clone());
                }
            }
        }

        // Disk fallback
        self.trace_writer.as_ref().and_then(|tw| tw.read(event_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_core::event::{Event, EventType};
    use foundry_core::throttle::Throttle;
    use foundry_core::trace::BlockExecution;

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
                raw_output: None,
                exit_code: None,
                trigger_payload: serde_json::json!({}),
                emitted_payloads: vec![],
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
