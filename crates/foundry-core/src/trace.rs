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
}
