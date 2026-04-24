use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::payload::{AssessmentCompletedPayload, ChainContext, TriageCompletedPayload};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_core::workflow::WorkflowType;

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentOutcome, AgentRequest};

use super::TriggerContext;

/// Filters assessments: rejects low-severity issues and busy-work.
///
/// Observer — sinks on `AssessmentCompleted`.
/// Uses `AgentGateway` with `Quick` capability and `ReadOnly` access.
/// Emits `TriageCompleted` with `accepted: true/false` and a reason.
pub struct TriageAssessment {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
}

impl TriageAssessment {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, agent }
    }
}

impl TaskBlock for TriageAssessment {
    task_block_meta! {
        name: "Triage Assessment",
        kind: Observer,
        sinks_on: [AssessmentCompleted],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let TriggerContext {
            project,
            throttle,
            payload,
        } = TriggerContext::from_trigger(trigger);

        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);

        let assessment_payload = parse_payload!(trigger, AssessmentCompletedPayload);

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);

            let severity = i64::try_from(assessment_payload.severity).unwrap_or(5);
            let principle = assessment_payload.principle.clone();
            let category = assessment_payload.category.clone();
            let assessment = assessment_payload.assessment.clone();

            let prompt = format!(
                "You are triaging an assessment for project '{project}'.\n\n\
                 Assessment:\n\
                 - Severity: {severity}/10\n\
                 - Principle: {principle}\n\
                 - Category: {category}\n\
                 - Details: {assessment}\n\n\
                 Decide whether this assessment should be accepted for correction.\n\
                 Accept if: severity >= 4 AND the work is substantive (not busy-work like \
                 trivial comment changes, whitespace formatting, or purely cosmetic tweaks).\n\
                 Reject if: severity < 4 OR the work is busy-work.\n\n\
                 Output ONLY valid JSON in this exact format, nothing else:\n\
                 {{\"accepted\": true/false, \"reason\": \"<brief explanation>\"}}"
            );

            let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

            let request = AgentRequest {
                prompt,
                working_dir: project_path,
                access: AgentAccess::ReadOnly,
                capability: AgentCapability::Quick,
                agent_file,
                timeout: std::time::Duration::from_secs(120),
            };

            tracing::info!(project = %project, severity = severity, "triaging assessment via agent");

            let response = agent.invoke(&request).await;

            let (accepted, reason) = match AgentOutcome::from_response(response) {
                AgentOutcome::Success { stdout } => parse_triage(&stdout),
                AgentOutcome::AgentFailed { stderr } => {
                    tracing::warn!(project = %project, stderr = %stderr, "triage agent failed");
                    // Default to accepting on agent failure — better to attempt the fix
                    (true, "triage agent failed, defaulting to accept".to_string())
                }
                AgentOutcome::Unavailable { error } => {
                    tracing::warn!(error = %error, "agent invocation failed for triage");
                    (true, format!("agent unavailable: {error}, defaulting to accept"))
                }
            };

            tracing::info!(
                project = %project,
                accepted = accepted,
                reason = %reason,
                "triage completed"
            );

            let chain = ChainContext::extract_from(&payload);
            super::emit_result(
                if accepted {
                    format!("{project}: triage accepted — {reason}")
                } else {
                    format!("{project}: triage rejected — {reason}")
                },
                EventType::TriageCompleted,
                &project,
                throttle,
                &TriageCompletedPayload {
                    project: project.clone(),
                    accepted,
                    reason: reason.clone(),
                    // SAFETY: severity.max(0) is guaranteed non-negative; cast is lossless.
                    #[allow(clippy::cast_sign_loss)]
                    severity: severity.max(0) as u64,
                    principle: principle.clone(),
                    category: category.clone(),
                    assessment: assessment.clone(),
                    workflow: WorkflowType::Iterate.to_string(),
                    chain,
                },
            )
        })
    }
}

/// Parse the JSON triage output from the agent.
fn parse_triage(output: &str) -> (bool, String) {
    let json_str = super::assess_project::extract_json(output);
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) {
        let accepted = json.get("accepted").and_then(serde_json::Value::as_bool).unwrap_or(true);
        let reason = json
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no reason given")
            .to_string();
        (accepted, reason)
    } else {
        // Default to accept if we can't parse
        (true, "could not parse triage response, defaulting to accept".to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::Registry;
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;

    use super::super::test_helpers;
    use super::{TriageAssessment, parse_triage};

    fn assessment_event(project: &str) -> Event {
        Event::new(
            EventType::AssessmentCompleted,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": project,
                "severity": 7,
                "principle": "DRY",
                "category": "duplication",
                "assessment": "Several methods duplicate validation logic.",
                "audit_name": "fix-duplication",
                "workflow": "iterate",
            }),
        )
    }

    #[test]
    fn kind_is_observer() {
        let agent = FakeAgentGateway::success();
        let block = TriageAssessment::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_assessment_completed() {
        let agent = FakeAgentGateway::success();
        let block = TriageAssessment::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.sinks_on(), &[EventType::AssessmentCompleted]);
    }

    #[tokio::test]
    async fn accepts_high_severity_assessment() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(
            r#"{"accepted": true, "reason": "severity warrants fix"}"#,
        );
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = TriageAssessment::new(agent, registry);
        let trigger = assessment_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::TriageCompleted);
        assert_eq!(result.events[0].payload["accepted"], true);
    }

    #[tokio::test]
    async fn rejects_low_severity_assessment() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(
            r#"{"accepted": false, "reason": "too trivial to fix"}"#,
        );
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = TriageAssessment::new(agent, registry);
        let trigger = assessment_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events[0].payload["accepted"], false);
        assert!(result.events[0].payload["reason"].as_str().unwrap().contains("trivial"));
    }

    #[tokio::test]
    async fn forwards_assessment_fields() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(r#"{"accepted": true, "reason": "ok"}"#);
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = TriageAssessment::new(agent, registry);
        let trigger = assessment_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(result.events[0].payload["severity"], 7);
        assert_eq!(result.events[0].payload["principle"], "DRY");
        assert_eq!(result.events[0].payload["audit_name"], "fix-duplication");
    }

    #[test]
    fn parse_triage_extracts_json() {
        let output = r#"{"accepted": false, "reason": "busy-work"}"#;
        let (accepted, reason) = parse_triage(output);
        assert!(!accepted);
        assert_eq!(reason, "busy-work");
    }

    #[test]
    fn parse_triage_defaults_to_accept_on_invalid() {
        let (accepted, _) = parse_triage("not json");
        assert!(accepted);
    }
}
