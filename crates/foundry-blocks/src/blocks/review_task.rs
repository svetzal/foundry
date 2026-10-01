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

/// Linux `execve(2)`'s `MAX_ARG_STRLEN` (`PAGE_SIZE * 32`) bounds any single
/// argv string, independently of the much larger `ARG_MAX` covering argv and
/// the environment together. Confirmed 128 KiB (4 KiB pages) on
/// `mojility-ops-01`, the host that hit this. Since 0.40.4 the codex gateway
/// delivers the prompt on stdin (`codex exec -`), as the claude gateway does,
/// so the prompt is no longer an argv element (see the module doc comment on
/// `gateway/codex.rs`). The budget stays as defence in depth, and for any
/// provider that still passes its prompt in argv.
const MAX_ARG_STRLEN_BYTES: usize = 128 * 1024;

/// Ceiling for the whole rendered review prompt, leaving headroom below
/// [`MAX_ARG_STRLEN_BYTES`] for the fixed instructional text around the gate
/// block and for the marker text truncation itself adds.
const REVIEW_PROMPT_BUDGET_BYTES: usize = MAX_ARG_STRLEN_BYTES - 4 * 1024;

/// Output kept from one gate that PASSED. A passing gate's raw output
/// (dependency-cache "Fresh" lines, framework banners, and the like) is not
/// reviewer-relevant — the header line already carries the pass/fail signal
/// that matters. This is deliberately small.
const PASSING_GATE_OUTPUT_BUDGET: usize = 800;

/// Output kept from one gate that FAILED. Diagnosing a failure is the
/// reviewer's actual job, so this budget is generous — but still bounded,
/// since every gate's output shares one argv string.
const FAILING_GATE_OUTPUT_BUDGET: usize = 16 * 1024;

/// Hard ceiling on the whole rendered gate-results block, independent of how
/// many gates ran. The per-gate budgets above bound the common case
/// (verbose gates); this bounds the pathological one (many failing gates in
/// one run).
const GATES_SECTION_BUDGET_BYTES: usize = 64 * 1024;

/// Keep the last `max_bytes` of `s`, rounded outward to a UTF-8 char
/// boundary, prefixed with a marker naming how much was cut. A no-op when
/// `s` already fits — failures more often explain themselves at the end of a
/// command's output (the final compiler error, the failing assertion) than
/// at the start, so the tail is what's worth keeping.
fn cap_tail_bytes(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut start = s.len().saturating_sub(max_bytes);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!(
        "[... {} bytes omitted; showing the last {} ...]\n{}",
        start,
        s.len() - start,
        &s[start..]
    )
}

fn render_gate(g: &foundry_sdk::gates::GateResult) -> String {
    let budget = if g.passed {
        PASSING_GATE_OUTPUT_BUDGET
    } else {
        FAILING_GATE_OUTPUT_BUDGET
    };
    format!(
        "- {} [{}]: {} (exit {})\n{}",
        g.name,
        if g.required { "REQUIRED" } else { "OPTIONAL" },
        if g.passed { "PASS" } else { "FAIL" },
        g.exit_code,
        cap_tail_bytes(&g.output, budget)
    )
}

/// Render every gate's result, bounded so the block can never exceed
/// [`GATES_SECTION_BUDGET_BYTES`] regardless of gate count or verbosity.
fn render_gates(gate_results: &[foundry_sdk::gates::GateResult]) -> String {
    let joined = gate_results.iter().map(render_gate).collect::<Vec<_>>().join("\n");
    cap_tail_bytes(&joined, GATES_SECTION_BUDGET_BYTES)
}

fn render_review_prompt(objective: &str, gates: &str) -> String {
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

fn build_review_prompt(objective: &str, gate_results: &[foundry_sdk::gates::GateResult]) -> String {
    let gates = render_gates(gate_results);
    let prompt = render_review_prompt(objective, &gates);
    if prompt.len() <= REVIEW_PROMPT_BUDGET_BYTES {
        return prompt;
    }

    // The gate-output budgets above bound the confirmed cause (verbose
    // gates: a real incident measured 162 KB of gate output alone from an
    // all-passing run of `fmt`/`clippy`/`test`/`build`/`coverage`). Reaching
    // this branch means the OBJECTIVE text itself — campaign-formation
    // content this function does not own or size-check — is large enough to
    // blow the argv budget on its own. That is a gap upstream (see
    // `foundry_sdk::campaign::MAX_INLINE_CONTEXT_BYTES`, which is sized
    // against macOS's 1 MiB `ARG_MAX` and does not account for Linux's much
    // tighter 128 KiB `MAX_ARG_STRLEN`), not something this function can
    // properly fix. Log it loudly rather than truncate silently, then
    // truncate as a last resort so the review still runs instead of
    // crashing again.
    let overage = prompt.len() - REVIEW_PROMPT_BUDGET_BYTES;
    tracing::error!(
        objective_bytes = objective.len(),
        prompt_bytes = prompt.len(),
        budget_bytes = REVIEW_PROMPT_BUDGET_BYTES,
        "review prompt exceeds the codex argv budget even after capping gate output; the \
         objective text alone is over budget. Truncating it as a last resort — this is a \
         workaround, not a fix; objective size needs bounding at campaign formation."
    );
    let objective_budget = objective.len().saturating_sub(overage + 512);
    let truncated_objective = cap_tail_bytes(objective, objective_budget);
    render_review_prompt(&truncated_objective, &gates)
}

fn early_review_payload(
    project: &str,
    objective: String,
    gate_results: Vec<foundry_sdk::gates::GateResult>,
    context: LoopContext,
    verdict: TaskVerdict,
    review: String,
) -> TaskReviewedPayload {
    TaskReviewedPayload {
        project: project.into(),
        objective,
        gate_results,
        context,
        verdict,
        review,
    }
}

fn review_objective(context: &LoopContext) -> String {
    context
        .prompt
        .as_ref()
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Check evidence shape cheaply; the skeptical reviewer checks its meaning.
fn proof_error(repo: &std::path::Path) -> Option<String> {
    let path = repo.join(".foundry/proof.json");
    let check = || -> anyhow::Result<()> {
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        let kind = value["kind"].as_str().ok_or_else(|| anyhow::anyhow!("missing proof kind"))?;
        match kind {
            "behavioral" => {
                if value["source_change"].as_str().is_none_or(|s| s.trim().is_empty()) {
                    anyhow::bail!("behavioral proof requires source_change");
                }
                validate_probe(repo, &value["rejecting"], false)?;
            }
            "direct" if value["reason"].as_str().is_some_and(|s| !s.trim().is_empty()) => {}
            _ => anyhow::bail!(
                "proof kind must be behavioral, or direct with a non-behavioral reason"
            ),
        }
        validate_probe(repo, &value["corrected"], true)
    };
    check().err().map(|e| {
        format!("invalid or missing early acceptance evidence at .foundry/proof.json: {e}")
    })
}

fn validate_probe(
    repo: &std::path::Path,
    probe: &serde_json::Value,
    passing: bool,
) -> anyhow::Result<()> {
    let code = probe["exit_code"]
        .as_i64()
        .ok_or_else(|| anyhow::anyhow!("probe requires actual exit_code"))?;
    if (code == 0) != passing {
        anyhow::bail!("probe exit code does not establish the required result");
    }
    if probe["command"].as_str().is_none_or(|s| s.trim().is_empty()) {
        anyhow::bail!("probe requires command");
    }
    let log = probe["log"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("probe requires full log path"))?;
    if !repo.join(log).is_file() {
        anyhow::bail!("probe log is missing: {log}");
    }
    Ok(())
}

fn task_review_prompt(
    objective: &str,
    results: &[foundry_sdk::gates::GateResult],
    campaign: bool,
) -> String {
    let mut prompt = build_review_prompt(objective, results);
    if campaign {
        prompt.push_str("\nInspect .foundry/proof.json and its logs: verify the earliest acceptance probe exercised the hardest real boundary before broad expansion. Reject missing evidence, invented results, marker toggles, or failure cases unrelated to the objective. A documented non-behavioral objective may use a direct acceptance probe instead of red/green.");
    }
    prompt
}

fn campaign_proof_review(
    directory: &std::path::Path,
    throttle: Throttle,
    context: &LoopContext,
    objective: &str,
    project: &str,
    results: &[foundry_sdk::gates::GateResult],
) -> Option<TaskReviewedPayload> {
    if context.campaign_limits.is_none() || throttle == Throttle::DryRun {
        return None;
    }
    let reason = proof_error(directory)?;
    Some(early_review_payload(
        project,
        objective.into(),
        results.to_vec(),
        context.clone(),
        TaskVerdict::Defect {
            diagnosis: reason.clone(),
        },
        reason,
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
        let objective = review_objective(&context);
        if let Some(reason) = p.failure.needs_review.clone() {
            // Domain skip: see `stop_for_review`.
            let reviewed = needs_review_payload(&project, objective, reason, p.results, context);
            return stop_for_review(project, throttle, reviewed);
        }
        let working_dir = match context.task_worktree.clone() {
            Some(worktree) => PathBuf::from(worktree),
            None if throttle == Throttle::DryRun => PathBuf::from(&entry.path),
            None => {
                let detail = "task review missing isolated worktree".to_string();
                let reviewed = early_review_payload(
                    &project,
                    objective,
                    p.results,
                    context,
                    TaskVerdict::RunnerError {
                        detail: detail.clone(),
                    },
                    detail,
                );
                return stop_for_review(project, throttle, reviewed);
            }
        };
        if let Some(reviewed) = campaign_proof_review(
            &working_dir,
            throttle,
            &context,
            &objective,
            &project,
            &p.results,
        ) {
            return stop_for_review(project, throttle, reviewed);
        }
        let prompt = task_review_prompt(&objective, &p.results, context.campaign.is_some());
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
                    env: vec![("FOUNDRY_AGENT_STAGE".into(), "review".into())],
                    timeout: context
                        .campaign_limits
                        .as_ref()
                        .map_or(entry.timeout(), |_| std::time::Duration::ZERO),
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
    #[tokio::test]
    async fn missing_campaign_proof_stops_before_a_paid_review() {
        use crate::{blocks::test_helpers, gateway::fakes::FakeAgentGateway};
        use foundry_sdk::{event::EventType, task_block::TaskBlock};
        let dir = tempfile::tempdir().unwrap();
        let registry = test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with("unused");
        let block = super::ReviewTask::new(agent.clone(), registry);
        let trigger = test_event!(EventType::GateVerificationCompleted, "p", {
            "project":"p", "workflow":"task", "all_passed":true,
            "required_passed":true, "results":[], "retry_count":0,
            "task_worktree":dir.path().to_str().unwrap(), "campaign_limits":{}
        });
        let result = block.execute(&trigger).await.unwrap();
        assert!(agent.invocations().is_empty());
        assert_eq!(result.events[0].payload["verdict"], "defect");
        assert!(result.events[0].payload["diagnosis"].as_str().unwrap().contains("proof.json"));
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

    fn gate_with_output(name: &str, passed: bool, required: bool, output: String) -> GateResult {
        GateResult {
            name: name.to_string(),
            command: format!("run {name}"),
            passed,
            required,
            output,
            exit_code: i32::from(!passed),
            duration_ms: None,
            fix_applied: false,
        }
    }

    // Real incident (2026-09-30, mojility-ops-01, trace 6a4c0739a793765e8dc64ecc6cc440df):
    // a `parite` campaign_cycle task ran `fmt`, `clippy`, `test`, `build`,
    // `coverage`, `security-deny`, `security-audit` — every gate PASSED, but
    // `cargo test --verbose`'s 200-line-capped output alone measured 116,958
    // bytes, and the seven gates summed to 162,152 bytes of output before this
    // fix. `codex exec`'s prompt argv element exceeded Linux's 128 KiB
    // `MAX_ARG_STRLEN`, `execve` returned E2BIG ("Argument list too long"),
    // and the review step never ran — passing work stalled as `runner_error`.
    #[test]
    fn a_run_of_verbose_passing_gates_that_once_broke_execve_now_fits_the_argv_budget() {
        let verbose_passing_output = |lines: usize| -> String {
            (0..lines)
                .map(|i| {
                    format!(
                        "Fresh some-crate-{i} v1.2.3 (registry `crates-io`): compiled test::case_{i} ... ok"
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let gates = vec![
            gate_with_output("format", true, true, String::new()),
            gate_with_output("lint", true, true, verbose_passing_output(6)),
            gate_with_output("test", true, true, verbose_passing_output(200)),
            gate_with_output("build", true, true, verbose_passing_output(200)),
            gate_with_output("coverage", true, true, verbose_passing_output(200)),
            gate_with_output("security-deny", false, false, verbose_passing_output(199)),
            gate_with_output("security-audit", true, false, verbose_passing_output(10)),
        ];
        let objective = "Close only the missing production probe-input sub-gap".repeat(20);

        let prompt = build_review_prompt(&objective, &gates);

        assert!(
            prompt.len() <= super::REVIEW_PROMPT_BUDGET_BYTES,
            "prompt was {} bytes, over the {}-byte budget",
            prompt.len(),
            super::REVIEW_PROMPT_BUDGET_BYTES
        );
        assert!(
            prompt.len() < super::MAX_ARG_STRLEN_BYTES,
            "prompt of {} bytes would still overflow execve's MAX_ARG_STRLEN ({} bytes)",
            prompt.len(),
            super::MAX_ARG_STRLEN_BYTES
        );
    }

    #[test]
    fn a_single_failing_gate_keeps_far_more_diagnostic_output_than_a_passing_one() {
        let huge = "x".repeat(200_000);
        let passing =
            build_review_prompt("obj", &[gate_with_output("g", true, true, huge.clone())]);
        let failing = build_review_prompt("obj", &[gate_with_output("g", false, true, huge)]);

        assert!(
            failing.len() > passing.len(),
            "a failing gate must retain more of its output than a passing one"
        );
        assert!(passing.len() <= super::REVIEW_PROMPT_BUDGET_BYTES);
        assert!(failing.len() <= super::REVIEW_PROMPT_BUDGET_BYTES);
    }

    #[test]
    fn many_failing_gates_stay_within_the_argv_budget() {
        let huge = "x".repeat(50_000);
        let gates: Vec<GateResult> = (0..20)
            .map(|i| gate_with_output(&format!("gate-{i}"), false, true, huge.clone()))
            .collect();

        let prompt = build_review_prompt("obj", &gates);

        assert!(
            prompt.len() < super::MAX_ARG_STRLEN_BYTES,
            "20 failing 50KB gates produced a {}-byte prompt, over MAX_ARG_STRLEN ({} bytes)",
            prompt.len(),
            super::MAX_ARG_STRLEN_BYTES
        );
    }

    // The gate-output budgets bound the confirmed cause of the incident, but
    // the objective text comes from campaign formation, which this function
    // does not control (see `foundry_sdk::campaign::MAX_INLINE_CONTEXT_BYTES`,
    // sized against macOS's 1 MiB ARG_MAX rather than Linux's 128 KiB
    // MAX_ARG_STRLEN). This proves the last-resort branch still keeps the
    // daemon from crashing even when the objective alone is pathological.
    #[test]
    fn a_pathologically_large_objective_is_truncated_rather_than_crashing_execve_again() {
        let huge_objective = "acceptance criterion. ".repeat(20_000); // ~460 KB
        assert!(huge_objective.len() > super::MAX_ARG_STRLEN_BYTES);

        let prompt = build_review_prompt(&huge_objective, &[]);

        assert!(
            prompt.len() < super::MAX_ARG_STRLEN_BYTES,
            "prompt of {} bytes would still overflow execve's MAX_ARG_STRLEN ({} bytes)",
            prompt.len(),
            super::MAX_ARG_STRLEN_BYTES
        );
        assert!(
            prompt.contains("bytes omitted"),
            "truncation of the oversized objective must be visible in the prompt, not silent"
        );
    }

    #[test]
    fn cap_tail_bytes_is_a_no_op_under_budget() {
        assert_eq!(super::cap_tail_bytes("short", 100), "short");
    }

    #[test]
    fn cap_tail_bytes_rounds_outward_to_a_char_boundary() {
        // "é" is 2 bytes in UTF-8; cutting at byte 1 would land mid-character.
        let s = "aéb";
        let capped = super::cap_tail_bytes(s, 2);
        assert!(capped.is_char_boundary(capped.len() - 2));
        assert!(String::from_utf8(capped.into_bytes()).is_ok());
    }
    #[test]
    fn early_proof_needs_actual_rejection_correction_and_existing_logs() {
        let dir = tempfile::tempdir().unwrap();
        assert!(super::proof_error(dir.path()).is_some());
        std::fs::create_dir(dir.path().join(".foundry")).unwrap();
        std::fs::write(dir.path().join("reject.log"), "assertion failed").unwrap();
        std::fs::write(dir.path().join("pass.log"), "assertion passed").unwrap();
        let mut proof = serde_json::json!({"kind":"behavioral", "source_change":"src/core.rs",
            "rejecting":{"command":"test core","exit_code":1,"log":"reject.log"},
            "corrected":{"command":"test core","exit_code":0,"log":"pass.log"}});
        let path = dir.path().join(".foundry/proof.json");
        std::fs::write(&path, proof.to_string()).unwrap();
        assert!(super::proof_error(dir.path()).is_none());
        proof["rejecting"]["exit_code"] = serde_json::json!(0);
        std::fs::write(&path, proof.to_string()).unwrap();
        assert!(super::proof_error(dir.path()).is_some());
        proof["rejecting"]["exit_code"] = serde_json::json!(1);
        proof["corrected"]["log"] = serde_json::json!("absent.log");
        std::fs::write(&path, proof.to_string()).unwrap();
        assert!(super::proof_error(dir.path()).is_some());
    }
}
