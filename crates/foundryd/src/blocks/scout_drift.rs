use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_core::event::{Event, EventType, PayloadExt};
use foundry_core::registry::Registry;
use foundry_core::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentCapability, AgentGateway, AgentRequest};

const DRIFT_SCOUT_PROMPT: &str = r#"You are a BUG SCOUT agent.

Your job is to explore a codebase and identify likely defects by detecting mismatches between intent and behavior.

You are not given a specific bug.
You must discover where investigation is warranted.

You do NOT fix anything.
You do NOT assume anything is a bug until you have evidence of divergence.
You do NOT treat failing tests as the only signal.
You do NOT treat passing tests as proof of correctness.

Your goal is to find and rank areas where:
- behavior appears inconsistent
- intent appears unclear or missing
- signals disagree (tests vs code vs product vs design)
- risk or impact is high

You produce a prioritized set of investigation targets.

---

## Core mindset

- You are mapping uncertainty, not solving it.
- You are looking for tension between signals.
- A "bug candidate" is a place where intent and behavior do not align cleanly.
- Missing intent is itself a high-value finding.

---

## Sources of signals

You may use:

- CHARTER.md (system/product intent)
- Epilogue Tracker (et): actors, goals, interactions
- tests (what is asserted)
- code (what is implemented)
- types, schemas, validation rules
- UI flows, copy, design patterns
- architecture rules and boundaries
- logs, errors, unusual handling
- naming and inconsistencies
- custom agents or skills representing perspectives

Do not rely on a single source.
You are specifically looking for disagreement across sources.

---

## Discovery strategy

You must actively scan for the following patterns:

### 1. Test / code mismatch
- tests expect behavior that code barely supports
- code handles cases tests do not cover
- tests are overly narrow or overly permissive

### 2. Code inconsistency
- similar operations behave differently in different places
- duplicated logic with subtle divergence
- inconsistent validation or error handling
- inconsistent naming that implies different intent

### 3. Intent gaps
- important flows with no tests
- behavior with no clear product or actor rationale
- edge cases handled arbitrarily

### 4. Actor goal violations (if et available)
- flows that do not clearly satisfy an actor goal
- interactions that seem incomplete or contradictory
- actions that do not map cleanly to outcomes

### 5. Design / UX mismatches
- inconsistent feedback patterns
- blocking vs non-blocking inconsistencies
- silent failures vs explicit errors
- flow interruptions that seem unintended

### 6. Architecture tension
- logic in the wrong layer
- cross-boundary leakage
- duplicated responsibility across modules

### 7. Risk hotspots
- complex conditionals
- state transitions
- async or concurrency handling
- data transformation pipelines
- validation and parsing
- error handling branches

### 8. Suspicious patterns
- TODO / FIXME / unclear comments
- defensive code that suggests uncertainty
- "should never happen" branches
- silent catches or ignored errors"#;

const JSON_OUTPUT_INSTRUCTIONS: &str = r#"

---

## Output requirements

You must output ONLY valid JSON in this exact format, nothing else:

{
  "candidates": [
    {
      "rank": 1,
      "summary": "<brief description of the suspected issue>",
      "triggering_scenario": "<how this behavior would occur, or null if unknown>",
      "signals": [
        {"source": "<code|tests|docs|architecture|design|naming|charter>", "observation": "<what was observed>"}
      ],
      "divergence_type": "<one of: code_vs_code, code_vs_intent, test_vs_code, architecture_tension, missing_intent, missing_code, design_inconsistency>",
      "explanation": "<why this might be wrong — the mismatch or ambiguity>",
      "impact": {
        "actors_affected": ["<actor names>"],
        "severity": "<low|medium|high|critical>",
        "frequency": "<rare|uncommon|common|pervasive>",
        "risk_type": "<correctness|reliability|security|usability|maintainability>"
      },
      "confidence": "<low|medium|high>",
      "confidence_notes": "<what would increase confidence>",
      "suggested_next_step": "<one of: ignore, needs_investigator, needs_clarification, needs_test, needs_intent_defined>",
      "high_value": true,
      "high_value_reason": "<why this is or is not a high-value investigation target>"
    }
  ]
}

Produce a minimum of 3 candidates if possible. Rank them by investigation priority.
Mark the top 1-3 as high_value: true.

Constraints:
- Do not propose fixes
- Do not rewrite code
- Do not assume intent where it is missing
- Do not inflate trivial inconsistencies into bugs
- Do not ignore missing intent — it is a first-class signal"#;

/// Detects intent drift in a codebase by identifying mismatches between intent and behavior.
///
/// Observer — sinks on `DriftAssessmentRequested`.
/// Uses `AgentGateway` with `Reasoning` capability and `ReadOnly` access to scan the
/// codebase for bug candidates where signals disagree.
/// Emits `DriftAssessmentCompleted` with ranked candidates and divergence types.
pub struct ScoutDrift {
    registry: Arc<Registry>,
    agent: Arc<dyn AgentGateway>,
}

impl ScoutDrift {
    pub fn new(agent: Arc<dyn AgentGateway>, registry: Arc<Registry>) -> Self {
        Self { registry, agent }
    }
}

impl TaskBlock for ScoutDrift {
    task_block_meta! {
        name: "Scout Drift",
        kind: Observer,
        sinks_on: [DriftAssessmentRequested],
    }

    fn execute(
        &self,
        trigger: &Event,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<TaskBlockResult>> + Send + '_>>
    {
        let project = trigger.project.clone();
        let throttle = trigger.throttle;

        let entry = self.registry.find_project(&project).cloned();
        let agent = Arc::clone(&self.agent);

        Box::pin(async move {
            let Some(entry) = entry else {
                return Ok(super::project_not_found_result(&project));
            };

            let project_path = PathBuf::from(&entry.path);
            let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

            let prompt = format!(
                "{DRIFT_SCOUT_PROMPT}\n\n\
                 You are analyzing the project '{project}'.\n\
                 {JSON_OUTPUT_INSTRUCTIONS}"
            );

            let request = AgentRequest {
                prompt,
                working_dir: project_path,
                access: AgentAccess::ReadOnly,
                capability: AgentCapability::Reasoning,
                agent_file,
                timeout: entry.timeout(),
            };

            tracing::info!(project = %project, "scouting for intent drift via agent");

            let response = match agent.invoke(&request).await {
                Ok(r) => r,
                Err(err) => {
                    tracing::warn!(error = %err, "agent invocation failed for drift scout");
                    return Ok(TaskBlockResult::failure(format!("agent unavailable: {err}")));
                }
            };

            let result = if response.success {
                parse_drift_assessment(&response.stdout)
            } else {
                tracing::warn!(project = %project, stderr = %response.stderr, "drift scout agent failed");
                DriftAssessmentResult {
                    candidate_count: 0,
                    high_value_count: 0,
                    candidates: serde_json::Value::Array(vec![]),
                    parse_error: Some(
                        response
                            .stderr
                            .lines()
                            .next()
                            .unwrap_or("agent returned non-success")
                            .to_string(),
                    ),
                }
            };

            tracing::info!(
                project = %project,
                candidate_count = result.candidate_count,
                high_value_count = result.high_value_count,
                "drift assessment completed"
            );

            let mut event_payload = serde_json::json!({
                "project": project,
                "candidate_count": result.candidate_count,
                "high_value_count": result.high_value_count,
                "candidates": result.candidates,
            });
            if let Some(err) = &result.parse_error {
                event_payload["parse_error"] = serde_json::Value::String(err.clone());
            }

            Ok(TaskBlockResult::success(
                format!(
                    "{project}: drift assessment — {} candidates, {} high-value",
                    result.candidate_count, result.high_value_count
                ),
                vec![Event::new(
                    EventType::DriftAssessmentCompleted,
                    project.clone(),
                    throttle,
                    event_payload,
                )],
            )
            .with_output(
                Some(format!("{}\n{}", response.stdout, response.stderr)),
                Some(response.exit_code),
            ))
        })
    }
}

struct DriftAssessmentResult {
    candidate_count: usize,
    high_value_count: usize,
    candidates: serde_json::Value,
    parse_error: Option<String>,
}

fn parse_drift_assessment(output: &str) -> DriftAssessmentResult {
    let json_str = super::assess_project::extract_json(output);
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&json_str) {
        let candidates = json
            .get("candidates")
            .and_then(serde_json::Value::as_array)
            .cloned()
            .unwrap_or_default();
        let candidate_count = candidates.len();
        let high_value_count = candidates.iter().filter(|c| c.bool_or("high_value", false)).count();
        DriftAssessmentResult {
            candidate_count,
            high_value_count,
            candidates: serde_json::Value::Array(candidates),
            parse_error: None,
        }
    } else {
        let first_line = output.lines().next().unwrap_or("parse failed");
        DriftAssessmentResult {
            candidate_count: 0,
            high_value_count: 0,
            candidates: serde_json::Value::Array(vec![]),
            parse_error: Some(first_line.to_string()),
        }
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
    use crate::gateway::{AgentAccess, AgentCapability};

    use super::{ScoutDrift, parse_drift_assessment};

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

    fn drift_requested_event(project: &str) -> Event {
        Event::new(
            EventType::DriftAssessmentRequested,
            project.to_string(),
            Throttle::Full,
            serde_json::json!({"project": project}),
        )
    }

    #[test]
    fn kind_is_observer() {
        let agent = FakeAgentGateway::success();
        let block = ScoutDrift::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.kind(), BlockKind::Observer);
    }

    #[test]
    fn sinks_on_drift_assessment_requested() {
        let agent = FakeAgentGateway::success();
        let block = ScoutDrift::new(
            agent,
            Arc::new(Registry {
                version: 2,
                projects: vec![],
            }),
        );
        assert_eq!(block.sinks_on(), &[EventType::DriftAssessmentRequested]);
    }

    #[tokio::test]
    async fn project_not_in_registry_returns_failure() {
        let agent = FakeAgentGateway::success();
        let registry = Arc::new(Registry {
            version: 2,
            projects: vec![],
        });
        let block = ScoutDrift::new(agent, registry);
        let trigger = drift_requested_event("unknown-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success);
        assert!(result.events.is_empty());
    }

    #[tokio::test]
    async fn scouts_drift_successfully() {
        let dir = tempfile::tempdir().unwrap();
        let agent_response = crate::gateway::AgentResponse {
            stdout: r#"{"candidates": [
                {
                    "rank": 1,
                    "summary": "Inconsistent error handling",
                    "triggering_scenario": "When invalid input is received",
                    "signals": [{"source": "code", "observation": "endpoint A returns 400, endpoint B ignores"}],
                    "divergence_type": "code_vs_code",
                    "explanation": "Two endpoints disagree on error behavior",
                    "impact": {"actors_affected": ["api_consumer"], "severity": "medium", "frequency": "common", "risk_type": "correctness"},
                    "confidence": "high",
                    "confidence_notes": "Direct code comparison",
                    "suggested_next_step": "needs_investigator",
                    "high_value": true,
                    "high_value_reason": "Affects all API consumers"
                },
                {
                    "rank": 2,
                    "summary": "Missing validation on user input",
                    "triggering_scenario": "When special characters are submitted",
                    "signals": [{"source": "tests", "observation": "no test for special chars"}],
                    "divergence_type": "missing_intent",
                    "explanation": "No stated intent for edge case handling",
                    "impact": {"actors_affected": ["end_user"], "severity": "low", "frequency": "uncommon", "risk_type": "usability"},
                    "confidence": "medium",
                    "confidence_notes": "Need to check requirements",
                    "suggested_next_step": "needs_clarification",
                    "high_value": false,
                    "high_value_reason": "Low severity"
                }
            ]}"#
                .to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        };
        let agent = FakeAgentGateway::sequence(vec![agent_response]);
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ScoutDrift::new(agent.clone(), registry);
        let trigger = drift_requested_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::DriftAssessmentCompleted);
        assert_eq!(result.events[0].payload["candidate_count"], 2);
        assert_eq!(result.events[0].payload["high_value_count"], 1);

        let candidates = result.events[0].payload["candidates"].as_array().unwrap();
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0]["divergence_type"], "code_vs_code");
        assert_eq!(candidates[1]["divergence_type"], "missing_intent");

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].access, AgentAccess::ReadOnly);
        assert_eq!(invocations[0].capability, AgentCapability::Reasoning);
    }

    #[tokio::test]
    async fn handles_empty_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::sequence(vec![crate::gateway::AgentResponse {
            stdout: r#"{"candidates": []}"#.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }]);
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ScoutDrift::new(agent, registry);
        let trigger = drift_requested_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].payload["candidate_count"], 0);
        assert_eq!(result.events[0].payload["high_value_count"], 0);
    }

    #[tokio::test]
    async fn handles_invalid_json_from_agent() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::sequence(vec![crate::gateway::AgentResponse {
            stdout: "This is not JSON at all".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }]);
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ScoutDrift::new(agent, registry);
        let trigger = drift_requested_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].payload["candidate_count"], 0);
        assert!(result.events[0].payload.get("parse_error").is_some());
    }

    #[tokio::test]
    async fn agent_non_success_emits_empty_assessment() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::failure("something went wrong");
        let registry = registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ScoutDrift::new(agent, registry);
        let trigger = drift_requested_event("my-project");

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].payload["candidate_count"], 0);
        assert!(result.events[0].payload.get("parse_error").is_some());
    }

    #[test]
    fn parse_drift_assessment_extracts_candidates() {
        let output = r#"{"candidates": [
            {"rank": 1, "summary": "Issue A", "high_value": true},
            {"rank": 2, "summary": "Issue B", "high_value": false}
        ]}"#;
        let result = parse_drift_assessment(output);
        assert_eq!(result.candidate_count, 2);
        assert_eq!(result.high_value_count, 1);
        assert!(result.parse_error.is_none());
    }

    #[test]
    fn parse_drift_assessment_counts_high_value() {
        let output = r#"{"candidates": [
            {"rank": 1, "high_value": true},
            {"rank": 2, "high_value": true},
            {"rank": 3, "high_value": false}
        ]}"#;
        let result = parse_drift_assessment(output);
        assert_eq!(result.candidate_count, 3);
        assert_eq!(result.high_value_count, 2);
    }

    #[test]
    fn parse_drift_assessment_handles_surrounding_text() {
        let output =
            "Here is my analysis:\n{\"candidates\": [{\"rank\": 1, \"high_value\": true}]}\nDone.";
        let result = parse_drift_assessment(output);
        assert_eq!(result.candidate_count, 1);
        assert!(result.parse_error.is_none());
    }

    #[test]
    fn parse_drift_assessment_fallback_on_invalid() {
        let output = "This is not JSON at all";
        let result = parse_drift_assessment(output);
        assert_eq!(result.candidate_count, 0);
        assert!(result.parse_error.is_some());
    }
}
