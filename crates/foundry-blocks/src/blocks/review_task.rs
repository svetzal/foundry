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
use crate::workspace::{BLOCKER_FILE, PROOF_FILE};

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
    // Section-aware: a byte tail of the whole view would keep only stderr
    // when a tool floods it, dropping the stdout line that names the failure.
    let logs = crate::gate_runner::full_logs_note(&g.logs)
        .map(|note| format!("\n({note})"))
        .unwrap_or_default();
    format!(
        "- {} [{}]: {} (exit {})\n{}{logs}",
        g.name,
        if g.required { "REQUIRED" } else { "OPTIONAL" },
        if g.passed { "PASS" } else { "FAIL" },
        g.exit_code,
        crate::gate_runner::output::compact_output(&g.output, budget)
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
         Decide exactly one typed verdict. COMPLETE requires every objective and evidence requirement to be satisfied and all REQUIRED gates to pass. OPTIONAL gate failures are advisory and must not block COMPLETE unless the objective itself explicitly makes that result acceptance evidence. This review runs before Finalize Task: tracked modifications and untracked new files are both valid deliverable changes and Finalize Task commits them after a COMPLETE verdict, so do not reject work merely because it is untracked or uncommitted. Files under .foundry/ are Foundry scratch and never land, so a deliverable that cites .foundry/ paths as repository content, or that leaves durable project material only under .foundry/, is a defect. Tests that mask identifiers, compare only counts, bypass the project's public API, or otherwise cannot detect the stated defect do not count. When the objective requires proof through a public or generated boundary, at least one test must drive that boundary the way a caller does and assert its observable request or response, with I/O stubbed in process at the transport or gateway; a double that replaces the code under test or mocks library internals is insufficient. Do not demand real servers, sockets, live services or wall-clock timing as proof. Use REMAINDER only for a finite list of missing work on a converging implementation. Use DEFECT for a faulty approach or regression. Use BLOCKED_ON_RECOVERY for a missing operational capability, access, environment or evidence, with a typed recovery kind and exact missing prerequisite. Use BLOCKED_ON_DECISION when reality exposes a genuine product/policy choice that makes the objective unsatisfiable as written.\n\n\
         {test_design}\n\n\
         {seam_review}\n\n\
         End with exactly one JSON object in a fenced json block, using one of these shapes:\n\
         {{\"verdict\":\"complete\"}}\n\
         {{\"verdict\":\"remainder\",\"gaps\":[\"specific gap\"]}}\n\
         {{\"verdict\":\"defect\",\"diagnosis\":\"specific diagnosis\"}}\n\
         {{\"verdict\":\"blocked_on_recovery\",\"kind\":\"capability_unavailable\",\"detail\":\"missing prerequisite\"}}\n\
         {{\"verdict\":\"blocked_on_decision\",\"finding\":\"finding\",\"options\":[\"option\"]}}",
        test_design = super::agent_helpers::TEST_DESIGN_GUIDANCE,
        seam_review = SEAM_REVIEW_RULE,
    ))
}

/// How review applies the shared rule's seam pre-authorization. The shared
/// rule tells executors they may add a seam; without this sentence a reviewer
/// could still read a new constructor parameter or initializer as scope creep,
/// or read the pre-authorization as permission to change the public API.
const SEAM_REVIEW_RULE: &str = "SEAMS IN REVIEW: accept an internal (non-public) injection seam for a clock, sleeper, transport or scheduler that removes real network or temporal or ordering coupling, when the public API and default production behaviour are unchanged and no dependency is added. It is in scope; never return DEFECT or BLOCKED_ON_DECISION for it. A public API change the objective does not license is a DEFECT, even when it was made for testability.";

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
        failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
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

/// The executor's own account of why it could not reach the acceptance
/// boundary, read from [`BLOCKER_FILE`].
struct Blocker {
    recovery_kind: Option<foundry_sdk::recovery::RecoveryKind>,
    tried: String,
    missing: String,
    question: String,
    options: Vec<String>,
}

/// The owner's choices when the executor offered none.
const DEFAULT_BLOCKER_OPTIONS: [&str; 2] = [
    "Provide the missing capability, access or decision, then run the objective again",
    "Change the objective so it no longer needs what is missing",
];

impl Blocker {
    /// The executor's question first, because the campaign shows the finding
    /// as the question the owner answers; what is missing and what was tried
    /// follow in the executor's own words.
    fn finding(&self) -> String {
        if self.question.is_empty() {
            format!("missing: {}; tried: {}", self.missing, self.tried)
        } else {
            format!("{} (missing: {}; tried: {})", self.question, self.missing, self.tried)
        }
    }

    fn into_verdict(self, finding: String) -> TaskVerdict {
        if let Some(kind) = self.recovery_kind {
            return TaskVerdict::BlockedOnRecovery {
                kind,
                detail: finding,
            };
        }
        let options = if self.options.is_empty() {
            DEFAULT_BLOCKER_OPTIONS.iter().map(ToString::to_string).collect()
        } else {
            self.options
        };
        TaskVerdict::BlockedOnDecision { finding, options }
    }
}

/// Read the executor's blocker record. `None` when there is none, so the
/// proof check runs as before; an `Err` names what is wrong with the record.
fn read_blocker(repo: &std::path::Path) -> Option<Result<Blocker, String>> {
    let path = repo.join(BLOCKER_FILE);
    // An unanswerable existence check falls through to the read, which
    // reports the fault as an invalid blocker rather than ignoring it.
    if matches!(path.try_exists(), Ok(false)) {
        return None;
    }
    let parse = || -> anyhow::Result<Blocker> {
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        require_object(&value, "blocker")?;
        if require_string(&value["kind"], "kind")? != "blocked" {
            anyhow::bail!("kind must be \"blocked\"");
        }
        let text = |field: &str| -> anyhow::Result<String> {
            let text = require_string(&value[field], field)?.trim();
            if text.is_empty() {
                anyhow::bail!("{field} must not be empty");
            }
            Ok(text.to_owned())
        };
        let options = match &value["options"] {
            serde_json::Value::Null => Vec::new(),
            serde_json::Value::Array(options) => options
                .iter()
                .map(|option| match option.as_str().map(str::trim) {
                    Some(choice) if !choice.is_empty() => Ok(choice.to_owned()),
                    _ => Err(anyhow::anyhow!(
                        "options must hold only non-empty strings, found {}",
                        json_type(option)
                    )),
                })
                .collect::<anyhow::Result<_>>()?,
            other => anyhow::bail!("options must be an array, found {}", json_type(other)),
        };
        let recovery_kind = value
            .get("recovery_kind")
            .map(|kind| serde_json::from_value(kind.clone()))
            .transpose()?;
        Ok(Blocker {
            recovery_kind,
            tried: text("tried")?,
            missing: text("missing")?,
            question: if recovery_kind.is_some() && value.get("question").is_none() {
                String::new()
            } else {
                text("question")?
            },
            options,
        })
    };
    Some(parse().map_err(|e| format!("invalid execution blocker at {BLOCKER_FILE}: {e}")))
}

/// Check evidence shape cheaply; the skeptical reviewer checks its meaning.
fn proof_error(repo: &std::path::Path) -> Option<String> {
    let path = repo.join(PROOF_FILE);
    let check = || -> anyhow::Result<()> {
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&path)?)?;
        require_object(&value, "proof")?;
        let kind = require_string(&value["kind"], "kind")?;
        match kind {
            "behavioral" => {
                let source_change = match &value["source_change"] {
                    serde_json::Value::Array(paths)
                        if !paths.is_empty()
                            && paths
                                .iter()
                                .all(|p| p.as_str().is_some_and(|s| !s.trim().is_empty())) =>
                    {
                        paths
                            .iter()
                            .filter_map(serde_json::Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n")
                    }
                    other => require_string(other, "source_change")?.to_owned(),
                };
                if source_change.trim().is_empty() {
                    anyhow::bail!("behavioral proof requires non-empty source_change");
                }
                validate_probe(repo, &value["rejecting"], "rejecting", false)?;
            }
            "direct" => {
                if require_string(&value["reason"], "reason")?.trim().is_empty() {
                    anyhow::bail!("direct proof requires a non-behavioral reason");
                }
            }
            _ => anyhow::bail!(
                "proof kind must be behavioral, or direct with a non-behavioral reason"
            ),
        }
        validate_probe(repo, &value["corrected"], "corrected", true)
    };
    check()
        .err()
        .map(|e| format!("invalid or missing early acceptance evidence at {PROOF_FILE}: {e}"))
}

fn json_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

fn require_string<'a>(value: &'a serde_json::Value, field: &str) -> anyhow::Result<&'a str> {
    value
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("{field} must be a string, found {}", json_type(value)))
}

fn require_object(value: &serde_json::Value, field: &str) -> anyhow::Result<()> {
    if !value.is_object() {
        anyhow::bail!("{field} must be an object, found {}", json_type(value));
    }
    Ok(())
}

fn validate_probe(
    repo: &std::path::Path,
    probe: &serde_json::Value,
    field: &str,
    passing: bool,
) -> anyhow::Result<()> {
    require_object(probe, field)?;
    let code = probe["exit_code"].as_i64().ok_or_else(|| {
        anyhow::anyhow!(
            "{field}.exit_code must be an integer, found {}",
            json_type(&probe["exit_code"])
        )
    })?;
    if (code == 0) != passing {
        anyhow::bail!("probe exit code does not establish the required result");
    }
    if require_string(&probe["command"], &format!("{field}.command"))?
        .trim()
        .is_empty()
    {
        anyhow::bail!("probe requires command");
    }
    let log = require_string(&probe["log"], &format!("{field}.log"))?;
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
        prompt.push_str("\nInspect .foundry/proof.json and its logs: verify the earliest acceptance probe exercised the hardest acceptance behavior through the project's public API, with I/O stubbed in process, before broad expansion. A probe that opens real sockets or local servers, calls external services or asserts wall-clock thresholds is a defect unless the project's own guidance designates that suite as integration or end-to-end. Reject missing evidence, invented results, marker toggles, or failure cases unrelated to the objective. A documented non-behavioral objective may use a direct acceptance probe instead of red/green.");
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
    if throttle == Throttle::DryRun {
        return None;
    }
    let (verdict, review) = if context.campaign_limits.is_some() {
        early_evidence_verdict(directory)?
    } else {
        match read_blocker(directory) {
            Some(Ok(blocker)) if blocker.recovery_kind.is_some() => {
                let detail = blocker.finding();
                (
                    blocker.into_verdict(detail.clone()),
                    format!("execution blocked before the acceptance boundary: {detail}"),
                )
            }
            Some(Err(reason)) => defect(reason),
            _ => return None,
        }
    };
    Some(early_review_payload(
        project,
        objective.into(),
        results.to_vec(),
        context.clone(),
        verdict,
        review,
    ))
}

/// The verdict and review text when a campaign cycle's early evidence stops it
/// before a paid review, or `None` when the proof is sound.
///
/// A blocker comes first: an executor that could not reach the acceptance
/// boundary owes the owner a question, not proof, and calling the absent proof
/// a defect would carry the same blocked objective into the next cycle.
fn early_evidence_verdict(directory: &std::path::Path) -> Option<(TaskVerdict, String)> {
    match read_blocker(directory) {
        Some(Ok(blocker)) => {
            let finding = blocker.finding();
            let review = format!("execution blocked before the acceptance boundary: {finding}");
            Some((blocker.into_verdict(finding), review))
        }
        Some(Err(reason)) => Some(defect(reason)),
        None => proof_error(directory).map(defect),
    }
}

fn defect(reason: String) -> (TaskVerdict, String) {
    (
        TaskVerdict::Defect {
            diagnosis: reason.clone(),
        },
        reason,
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
            Some(worktree) => Some(PathBuf::from(worktree)),
            None if throttle == Throttle::DryRun => Some(PathBuf::from(&entry.path)),
            None => None,
        };
        if let Some(reviewed) =
            failed_session_review(&project, &objective, &p, &context, working_dir.as_deref())
        {
            return settle_without_review(project, throttle, reviewed);
        }
        let Some(working_dir) = working_dir else {
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
        let prompt = super::with_initiative_context(
            &task_review_prompt(&objective, &p.results, context.campaign.is_some()),
            &payload,
        );
        let provider =
            super::chain_stage_provider(&payload, foundry_sdk::gateway::AgentStage::Review);

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

            let (review, verdict) = review_outcome(outcome);

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
                    failure: p.failure,
                },
            )
        })
    }
}

fn review_outcome(outcome: crate::gateway::AgentOutcome) -> (String, TaskVerdict) {
    match outcome {
        crate::gateway::AgentOutcome::Success { stdout } => {
            let verdict = parse_verdict(&stdout).unwrap_or_else(|error| TaskVerdict::Defect {
                diagnosis: error.to_string(),
            });
            (stdout, verdict)
        }
        crate::gateway::AgentOutcome::AgentFailed { stderr, failure } => {
            let detail = failure.map_or(stderr.clone(), |failure| failure.execution_summary());
            (stderr, TaskVerdict::RunnerError { detail })
        }
        crate::gateway::AgentOutcome::Unavailable { error } => {
            (error.clone(), TaskVerdict::RunnerError { detail: error })
        }
    }
}

/// The review of a run whose execution session did none of the objective's
/// work, or `None` when the run is to be reviewed as usual.
///
/// There is nothing to review and no proof to expect, so the run settles as
/// a runner error naming the session's failure, before the proof check and
/// without a review session. Reporting missing `.foundry/proof.json` as a
/// defect would tell campaign formation the work was wrong when it never ran
/// (the 2026-10-19 Codex capacity outage). Two cases qualify:
///
/// - A classified provider failure (the provider unavailable, or a terminal
///   account state): the provider refused the session. The detail names the
///   failure kind (`provider unavailable: <message>`).
/// - Any other failed session ([`GateVerificationCompletedPayload::session_failure`])
///   that changed no project file and wrote no early evidence to the
///   worktree. The detail is the session's own failure message. A failed
///   session that changed files, or that wrote `.foundry/proof.json` or
///   `.foundry/blocker.json`, did work and goes through the proof check and
///   the reviewer as usual.
fn failed_session_review(
    project: &str,
    objective: &str,
    verification: &GateVerificationCompletedPayload,
    context: &LoopContext,
    working_dir: Option<&std::path::Path>,
) -> Option<TaskReviewedPayload> {
    let failure = &verification.failure;
    let (detail, review) = if failure.failure_kind.is_some() {
        let detail = failure.execution_summary();
        let review = format!("execution did not run the objective; review skipped: {detail}");
        (detail, review)
    } else {
        let session = verification.session_failure.as_ref()?;
        if session.left_work || working_dir.is_none_or(wrote_early_evidence) {
            return None;
        }
        let detail = session.message.clone();
        let review = format!(
            "execution session failed before doing any of the objective's work; review skipped: {detail}"
        );
        (detail, review)
    };
    Some(TaskReviewedPayload {
        project: project.to_string(),
        objective: objective.to_string(),
        review,
        gate_results: verification.results.clone(),
        verdict: TaskVerdict::RunnerError { detail },
        failure: failure.clone(),
        context: context.clone(),
    })
}

/// Whether the executor left an early-evidence record (proof or blocker) in
/// `directory`. An existence check that cannot be answered counts as one, so
/// the proof check reads it and reports the fault.
fn wrote_early_evidence(directory: &std::path::Path) -> bool {
    [PROOF_FILE, BLOCKER_FILE]
        .iter()
        .any(|file| !matches!(directory.join(file).try_exists(), Ok(false)))
}

fn settle_without_review(
    project: String,
    throttle: Throttle,
    reviewed: TaskReviewedPayload,
) -> foundry_sdk::task_block::BlockFuture<'static> {
    tracing::warn!(project = %project, reason = %reviewed.review, "execution session did none of the objective's work; skipping the reviewer");
    Box::pin(async move {
        super::emit_event_result(
            format!("{project}: task not reviewed, execution session did no work"),
            false,
            EventType::TaskReviewed,
            &project,
            throttle,
            &reviewed,
        )
    })
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
        failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
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
    use super::{LoopContext, Throttle, build_review_prompt, campaign_proof_review, parse_verdict};

    #[tokio::test]
    async fn reviewer_receives_its_provider_and_shared_brief_identity() {
        use crate::{blocks::test_helpers, gateway::fakes::FakeAgentGateway};
        use foundry_sdk::{event::EventType, gateway::AgentProvider, task_block::TaskBlock};
        let agent = FakeAgentGateway::success_with("{\"verdict\":\"complete\"}");
        let block = super::ReviewTask::new(
            agent.clone(),
            test_helpers::registry_with_project("p", "/repo"),
        );
        let trigger = test_event!(EventType::GateVerificationCompleted, "p", {
            "project":"p", "workflow":"task", "all_passed":true, "required_passed":true,
            "results":[], "retry_count":0, "task_worktree":"/repo/worktree", "prompt":"fix the diagnosis",
            "agent_provider":"codex", "providers":{"execution":"codex", "review":"claude"},
            "initiative_brief":{"path":"/brief/BRIEF.md","hash":"abc123"},
            "initiative_findings":[{"id":"fnd_1","initiative":"i", "from":{"kind":"campaign","id":"other"}, "kind":"regression","text":"shared failure diagnosis"}]
        });
        block.execute(&trigger).await.unwrap();
        let requests = agent.invocations();
        assert_eq!(requests[0].provider, Some(AgentProvider::Claude));
        assert!(requests[0].prompt.contains("/brief/BRIEF.md"));
        assert!(requests[0].prompt.contains("abc123"));
        assert!(requests[0].prompt.contains("shared failure diagnosis"));
    }

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

    /// The 2026-10-19 incident: Codex was at capacity, execution never
    /// started, and review called the absent proof a defect. A run the
    /// provider refused is a runner error naming the outage, with no review
    /// session and no proof check.
    #[tokio::test]
    async fn a_provider_refused_execution_is_a_runner_error_not_a_missing_proof_defect() {
        use crate::{blocks::test_helpers, gateway::fakes::FakeAgentGateway};
        use foundry_sdk::{event::EventType, task_block::TaskBlock};
        let dir = tempfile::tempdir().unwrap();
        let registry = test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with("```json\n{\"verdict\":\"complete\"}\n```");
        let block = super::ReviewTask::new(agent.clone(), registry);
        let trigger = test_event!(EventType::GateVerificationCompleted, "p", {
            "project":"p", "workflow":"task", "all_passed":false,
            "required_passed":false, "retry_count":0,
            "results":[{"name":"agent_execution","command":"","passed":false,"required":true,
                "output":"provider unavailable: Selected model is at capacity.","exit_code":1}],
            "provider":"codex", "failure_kind":"provider_unavailable",
            "message":"Selected model is at capacity. Please try a different model.",
            "task_worktree":dir.path().to_str().unwrap(), "campaign_limits":{}
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(agent.invocations().is_empty(), "the reviewer must not be asked");
        assert!(!result.success);
        let payload = &result.events[0].payload;
        assert_eq!(result.events[0].event_type, EventType::TaskReviewed);
        assert_eq!(payload["verdict"], "runner_error");
        assert_eq!(
            payload["detail"],
            "provider unavailable: Selected model is at capacity. Please try a different model."
        );
        assert_eq!(payload["failure_kind"], "provider_unavailable");
        assert!(!payload.to_string().contains("proof.json"), "{payload}");
    }

    /// Review a campaign cycle the way the task chain does: the execution
    /// session ended with `outcome` having changed `changed` (as change
    /// detection reports it), its result goes through Run Verify Gates, and
    /// that verification triggers the review. `worktree` files are written
    /// to the task worktree first. The reviewer would answer `complete` if
    /// asked. Returns the `TaskReviewed` payload and how often it was asked.
    async fn review_after_session(
        outcome: crate::gateway::AgentOutcome,
        changed: &[&str],
        worktree: &[(&str, &str)],
    ) -> (serde_json::Value, usize) {
        use crate::blocks::execution::{
            ExecutionContext, SessionEnd, build_agent_execution_result,
        };
        use crate::{blocks::test_helpers, gateway::fakes::FakeAgentGateway};
        use foundry_sdk::task_block::TaskBlock;
        use foundry_sdk::workflow::WorkflowType;
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".foundry")).unwrap();
        for (path, content) in worktree {
            std::fs::write(dir.path().join(path), content).unwrap();
        }
        let registry = test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let dispatch = serde_json::json!({
            "project": "p", "workflow": "task", "prompt": "Parse the config file.",
            "campaign": "c", "campaign_limits": {},
            "task_worktree": dir.path().to_str().unwrap(),
        });
        let ctx = ExecutionContext {
            project: "p",
            trace_id: None,
            workflow: WorkflowType::Task,
            payload: &dispatch,
            throttle: Throttle::Full,
            label: "plan execution",
            retry_count: None,
            correction_needed: true,
        };
        let session = SessionEnd::of(&outcome);
        let executed = build_agent_execution_result(
            &ctx,
            outcome,
            session,
            !changed.is_empty(),
            changed.iter().map(ToString::to_string).collect(),
        );
        let verified = crate::blocks::RunVerifyGates::new(
            crate::gateway::fakes::FakeShellGateway::success(),
            registry.clone(),
        )
        .execute(&executed.events[0])
        .await
        .unwrap();
        let agent = FakeAgentGateway::success_with("```json\n{\"verdict\":\"complete\"}\n```");
        let reviewed = super::ReviewTask::new(agent.clone(), registry)
            .execute(&verified.events[0])
            .await
            .unwrap();
        (reviewed.events[0].payload.clone(), agent.invocations().len())
    }

    /// Cycles 3 and 5 of mojentic-py-no-network-tests on 2026-10-19: the
    /// Codex session held only an error event that no gateway rule
    /// recognised, and review reported missing proof as a defect. A failed
    /// session that did no work, unclassified or unable to start at all, is
    /// a runner error carrying the session's message, with no reviewer and
    /// no proof check.
    #[tokio::test]
    async fn a_failed_session_that_did_no_work_is_a_runner_error_not_a_missing_proof_defect() {
        use crate::gateway::{AgentFailureMetadata, AgentOutcome, AgentProvider};
        let unrecognised = "unexpected error event from the model stream";
        for (outcome, detail) in [
            (
                AgentOutcome::AgentFailed {
                    stderr: "Reading additional input from stdin...".into(),
                    failure: Some(
                        AgentFailureMetadata::new(AgentProvider::Codex).with_message(unrecognised),
                    ),
                },
                format!("plan execution failed: {unrecognised}"),
            ),
            (
                AgentOutcome::Unavailable {
                    error: "codex stream produced no output for 600s".into(),
                },
                "agent unavailable: codex stream produced no output for 600s".to_string(),
            ),
        ] {
            let (payload, asked) =
                review_after_session(outcome, &[".foundry/session.log"], &[]).await;

            assert_eq!(asked, 0, "the reviewer must not be asked: {payload}");
            assert_eq!(payload["verdict"], "runner_error", "{payload}");
            assert_eq!(payload["detail"], detail);
            assert!(payload.get("failure_kind").is_none(), "{payload}");
            assert!(!payload.to_string().contains("proof.json"), "{payload}");
        }
    }

    /// A session that finished normally but wrote no proof is still the
    /// missing-proof defect: only a failed session is excused.
    #[tokio::test]
    async fn a_finished_session_without_proof_is_still_a_missing_proof_defect() {
        let (payload, asked) = review_after_session(
            crate::gateway::AgentOutcome::Success {
                stdout: "done".into(),
            },
            &["src/config.rs"],
            &[],
        )
        .await;

        assert_eq!(asked, 0);
        assert_eq!(payload["verdict"], "defect");
        assert!(payload["diagnosis"].as_str().unwrap().contains("proof.json"), "{payload}");
    }

    /// A failed session that changed project files, or that wrote early
    /// evidence, did work: it goes through the proof check as usual.
    #[tokio::test]
    async fn a_failed_session_that_did_work_still_goes_through_the_proof_path() {
        let failed = || crate::gateway::AgentOutcome::AgentFailed {
            stderr: "turn failed after editing".into(),
            failure: None,
        };

        let (changed_source, asked) = review_after_session(failed(), &["src/config.rs"], &[]).await;
        assert_eq!(asked, 0);
        assert_eq!(changed_source["verdict"], "defect", "{changed_source}");
        assert!(changed_source["diagnosis"].as_str().unwrap().contains("proof.json"));

        let (wrote_proof, _) = review_after_session(
            failed(),
            &[],
            &[(".foundry/proof.json", "{\"kind\":\"direct\"}")],
        )
        .await;
        assert_eq!(wrote_proof["verdict"], "defect", "{wrote_proof}");
        assert!(
            wrote_proof["diagnosis"].as_str().unwrap().contains("early acceptance evidence"),
            "{wrote_proof}"
        );
    }

    /// Review a campaign cycle whose worktree holds `files`, with a reviewer
    /// that would answer `complete` if asked. Returns the `TaskReviewed`
    /// payload and how many times the reviewer was asked.
    async fn review_campaign_worktree(files: &[(&str, String)]) -> (serde_json::Value, usize) {
        use crate::{blocks::test_helpers, gateway::fakes::FakeAgentGateway};
        use foundry_sdk::{event::EventType, task_block::TaskBlock};
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".foundry")).unwrap();
        for (path, content) in files {
            std::fs::write(dir.path().join(path), content).unwrap();
        }
        let registry = test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with("```json\n{\"verdict\":\"complete\"}\n```");
        let block = super::ReviewTask::new(agent.clone(), registry);
        let trigger = test_event!(EventType::GateVerificationCompleted, "p", {
            "project":"p", "workflow":"task", "all_passed":true,
            "required_passed":true, "results":[], "retry_count":0,
            "task_worktree":dir.path().to_str().unwrap(), "campaign_limits":{}
        });
        let result = block.execute(&trigger).await.unwrap();
        (result.events[0].payload.clone(), agent.invocations().len())
    }

    fn darwin_blocker() -> serde_json::Value {
        serde_json::json!({
            "kind": "blocked",
            "tried": "listed every SSH host; all report Linux",
            "missing": "a macOS host to run the Darwin URLSession tests",
            "question": "Can you provide a macOS runner for the Swift timeout diagnosis?",
            "options": ["Register a macOS runner", "Drop Darwin timeouts from this objective"]
        })
    }

    #[tokio::test]
    async fn explicitly_typed_operational_blocker_requires_recovery_instead_of_owner_judgement() {
        let mut blocker = darwin_blocker();
        blocker["recovery_kind"] = serde_json::json!("capability_unavailable");
        blocker.as_object_mut().unwrap().remove("question");
        let (payload, asked) =
            review_campaign_worktree(&[(".foundry/blocker.json", blocker.to_string())]).await;
        assert_eq!(asked, 0);
        assert_eq!(payload["verdict"], "blocked_on_recovery");
        assert_eq!(payload["kind"], "capability_unavailable");
        assert!(payload["detail"].as_str().unwrap().contains("macOS host"));
        assert!(payload.get("options").is_none());
        blocker["recovery_kind"] = serde_json::json!("anything_goes");
        let (invalid, _) =
            review_campaign_worktree(&[(".foundry/blocker.json", blocker.to_string())]).await;
        assert_eq!(
            invalid["verdict"], "defect",
            "unknown operational kinds cannot silently become owner choices"
        );
    }

    #[test]
    fn standalone_operational_blocker_is_checked_without_requiring_campaign_proof() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join(".foundry")).unwrap();
        let mut blocker = darwin_blocker();
        blocker["recovery_kind"] = serde_json::json!("capability_unavailable");
        blocker.as_object_mut().unwrap().remove("question");
        std::fs::write(directory.path().join(".foundry/blocker.json"), blocker.to_string())
            .unwrap();
        let result = campaign_proof_review(
            directory.path(),
            Throttle::Full,
            &LoopContext::default(),
            "diagnose timeout",
            "p",
            &[],
        )
        .unwrap();
        assert!(matches!(result.verdict, TaskVerdict::BlockedOnRecovery { .. }));
        std::fs::remove_file(directory.path().join(".foundry/blocker.json")).unwrap();
        assert!(
            campaign_proof_review(
                directory.path(),
                Throttle::Full,
                &LoopContext::default(),
                "ordinary task",
                "p",
                &[]
            )
            .is_none()
        );
    }

    // Production, 2026-10-17 (campaign mojentic-3-0-sw): an executor that
    // could not reach Darwin wrote no proof and the cycle became a
    // missing-proof defect, carried into the next cycle unchanged.
    #[tokio::test]
    async fn a_campaign_blocker_asks_the_owner_instead_of_reporting_missing_proof() {
        let (payload, asked) =
            review_campaign_worktree(&[(".foundry/blocker.json", darwin_blocker().to_string())])
                .await;

        assert_eq!(asked, 0, "a blocked cycle needs the owner, not a paid review");
        assert_eq!(payload["verdict"], "blocked_on_decision");
        let finding = payload["finding"].as_str().unwrap();
        assert!(finding.starts_with("Can you provide a macOS runner"), "{finding}");
        assert!(finding.contains("missing: a macOS host to run the Darwin URLSession tests"));
        assert!(finding.contains("tried: listed every SSH host; all report Linux"));
        assert_eq!(
            payload["options"],
            serde_json::json!([
                "Register a macOS runner",
                "Drop Darwin timeouts from this objective"
            ])
        );
        let review = payload["review"].as_str().unwrap();
        assert!(
            review.starts_with("execution blocked before the acceptance boundary"),
            "{review}"
        );
        assert!(!review.contains("proof.json"), "{review}");
    }

    #[tokio::test]
    async fn a_blocker_without_options_offers_the_default_choices() {
        for options in [None, Some(serde_json::json!([]))] {
            let mut blocker = darwin_blocker();
            match options {
                None => blocker.as_object_mut().unwrap().remove("options"),
                Some(empty) => blocker.as_object_mut().unwrap().insert("options".into(), empty),
            };
            let (payload, _) =
                review_campaign_worktree(&[(".foundry/blocker.json", blocker.to_string())]).await;

            assert_eq!(payload["verdict"], "blocked_on_decision");
            assert_eq!(payload["options"], serde_json::json!(super::DEFAULT_BLOCKER_OPTIONS));
        }
    }

    #[tokio::test]
    async fn an_invalid_blocker_is_a_defect_naming_the_blocker_file() {
        let mut cases: Vec<(String, &str)> = Vec::new();
        let mut wrong_kind = darwin_blocker();
        wrong_kind["kind"] = serde_json::json!("behavioral");
        cases.push((wrong_kind.to_string(), "kind must be \"blocked\""));
        let mut empty_question = darwin_blocker();
        empty_question["question"] = serde_json::json!("  ");
        cases.push((empty_question.to_string(), "question must not be empty"));
        let mut missing_tried = darwin_blocker();
        missing_tried.as_object_mut().unwrap().remove("tried");
        cases.push((missing_tried.to_string(), "tried must be a string, found null"));
        let mut number_option = darwin_blocker();
        number_option["options"] = serde_json::json!(["Register a macOS runner", 2]);
        cases.push((number_option.to_string(), "options must hold only non-empty strings"));
        let mut options_string = darwin_blocker();
        options_string["options"] = serde_json::json!("Register a macOS runner");
        cases.push((options_string.to_string(), "options must be an array, found string"));
        cases.push(("[]".to_string(), "blocker must be an object, found array"));
        cases.push(("not json".to_string(), "expected ident"));

        for (content, expected) in cases {
            let (payload, asked) =
                review_campaign_worktree(&[(".foundry/blocker.json", content.clone())]).await;

            assert_eq!(asked, 0);
            assert_eq!(payload["verdict"], "defect", "{content}");
            let diagnosis = payload["diagnosis"].as_str().unwrap();
            assert!(
                diagnosis.starts_with("invalid execution blocker at .foundry/blocker.json: "),
                "{content}: {diagnosis}"
            );
            assert!(diagnosis.contains(expected), "{content}: {diagnosis}");
        }
    }

    #[tokio::test]
    async fn no_blocker_and_no_proof_is_still_the_missing_proof_defect() {
        let (payload, asked) = review_campaign_worktree(&[]).await;

        assert_eq!(asked, 0);
        assert_eq!(payload["verdict"], "defect");
        let diagnosis = payload["diagnosis"].as_str().unwrap();
        assert!(
            diagnosis.starts_with(
                "invalid or missing early acceptance evidence at .foundry/proof.json: "
            ),
            "{diagnosis}"
        );
        assert_eq!(payload["review"], diagnosis);
    }

    #[tokio::test]
    async fn a_valid_proof_without_a_blocker_goes_to_the_reviewer() {
        let proof = serde_json::json!({"kind":"direct", "reason":"documentation-only objective",
            "corrected":{"command":"mdbook build book","exit_code":0,"log":".foundry/pass.log"}});
        let (payload, asked) = review_campaign_worktree(&[
            (".foundry/proof.json", proof.to_string()),
            (".foundry/pass.log", "book built".to_string()),
        ])
        .await;

        assert_eq!(asked, 1, "sound proof proceeds to the skeptical reviewer");
        assert_eq!(payload["verdict"], "complete");
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
                logs: Vec::new(),
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
                logs: Vec::new(),
            },
        ];

        let prompt = build_review_prompt("ship the slice", &gates);

        assert!(prompt.contains("test [REQUIRED]: PASS"));
        assert!(prompt.contains("dialyzer [OPTIONAL]: FAIL"));
        assert!(prompt.contains("OPTIONAL gate failures are advisory"));
        assert!(prompt.contains("untracked new files are both valid deliverable changes"));
        assert!(prompt.contains("drive that boundary the way a caller does"));
        assert!(prompt.contains("with I/O stubbed in process at the transport or gateway"));
        assert!(prompt.contains(
            "a double that replaces the code under test or mocks library internals is insufficient"
        ));
        assert!(
            prompt.contains(
                "Do not demand real servers, sockets, live services or wall-clock timing"
            )
        );
    }

    /// "Real boundary" was read as "real network": reviewers rejected stubbed
    /// transports and campaigns built suites on local HTTP servers. Every task
    /// review, standalone or campaign, judges tests by the shared rule, and the
    /// campaign proof check names the public API instead of a real boundary.
    #[test]
    fn every_task_review_judges_tests_by_the_shared_test_design_rule() {
        let guidance = super::super::agent_helpers::TEST_DESIGN_GUIDANCE;
        for campaign in [false, true] {
            let prompt = super::task_review_prompt("ship the slice", &[], campaign);
            assert!(prompt.contains(guidance), "{prompt}");
            assert!(!prompt.contains("hardest real boundary"), "{prompt}");
            assert!(!prompt.contains("inject around the real boundary"), "{prompt}");
            assert!(!prompt.contains("stub installed above that boundary"), "{prompt}");
        }

        let campaign = super::task_review_prompt("ship the slice", &[], true);
        assert!(
            campaign.contains(
                "exercised the hardest acceptance behavior through the project's public API, with I/O stubbed in process"
            ),
            "{campaign}"
        );
        assert!(
            campaign.contains(
                "opens real sockets or local servers, calls external services or asserts wall-clock thresholds is a defect"
            ),
            "{campaign}"
        );
    }

    /// Campaigns stopped on owner questions about clock and transport seams.
    /// Every review carries the shared pre-authorization and states how to
    /// apply it: accept an internal seam, and still reject an unlicensed public
    /// API change as a defect.
    #[test]
    fn every_task_review_accepts_internal_seams_and_rejects_unlicensed_public_api_changes() {
        for campaign in [false, true] {
            let prompt = super::task_review_prompt("ship the slice", &[], campaign);
            for phrase in super::super::agent_helpers::SEAM_AUTHORIZATION_PHRASES {
                assert!(prompt.contains(phrase), "missing {phrase}: {prompt}");
            }
            assert!(prompt.contains(super::SEAM_REVIEW_RULE), "{prompt}");
            assert!(
                prompt.contains("never return DEFECT or BLOCKED_ON_DECISION for it"),
                "{prompt}"
            );
            assert!(
                prompt.contains(
                    "A public API change the objective does not license is a DEFECT, even when it was made for testability"
                ),
                "{prompt}"
            );
        }
    }

    /// Finalize Task commits nothing under `.foundry/`, so every task review,
    /// not only a campaign's, treats project material left there as missing.
    #[test]
    fn every_task_review_treats_dot_foundry_deliverables_as_a_defect() {
        for campaign in [false, true] {
            let prompt = super::task_review_prompt("ship the slice", &[], campaign);
            assert!(
                prompt.contains("Files under .foundry/ are Foundry scratch and never land"),
                "{prompt}"
            );
            assert!(prompt.contains("cites .foundry/ paths as repository content"), "{prompt}");
            assert!(
                prompt.contains("durable project material only under .foundry/, is a defect"),
                "{prompt}"
            );
        }
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
            logs: Vec::new(),
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
    fn a_failing_gate_keeps_its_stdout_failure_line_and_names_its_full_logs() {
        let stderr = (0..2_000)
            .map(|i| format!("   Compiling noisy-crate-{i} v1.0.0 (registry `crates-io`)"))
            .collect::<Vec<_>>()
            .join("\n");
        let output = format!(
            "test foo::bar ... FAILED\n{}\n{stderr}",
            crate::gate_runner::output::STDERR_MARKER
        );
        let mut gate = gate_with_output("test", false, true, output);
        gate.logs = vec![
            "/logs/test.stdout.log".to_string(),
            "/logs/test.stderr.log".to_string(),
        ];

        let prompt = build_review_prompt("obj", &[gate]);

        assert!(prompt.contains("test foo::bar ... FAILED"));
        assert!(prompt.contains("full logs: /logs/test.stdout.log, /logs/test.stderr.log"));
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
    #[test]
    fn early_proof_accepts_source_path_arrays() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".foundry")).unwrap();
        std::fs::write(dir.path().join("probe.log"), "actual probe output").unwrap();
        let proof = serde_json::json!({"kind":"behavioral", "source_change":["src/core.rs", "tests/core.rs"],
            "rejecting":{"command":"test core","exit_code":1,"log":"probe.log"},
            "corrected":{"command":"test core","exit_code":0,"log":"probe.log"}});
        let path = dir.path().join(".foundry/proof.json");
        std::fs::write(&path, proof.to_string()).unwrap();
        assert_eq!(super::proof_error(dir.path()), None);
    }

    #[test]
    fn early_proof_names_wrong_field_types() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(".foundry")).unwrap();
        std::fs::write(dir.path().join("probe.log"), "actual probe output").unwrap();
        let mut proof = serde_json::json!({"kind":"behavioral", "source_change":"src/core.rs",
            "rejecting":{"command":"test core","exit_code":1,"log":"probe.log"},
            "corrected":{"command":"test core","exit_code":0,"log":"probe.log"}});
        let path = dir.path().join(".foundry/proof.json");
        for (value, expected) in [
            (serde_json::json!(42), "number"),
            (serde_json::json!({}), "object"),
        ] {
            proof["source_change"] = value;
            std::fs::write(&path, proof.to_string()).unwrap();
            assert!(
                super::proof_error(dir.path())
                    .unwrap()
                    .ends_with(&format!("source_change must be a string, found {expected}"))
            );
        }
        for invalid in [
            serde_json::json!([]),
            serde_json::json!([""]),
            serde_json::json!(["src/core.rs", 42]),
        ] {
            proof["source_change"] = invalid;
            std::fs::write(&path, proof.to_string()).unwrap();
            assert!(super::proof_error(dir.path()).is_some());
        }
        proof["source_change"] = serde_json::json!("src/core.rs");
        proof["corrected"] = serde_json::json!([]);
        std::fs::write(&path, proof.to_string()).unwrap();
        assert!(
            super::proof_error(dir.path())
                .unwrap()
                .ends_with("corrected must be an object, found array")
        );
    }
}
