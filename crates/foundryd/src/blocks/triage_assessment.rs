use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType, PayloadExt};
use foundry_core::loop_context::forward_chain_context;
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentRequest};

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

    #[allow(clippy::too_many_lines)]
    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let payload = trigger.payload.clone();

        let entry = self.registry.find_project(&project).cloned();
        let agent = Arc::clone(&self.agent);

        Box::pin(async move {
            let Some(entry) = entry else {
                return Ok(super::project_not_found_result(&project));
            };

            let project_path = PathBuf::from(&entry.path);

            let severity = payload.i64_or("severity", 0);
            let principle = payload.str_or("principle", "unknown");
            let category = payload.str_or("category", "unknown");
            let assessment = payload.str_or("assessment", "");

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

            let (accepted, reason) = match response {
                Ok(r) if r.success => parse_triage(&r.stdout),
                Ok(r) => {
                    tracing::warn!(project = %project, stderr = %r.stderr, "triage agent failed");
                    // Default to accepting on agent failure — better to attempt the fix
                    (true, "triage agent failed, defaulting to accept".to_string())
                }
                Err(err) => {
                    tracing::warn!(error = %err, "agent invocation failed for triage");
                    (true, format!("agent unavailable: {err}, defaulting to accept"))
                }
            };

            tracing::info!(
                project = %project,
                accepted = accepted,
                reason = %reason,
                "triage completed"
            );

            let mut event_payload = serde_json::json!({
                "project": project,
                "accepted": accepted,
                "reason": reason,
                "severity": severity,
                "principle": principle,
                "category": category,
                "assessment": assessment,
                "workflow": "iterate",
            });
            forward_chain_context(&payload, &mut event_payload);

            Ok(TaskBlockResult::success(
                if accepted {
                    format!("{project}: triage accepted — {reason}")
                } else {
                    format!("{project}: triage rejected — {reason}")
                },
                vec![Event::new(
                    EventType::TriageCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
            ))
        })
    }
}

/// Parse the JSON triage output from the agent.
fn parse_triage(output: &str) -> (bool, String) {
    let json_str = super::assess_project::extract_json(output);
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) {
        let accepted = json.bool_or("accepted", true);
        let reason = json.str_or("reason", "no reason given").to_string();
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
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;

    use super::{TriageAssessment, parse_triage};

    fn registry_with_project(name: &str, path: &str) -> Arc<Registry> {
        Arc::new(Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: name.to_string(),
                path: path.to_string(),
                stack: Stack::Rust,
                agent: "claude".to_string(),
                repo: String::new(),
                branch: "main".to_string(),
                skip: None,
                notes: None,
                actions: ActionFlags::default(),
                install: None,
                timeout_secs: None,
            }],
        })
    }

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
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
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
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
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
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
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
