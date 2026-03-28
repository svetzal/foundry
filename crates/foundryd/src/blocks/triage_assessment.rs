use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentRequest};

/// Triages an assessment to decide whether the fix is worth pursuing.
///
/// Observer — sinks on `AssessmentCompleted`.
/// Uses `AgentGateway` with `Quick` capability and `ReadOnly` access.
/// Emits `TriageCompleted` with accepted/rejected and reason.
///
/// Triage rejection is not a failure — it's a filter. `result.success` is always `true`,
/// but downstream blocks check the `accepted` field in the payload.
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
    fn name(&self) -> &'static str {
        "Triage Assessment"
    }

    fn kind(&self) -> BlockKind {
        BlockKind::Observer
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::AssessmentCompleted]
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

        let entry = self.registry.projects.iter().find(|p| p.name == project).cloned();
        let agent = Arc::clone(&self.agent);

        Box::pin(async move {
            let Some(entry) = entry else {
                tracing::warn!(project = %project, "project not found in registry");
                return Ok(TaskBlockResult {
                    events: vec![],
                    success: false,
                    summary: format!("Project '{project}' not found in registry"),
                    raw_output: None,
                    exit_code: None,
                    audit_artifacts: vec![],
                });
            };

            let project_path = PathBuf::from(&entry.path);

            let severity =
                payload.get("severity").and_then(serde_json::Value::as_str).unwrap_or("unknown");
            let principle = payload
                .get("principle")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            let description =
                payload.get("description").and_then(serde_json::Value::as_str).unwrap_or("");

            let prompt = format!(
                "You are triaging a code assessment. Accept if severity is medium or high \
                 AND the fix is meaningful (not busy-work like renaming or comment changes). \
                 Reject if severity is low or the fix would be trivial busy-work. \
                 Assessment: {{ severity: \"{severity}\", principle: \"{principle}\", \
                 description: \"{description}\" }}. \
                 Return JSON: {{ \"accepted\": true|false, \"reason\": \"<explanation>\" }}"
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

            tracing::info!(project = %project, "triaging assessment via agent");

            let response = agent.invoke(&request).await;

            let (accepted, reason) = match response {
                Ok(r) if r.success => parse_triage_response(&r.stdout),
                Ok(r) => {
                    tracing::warn!(project = %project, stderr = %r.stderr, "triage agent failed");
                    // Default to accepting on agent failure so the chain continues
                    (true, "triage agent failed, defaulting to accept".to_string())
                }
                Err(err) => {
                    tracing::warn!(error = %err, "agent invocation failed for triage");
                    (true, format!("agent unavailable, defaulting to accept: {err}"))
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
                "assessment": {
                    "severity": severity,
                    "principle": principle,
                    "category": payload.get("category").and_then(serde_json::Value::as_str).unwrap_or("unknown"),
                    "description": description,
                },
            });

            // Forward actions, audit_name from trigger payload
            if let Some(actions) = payload.get("actions") {
                event_payload["actions"] = actions.clone();
            }
            if let Some(audit_name) = payload.get("audit_name") {
                event_payload["audit_name"] = audit_name.clone();
            }

            let summary = if accepted {
                format!("{project}: triage accepted — {reason}")
            } else {
                format!("{project}: triage rejected — {reason}")
            };

            Ok(TaskBlockResult {
                events: vec![Event::new(
                    EventType::TriageCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
                // Triage rejection is not a failure — it's a filter
                success: true,
                summary,
                raw_output: None,
                exit_code: None,
                audit_artifacts: vec![],
            })
        })
    }
}

/// Parse the triage agent output for accepted/reason fields.
fn parse_triage_response(output: &str) -> (bool, String) {
    // Try to extract JSON from the output
    if let Some(val) = super::assess_project::extract_json(output) {
        let accepted = val.get("accepted").and_then(serde_json::Value::as_bool).unwrap_or(true);
        let reason = val
            .get("reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no reason provided")
            .to_string();
        return (accepted, reason);
    }

    // Fallback: accept by default
    (true, "could not parse triage response, defaulting to accept".to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use foundry_core::event::{Event, EventType};
    use foundry_core::registry::{ActionFlags, ProjectEntry, Registry, Stack};
    use foundry_core::task_block::{BlockKind, TaskBlock};
    use foundry_core::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;
    use crate::gateway::{AgentAccess, AgentCapability};

    use super::TriageAssessment;

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
                "severity": "high",
                "principle": "Single Responsibility",
                "category": "design",
                "description": "Classes have too many responsibilities",
                "audit_name": "srp-violation",
                "actions": {"iterate": true},
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
    async fn accepted_assessment_emits_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(
            r#"{ "accepted": true, "reason": "High severity issue worth fixing" }"#,
        );
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = TriageAssessment::new(agent.clone(), registry);
        let trigger = assessment_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::TriageCompleted);
        assert_eq!(result.events[0].payload["accepted"], true);
        assert_eq!(result.events[0].payload["reason"], "High severity issue worth fixing");

        // Verify assessment is forwarded
        assert_eq!(result.events[0].payload["assessment"]["severity"], "high");
        assert_eq!(result.events[0].payload["assessment"]["principle"], "Single Responsibility");

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].access, AgentAccess::ReadOnly);
        assert_eq!(invocations[0].capability, AgentCapability::Quick);
    }

    #[tokio::test]
    async fn rejected_assessment_emits_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(
            r#"{ "accepted": false, "reason": "Low severity, trivial rename" }"#,
        );
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = TriageAssessment::new(agent, registry);
        let trigger = assessment_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        // Triage rejection is not a failure
        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].payload["accepted"], false);
        assert_eq!(result.events[0].payload["reason"], "Low severity, trivial rename");
    }

    #[tokio::test]
    async fn forwards_actions_and_audit_name() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(r#"{ "accepted": true, "reason": "ok" }"#);
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = TriageAssessment::new(agent, registry);
        let trigger = assessment_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(result.events[0].payload["actions"]["iterate"], true);
        assert_eq!(result.events[0].payload["audit_name"], "srp-violation");
    }
}
