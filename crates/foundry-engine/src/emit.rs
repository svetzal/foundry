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
#[derive(Clone)]
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
            // Best-effort: a send error means no Watch subscribers are
            // attached, which is the normal steady state; event processing
            // must not depend on a listener.
            if let Err(e) = tx.send(event.clone()) {
                tracing::debug!(error = %e, event_id = %event.id, "no Watch subscribers for event");
            }
        }
    }

    /// Admission requires a configured writer and a successful append before
    /// publication. Ordinary processing deliberately retains its best-effort policy.
    pub(crate) fn persist_required(&self, event: &Event) -> anyhow::Result<()> {
        let writer = self
            .writer
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("event writer is not configured"))?;
        writer.write(event)?;
        if let Some(tx) = &self.tx {
            // Best-effort: admission depends on durable evidence, not on a
            // Watch subscriber being attached.
            if let Err(error) = tx.send(event.clone()) {
                tracing::debug!(%error, event_id = %event.id, "no Watch subscribers for event");
            }
        }
        Ok(())
    }

    /// Persist and broadcast an audit observation without routing it.
    ///
    /// Block progress is durable operational evidence, but it is not a domain
    /// input and must never be delivered to downstream task blocks.
    pub(crate) fn record_progress(
        &self,
        current: &Event,
        event_type: &str,
        payload: serde_json::Value,
    ) {
        let progress = Event::new(
            EventType::Custom(event_type.to_string()),
            current.project.clone(),
            current.throttle,
            payload,
        )
        .with_trace_id(current.trace_id.clone())
        .with_span_ids(current.span_id.clone(), current.parent_span_id.clone())
        .with_causation_id(Some(current.id.clone()));

        self.persist_one(&progress);
    }

    pub(crate) fn record_block_started(&self, block: &dyn TaskBlock, current: &Event) {
        self.record_progress(
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

    pub(crate) fn record_block_completed(&self, execution: &BlockExecution, current: &Event) {
        self.record_progress(
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn required_append_failure_is_not_published_but_best_effort_is_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("not-a-directory");
        std::fs::write(&destination, "existing bytes").unwrap();
        let (tx, mut rx) = broadcast::channel(16);
        let emitter = EventEmitter::new()
            .with_writer(Arc::new(EventWriter::new(&destination)))
            .with_broadcaster(tx);
        let event = Event::new(
            EventType::WorkItemSubmitted,
            "test".to_string(),
            foundry_sdk::throttle::Throttle::Full,
            serde_json::json!({}),
        );
        assert!(emitter.persist_required(&event).is_err());
        assert!(rx.try_recv().is_err());
        emitter.persist_one(&event);
        assert_eq!(rx.try_recv().unwrap().id, event.id);
        emitter.record_progress(&event, "test_progress", serde_json::json!({}));
        assert_eq!(rx.try_recv().unwrap().causation_id, Some(event.id));
        assert_eq!(std::fs::read_to_string(destination).unwrap(), "existing bytes");
    }

    #[test]
    fn required_append_needs_a_writer_but_not_a_watch_subscriber() {
        let event = Event::new(
            EventType::WorkItemSubmitted,
            "test".to_string(),
            foundry_sdk::throttle::Throttle::Full,
            serde_json::json!({}),
        );
        assert!(EventEmitter::new().persist_required(&event).is_err());
        let dir = tempfile::tempdir().unwrap();
        EventEmitter::new()
            .with_writer(Arc::new(EventWriter::new(dir.path())))
            .persist_required(&event)
            .unwrap();
    }
}
