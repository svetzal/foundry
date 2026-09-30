use std::path::PathBuf;
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    GateVerificationCompletedPayload, LoopContext, TaskReviewedPayload, TaskVerdict,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock};
use foundry_sdk::throttle::Throttle;
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
                "- {} [{}]: {} (exit {})\n{}",
                g.name,
                if g.required { "REQUIRED" } else { "OPTIONAL" },
                if g.passed { "PASS" } else { "FAIL" },
                g.exit_code,
                g.output
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    super::with_single_turn_discipline(&format!(
        "You are the skeptical reviewer for a one-shot engineering task. Inspect the actual source, diff, and tests in the current worktree; do not trust the executor's self-report.\n\n\
         OBJECTIVE (every acceptance/evidence phrase is binding):\n{objective}\n\n\
         MECHANICAL GATE RESULTS:\n{gates}\n\n\
         Decide exactly one typed verdict. COMPLETE requires every objective and evidence requirement to be satisfied and all REQUIRED gates to pass. OPTIONAL gate failures are advisory and must not block COMPLETE unless the objective itself explicitly makes that result acceptance evidence. This review runs before Finalize Task: tracked modifications and untracked new files are both valid deliverable changes and Finalize Task commits them after a COMPLETE verdict, so do not reject work merely because it is untracked or uncommitted. Tests that mask identifiers, compare only counts, inject around the real boundary, or otherwise cannot detect the stated defect do not count. When the objective requires proof through a real or generated boundary, at least one test must exercise that boundary itself and assert its observable request or response; a mock, fake, or stub installed above that boundary is insufficient. Use REMAINDER only for a finite list of missing work on a converging implementation. Use DEFECT for a faulty approach or regression. Use BLOCKED_ON_DECISION when reality exposes a genuine product/policy choice that makes the objective unsatisfiable as written.\n\n\
         End with exactly one JSON object in a fenced json block, using one of these shapes:\n\
         {{\"verdict\":\"complete\"}}\n\
         {{\"verdict\":\"remainder\",\"gaps\":[\"specific gap\"]}}\n\
         {{\"verdict\":\"defect\",\"diagnosis\":\"specific diagnosis\"}}\n\
         {{\"verdict\":\"blocked_on_decision\",\"finding\":\"finding\",\"options\":[\"option\"]}}"
    ))
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
            trace_id,
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
        if let Some(reason) = p.failure.needs_review.clone() {
            // Domain skip: see `stop_for_review`.
            let reviewed = needs_review_payload(&project, objective, reason, p.results, context);
            return stop_for_review(project, throttle, reviewed);
        }
        let working_dir = match context.task_worktree.clone() {
            Some(worktree) => PathBuf::from(worktree),
            None if throttle == Throttle::DryRun => PathBuf::from(&entry.path),
            None => {
                return Box::pin(async move {
                    let detail = "task review missing isolated worktree".to_string();
                    super::emit_event_result(
                        format!("{project}: {detail}"),
                        false,
                        EventType::TaskReviewed,
                        &project,
                        throttle,
                        &TaskReviewedPayload {
                            project: project.clone(),
                            objective,
                            review: detail.clone(),
                            gate_results: p.results,
                            verdict: TaskVerdict::RunnerError { detail },
                            context,
                        },
                    )
                });
            }
        };
        let prompt = build_review_prompt(&objective, &p.results);
        let provider = super::chain_agent_provider(&payload);

        Box::pin(async move {
            let outcome = invoke_agent(
                &*agent,
                AgentBlockSpec {
                    prompt,
                    working_dir,
                    access: AgentAccess::ReadOnly,
                    tier: ModelTier::Deep,
                    effort: ReasoningEffort::High,
                    agent_file: super::resolve_agent_file(&entry.agent),
                    provider,
                    env: Vec::new(),
                    timeout: entry.timeout(),
                    trace_id: trace_id.clone(),
                    requires_json: true,
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

/// The review of a task Foundry's post-agent checks stopped: the agent
/// pushed, or added an advisory suppression. Accepting that is a person's
/// decision, not the reviewer's, so no review session runs and the verdict
/// keeps the task from landing.
fn needs_review_payload(
    project: &str,
    objective: String,
    reason: String,
    gate_results: Vec<foundry_sdk::gates::GateResult>,
    context: LoopContext,
) -> TaskReviewedPayload {
    TaskReviewedPayload {
        project: project.to_string(),
        objective,
        review: reason.clone(),
        gate_results,
        verdict: needs_review_verdict(reason),
        context,
    }
}

fn stop_for_review(
    project: String,
    throttle: Throttle,
    reviewed: TaskReviewedPayload,
) -> foundry_sdk::task_block::BlockFuture<'static> {
    tracing::warn!(project = %project, reason = %reviewed.review, "task needs review; skipping the reviewer");
    Box::pin(async move {
        super::emit_event_result(
            format!("{project}: task needs review"),
            false,
            EventType::TaskReviewed,
            &project,
            throttle,
            &reviewed,
        )
    })
}

/// The verdict for a task Foundry's post-agent checks stopped.
fn needs_review_verdict(reason: String) -> TaskVerdict {
    TaskVerdict::BlockedOnDecision {
        finding: reason,
        options: vec![
            "Remove the flagged change (upgrade instead of suppressing) and run the task again"
                .to_string(),
            "Accept the advisory yourself, in a commit of your own, then run the task again"
                .to_string(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::{build_review_prompt, parse_verdict};

    #[tokio::test]
    async fn a_task_foundry_stopped_for_review_is_blocked_without_a_review_session() {
        use foundry_sdk::event::EventType;
        use foundry_sdk::task_block::TaskBlock;

        use crate::blocks::test_helpers;
        use crate::gateway::fakes::FakeAgentGateway;

        let reason = "needs review: this run added advisory suppressions, which an agent must \
                      never do; upgrade to the fixed release instead: CVE-2026-64941";
        let registry = test_helpers::registry_with_project("p", "/nonexistent");
        let agent = FakeAgentGateway::success_with("```json\n{\"verdict\":\"complete\"}\n```");
        let block = super::ReviewTask::new(agent.clone(), registry);
        let trigger = test_event!(EventType::GateVerificationCompleted, "p", {
            "project": "p",
            "workflow": "task",
            "all_passed": false,
            "required_passed": false,
            "results": [],
            "retry_count": 0,
            "needs_review": reason,
            "task_worktree": "/nonexistent/worktree",
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(agent.invocations().is_empty(), "the reviewer must not be asked");
        assert!(!result.success);
        let payload = &result.events[0].payload;
        assert_eq!(payload["verdict"], "blocked_on_decision");
        assert_eq!(payload["finding"], reason);
    }
    use foundry_sdk::gates::GateResult;
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
    fn parses_terminal_fenced_complete_after_brace_notation_prose() {
        let output = "The earlier gate{command,required} example is explanatory prose.\n```json\n{\"verdict\":\"complete\"}\n```";
        assert_eq!(parse_verdict(output).unwrap(), TaskVerdict::Complete);
    }

    #[test]
    fn rejects_prose_pass_without_typed_verdict() {
        assert!(parse_verdict("VALIDATE: PASS").is_err());
    }

    #[test]
    fn review_prompt_labels_gate_criticality_and_explains_finalize_boundary() {
        let gates = vec![
            GateResult {
                name: "test".to_string(),
                command: "cargo test".to_string(),
                passed: true,
                required: true,
                output: String::new(),
                exit_code: 0,
                duration_ms: None,
                fix_applied: false,
            },
            GateResult {
                name: "dialyzer".to_string(),
                command: "mix dialyzer".to_string(),
                passed: false,
                required: false,
                output: "baseline warnings".to_string(),
                exit_code: 2,
                duration_ms: None,
                fix_applied: false,
            },
        ];

        let prompt = build_review_prompt("ship the slice", &gates);

        assert!(prompt.contains("test [REQUIRED]: PASS"));
        assert!(prompt.contains("dialyzer [OPTIONAL]: FAIL"));
        assert!(prompt.contains("OPTIONAL gate failures are advisory"));
        assert!(prompt.contains("untracked new files are both valid deliverable changes"));
        assert!(prompt.contains("test must exercise that boundary itself"));
        assert!(
            prompt.contains("mock, fake, or stub installed above that boundary is insufficient")
        );
    }

    // A reviewer that backgrounds a command and ends its turn waiting for a
    // notification returns prose, and the verdict parse fails on a non-result.
    #[test]
    fn review_prompt_ends_with_the_single_turn_discipline() {
        let prompt = build_review_prompt("ship the slice", &[]);
        assert!(
            prompt.trim_end().ends_with(super::super::SINGLE_TURN_JSON_DISCIPLINE),
            "reviewer prompt must close with the single-turn rule: {prompt}"
        );
    }
}
