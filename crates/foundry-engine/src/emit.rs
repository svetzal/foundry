//! Event persistence and broadcast.

use std::sync::Arc;

use tokio::sync::broadcast;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::task_block::TaskBlock;
use foundry_sdk::trace::BlockExecution;

use crate::event_writer::EventWriter;

/// Handles persisting events to JSONL and broadcasting them to Watch subscribers.
///
/// Both are best-effort: write failures are logged, and a broadcast with no
/// receivers is normal. Neither ever interrupts event processing.
pub(crate) struct EventEmitter {
    writer: Option<Arc<EventWriter>>,
    tx: Option<broadcast::Sender<Event>>,
}

impl Default for EventEmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl EventEmitter {
    pub(crate) fn new() -> Self {
        Self {
            writer: None,
            tx: None,
        }
    }

    /// Attach an `EventWriter` so every persisted event is written to JSONL.
    pub(crate) fn with_writer(mut self, writer: Arc<EventWriter>) -> Self {
        self.writer = Some(writer);
        self
    }

    /// Attach a broadcast sender so events are pushed to Watch subscribers in real time.
    pub(crate) fn with_broadcaster(mut self, tx: broadcast::Sender<Event>) -> Self {
        self.tx = Some(tx);
        self
    }

    /// Persist an event to JSONL and broadcast it to Watch subscribers.
    ///
    /// Both are best-effort: write failures are logged, and a broadcast with
    /// no receivers is normal. Neither ever interrupts event processing.
    pub(crate) fn persist_one(&self, event: &Event) {
        if let Some(writer) = &self.writer
            && let Err(e) = writer.write(event)
        {
            tracing::warn!(error = %e, event_id = %event.id, "failed to write event to JSONL");
        }
        if let Some(tx) = &self.tx {
            let _ = tx.send(event.clone());
        }
    }

    /// Broadcast a transient progress message to Watch subscribers.
    ///
    /// Progress messages are intentionally not persisted to the event log and
    /// are never delivered to downstream blocks. They exist only so interactive
    /// clients can show that a long-running block is active.
    pub(crate) fn broadcast_progress(
        &self,
        current: &Event,
        event_type: &str,
        payload: serde_json::Value,
    ) {
        let Some(tx) = &self.tx else {
            return;
        };

        let progress = Event::new(
            EventType::Custom(event_type.to_string()),
            current.project.clone(),
            current.throttle,
            payload,
        )
        .with_trace_id(current.trace_id.clone())
        .with_span_ids(current.span_id.clone(), current.parent_span_id.clone());

        let _ = tx.send(progress);
    }

    pub(crate) fn broadcast_block_started(&self, block: &dyn TaskBlock, current: &Event) {
        self.broadcast_progress(
            current,
            "block_started",
            serde_json::json!({
                "block": block.name(),
                "trigger_event_id": current.id,
                "trigger_event_type": current.event_type.to_string(),
                "status": "running",
            }),
        );
    }

    pub(crate) fn broadcast_block_completed(&self, execution: &BlockExecution, current: &Event) {
        self.broadcast_progress(
            current,
            "block_completed",
            serde_json::json!({
                "block": execution.block_name,
                "duration_ms": execution.duration_ms,
                "status": if execution.success { "ok" } else { "failed" },
                "success": execution.success,
                "summary": execution.summary,
                "trigger_event_id": execution.trigger_event_id,
                "trigger_event_type": current.event_type.to_string(),
            }),
        );
    }
}
