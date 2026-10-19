use serde::{Deserialize, Serialize};

use super::context::{ChainContext, LoopContext};
use crate::gateway::AgentFailureMetadata;

// ---------------------------------------------------------------------------
// Gate orchestration workflow
// ---------------------------------------------------------------------------

/// Payload for `GateResolutionCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateResolutionCompletedPayload {
    pub project: String,
    pub workflow: String,
    pub gates: serde_json::Value,
    #[serde(flatten)]
    pub chain: ChainContext,
}

/// Payload for `PreflightCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightCompletedPayload {
    pub project: String,
    pub workflow: String,
    pub all_passed: bool,
    pub required_passed: bool,
    pub results: Vec<crate::gates::GateResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped: Option<bool>,
    #[serde(flatten)]
    pub chain: ChainContext,
}

/// Payload for `ExecutionCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionCompletedPayload {
    pub project: String,
    pub workflow: String,
    pub success: bool,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_output: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dry_run: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_count: Option<u64>,
    /// Whether the working tree had uncommitted changes after agent execution.
    /// `None` for events recorded before this field was added.
    /// Entries in `files_changed` are raw `git status --porcelain` paths;
    /// rename entries appear as `"old -> new"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes_detected: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files_changed: Vec<String>,
    #[serde(default, flatten)]
    pub failure: AgentFailureMetadata,
    /// Set when the agent session itself failed (the provider reported an
    /// error, or the agent could not be started or timed out), as opposed to
    /// a session that finished and was then failed by Foundry's own checks,
    /// such as iterate's silent no-op. Absent on the wire otherwise, and on
    /// events recorded before this field was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_failure: Option<ExecutionSessionFailure>,
    #[serde(flatten)]
    pub context: LoopContext,
}

/// An execution agent session that ended in failure rather than finishing.
///
/// Carried from `ExecutionCompleted` to `GateVerificationCompleted`, so a
/// reviewer downstream can tell a session that never did the work from one
/// that did it badly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionSessionFailure {
    /// The session's failure, as the execution summary states it: the
    /// provider's message (`plan execution failed: <message>`), a classified
    /// provider failure (`provider unavailable: <message>`), or why the agent
    /// could not run (`agent unavailable: <error>`).
    pub message: String,
    /// Whether the session left changes outside Foundry's scratch
    /// (`.foundry/`) and auxiliary worktree paths. A session that changed
    /// project files is reviewed as usual, even though it failed.
    pub left_work: bool,
}

/// Payload for `GateVerificationCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateVerificationCompletedPayload {
    pub project: String,
    pub workflow: String,
    pub all_passed: bool,
    pub required_passed: bool,
    pub results: Vec<crate::gates::GateResult>,
    pub retry_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution_output: Option<String>,
    #[serde(default, flatten)]
    pub failure: AgentFailureMetadata,
    /// The execution session's failure, forwarded from `ExecutionCompleted`.
    /// `None` when the session finished, including when Foundry failed it
    /// afterwards (iterate's silent no-op), and on events recorded before
    /// this field was added.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_failure: Option<ExecutionSessionFailure>,
    #[serde(flatten)]
    pub context: LoopContext,
}

/// Payload for `RetryRequested`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryRequestedPayload {
    pub project: String,
    pub workflow: String,
    pub retry_count: u64,
    pub failure_context: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prior_execution_output: Option<String>,
    #[serde(flatten)]
    pub context: LoopContext,
}

/// Payload for `SummarizeCompleted`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SummarizeCompletedPayload {
    pub project: String,
    pub headline: String,
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::{
        ExecutionCompletedPayload, ExecutionSessionFailure, GateVerificationCompletedPayload,
    };

    /// Events recorded before `session_failure` existed still read, as
    /// sessions that did not fail.
    #[test]
    fn events_without_a_session_failure_still_deserialize() {
        let executed: ExecutionCompletedPayload = serde_json::from_value(serde_json::json!({
            "project": "p", "workflow": "task", "success": false,
            "summary": "plan execution failed: boom", "changes_detected": false
        }))
        .unwrap();
        assert_eq!(executed.session_failure, None);
        let verified: GateVerificationCompletedPayload =
            serde_json::from_value(serde_json::json!({
                "project": "p", "workflow": "task", "all_passed": false, "required_passed": false,
                "results": [], "retry_count": 0
            }))
            .unwrap();
        assert_eq!(verified.session_failure, None);
        assert!(
            serde_json::to_value(&verified).unwrap().get("session_failure").is_none(),
            "absent stays absent on the wire"
        );
    }

    #[test]
    fn a_session_failure_round_trips() {
        let failure = ExecutionSessionFailure {
            message: "agent unavailable: codex stream timed out".into(),
            left_work: false,
        };
        let verified: GateVerificationCompletedPayload =
            serde_json::from_value(serde_json::json!({
                "project": "p", "workflow": "task", "all_passed": false, "required_passed": false,
                "results": [], "retry_count": 0,
                "session_failure": {"message": failure.message, "left_work": false}
            }))
            .unwrap();
        assert_eq!(verified.session_failure, Some(failure));
        let json = serde_json::to_value(&verified).unwrap();
        assert_eq!(json["session_failure"]["left_work"], false);
    }
}
