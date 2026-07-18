use std::path::PathBuf;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    GateVerificationCompletedPayload, LoopContext, TaskReviewedPayload, TaskVerdict,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::workflow::WorkflowType;

use crate::gateway::{AgentAccess, AgentGateway, ModelTier, ReasoningEffort};

use super::{AgentBlockSpec, TriggerContext, invoke_agent};

agent_block_new!(
    /// Performs skeptical, source-aware validation for one-shot tasks.
    pub struct ReviewTask
);

fn parse_verdict(output: &str) -> anyhow::Result<TaskVerdict> {
    let candidate = super::extract_json(output);
    serde_json::from_str::<TaskVerdict>(&candidate)
        .map_err(|e| anyhow::anyhow!("reviewer returned no valid task verdict: {e}"))
}

fn build_review_prompt(objective: &str, gate_results: &[foundry_sdk::gates::GateResult]) -> String {
    let gates = gate_results
        .iter()
        .map(|g| {
            format!(
                "- {}: {} (exit {})\n{}",
                g.name,
                if g.passed { "PASS" } else { "FAIL" },
                g.exit_code,
                g.output
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "You are the skeptical reviewer for a one-shot engineering task. Inspect the actual source, diff, and tests in the current worktree; do not trust the executor's self-report.\n\n\
         OBJECTIVE (every acceptance/evidence phrase is binding):\n{objective}\n\n\
         MECHANICAL GATE RESULTS:\n{gates}\n\n\
         Decide exactly one typed verdict. COMPLETE requires every objective and evidence requirement to be satisfied and all required gates to pass. Tests that mask identifiers, compare only counts, inject around the real boundary, or otherwise cannot detect the stated defect do not count. Use REMAINDER only for a finite list of missing work on a converging implementation. Use DEFECT for a faulty approach or regression. Use BLOCKED_ON_DECISION when reality exposes a genuine product/policy choice that makes the objective unsatisfiable as written.\n\n\
         End with exactly one JSON object in a fenced json block, using one of these shapes:\n\
         {{\"verdict\":\"complete\"}}\n\
         {{\"verdict\":\"remainder\",\"gaps\":[\"specific gap\"]}}\n\
         {{\"verdict\":\"defect\",\"diagnosis\":\"specific diagnosis\"}}\n\
         {{\"verdict\":\"blocked_on_decision\",\"finding\":\"finding\",\"options\":[\"option\"]}}"
    )
}

impl TaskBlock for ReviewTask {
    task_block_meta! {
        name: "Review Task",
        kind: Observer,
        sinks_on: [GateVerificationCompleted],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        WorkflowType::from_payload(&trigger.payload) == WorkflowType::Task
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project,
            throttle,
            payload,
        } = TriggerContext::from_trigger(trigger);
        let p = parse_payload!(trigger, GateVerificationCompletedPayload);
        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);
        let context = LoopContext::extract_from(&payload);
        let objective = context
            .prompt
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some(worktree) = context.task_worktree.clone() else {
            return Box::pin(async {
                Ok(TaskBlockResult::failure("task review missing isolated worktree"))
            });
        };
        let prompt = build_review_prompt(&objective, &p.results);
        let provider = super::chain_agent_provider(&payload);

        Box::pin(async move {
            let outcome = invoke_agent(
                &*agent,
                AgentBlockSpec {
                    prompt,
                    working_dir: PathBuf::from(worktree),
                    access: AgentAccess::ReadOnly,
                    tier: ModelTier::Deep,
                    effort: ReasoningEffort::High,
                    agent_file: super::resolve_agent_file(&entry.agent),
                    provider,
                    timeout: entry.timeout(),
                },
                "task review",
                &project,
            )
            .await;

            let (review, verdict) = match outcome {
                crate::gateway::AgentOutcome::Success { stdout } => {
                    let verdict = parse_verdict(&stdout).unwrap_or_else(|e| TaskVerdict::Defect {
                        diagnosis: e.to_string(),
                    });
                    (stdout, verdict)
                }
                crate::gateway::AgentOutcome::AgentFailed { stderr, failure } => {
                    let detail = failure.map_or(stderr.clone(), |f| f.execution_summary());
                    (stderr, TaskVerdict::RunnerError { detail })
                }
                crate::gateway::AgentOutcome::Unavailable { error } => {
                    (error.clone(), TaskVerdict::RunnerError { detail: error })
                }
            };

            super::emit_event_result(
                format!("{project}: task reviewed"),
                verdict.is_complete(),
                EventType::TaskReviewed,
                &project,
                throttle,
                &TaskReviewedPayload {
                    project: project.clone(),
                    objective,
                    review,
                    gate_results: p.results,
                    verdict,
                    context,
                },
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::parse_verdict;
    use foundry_sdk::payload::TaskVerdict;

    #[test]
    fn parses_terminal_fenced_verdict_structurally() {
        let output = "review notes\n```json\n{\"verdict\":\"remainder\",\"gaps\":[\"compare raw ids\"]}\n```";
        assert_eq!(
            parse_verdict(output).unwrap(),
            TaskVerdict::Remainder {
                gaps: vec!["compare raw ids".to_string()]
            }
        );
    }

    #[test]
    fn rejects_prose_pass_without_typed_verdict() {
        assert!(parse_verdict("VALIDATE: PASS").is_err());
    }
}
