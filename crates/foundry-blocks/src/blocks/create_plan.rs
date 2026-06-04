use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    ChainContext, PlanCompletedPayload, ProjectCompletedPayload, TriageCompletedPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::workflow::WorkflowType;

use crate::gateway::{AgentAccess, AgentGateway, ModelTier, ReasoningEffort};

use super::{AgentBlockSpec, TriggerContext, invoke_agent};

agent_block_new!(
    /// Creates a step-by-step correction plan for an accepted assessment.
    ///
    /// Observer — sinks on `TriageCompleted` (filters for `accepted=true` only).
    /// Uses `AgentGateway` with `Reasoning` capability and `ReadOnly` access.
    /// Emits `PlanCompleted` with the plan text.
    pub struct CreatePlan
);

/// Build the terminal result emitted when triage is rejected.
///
/// A triage rejection is the triage stage doing its job: it judged the assessment
/// not worth correcting — whether for low severity or because the work is busy-work.
/// Severity is already folded into the triage agent's accept/reject decision, so the
/// outcome is a *successful* no-op regardless of the assessor's severity number. Emit
/// a terminal `ProjectIterationCompleted { success: true }` so the trace has an
/// accurate terminal event rather than falling back to block-level aggregation.
fn triage_rejection_result(
    payload: &TriageCompletedPayload,
    throttle: foundry_sdk::throttle::Throttle,
) -> anyhow::Result<TaskBlockResult> {
    let project = payload.project.as_str();
    let summary = format!("no correction warranted — {}", payload.reason);

    super::emit_event_result(
        format!("{project}: {summary}"),
        true,
        EventType::ProjectIterationCompleted,
        project,
        throttle,
        &ProjectCompletedPayload {
            project: project.to_string(),
            success: true,
            summary,
            workflow: WorkflowType::Iterate.to_string(),
            loop_context: None,
            changes: None,
        },
    )
}

impl TaskBlock for CreatePlan {
    task_block_meta! {
        name: "Create Plan",
        kind: Observer,
        sinks_on: [TriageCompleted],
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

        // Self-filter: only create plan for accepted triages.
        // When triage is rejected, emit ProjectIterationCompleted { success: false }
        // so the trace has a truthful terminal event.  Without it, is_success() falls
        // back to block-level aggregation and the watch client shows "running" forever.
        let p = parse_payload!(trigger, TriageCompletedPayload);

        if !p.accepted {
            let result = triage_rejection_result(&p, throttle);
            return Box::pin(async move { result });
        }

        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);
        let provider = super::chain_agent_provider(&payload);

        let principle = p.principle.clone();
        let category = p.category.clone();
        let assessment = p.assessment.clone();

        Box::pin(async move {
            let project_path = PathBuf::from(&entry.path);

            let principle = principle.as_str();
            let category = category.as_str();
            let assessment = assessment.as_str();

            let prompt = build_plan_prompt(&project, principle, category, assessment);

            let agent_file = super::execute_maintain::resolve_agent_file(&entry.agent);

            let outcome = invoke_agent(
                &*agent,
                AgentBlockSpec {
                    prompt,
                    working_dir: project_path,
                    access: AgentAccess::ReadOnly,
                    tier: ModelTier::Deep,
                    effort: ReasoningEffort::High,
                    agent_file,
                    provider,
                    timeout: entry.timeout(),
                },
                "create plan",
                &project,
            )
            .await;

            let (plan, success) =
                match super::match_agent_text_outcome(outcome, &project, "plan agent") {
                    Ok(pair) => pair,
                    Err(result) => return Ok(result),
                };

            let (correction_needed, correction_reason) = parse_correction_needed(&plan);

            tracing::info!(
                project = %project,
                success = success,
                correction_needed = correction_needed,
                "plan created"
            );

            let chain = ChainContext::extract_from(&payload);
            super::emit_event_result(
                format!("{project}: plan created for {principle} violation"),
                success,
                EventType::PlanCompleted,
                &project,
                throttle,
                &PlanCompletedPayload {
                    project: project.clone(),
                    plan: plan.clone(),
                    principle: principle.to_string(),
                    category: category.to_string(),
                    assessment: assessment.to_string(),
                    workflow: WorkflowType::Iterate.to_string(),
                    correction_needed,
                    correction_reason,
                    chain,
                },
            )
        })
    }
}

fn build_plan_prompt(project: &str, principle: &str, category: &str, assessment: &str) -> String {
    format!(
        "You are creating a correction plan for project '{project}'.\n\n\
         Assessment:\n\
         - Principle violated: {principle}\n\
         - Category: {category}\n\
         - Details: {assessment}\n\n\
         Create a step-by-step plan to correct this violation. Each step should be:\n\
         - Specific (name exact files and functions where possible)\n\
         - Minimal (only changes needed to address this violation)\n\
         - Testable (describe how to verify the step succeeded)\n\n\
         Output the plan as a numbered list of concrete steps.\n\n\
         At the very end of your response, after the plan, output a fenced JSON block \
         (``` json ... ```) containing exactly:\n\
         ```json\n\
         {{ \"correctionNeeded\": true, \"reason\": \"<one sentence>\" }}\n\
         ```\n\
         Set `correctionNeeded` to `false` ONLY if you examined the codebase and \
         concluded the assessment is inaccurate — the codebase already satisfies \
         the principle and no changes are warranted. In that case set `reason` to a \
         brief explanation. Otherwise leave it `true`."
    )
}

/// Parse the `correctionNeeded` flag from the plan agent's output.
///
/// The agent is asked to append a fenced JSON block at the end of its response:
/// ` ```json\n{ "correctionNeeded": true|false, "reason": "..." }\n``` `
///
/// This function splits the output on fence markers and tries every JSON candidate
/// in order.  For each candidate it attempts to deserialise as a JSON object and
/// checks for a boolean `correctionNeeded` field.
///
/// **Fail-closed**: any parse miss (missing field, wrong type, malformed JSON,
/// no fence at all) returns `(true, <explanation>)` so that the downstream
/// override logic treats the run as needing real work.
fn parse_correction_needed(output: &str) -> (bool, String) {
    // Walk every JSON-looking segment extracted from the output.
    // We deliberately try all candidates (not just the first fenced block) in
    // case the agent embeds reasoning prose alongside the terminal JSON block.
    let candidates: Vec<String> = {
        let mut acc = Vec::new();
        // Collect content inside fenced blocks (```...```)
        let mut remaining = output;
        while let Some(open) = remaining.find("```") {
            let after_open = &remaining[open + 3..];
            // Skip optional language identifier on the same line as the opening fence
            let content_start = after_open.find('\n').map_or(0, |n| n + 1);
            let content = &after_open[content_start..];
            if let Some(close) = content.find("```") {
                acc.push(content[..close].trim().to_string());
                remaining = &content[close + 3..];
            } else {
                break;
            }
        }
        // Also try extracting the outermost {...} object from the whole string
        // (handles bare JSON without fences)
        if output.contains('{') {
            acc.push(super::extract_json(output));
        }
        acc
    };

    for candidate in candidates {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&candidate)
            && let Some(serde_json::Value::Bool(b)) = v.get("correctionNeeded")
        {
            let reason =
                v.get("reason").and_then(serde_json::Value::as_str).unwrap_or("").to_string();
            return (*b, reason);
        }
    }

    (
        true,
        "no machine-readable correctionNeeded flag in plan output — assuming correction needed"
            .to_string(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::registry::Registry;
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use crate::gateway::fakes::FakeAgentGateway;
    use crate::gateway::{AgentAccess, ModelTier, ReasoningEffort};

    use super::super::test_helpers;
    use super::{CreatePlan, build_plan_prompt, parse_correction_needed};

    assert_block_meta!(
        CreatePlan::new(
            FakeAgentGateway::success(),
            Arc::new(RwLock::new(Registry { version: 2, projects: vec![] })),
        ),
        kind: Observer,
        sinks_on: [TriageCompleted],
    );

    #[tokio::test]
    async fn high_severity_triage_rejection_emits_terminal_success() {
        // A triage rejection is a successful no-op even when the assessor's severity
        // is high — triage already folds severity into its accept/reject decision.
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_project("my-project", "/tmp/test");
        let block = CreatePlan::new(agent.clone(), registry);
        let trigger = test_event!(EventType::TriageCompleted, "my-project", {
            "project": "my-project",
            "accepted": false,
            "reason": "purely cosmetic whitespace, busy-work",
            "severity": 6,
            "principle": "unknown",
            "category": "conventions",
            "assessment": "",
            "workflow": "iterate",
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success, "triage rejection is a successful no-op regardless of severity");
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
        assert_eq!(result.events[0].payload["success"], true);
        let summary = result.events[0].payload["summary"].as_str().unwrap();
        assert!(
            summary.contains("no correction warranted"),
            "summary should mention no correction warranted"
        );
        assert!(
            summary.contains("purely cosmetic whitespace"),
            "summary should carry the triage agent's reason"
        );
        // No plan agent invoked
        assert!(agent.invocations().is_empty());
    }

    #[tokio::test]
    async fn low_severity_triage_rejection_emits_terminal_success() {
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_project("my-project", "/tmp/test");
        let block = CreatePlan::new(agent.clone(), registry);
        let trigger = test_event!(EventType::TriageCompleted, "my-project", {
            "project": "my-project",
            "accepted": false,
            "reason": "too trivial, severity only 3",
            "severity": 3,
            "principle": "unknown",
            "category": "conventions",
            "assessment": "",
            "workflow": "iterate",
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success, "below-threshold rejection should be a successful no-op");
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ProjectIterationCompleted);
        assert_eq!(result.events[0].payload["success"], true);
        let summary = result.events[0].payload["summary"].as_str().unwrap();
        assert!(
            summary.contains("no correction warranted"),
            "summary should mention no correction warranted"
        );
        // No plan agent invoked
        assert!(agent.invocations().is_empty());
    }

    #[tokio::test]
    async fn creates_plan_for_accepted_triage() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with(
            "1. Extract shared validation into a helper function\n2. Update callers\n3. Add tests\n\n\
             ```json\n{ \"correctionNeeded\": true, \"reason\": \"Duplicate validation found in three locations.\" }\n```",
        );
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CreatePlan::new(agent.clone(), registry);
        let trigger = test_event!(EventType::TriageCompleted, "my-project", {
            "project": "my-project",
            "accepted": true,
            "reason": "violation is significant",
            "severity": 7,
            "principle": "DRY",
            "category": "duplication",
            "assessment": "Duplicate validation logic.",
            "audit_name": "fix-duplication",
            "workflow": "iterate",
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::PlanCompleted);
        assert!(result.events[0].payload["plan"].as_str().unwrap().contains("Extract"));
        assert_eq!(result.events[0].payload["principle"], "DRY");
        assert_eq!(result.events[0].payload["correction_needed"], true);

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].access, AgentAccess::ReadOnly);
        assert_eq!(invocations[0].tier, ModelTier::Deep);
        assert_eq!(invocations[0].effort, ReasoningEffort::High);
    }

    #[tokio::test]
    async fn forwards_actions_and_audit_name() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("1. Do the thing");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = CreatePlan::new(agent, registry);
        let trigger = Event::new(
            EventType::TriageCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "accepted": true,
                "reason": "violation is significant",
                "severity": 7,
                "principle": "SRP",
                "category": "architecture",
                "assessment": "Too many responsibilities.",
                "audit_name": "fix-srp",
                "actions": {"maintain": true},
                "workflow": "iterate",
            }),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(result.events[0].payload["audit_name"], "fix-srp");
        assert_eq!(result.events[0].payload["actions"]["maintain"], true);
    }

    // --- build_plan_prompt unit tests ---

    #[test]
    fn prompt_contains_plan_instructions() {
        let prompt = build_plan_prompt(
            "my-project",
            "DRY",
            "duplication",
            "Duplicate validation found in three locations.",
        );
        assert!(prompt.contains("my-project"), "expected project name in prompt");
        assert!(prompt.contains("DRY"), "expected principle in prompt");
        assert!(prompt.contains("duplication"), "expected category in prompt");
        assert!(prompt.contains("Duplicate validation"), "expected assessment in prompt");
        assert!(prompt.contains("numbered list"), "expected plan format instructions");
        assert!(prompt.contains("correctionNeeded"), "expected JSON output instructions");
        assert!(prompt.contains("Specific"), "expected specificity requirement");
    }

    // --- parse_correction_needed unit tests ---

    #[test]
    fn well_formed_fenced_block_correction_needed_true() {
        let output = "1. Do step one\n2. Do step two\n\n\
                      ```json\n{ \"correctionNeeded\": true, \"reason\": \"Changes required.\" }\n```";
        let (needed, reason) = parse_correction_needed(output);
        assert!(needed);
        assert_eq!(reason, "Changes required.");
    }

    #[test]
    fn well_formed_fenced_block_correction_needed_false() {
        let output = "The codebase already satisfies the principle.\n\n\
                      ```json\n{ \"correctionNeeded\": false, \"reason\": \"Already clean.\" }\n```";
        let (needed, reason) = parse_correction_needed(output);
        assert!(!needed);
        assert_eq!(reason, "Already clean.");
    }

    #[test]
    fn block_embedded_in_prose() {
        let output = "Step 1: refactor.\nStep 2: test.\n\n\
                      Some closing remarks.\n\
                      ```json\n{ \"correctionNeeded\": true, \"reason\": \"Refactor needed.\" }\n```\n\
                      End of plan.";
        let (needed, _) = parse_correction_needed(output);
        assert!(needed);
    }

    #[test]
    fn bare_object_without_fence() {
        let output =
            "Here is my plan. { \"correctionNeeded\": false, \"reason\": \"No work needed.\" }";
        let (needed, reason) = parse_correction_needed(output);
        assert!(!needed);
        assert_eq!(reason, "No work needed.");
    }

    #[test]
    fn missing_correction_needed_field_returns_true() {
        let output = "```json\n{ \"reason\": \"something\" }\n```";
        let (needed, reason) = parse_correction_needed(output);
        assert!(needed);
        assert!(
            reason.contains("no machine-readable"),
            "expected fail-closed message, got: {reason}"
        );
    }

    #[test]
    fn malformed_json_returns_true() {
        let output = "```json\n{ correctionNeeded: true }\n```";
        let (needed, reason) = parse_correction_needed(output);
        assert!(needed);
        assert!(
            reason.contains("no machine-readable"),
            "expected fail-closed message, got: {reason}"
        );
    }

    #[test]
    fn non_boolean_correction_needed_returns_true() {
        let output = "```json\n{ \"correctionNeeded\": \"yes\" }\n```";
        let (needed, reason) = parse_correction_needed(output);
        assert!(needed);
        assert!(
            reason.contains("no machine-readable"),
            "expected fail-closed message, got: {reason}"
        );
    }

    #[test]
    fn two_json_blocks_only_second_has_correction_needed() {
        let output = "```json\n{ \"severity\": 7 }\n```\n\
                      Some prose.\n\
                      ```json\n{ \"correctionNeeded\": false, \"reason\": \"Second block wins.\" }\n```";
        let (needed, reason) = parse_correction_needed(output);
        assert!(!needed);
        assert_eq!(reason, "Second block wins.");
    }

    #[test]
    fn no_json_at_all_returns_true() {
        let output = "1. Extract helper\n2. Update callers\n3. Add tests";
        let (needed, reason) = parse_correction_needed(output);
        assert!(needed);
        assert!(
            reason.contains("no machine-readable"),
            "expected fail-closed message, got: {reason}"
        );
    }
}
