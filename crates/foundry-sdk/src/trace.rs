use serde::{Deserialize, Serialize};

use crate::event::{Event, EventType};

/// Lightweight summary of a stored trace, suitable for listing without loading
/// the full `raw_output` content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceIndex {
    pub event_id: String,
    pub event_type: String,
    pub project: String,
    pub success: bool,
    pub total_duration_ms: u64,
    #[serde(default)]
    pub trace_id: Option<String>,
}

/// Record of a single block execution within a processing chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlockExecution {
    /// Name of the block that ran.
    pub block_name: String,
    /// The `event_id` that triggered this block.
    pub trigger_event_id: String,
    /// Whether the block succeeded.
    pub success: bool,
    /// Human-readable summary from the block.
    pub summary: String,
    /// Event IDs emitted by this block (empty if suppressed or failed).
    pub emitted_event_ids: Vec<String>,
    /// Wall-clock milliseconds spent executing this block (including retries).
    pub duration_ms: u64,
    /// Combined stdout+stderr from any shell command run by this block.
    pub raw_output: Option<String>,
    /// Exit code from any shell command run by this block.
    pub exit_code: Option<i32>,
    /// The payload of the event that triggered this block.
    pub trigger_payload: serde_json::Value,
    /// The payloads of events emitted by this block.
    pub emitted_payloads: Vec<serde_json::Value>,
    /// Paths to audit artifacts produced by this block.
    #[serde(default)]
    pub audit_artifacts: Vec<String>,

    /// This block's own `span_id`. The block's span is a child of the
    /// workflow span this block runs inside. Events emitted by the
    /// block do **not** carry this `span_id` — they carry the workflow's
    /// `span_id` under the default propagation rule. Use
    /// `emitted_event_ids` to find what this block produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span_id: Option<String>,

    /// The workflow span this block executes inside (= the trigger event's
    /// `span_id`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,
}

impl BlockExecution {
    pub fn new(
        block_name: &str,
        trigger_event_id: &str,
        duration_ms: u64,
        trigger_payload: serde_json::Value,
    ) -> Self {
        Self {
            block_name: block_name.to_string(),
            trigger_event_id: trigger_event_id.to_string(),
            success: false,
            summary: String::new(),
            emitted_event_ids: vec![],
            duration_ms,
            raw_output: None,
            exit_code: None,
            trigger_payload,
            emitted_payloads: vec![],
            audit_artifacts: vec![],
            span_id: None,
            parent_span_id: None,
        }
    }
}

/// The full result of processing an event chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessResult {
    /// All events produced during the chain (including the root).
    pub events: Vec<Event>,
    /// Record of each block execution in order.
    pub block_executions: Vec<BlockExecution>,
    /// Wall-clock milliseconds for the entire `process()` call.
    pub total_duration_ms: u64,
}

/// Terminal completion event types whose `success` payload field determines
/// overall chain outcome.
const TERMINAL_EVENT_TYPES: &[EventType] = &[
    EventType::ProjectIterationCompleted,
    EventType::ProjectMaintenanceCompleted,
    EventType::InnerIterationCompleted,
];

impl ProcessResult {
    /// Iterate events of a given type, parsing each into payload `T`.
    ///
    /// Events whose payload fails to parse are silently skipped, matching
    /// the best-effort decode used by trace consumers.
    pub fn parsed_events_of<T: serde::de::DeserializeOwned>(
        &self,
        event_type: EventType,
    ) -> impl Iterator<Item = T> + '_ {
        let log_event_type = event_type.clone();
        self.events
            .iter()
            .filter(move |e| e.event_type == event_type)
            .filter_map(move |e| match e.parse_payload::<T>() {
                Ok(v) => Some(v),
                Err(err) => {
                    // Best-effort: a payload that doesn't match T is skipped rather than
                    // failing the whole trace scan, matching the best-effort decode used
                    // by trace consumers; log so unexpected payload-shape drift is visible.
                    tracing::debug!(
                        event_type = ?log_event_type,
                        error = %err,
                        "skipping event with unparseable payload in parsed_events_of"
                    );
                    None
                }
            })
    }

    /// Determine overall success of the processing chain.
    ///
    /// When the chain contains terminal completion events (e.g.
    /// `ProjectIterationCompleted`), their `success` payload field is
    /// authoritative — intermediate retry failures are irrelevant.
    /// Falls back to checking all block executions when no terminal event
    /// exists.
    pub fn is_success(&self) -> bool {
        let terminal: Vec<&Event> = self
            .events
            .iter()
            .filter(|e| TERMINAL_EVENT_TYPES.contains(&e.event_type))
            .collect();

        if terminal.is_empty() {
            self.block_executions.iter().all(|b| b.success)
        } else {
            terminal.iter().all(|e| e.payload_bool_or("success", false))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::ReleaseCompletedPayload;
    use crate::throttle::Throttle;

    fn block(name: &str, success: bool) -> BlockExecution {
        BlockExecution {
            block_name: name.to_string(),
            trigger_event_id: "trigger".to_string(),
            success,
            summary: String::new(),
            emitted_event_ids: vec![],
            duration_ms: 10,
            raw_output: None,
            exit_code: None,
            trigger_payload: serde_json::json!({}),
            emitted_payloads: vec![],
            audit_artifacts: vec![],
            span_id: None,
            parent_span_id: None,
        }
    }

    fn completion_event(event_type: EventType, success: bool) -> Event {
        Event::new(
            event_type,
            "test".to_string(),
            Throttle::Full,
            serde_json::json!({"success": success}),
        )
    }

    #[test]
    fn all_blocks_succeed_no_terminal_events() {
        let result = ProcessResult {
            events: vec![],
            block_executions: vec![block("A", true), block("B", true)],
            total_duration_ms: 100,
        };
        assert!(result.is_success());
    }

    #[test]
    fn failed_block_no_terminal_events_is_failure() {
        let result = ProcessResult {
            events: vec![],
            block_executions: vec![block("A", true), block("B", false)],
            total_duration_ms: 100,
        };
        assert!(!result.is_success());
    }

    #[test]
    fn terminal_success_overrides_intermediate_block_failures() {
        let result = ProcessResult {
            events: vec![completion_event(EventType::ProjectIterationCompleted, true)],
            block_executions: vec![
                block("RunVerifyGates", false),
                block("RouteGateResult", false),
                block("RetryExecution", true),
                block("RunVerifyGates", true),
                block("RouteGateResult", true),
            ],
            total_duration_ms: 100,
        };
        assert!(result.is_success());
    }

    #[test]
    fn terminal_failure_reports_failure() {
        let result = ProcessResult {
            events: vec![completion_event(
                EventType::ProjectMaintenanceCompleted,
                false,
            )],
            block_executions: vec![block("RouteGateResult", false)],
            total_duration_ms: 100,
        };
        assert!(!result.is_success());
    }

    #[test]
    fn mixed_terminal_events_all_must_succeed() {
        let result = ProcessResult {
            events: vec![
                completion_event(EventType::ProjectIterationCompleted, true),
                completion_event(EventType::ProjectMaintenanceCompleted, false),
            ],
            block_executions: vec![],
            total_duration_ms: 100,
        };
        assert!(!result.is_success());
    }

    #[test]
    fn inner_iteration_completed_is_terminal() {
        let result = ProcessResult {
            events: vec![completion_event(EventType::InnerIterationCompleted, true)],
            block_executions: vec![block("RouteGateResult", false)],
            total_duration_ms: 100,
        };
        assert!(result.is_success());
    }

    #[test]
    fn empty_block_executions_is_success() {
        let result = ProcessResult {
            events: vec![],
            block_executions: vec![],
            total_duration_ms: 0,
        };
        assert!(result.is_success());
    }

    #[test]
    fn block_execution_span_fields_round_trip() {
        let mut b = BlockExecution::new("X", "evt_abc", 10, serde_json::json!({}));
        b.span_id = Some("0123456789abcdef".to_string());
        b.parent_span_id = Some("fedcba9876543210".to_string());

        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(json["span_id"], "0123456789abcdef");
        assert_eq!(json["parent_span_id"], "fedcba9876543210");

        let restored: BlockExecution = serde_json::from_value(json).unwrap();
        assert_eq!(restored.span_id.as_deref(), Some("0123456789abcdef"));
        assert_eq!(restored.parent_span_id.as_deref(), Some("fedcba9876543210"));
    }

    #[test]
    fn parsed_events_of_yields_matching_payloads_and_skips_others() {
        let release1 = Event::new(
            EventType::ReleaseCompleted,
            "proj".to_string(),
            Throttle::Full,
            serde_json::json!({"success": true, "new_tag": "v1.0.0"}),
        );
        let release2 = Event::new(
            EventType::ReleaseCompleted,
            "proj".to_string(),
            Throttle::Full,
            serde_json::json!({"success": false, "new_tag": "v1.0.1"}),
        );
        let other = Event::new(
            EventType::ProjectRunStarted,
            "proj".to_string(),
            Throttle::Full,
            serde_json::json!({}),
        );
        let result = ProcessResult {
            events: vec![release1, other, release2],
            block_executions: vec![],
            total_duration_ms: 0,
        };

        let payloads: Vec<ReleaseCompletedPayload> =
            result.parsed_events_of(EventType::ReleaseCompleted).collect();
        assert_eq!(payloads.len(), 2);
        assert!(payloads[0].success);
        assert_eq!(payloads[0].new_tag, Some("v1.0.0".to_string()));
        assert!(!payloads[1].success);
    }

    #[test]
    fn block_execution_span_fields_deserialize_default_none() {
        let json = serde_json::json!({
            "block_name": "X",
            "trigger_event_id": "evt_abc",
            "success": true,
            "summary": "",
            "emitted_event_ids": [],
            "duration_ms": 0,
            "trigger_payload": {},
            "emitted_payloads": []
        });
        let b: BlockExecution = serde_json::from_value(json).unwrap();
        assert!(
            b.span_id.is_none(),
            "span_id missing from on-disk record must deserialize as None"
        );
        assert!(b.parent_span_id.is_none());
    }
}
