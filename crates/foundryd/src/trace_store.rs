use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use foundry_sdk::event::Event;
use foundry_sdk::trace::{BlockExecution, ProcessResult};

use foundry_blocks::trace_writer::TraceWriter;

/// A stored trace entry with expiry tracking.
struct TraceEntry {
    result: ProcessResult,
    stored_at: Instant,
}

/// In-memory state guarded by a single lock so the primary store and the
/// secondary span indexes stay consistent with one another.
#[derive(Default)]
struct State {
    /// Primary store keyed by the root `event_id` of the chain.
    entries: HashMap<String, TraceEntry>,

    /// Map: `span_id` → `trace_id`, for resolving Span RPC requests quickly.
    span_to_trace: HashMap<String, String>,

    /// Map: `trace_id` → set of `span_id`s in that trace, for cycle-level
    /// queries.
    trace_to_spans: HashMap<String, HashSet<String>>,

    /// Map: `trace_id` → root `event_id`, so a span lookup can find the
    /// `ProcessResult` that owns it.
    trace_to_root_event: HashMap<String, String>,
}

/// Result of looking up a single span in the trace store.
#[derive(Debug, Clone)]
pub struct SpanResult {
    /// All events whose `span_id` matches the requested span.
    pub events: Vec<Event>,
    /// All block executions that either *are* the requested span
    /// (`block.span_id == requested`) or that ran *inside* it
    /// (`block.parent_span_id == requested`).
    pub blocks: Vec<BlockExecution>,
    /// Sum of `duration_ms` across `blocks`.
    pub total_duration_ms: u64,
}

/// In-memory store for completed event chain results, queryable by root `event_id`.
///
/// When a `TraceWriter` is attached, memory-miss lookups fall back to disk.
pub struct TraceStore {
    state: RwLock<State>,
    ttl: Duration,
    trace_writer: Option<Arc<TraceWriter>>,
}

impl TraceStore {
    /// Create a new trace store with the given TTL for entries and no disk fallback.
    #[cfg(test)]
    pub fn new(ttl: Duration) -> Self {
        Self {
            state: RwLock::new(State::default()),
            ttl,
            trace_writer: None,
        }
    }

    /// Create a trace store backed by a `TraceWriter` for disk fallback on
    /// memory misses.
    pub fn with_trace_writer(ttl: Duration, trace_writer: Arc<TraceWriter>) -> Self {
        Self {
            state: RwLock::new(State::default()),
            ttl,
            trace_writer: Some(trace_writer),
        }
    }

    /// Store a completed process result keyed by root `event_id`.
    pub fn insert(&self, event_id: String, result: ProcessResult) {
        let mut state = self.state.write().expect("trace store lock poisoned");

        // Evict expired entries opportunistically. When an entry is dropped
        // we also have to retract its contributions to the span indexes so
        // those don't leak memory or point at vanished traces.
        let now = Instant::now();
        let expired_ids: Vec<String> = state
            .entries
            .iter()
            .filter(|(_, entry)| now.duration_since(entry.stored_at) >= self.ttl)
            .map(|(id, _)| id.clone())
            .collect();
        for id in expired_ids {
            if let Some(entry) = state.entries.remove(&id) {
                Self::retract_indexes(&mut state, &entry.result);
            }
        }

        // If we are overwriting an existing entry for this root event_id,
        // retract its old indexes first so we don't keep stale span_ids.
        if let Some(prev) = state.entries.remove(&event_id) {
            Self::retract_indexes(&mut state, &prev.result);
        }

        // Index the new entry's spans before we move `result` into the map.
        Self::record_indexes(&mut state, &event_id, &result);

        state.entries.insert(
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
            let state = self.state.read().expect("trace store lock poisoned");
            if let Some(entry) = state.entries.get(event_id)
                && Instant::now().duration_since(entry.stored_at) < self.ttl
            {
                return Some(entry.result.clone());
            }
        }

        // Disk fallback
        self.trace_writer.as_ref().and_then(|tw| tw.read(event_id))
    }

    /// Return all events and block executions associated with `span_id`.
    ///
    /// Returns `None` when the span is unknown (either never seen or evicted).
    /// Only consults in-memory state — disk-backed traces are not searched
    /// because the on-disk format is keyed by root `event_id`, not span.
    pub fn find_span(&self, span_id: &str) -> Option<SpanResult> {
        let state = self.state.read().expect("trace store lock poisoned");
        let trace_id = state.span_to_trace.get(span_id)?;
        let root_event_id = state.trace_to_root_event.get(trace_id)?;
        let entry = state.entries.get(root_event_id)?;

        // Honour TTL on lookup, matching `get`.
        if Instant::now().duration_since(entry.stored_at) >= self.ttl {
            return None;
        }

        let events: Vec<Event> = entry
            .result
            .events
            .iter()
            .filter(|e| e.span_id.as_deref() == Some(span_id))
            .cloned()
            .collect();

        let blocks: Vec<BlockExecution> = entry
            .result
            .block_executions
            .iter()
            .filter(|b| {
                b.span_id.as_deref() == Some(span_id)
                    || b.parent_span_id.as_deref() == Some(span_id)
            })
            .cloned()
            .collect();

        let total_duration_ms = blocks.iter().map(|b| b.duration_ms).sum();

        Some(SpanResult {
            events,
            blocks,
            total_duration_ms,
        })
    }

    /// Record `(span_id, trace_id)` index entries for every event and block
    /// in `result`. Block spans inherit the `trace_id` from the trigger event
    /// (matched by `trigger_event_id`); if that event isn't present in the
    /// result the block span is skipped rather than indexed under a wrong
    /// trace.
    fn record_indexes(state: &mut State, event_id: &str, result: &ProcessResult) {
        // Build a trigger_event_id → trace_id lookup once per insert so block
        // span indexing is O(n) total instead of O(n*m).
        let event_trace_by_id: HashMap<&str, &str> = result
            .events
            .iter()
            .filter_map(|e| e.trace_id.as_deref().map(|tid| (e.id.as_str(), tid)))
            .collect();

        for event in &result.events {
            if let (Some(trace_id), Some(span_id)) =
                (event.trace_id.as_deref(), event.span_id.as_deref())
            {
                state
                    .span_to_trace
                    .entry(span_id.to_string())
                    .or_insert_with(|| trace_id.to_string());
                state
                    .trace_to_spans
                    .entry(trace_id.to_string())
                    .or_default()
                    .insert(span_id.to_string());
                state
                    .trace_to_root_event
                    .entry(trace_id.to_string())
                    .or_insert_with(|| event_id.to_string());
            }
        }

        for block in &result.block_executions {
            // The block's own span_id is its child span; its trace_id comes
            // from the trigger event.
            if let Some(span_id) = block.span_id.as_deref()
                && let Some(trace_id) =
                    event_trace_by_id.get(block.trigger_event_id.as_str()).copied()
            {
                state
                    .span_to_trace
                    .entry(span_id.to_string())
                    .or_insert_with(|| trace_id.to_string());
                state
                    .trace_to_spans
                    .entry(trace_id.to_string())
                    .or_default()
                    .insert(span_id.to_string());
                state
                    .trace_to_root_event
                    .entry(trace_id.to_string())
                    .or_insert_with(|| event_id.to_string());
            }
        }
    }

    /// Inverse of `record_indexes`: remove any span/trace entries that were
    /// contributed by `result`. Called when an entry is evicted (TTL or
    /// overwrite) so the secondary indexes don't drift away from the
    /// primary store.
    fn retract_indexes(state: &mut State, result: &ProcessResult) {
        let mut affected_traces: HashSet<String> = HashSet::new();

        let event_trace_by_id: HashMap<&str, &str> = result
            .events
            .iter()
            .filter_map(|e| e.trace_id.as_deref().map(|tid| (e.id.as_str(), tid)))
            .collect();

        for event in &result.events {
            if let (Some(trace_id), Some(span_id)) =
                (event.trace_id.as_deref(), event.span_id.as_deref())
            {
                state.span_to_trace.remove(span_id);
                if let Some(spans) = state.trace_to_spans.get_mut(trace_id) {
                    spans.remove(span_id);
                }
                affected_traces.insert(trace_id.to_string());
            }
        }

        for block in &result.block_executions {
            if let Some(span_id) = block.span_id.as_deref()
                && let Some(trace_id) =
                    event_trace_by_id.get(block.trigger_event_id.as_str()).copied()
            {
                state.span_to_trace.remove(span_id);
                if let Some(spans) = state.trace_to_spans.get_mut(trace_id) {
                    spans.remove(span_id);
                }
                affected_traces.insert(trace_id.to_string());
            }
        }

        for trace_id in affected_traces {
            let empty = state.trace_to_spans.get(&trace_id).is_some_and(HashSet::is_empty);
            if empty {
                state.trace_to_spans.remove(&trace_id);
                state.trace_to_root_event.remove(&trace_id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::trace::BlockExecution;

    fn sample_result() -> ProcessResult {
        let event = Event::new(
            EventType::GreetingRequested,
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
                audit_artifacts: vec![],
                span_id: None,
                parent_span_id: None,
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

    /// Build an event with explicit trace/span identifiers — handy for
    /// exercising the span indexes without depending on `mint_*` helpers.
    fn event_with_ids(trace_id: &str, span_id: &str) -> Event {
        Event::new(
            EventType::GreetingRequested,
            "test".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        )
        .with_trace_id(Some(trace_id.to_string()))
        .with_span_ids(Some(span_id.to_string()), None)
    }

    #[test]
    fn find_span_returns_only_matching_span_events() {
        let store = TraceStore::new(Duration::from_secs(60));
        let trace = "0123456789abcdef0123456789abcdef".to_string();
        let span_a = "aaaaaaaaaaaaaaaa".to_string();
        let span_b = "bbbbbbbbbbbbbbbb".to_string();

        let event_a = event_with_ids(&trace, &span_a);
        let event_b = event_with_ids(&trace, &span_b);
        let root_event_id = event_a.id.clone();

        let result = ProcessResult {
            events: vec![event_a, event_b],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        store.insert(root_event_id, result);

        let span_result = store.find_span(&span_a).expect("span_a must be found");
        assert_eq!(span_result.events.len(), 1);
        assert_eq!(span_result.events[0].span_id.as_deref(), Some(span_a.as_str()));
        assert!(span_result.blocks.is_empty());
        assert_eq!(span_result.total_duration_ms, 0);
    }

    #[test]
    fn find_span_returns_none_for_unknown_span() {
        let store = TraceStore::new(Duration::from_secs(60));
        assert!(store.find_span("nonexistent").is_none());
    }

    #[test]
    fn find_span_includes_block_with_matching_span_id() {
        // A block whose own `span_id` matches the query should come back,
        // and its duration should be summed into `total_duration_ms`.
        let store = TraceStore::new(Duration::from_secs(60));
        let trace = "11111111111111111111111111111111".to_string();
        let workflow_span = "1111111111111111".to_string();
        let block_span = "2222222222222222".to_string();

        let event = event_with_ids(&trace, &workflow_span);
        let trigger_event_id = event.id.clone();

        let mut block =
            BlockExecution::new("TestBlock", &trigger_event_id, 42, serde_json::json!({}));
        block.span_id = Some(block_span.clone());
        block.parent_span_id = Some(workflow_span.clone());

        let result = ProcessResult {
            events: vec![event],
            block_executions: vec![block],
            total_duration_ms: 42,
        };
        store.insert(trigger_event_id, result);

        let span_result = store
            .find_span(&block_span)
            .expect("block span must be indexed via its trigger event's trace");
        assert!(span_result.events.is_empty(), "no events carry the block's own span_id");
        assert_eq!(span_result.blocks.len(), 1);
        assert_eq!(span_result.blocks[0].span_id.as_deref(), Some(block_span.as_str()));
        assert_eq!(span_result.total_duration_ms, 42);
    }

    #[test]
    fn find_span_includes_child_blocks_via_parent_span_id() {
        // Querying for a workflow span should return events on that span
        // plus blocks whose `parent_span_id` matches (children running
        // inside the workflow span).
        let store = TraceStore::new(Duration::from_secs(60));
        let trace = "deadbeefdeadbeefdeadbeefdeadbeef".to_string();
        let workflow_span = "cafecafecafecafe".to_string();
        let child_block_span = "f00df00df00df00d".to_string();

        let event = event_with_ids(&trace, &workflow_span);
        let trigger_event_id = event.id.clone();

        let mut child =
            BlockExecution::new("ChildBlock", &trigger_event_id, 7, serde_json::json!({}));
        child.span_id = Some(child_block_span);
        child.parent_span_id = Some(workflow_span.clone());

        let result = ProcessResult {
            events: vec![event],
            block_executions: vec![child],
            total_duration_ms: 7,
        };
        store.insert(trigger_event_id, result);

        let span_result = store.find_span(&workflow_span).expect("workflow span must be indexed");
        assert_eq!(span_result.events.len(), 1);
        assert_eq!(span_result.blocks.len(), 1);
        assert_eq!(span_result.total_duration_ms, 7);
    }

    #[test]
    fn find_span_returns_none_after_ttl_expiry() {
        let store = TraceStore::new(Duration::from_millis(0));
        let trace = "33333333333333333333333333333333".to_string();
        let span = "3333333333333333".to_string();

        let event = event_with_ids(&trace, &span);
        let root_event_id = event.id.clone();

        store.insert(
            root_event_id,
            ProcessResult {
                events: vec![event],
                block_executions: vec![],
                total_duration_ms: 0,
            },
        );

        assert!(store.find_span(&span).is_none());
    }
}
