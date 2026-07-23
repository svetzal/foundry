use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use foundry_sdk::campaign::{
    Campaign, CampaignStatus, CampaignStore, CampaignStoreGuard, CycleOutcome, DoneEvidence,
};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gates::GateDefinition;
use foundry_sdk::payload::{
    CampaignAdvanceCompletedPayload, CampaignAdvanceRequestedPayload, CampaignDecision,
    CampaignTerminalPayload, TaskVerdict,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{
    AgentAccess, AgentFailureKind, AgentFailureMetadata, AgentGateway, AgentOutcome, AgentProvider,
    ModelTier, ReasoningEffort, ShellGateway,
};

use super::{AgentBlockSpec, SimulatedSuccess, invoke_agent};

pub struct AdvanceCampaign {
    agent: Arc<dyn AgentGateway>,
    shell: Arc<dyn ShellGateway>,
    registry: Arc<std::sync::RwLock<Registry>>,
    store_path: PathBuf,
    lock: Arc<tokio::sync::Mutex<()>>,
}

impl AdvanceCampaign {
    pub fn new(
        agent: Arc<dyn AgentGateway>,
        shell: Arc<dyn ShellGateway>,
        registry: Arc<std::sync::RwLock<Registry>>,
        store_path: PathBuf,
    ) -> Self {
        Self {
            agent,
            shell,
            registry,
            store_path,
            lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

fn read_context_files(repo: &Path, paths: &[String]) -> anyhow::Result<String> {
    let repo = repo.canonicalize()?;
    let mut sections = Vec::new();
    for relative in paths {
        let path = repo.join(relative).canonicalize()?;
        if !path.starts_with(&repo) {
            anyhow::bail!("campaign context path escapes project: {relative}");
        }
        sections.push(format!("## {relative}\n{}", std::fs::read_to_string(&path)?));
    }
    Ok(sections.join("\n\n"))
}

async fn repo_snapshot(shell: &dyn ShellGateway, repo: &Path) -> String {
    let status = shell.run(repo, "git", &["status", "--short", "--branch"], None, None).await;
    let log = shell.run(repo, "git", &["log", "-8", "--oneline"], None, None).await;
    format!(
        "STATUS:\n{}\n\nRECENT COMMITS:\n{}",
        status.map_or_else(|e| e.to_string(), |r| format!("{}{}", r.stdout, r.stderr)),
        log.map_or_else(|e| e.to_string(), |r| format!("{}{}", r.stdout, r.stderr))
    )
}

fn or_none(text: String) -> String {
    if text.trim().is_empty() {
        "(none)".to_string()
    } else {
        text
    }
}

/// The ref the next execution will branch from, which is the previous cycle's
/// preserved task branch whenever that cycle did not land.
fn next_base_ref(request: &CampaignAdvanceRequestedPayload) -> Option<&str> {
    request
        .run_result
        .as_ref()
        .and_then(|result| result.preservation_ref.as_deref())
}

/// Work reachable from the next cycle's base ref but absent from the canonical
/// checkout.
///
/// Non-landing cycles (`remainder`, `defect`) preserve their branch and chain
/// forward, so from the second such cycle onward the canonical checkout no
/// longer reflects what the executor will actually start from. Without this the
/// decision agent re-cuts objectives the accumulated work already satisfies.
async fn accumulated_work(shell: &dyn ShellGateway, repo: &Path, base_ref: Option<&str>) -> String {
    let Some(reference) = base_ref else {
        return "NEXT CYCLE BASE: the live checkout above — no preserved work carries forward."
            .to_string();
    };
    let commit_range = format!("HEAD..{reference}");
    let diff_range = format!("HEAD...{reference}");
    let commits = shell.run(repo, "git", &["log", "--oneline", &commit_range], None, None).await;
    let stat = shell.run(repo, "git", &["diff", "--stat", &diff_range], None, None).await;
    format!(
        "NEXT CYCLE BASE: {reference}\n\nCOMMITS ON THAT REF ABSENT FROM THE LIVE CHECKOUT:\n{}\n\nFILES IT CHANGES vs THE LIVE CHECKOUT:\n{}",
        or_none(commits.map_or_else(|e| e.to_string(), |r| format!("{}{}", r.stdout, r.stderr))),
        or_none(stat.map_or_else(|e| e.to_string(), |r| format!("{}{}", r.stdout, r.stderr)))
    )
}

/// How much of a past objective or outcome is rendered into the history
/// section.
///
/// Long enough to identify what was asked for and what came back, short enough
/// that a full history stays a small fraction of the prompt — a synthesized
/// gate objective alone runs to several paragraphs. The untruncated text of
/// every objective is in the `CampaignAdvanceCompleted` event stream.
const HISTORY_ENTRY_CHARS: usize = 280;

/// Flatten a stored objective or outcome onto one bounded line.
///
/// Objectives are multi-line by construction, and the history is read as a list
/// — an entry that spans lines stops looking like one item.
fn condense(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(HISTORY_ENTRY_CHARS) {
        Some((index, _)) => format!("{}…", &flat[..index]),
        None => flat,
    }
}

fn outcome_label(outcome: Option<&CycleOutcome>) -> String {
    let Some(outcome) = outcome else {
        return "dispatched, no result recorded".to_string();
    };
    let landing = if outcome.landed {
        "landed"
    } else {
        "did not land"
    };
    let detail = match &outcome.verdict {
        TaskVerdict::Complete => format!("complete, {landing}"),
        TaskVerdict::Remainder { gaps } => {
            format!("remainder, {landing}; gaps: {}", gaps.join("; "))
        }
        TaskVerdict::Defect { diagnosis } => format!("defect, {landing}; {diagnosis}"),
        TaskVerdict::BlockedOnDecision { finding, .. } => format!("blocked on decision; {finding}"),
        TaskVerdict::RunnerError { detail } => format!("runner error; {detail}"),
    };
    condense(&detail)
}

/// The objectives this campaign has already cut, oldest first.
///
/// Shown only one cycle deep, the agent cannot tell that it is asking for the
/// same thing it asked for five cycles ago; one campaign restated the same
/// reason across nine consecutive cycles and landed once. The typed verdict
/// travels with each entry because it is what separates a legitimate follow-up
/// from a repeat: a `remainder` says the last attempt at this already came back
/// short.
fn objective_history(campaign: &Campaign) -> String {
    if campaign.objective_history.is_empty() {
        return "none — this is the first objective this campaign has cut.".to_string();
    }
    campaign
        .objective_history
        .iter()
        .map(|cycle| {
            format!(
                "- cycle {} [{}] {}",
                cycle.cycle,
                outcome_label(cycle.outcome.as_ref()),
                condense(&cycle.objective)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn run_done_gates(
    shell: &dyn ShellGateway,
    repo: &Path,
    evidence: &[DoneEvidence],
) -> anyhow::Result<Vec<foundry_sdk::gates::GateResult>> {
    let mut results = Vec::new();
    for (index, item) in evidence.iter().enumerate() {
        let DoneEvidence::Gate {
            command,
            required,
            artifacts,
        } = item
        else {
            continue;
        };
        let name = format!("campaign_done_{}", index + 1);
        if let Some(error) = artifact_error(repo, artifacts) {
            results.push(foundry_sdk::gates::GateResult {
                name,
                command: command.clone(),
                passed: false,
                required: *required,
                output: error,
                exit_code: 1,
                duration_ms: Some(0),
                fix_applied: false,
            });
            continue;
        }
        let gate = GateDefinition {
            name,
            command: command.clone(),
            required: *required,
            timeout: None,
            fix_command: None,
        };
        results.extend(crate::gate_runner::run_gates(&[gate], repo, shell).await?.results);
    }
    Ok(results)
}

fn artifact_error(repo: &Path, artifacts: &[String]) -> Option<String> {
    artifacts.iter().find_map(|artifact| {
        let relative = Path::new(artifact);
        let safe = !relative.is_absolute()
            && relative.components().all(|component| {
                matches!(component, std::path::Component::Normal(_) | std::path::Component::CurDir)
            });
        if !safe {
            return Some(format!("campaign gate artifact must be repository-relative: {artifact}"));
        }
        if !repo.join(relative).exists() {
            return Some(format!("campaign gate artifact missing: {artifact}"));
        }
        None
    })
}

fn decision_prompt(
    campaign: &Campaign,
    context: &str,
    snapshot: &str,
    accumulated: &str,
    gate_results: &[foundry_sdk::gates::GateResult],
    request: &CampaignAdvanceRequestedPayload,
) -> String {
    let review_evidence = campaign
        .done_evidence
        .iter()
        .filter_map(|item| match item {
            DoneEvidence::Review { statement } => Some(format!("- {statement}")),
            DoneEvidence::Gate { .. } => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let owner_decisions = if campaign.owner_decisions.is_empty() {
        "none recorded".to_string()
    } else {
        campaign
            .owner_decisions
            .iter()
            .map(|decision| {
                format!(
                    "- {} [{}] {}",
                    decision.decided_at.to_rfc3339(),
                    decision.authorized_by,
                    decision.decision
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let gates = serde_json::to_string_pretty(gate_results).unwrap_or_default();
    let last_result = request.run_result.as_ref().map_or_else(
        || "none (initial/manual advance)".to_string(),
        |result| serde_json::to_string_pretty(result).unwrap_or_default(),
    );
    let history = objective_history(campaign);
    format!(
        "You are advancing a durable engineering campaign. Inspect the repository yourself; descriptive metadata is never current state. Decide exactly one of done, advance, or escalate.\n\n\
         CAMPAIGN: {}\nMISSION: {}\nINTENT REFS: {}\nCYCLES: {} completed / {} landed / {} max\nESCALATION RULES:\n- {}\n\n\
         OWNER DECISIONS (binding policy for this and future advances):\n{}\n\nREQUIRED REVIEW EVIDENCE:\n{}\n\nMECHANICAL DONE-GATE RESULTS:\n{}\n\nOBJECTIVE HISTORY (what this campaign has already asked for, oldest first, with the typed verdict each returned):\n{}\n\nLAST TYPED RUN RESULT:\n{}\n\nLIVE REPO SNAPSHOT (delivered trunk state):\n{}\n\nACCUMULATED UNMERGED WORK:\n{}\n\nCONTEXT ARTIFACTS (wording is binding and must be threaded into acceptance criteria):\n{}\n\n\
         TWO TREES. The live snapshot is the delivered trunk state. The accumulated section describes a preserved branch carrying earlier cycles of THIS campaign that did not land; the next cycle starts from that ref, not from the trunk. Its work is real and already written—it is invisible to a trunk-only inspection, and any tool you run in the working directory sees the trunk, not it.\n\n\
         DONE only when every required gate passes and every review statement is true against the delivered trunk state. Accumulated unmerged work does not make a campaign done. ADVANCE must cut exactly ONE objective from mission minus current state, where current state is the trunk PLUS the accumulated work. Never re-cut an objective the accumulated commits already satisfy; if the accumulated work appears to carry the mission but has not landed, the objective is to reconcile and land it—finish the remaining gaps, get the required gates green on that branch—not to rebuild it from scratch. NO RESTATEMENT. Check the objective you are about to cut against OBJECTIVE HISTORY before you commit to it. If it substantially restates an entry there, do not re-dispatch it; decide which of exactly two things is true. Either that earlier work exists and you are reading the wrong tree—re-read the accumulated section, and the objective becomes reconcile-and-land, not rebuild. Or the earlier cycle returned `remainder` and its gaps are genuinely still open on a tree you can see, in which case asking for the same thing has already failed once and repeating it is not the correction: name the single sub-gap that blocked it, change the approach, or escalate for an owner decision. A third identical objective is never the answer. Constraints, change licenses, scope guards, and forbidden moves are gates—not co-equal objectives. State concrete proof capable of rejecting masked IDs, count-only checks, or tests that bypass the real boundary. Do not prescribe implementation mechanism. Before removal/refactor packets, perform cheap live structural probes for callers and cross-module coupling and encode discoveries as scope guards. For migrations, name each licensed behavior change with its intent ref; all other characterized behavior is frozen. Build characterization before migration. ESCALATE on a human judgment, fired escalation rule, unusable provider, or invalidated campaign assumption.\n\n\
         End with exactly one fenced JSON object:\n\
         {{\"decision\":\"done\",\"reason\":\"evidence\"}}\n\
         {{\"decision\":\"advance\",\"objective\":\"single objective plus gates/forbidden moves/evidence\",\"reason\":\"gap from mission minus state\"}}\n\
         {{\"decision\":\"escalate\",\"reason\":\"owner decision required\"}}",
        campaign.name,
        campaign.mission,
        campaign.intent_refs.join(", "),
        campaign.cycles_completed,
        campaign.cycles_landed,
        campaign.budget.max_cycles,
        campaign.escalation.join("\n- "),
        owner_decisions,
        review_evidence,
        gates,
        history,
        last_result,
        snapshot,
        accumulated,
        context,
    )
}

fn parse_decision(output: &str) -> anyhow::Result<CampaignDecision> {
    let decision: CampaignDecision = serde_json::from_str(&super::extract_json(output))
        .map_err(|e| anyhow::anyhow!("campaign agent returned no valid decision: {e}"))?;

    match &decision {
        CampaignDecision::Done { reason } | CampaignDecision::Escalate { reason }
            if reason.trim().is_empty() =>
        {
            anyhow::bail!("campaign agent returned a decision without a reason")
        }
        CampaignDecision::Advance { objective, .. } if objective.trim().is_empty() => {
            anyhow::bail!("campaign agent returned an advance decision without an objective")
        }
        CampaignDecision::Advance { reason, .. } if reason.trim().is_empty() => {
            anyhow::bail!("campaign agent returned an advance decision without a reason")
        }
        _ => Ok(decision),
    }
}

/// Total attempts against the decision agent before a transient failure is
/// treated as terminal.
///
/// One provider hiccup used to end a campaign outright — the campaign with the
/// best landing rate on record was killed twice this way, both times
/// immediately after a successful landed cycle. Three attempts make that
/// outcome require three independent failures instead of one. A fourth would
/// cost another Deep-tier invocation for no measurable reduction in risk.
const DECISION_ATTEMPTS: u32 = 3;

/// Delay before the first retry; doubled for each subsequent one.
///
/// Long enough to outlast the transport blips that caused the observed
/// escalations, and negligible against a Deep-tier invocation already measured
/// in minutes. No jitter: campaign advances are serialized by a process mutex
/// and a store-wide file lock, so there is no herd to spread.
const DECISION_RETRY_DELAY: Duration = Duration::from_secs(5);

/// What an advance concluded, once provider outages are separated from the
/// decisions the campaign can act on.
///
/// `Pause` deliberately has no [`CampaignDecision`] counterpart: pausing is a
/// status transition the owner clears with `campaign resume`, not an outcome
/// the campaign dispatches or dies on.
enum AdvanceOutcome {
    Decided(CampaignDecision),
    Pause { reason: String },
}

impl AdvanceOutcome {
    /// Apply a rule that only makes sense for a real decision, leaving a pause
    /// untouched — the campaign never chose to pause, so no decision rule
    /// governs it.
    fn map_decision(self, rule: impl FnOnce(CampaignDecision) -> CampaignDecision) -> Self {
        match self {
            Self::Decided(decision) => Self::Decided(rule(decision)),
            Self::Pause { .. } => self,
        }
    }
}

/// The fixed part of a decision-agent invocation. Every attempt re-sends
/// exactly this; only the attempt number differs.
struct DecisionRequest {
    prompt: String,
    working_dir: PathBuf,
    agent_file: Option<PathBuf>,
    provider: Option<AgentProvider>,
    timeout: Duration,
}

impl DecisionRequest {
    fn spec(&self) -> AgentBlockSpec {
        AgentBlockSpec {
            prompt: self.prompt.clone(),
            working_dir: self.working_dir.clone(),
            access: AgentAccess::ReadOnly,
            tier: ModelTier::Deep,
            effort: ReasoningEffort::High,
            agent_file: self.agent_file.clone(),
            provider: self.provider,
            env: Vec::new(),
            timeout: self.timeout,
        }
    }
}

/// Why a provider is unusable right now, or `None` when the failure is worth
/// another attempt.
///
/// An exhausted account or a tripped circuit breaker is not a hiccup: the
/// breaker stays open for the life of the process, so every further attempt
/// returns the same refusal. Escalating burns the campaign over something that
/// has nothing to do with its work.
fn provider_outage_reason(failure: Option<&AgentFailureMetadata>) -> Option<String> {
    failure
        .filter(|failure| failure.is_terminal_provider_failure())
        .map(AgentFailureMetadata::execution_summary)
}

/// Whether a `RunnerError` detail describes a provider outage rather than a
/// fault in the run.
///
/// The structured [`AgentFailureMetadata`] does not survive into the typed
/// verdict, so the labels the SDK itself stamps onto the detail string are the
/// only signal left. Sourcing them from [`AgentFailureKind`] keeps this
/// matching the SDK's wording rather than a private copy of it.
fn is_provider_outage_detail(detail: &str) -> bool {
    const BREAKER_MARKER: &str = "circuit breaker open";
    detail.contains(BREAKER_MARKER)
        || [
            AgentFailureKind::AccountLimit,
            AgentFailureKind::AccountDisabled,
            AgentFailureKind::Authentication,
        ]
        .iter()
        .any(|kind| detail.contains(kind.summary_label()))
}

/// Ask the decision agent, retrying a transport failure before giving up.
///
/// Only failures to *get* an answer are retried. A malformed decision is the
/// agent answering badly rather than failing to answer, so it propagates on the
/// first attempt — re-asking would burn identical invocations. A terminal
/// provider failure short-circuits to a pause for the same reason.
async fn ask_decision_agent(
    agent: &dyn AgentGateway,
    request: &DecisionRequest,
    project: &str,
) -> anyhow::Result<AdvanceOutcome> {
    let mut diagnostics = Vec::new();
    let mut delay = DECISION_RETRY_DELAY;
    for attempt in 1..=DECISION_ATTEMPTS {
        let (context, detail) =
            match invoke_agent(agent, request.spec(), "campaign advance", project).await {
                AgentOutcome::Success { stdout } => {
                    return Ok(AdvanceOutcome::Decided(parse_decision(&stdout)?));
                }
                AgentOutcome::AgentFailed { stderr, failure } => {
                    if let Some(reason) = provider_outage_reason(failure.as_ref()) {
                        return Ok(AdvanceOutcome::Pause { reason });
                    }
                    ("failed", stderr)
                }
                AgentOutcome::Unavailable { error } => ("unavailable", error),
            };
        let detail = if detail.trim().is_empty() {
            "no diagnostic output".to_string()
        } else {
            detail.trim().to_string()
        };
        tracing::warn!(project = %project, attempt, %context, %detail, "campaign decision agent did not answer");
        diagnostics.push(format!("attempt {attempt} {context}: {detail}"));
        if attempt < DECISION_ATTEMPTS {
            tokio::time::sleep(delay).await;
            delay *= 2;
        }
    }
    // Every attempt is reported, so an escalation can never be reasoned about
    // from a single empty stderr the way the observed ones had to be.
    Ok(AdvanceOutcome::Decided(CampaignDecision::Escalate {
        reason: format!(
            "campaign decision agent did not answer in {DECISION_ATTEMPTS} attempts ({})",
            diagnostics.join("; ")
        ),
    }))
}

fn completed_event(
    campaign: &Campaign,
    throttle: foundry_sdk::throttle::Throttle,
    outcome: CampaignDecision,
) -> Event {
    super::event_from_infallible_payload(
        EventType::CampaignAdvanceCompleted,
        &campaign.project,
        throttle,
        &CampaignAdvanceCompletedPayload {
            campaign: campaign.name.clone(),
            project: campaign.project.clone(),
            cycles_completed: campaign.cycles_completed,
            cycles_landed: campaign.cycles_landed,
            outcome,
        },
    )
}

fn terminal_event(
    ty: EventType,
    campaign: &Campaign,
    throttle: foundry_sdk::throttle::Throttle,
    reason: String,
) -> Event {
    super::event_from_infallible_payload(
        ty,
        &campaign.project,
        throttle,
        &CampaignTerminalPayload {
            campaign: campaign.name.clone(),
            project: campaign.project.clone(),
            reason,
            cycles_completed: campaign.cycles_completed,
            cycles_landed: campaign.cycles_landed,
        },
    )
}

fn execution_event(
    campaign: &Campaign,
    throttle: foundry_sdk::throttle::Throttle,
    objective: &str,
    base_ref: Option<String>,
) -> Event {
    let mut payload = serde_json::json!({
        "project": campaign.project,
        "workflow": "task",
        "prompt": objective,
        "campaign": campaign.name,
    });
    if let Some(agent) = &campaign.agent_provider {
        payload["agent_provider"] = serde_json::json!(agent);
    }
    if let Some(reference) = base_ref {
        payload["base_ref"] = serde_json::json!(reference);
    }
    Event::new(EventType::ExecutionRequested, campaign.project.clone(), throttle, payload)
}

fn terminal_error_result(
    campaign: &str,
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    cycles_completed: u64,
    cycles_landed: u64,
    reason: String,
) -> TaskBlockResult {
    let summary = format!("campaign '{campaign}' escalated: {reason}");
    let completed = super::event_from_infallible_payload(
        EventType::CampaignAdvanceCompleted,
        project,
        throttle,
        &CampaignAdvanceCompletedPayload {
            campaign: campaign.to_string(),
            project: project.to_string(),
            cycles_completed,
            cycles_landed,
            outcome: CampaignDecision::Escalate {
                reason: reason.clone(),
            },
        },
    );
    let escalated = super::event_from_infallible_payload(
        EventType::CampaignEscalated,
        project,
        throttle,
        &CampaignTerminalPayload {
            campaign: campaign.to_string(),
            project: project.to_string(),
            reason,
            cycles_completed,
            cycles_landed,
        },
    );
    TaskBlockResult {
        success: false,
        summary,
        events: vec![completed, escalated],
        ..Default::default()
    }
}

struct AdvanceExecution {
    agent: Arc<dyn AgentGateway>,
    shell: Arc<dyn ShellGateway>,
    registry: Arc<std::sync::RwLock<Registry>>,
    store_path: PathBuf,
    lock: Arc<tokio::sync::Mutex<()>>,
    request: CampaignAdvanceRequestedPayload,
    project: String,
    throttle: foundry_sdk::throttle::Throttle,
}

fn persist_or_terminal(
    execution: &AdvanceExecution,
    guard: &CampaignStoreGuard,
    events: Vec<Event>,
    success_summary: String,
    failure_context: &str,
    cycles_completed: u64,
    cycles_landed: u64,
) -> TaskBlockResult {
    match guard.save() {
        Ok(()) => TaskBlockResult::success(success_summary, events),
        Err(error) => terminal_error_result(
            &execution.request.campaign,
            &execution.project,
            execution.throttle,
            cycles_completed,
            cycles_landed,
            format!("{failure_context}: {error}"),
        ),
    }
}

fn update_run_and_forced_outcome(
    campaign: &mut Campaign,
    request: &CampaignAdvanceRequestedPayload,
) -> Option<AdvanceOutcome> {
    if let Some(run_event_id) = &request.run_event_id
        && Some(run_event_id) != campaign.last_run_event_id.as_ref()
    {
        if let Some(run_result) = &request.run_result {
            if run_result.landed {
                campaign.cycles_landed += 1;
            }
            campaign.record_cycle_outcome(run_result);
            campaign.pending_run_result = Some(run_result.clone());
        }
        campaign.last_run_event_id = Some(run_event_id.clone());
    }

    if campaign.authorized_by.is_none() {
        return Some(AdvanceOutcome::Decided(CampaignDecision::Escalate {
            reason: "campaign has not been authorized by its owner".to_string(),
        }));
    }
    if campaign.status == CampaignStatus::Completed {
        return Some(AdvanceOutcome::Decided(CampaignDecision::Done {
            reason: "campaign is already completed".to_string(),
        }));
    }
    if campaign.status == CampaignStatus::Escalated {
        return Some(AdvanceOutcome::Decided(CampaignDecision::Escalate {
            reason: format!("campaign is {} and requires owner resumption", campaign.status),
        }));
    }
    if let Some(result) = &request.run_result {
        match &result.verdict {
            TaskVerdict::BlockedOnDecision { finding, options } => {
                return Some(AdvanceOutcome::Decided(CampaignDecision::Escalate {
                    reason: format!("{finding}; options: {}", options.join(" | ")),
                }));
            }
            // The same tripped breaker that stops a decision agent also arrives
            // here, as the executor's typed verdict. Both paths must wait it out
            // rather than spend the campaign on it.
            TaskVerdict::RunnerError { detail } if is_provider_outage_detail(detail) => {
                return Some(AdvanceOutcome::Pause {
                    reason: format!("provider unusable: {detail}"),
                });
            }
            TaskVerdict::RunnerError { detail } => {
                return Some(AdvanceOutcome::Decided(CampaignDecision::Escalate {
                    reason: format!("runner/provider unavailable: {detail}"),
                }));
            }
            TaskVerdict::Complete | TaskVerdict::Remainder { .. } | TaskVerdict::Defect { .. } => {}
        }
    }
    None
}

fn enforce_campaign_budget(campaign: &Campaign, decision: CampaignDecision) -> CampaignDecision {
    if campaign.cycles_completed < campaign.budget.max_cycles
        || !matches!(decision, CampaignDecision::Advance { .. })
    {
        return decision;
    }
    CampaignDecision::Escalate {
        reason: format!("campaign cycle budget exhausted ({})", campaign.budget.max_cycles),
    }
}

/// Rewrite a `done` decision into an `advance` when required mechanical
/// evidence is still red.
///
/// The synthesized objective carries the mission and each gate's own output,
/// because a bare command string is not enough to act on: dispatched with
/// nothing but `cargo clippy …`, cycles were observed rewriting the very test
/// file the lint was complaining about, undoing landed mission work to turn the
/// gate green.
fn enforce_done_gate_truth(
    campaign: &Campaign,
    decision: CampaignDecision,
    gate_results: &[foundry_sdk::gates::GateResult],
) -> CampaignDecision {
    let required_pass = gate_results.iter().filter(|gate| gate.required).all(|gate| gate.passed);
    if !matches!(decision, CampaignDecision::Done { .. }) || required_pass {
        return decision;
    }
    let failures = gate_results
        .iter()
        .filter(|gate| gate.required && !gate.passed)
        .map(|gate| format!("$ {}\n{}", gate.command, or_none(gate.output.clone())))
        .collect::<Vec<_>>()
        .join("\n\n");
    CampaignDecision::Advance {
        objective: format!(
            "Make every required campaign done-evidence gate pass without regressing the mission.\n\n\
             MISSION THIS CAMPAIGN MUST STILL SATISFY:\n{}\n\n\
             FAILING REQUIRED GATES, WITH THE OUTPUT THAT REJECTED THEM:\n{failures}\n\n\
             Fix what each gate reports, at the location it names. FORBIDDEN MOVES: deleting, \
             reverting, skipping, or shrinking landed mission work to turn a gate green; \
             broadening a lint allowance beyond the single item the gate named; weakening the \
             project's lint or gate configuration. EVIDENCE: every gate above passes and the \
             mission behavior is still present in the delivered trunk state.",
            campaign.mission
        ),
        reason: "review proposed done while required mechanical evidence was failing".to_string(),
    }
}

async fn derive_advance_outcome(
    agent: &dyn AgentGateway,
    shell: &dyn ShellGateway,
    entry: &foundry_sdk::registry::ProjectEntry,
    campaign: &Campaign,
    request: &CampaignAdvanceRequestedPayload,
) -> anyhow::Result<AdvanceOutcome> {
    let repo = Path::new(&entry.path);
    let gate_results = run_done_gates(shell, repo, &campaign.done_evidence).await?;
    let context = read_context_files(repo, &campaign.context_paths)?;
    let snapshot = repo_snapshot(shell, repo).await;
    let accumulated = accumulated_work(shell, repo, next_base_ref(request)).await;
    // The gates, the snapshot, and the prompt are all deterministic for this
    // advance, so a retry re-asks the same question rather than re-deriving it.
    let decision_request = DecisionRequest {
        prompt: decision_prompt(
            campaign,
            &context,
            &snapshot,
            &accumulated,
            &gate_results,
            request,
        ),
        working_dir: repo.to_path_buf(),
        agent_file: super::resolve_agent_file(&entry.agent),
        provider: campaign
            .agent_provider
            .as_deref()
            .and_then(|provider| super::parse_agent_provider(Some(provider))),
        timeout: entry.timeout(),
    };
    let outcome = ask_decision_agent(agent, &decision_request, &campaign.project).await?;
    Ok(outcome.map_decision(|decision| enforce_done_gate_truth(campaign, decision, &gate_results)))
}

fn apply_advance_outcome(
    campaign: &mut Campaign,
    request: &CampaignAdvanceRequestedPayload,
    throttle: foundry_sdk::throttle::Throttle,
    outcome: AdvanceOutcome,
) -> Vec<Event> {
    let decision = match outcome {
        AdvanceOutcome::Decided(decision) => decision,
        // `CampaignPaused` is deliberately not a terminal event — it does not
        // end the campaign, and the pending run result is left intact so the
        // advance after `campaign resume` forms from it and the executor
        // continues from preserved work. It is emitted purely so the stop is
        // observable: an operator-issued pause needs no event because the
        // operator already knows, but this one stops the campaign with nobody
        // watching, and silence here is indistinguishable from progress.
        AdvanceOutcome::Pause { reason } => {
            campaign.status = CampaignStatus::Paused;
            tracing::warn!(
                campaign = %campaign.name,
                project = %campaign.project,
                %reason,
                "campaign paused instead of escalated: provider unusable"
            );
            return vec![terminal_event(
                EventType::CampaignPaused,
                campaign,
                throttle,
                reason,
            )];
        }
    };
    match decision {
        CampaignDecision::Done { reason } => {
            campaign.status = CampaignStatus::Completed;
            campaign.pending_run_result = None;
            vec![
                completed_event(
                    campaign,
                    throttle,
                    CampaignDecision::Done {
                        reason: reason.clone(),
                    },
                ),
                terminal_event(EventType::CampaignCompleted, campaign, throttle, reason),
            ]
        }
        CampaignDecision::Escalate { reason } => {
            campaign.status = CampaignStatus::Escalated;
            campaign.pending_run_result = None;
            vec![
                completed_event(
                    campaign,
                    throttle,
                    CampaignDecision::Escalate {
                        reason: reason.clone(),
                    },
                ),
                terminal_event(EventType::CampaignEscalated, campaign, throttle, reason),
            ]
        }
        CampaignDecision::Advance { objective, reason } => {
            campaign.status = CampaignStatus::Active;
            campaign.cycles_completed += 1;
            campaign.record_dispatched_objective(objective.clone());
            let base_ref = next_base_ref(request).map(ToString::to_string);
            campaign.pending_run_result = None;
            vec![
                completed_event(
                    campaign,
                    throttle,
                    CampaignDecision::Advance {
                        objective: objective.clone(),
                        reason,
                    },
                ),
                execution_event(campaign, throttle, &objective, base_ref),
            ]
        }
    }
}

async fn choose_advance_outcome(
    execution: &AdvanceExecution,
    campaign: &Campaign,
    forced: Option<AdvanceOutcome>,
) -> AdvanceOutcome {
    if let Some(outcome) = forced {
        return outcome;
    }
    let entry = match super::read_registry(&execution.registry) {
        Ok(registry) => registry
            .find_project(&campaign.project)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("project '{}' not found", campaign.project)),
        Err(error) => Err(error),
    };
    let entry = match entry {
        Ok(entry) => entry,
        Err(error) => {
            return AdvanceOutcome::Decided(CampaignDecision::Escalate {
                reason: format!("campaign formation failed: {error}"),
            });
        }
    };
    derive_advance_outcome(
        &*execution.agent,
        &*execution.shell,
        &entry,
        campaign,
        &execution.request,
    )
    .await
    .unwrap_or_else(|error| {
        AdvanceOutcome::Decided(CampaignDecision::Escalate {
            reason: format!("campaign formation failed: {error}"),
        })
    })
}

/// The advance to actually form from.
///
/// A manual advance carries no run result, so it replays whatever the campaign
/// recorded while it was paused — formation then sees the reviewer gaps and the
/// executor continues from the preserved ref.
fn replay_pending_run(execution: &AdvanceExecution, campaign: &Campaign) -> AdvanceExecution {
    let mut request = execution.request.clone();
    if request.run_result.is_none() {
        request.run_result.clone_from(&campaign.pending_run_result);
        request.run_event_id.clone_from(&campaign.last_run_event_id);
    }
    AdvanceExecution {
        request,
        agent: Arc::clone(&execution.agent),
        shell: Arc::clone(&execution.shell),
        registry: Arc::clone(&execution.registry),
        store_path: execution.store_path.clone(),
        lock: Arc::clone(&execution.lock),
        project: execution.project.clone(),
        throttle: execution.throttle,
    }
}

async fn execute_campaign_advance(execution: AdvanceExecution) -> TaskBlockResult {
    let _process_guard = execution.lock.lock().await;
    let mut guard = match CampaignStore::lock_exclusive(&execution.store_path) {
        Ok(guard) => guard,
        Err(error) => {
            return terminal_error_result(
                &execution.request.campaign,
                &execution.project,
                execution.throttle,
                0,
                0,
                format!("campaign store unavailable: {error}"),
            );
        }
    };
    let Some(campaign) = guard.store.find_mut(&execution.request.campaign) else {
        return terminal_error_result(
            &execution.request.campaign,
            &execution.project,
            execution.throttle,
            0,
            0,
            format!("campaign '{}' not found", execution.request.campaign),
        );
    };

    if let Err(error) = campaign.validate() {
        let events = apply_advance_outcome(
            campaign,
            &execution.request,
            execution.throttle,
            AdvanceOutcome::Decided(CampaignDecision::Escalate {
                reason: error.to_string(),
            }),
        );
        let cycles_completed = campaign.cycles_completed;
        let cycles_landed = campaign.cycles_landed;
        return persist_or_terminal(
            &execution,
            &guard,
            events,
            format!("campaign '{}' escalated", execution.request.campaign),
            "campaign invalid and escalation could not be saved",
            cycles_completed,
            cycles_landed,
        );
    }

    let forced = update_run_and_forced_outcome(campaign, &execution.request);
    if campaign.status == CampaignStatus::Paused {
        let cycles_completed = campaign.cycles_completed;
        let cycles_landed = campaign.cycles_landed;
        return persist_or_terminal(
            &execution,
            &guard,
            vec![],
            format!(
                "campaign '{}' is paused; run recorded without advancing",
                execution.request.campaign
            ),
            "paused campaign result could not be saved",
            cycles_completed,
            cycles_landed,
        );
    }

    let effective_execution = replay_pending_run(&execution, campaign);
    let outcome = choose_advance_outcome(&effective_execution, campaign, forced)
        .await
        .map_decision(|decision| enforce_campaign_budget(campaign, decision));
    // A pause emits no event, so its reason would otherwise be invisible; the
    // block summary is what carries it into the run trace.
    let summary = match &outcome {
        AdvanceOutcome::Decided(_) => format!("campaign '{}' advanced", execution.request.campaign),
        AdvanceOutcome::Pause { reason } => {
            format!("campaign '{}' paused: {reason}", execution.request.campaign)
        }
    };
    let events =
        apply_advance_outcome(campaign, &effective_execution.request, execution.throttle, outcome);
    let cycles_completed = campaign.cycles_completed;
    let cycles_landed = campaign.cycles_landed;
    persist_or_terminal(
        &execution,
        &guard,
        events,
        summary,
        "campaign decision could not be saved",
        cycles_completed,
        cycles_landed,
    )
}

impl SimulatedSuccess for AdvanceCampaign {
    type Outcome = Vec<Event>;

    fn simulate(&self, trigger: &Event) -> Vec<Event> {
        let request = trigger.parse_payload::<CampaignAdvanceRequestedPayload>().unwrap_or(
            CampaignAdvanceRequestedPayload {
                campaign: "unknown".to_string(),
                run_event_id: None,
                run_result: None,
            },
        );
        let campaign = CampaignStore::load(&self.store_path)
            .ok()
            .and_then(|store| store.find(&request.campaign).cloned());
        let Some(campaign) = campaign else {
            return vec![];
        };
        let outcome = CampaignDecision::Advance {
            objective: format!("Dry-run next objective for campaign '{}'.", campaign.name),
            reason: "dry-run formation".to_string(),
        };
        vec![
            completed_event(&campaign, trigger.throttle, outcome),
            execution_event(
                &campaign,
                foundry_sdk::throttle::Throttle::DryRun,
                &format!("Dry-run next objective for campaign '{}'.", campaign.name),
                None,
            ),
        ]
    }

    fn success_events(&self, _trigger: &Event, outcome: &Vec<Event>) -> Vec<Event> {
        outcome.clone()
    }
}

impl TaskBlock for AdvanceCampaign {
    task_block_meta! {
        name: "Advance Campaign",
        kind: Mutator,
        sinks_on: [CampaignAdvanceRequested],
    }

    dry_run_via_simulation!();

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let request = parse_payload!(trigger, CampaignAdvanceRequestedPayload);
        let project = trigger.project.clone();
        let throttle = trigger.throttle;
        let agent = Arc::clone(&self.agent);
        let shell = Arc::clone(&self.shell);
        let registry = Arc::clone(&self.registry);
        let store_path = self.store_path.clone();
        let lock = Arc::clone(&self.lock);

        Box::pin(async move {
            Ok(execute_campaign_advance(AdvanceExecution {
                agent,
                shell,
                registry,
                store_path,
                lock,
                request,
                project,
                throttle,
            })
            .await)
        })
    }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::campaign::{
        Campaign, CampaignBudget, CampaignCycle, CampaignStatus, CampaignStore, CycleOutcome,
        DoneEvidence, OwnerDecision,
    };
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::payload::{
        CampaignAdvanceRequestedPayload, CampaignDecision, LoopContext, TaskRunCompletedPayload,
        TaskVerdict,
    };
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use crate::gateway::fakes::{FakeAgentGateway, FakeShellGateway};
    use crate::gateway::{AgentFailureKind, AgentProvider, AgentResponse};

    use super::{
        AdvanceCampaign, DECISION_ATTEMPTS, enforce_campaign_budget, enforce_done_gate_truth,
        parse_decision, run_done_gates,
    };

    #[test]
    fn parses_structural_advance_decision() {
        let output = "```json\n{\"decision\":\"advance\",\"objective\":\"one thing\",\"reason\":\"gap\"}\n```";
        assert_eq!(
            parse_decision(output).unwrap(),
            CampaignDecision::Advance {
                objective: "one thing".to_string(),
                reason: "gap".to_string(),
            }
        );
    }

    #[test]
    fn rejects_decisions_without_human_readable_context() {
        for output in [
            r#"{"decision":"done","reason":""}"#,
            r#"{"decision":"escalate","reason":"   "}"#,
            r#"{"decision":"advance","objective":"","reason":"gap"}"#,
            r#"{"decision":"advance","objective":"next slice","reason":"\n"}"#,
        ] {
            assert!(parse_decision(output).is_err(), "accepted invalid decision: {output}");
        }
    }

    fn campaign_at_budget() -> Campaign {
        Campaign {
            name: "c".to_string(),
            project: "p".to_string(),
            mission: "ship".to_string(),
            intent_refs: vec![],
            context_paths: vec![],
            done_evidence: vec![DoneEvidence::Review {
                statement: "shipped".to_string(),
            }],
            budget: CampaignBudget { max_cycles: 2 },
            escalation: vec![],
            status: CampaignStatus::Active,
            cycles_completed: 2,
            cycles_landed: 2,
            authorized_by: Some("owner".to_string()),
            agent_provider: None,
            last_run_event_id: Some("run-2".to_string()),
            owner_decisions: vec![],
            pending_run_result: None,
            objective_history: vec![],
        }
    }

    #[test]
    fn final_budgeted_result_can_complete_campaign() {
        let decision = CampaignDecision::Done {
            reason: "all evidence passes".to_string(),
        };

        assert_eq!(enforce_campaign_budget(&campaign_at_budget(), decision.clone()), decision);
    }

    #[test]
    fn exhausted_budget_prevents_another_task_dispatch() {
        let decision = enforce_campaign_budget(
            &campaign_at_budget(),
            CampaignDecision::Advance {
                objective: "one more task".to_string(),
                reason: "gap remains".to_string(),
            },
        );

        assert_eq!(
            decision,
            CampaignDecision::Escalate {
                reason: "campaign cycle budget exhausted (2)".to_string(),
            }
        );
    }

    #[tokio::test]
    async fn missing_declared_artifact_fails_gate_without_running_command() {
        let dir = tempfile::tempdir().unwrap();
        let shell = FakeShellGateway::success();
        let evidence = vec![DoneEvidence::Gate {
            command: "mix test test/required_test.exs".to_string(),
            required: true,
            artifacts: vec!["test/required_test.exs".to_string()],
        }];

        let results = run_done_gates(&*shell, dir.path(), &evidence).await.unwrap();

        assert_eq!(results.len(), 1);
        assert!(!results[0].passed);
        assert!(results[0].output.contains("campaign gate artifact missing"));
        assert!(shell.invocations().is_empty());
    }

    #[tokio::test]
    async fn present_declared_artifact_allows_gate_command_to_run() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("test")).unwrap();
        std::fs::write(dir.path().join("test/required_test.exs"), "test").unwrap();
        let shell = FakeShellGateway::success();
        let evidence = vec![DoneEvidence::Gate {
            command: "mix test test/required_test.exs".to_string(),
            required: true,
            artifacts: vec!["test/required_test.exs".to_string()],
        }];

        let results = run_done_gates(&*shell, dir.path(), &evidence).await.unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].passed);
        assert_eq!(shell.invocations().len(), 1);
    }

    #[tokio::test]
    async fn blocked_on_decision_escalates_without_asking_agent() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                name: "c".to_string(),
                project: "p".to_string(),
                mission: "ship".to_string(),
                intent_refs: vec![],
                context_paths: vec![],
                done_evidence: vec![DoneEvidence::Review {
                    statement: "shipped".to_string(),
                }],
                budget: CampaignBudget::default(),
                escalation: vec![],
                status: CampaignStatus::Active,
                cycles_completed: 1,
                cycles_landed: 0,
                authorized_by: Some("owner".to_string()),
                agent_provider: None,
                last_run_event_id: None,
                owner_decisions: vec![],
                pending_run_result: None,
                objective_history: vec![],
            })
            .unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success();
        let block = AdvanceCampaign::new(
            agent.clone(),
            FakeShellGateway::success(),
            registry,
            store_path.clone(),
        );
        let run_result = TaskRunCompletedPayload {
            project: "p".to_string(),
            success: false,
            landed: false,
            summary: "decision needed".to_string(),
            preservation_ref: Some("foundry-task/preserved".to_string()),
            verdict: TaskVerdict::BlockedOnDecision {
                finding: "boundaries differ".to_string(),
                options: vec!["A".to_string(), "B".to_string()],
            },
            context: LoopContext {
                campaign: Some("c".to_string()),
                ..LoopContext::default()
            },
        };
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                campaign: "c".to_string(),
                run_event_id: Some("run-1".to_string()),
                run_result: Some(run_result),
            })
            .unwrap(),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.iter().any(|e| e.event_type == EventType::CampaignEscalated));
        assert!(agent.invocations().is_empty());
        assert_eq!(
            CampaignStore::load(&store_path).unwrap().find("c").unwrap().status,
            CampaignStatus::Escalated
        );
    }

    #[tokio::test]
    async fn paused_campaign_records_run_without_escalating_or_advancing() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                name: "c".to_string(),
                project: "p".to_string(),
                mission: "ship".to_string(),
                intent_refs: vec![],
                context_paths: vec![],
                done_evidence: vec![DoneEvidence::Review {
                    statement: "shipped".to_string(),
                }],
                budget: CampaignBudget::default(),
                escalation: vec![],
                status: CampaignStatus::Paused,
                cycles_completed: 1,
                cycles_landed: 0,
                authorized_by: Some("owner".to_string()),
                agent_provider: None,
                last_run_event_id: None,
                owner_decisions: vec![],
                pending_run_result: None,
                objective_history: vec![],
            })
            .unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success();
        let block = AdvanceCampaign::new(
            agent.clone(),
            FakeShellGateway::success(),
            registry,
            store_path.clone(),
        );
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                campaign: "c".to_string(),
                run_event_id: Some("run-1".to_string()),
                run_result: Some(TaskRunCompletedPayload {
                    project: "p".to_string(),
                    success: true,
                    landed: true,
                    summary: "landed".to_string(),
                    preservation_ref: None,
                    verdict: TaskVerdict::Complete,
                    context: LoopContext {
                        campaign: Some("c".to_string()),
                        ..LoopContext::default()
                    },
                }),
            })
            .unwrap(),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.events.is_empty());
        assert!(agent.invocations().is_empty());
        let stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Paused);
        assert_eq!(campaign.cycles_landed, 1);
        assert_eq!(campaign.last_run_event_id.as_deref(), Some("run-1"));
        assert_eq!(
            campaign
                .pending_run_result
                .as_ref()
                .and_then(|result| result.preservation_ref.as_deref()),
            None
        );
    }

    #[tokio::test]
    async fn paused_campaign_does_not_count_complete_result_that_did_not_land() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                name: "c".to_string(),
                project: "p".to_string(),
                mission: "ship".to_string(),
                intent_refs: vec![],
                context_paths: vec![],
                done_evidence: vec![DoneEvidence::Review {
                    statement: "shipped".to_string(),
                }],
                budget: CampaignBudget::default(),
                escalation: vec![],
                status: CampaignStatus::Paused,
                cycles_completed: 1,
                cycles_landed: 0,
                authorized_by: Some("owner".to_string()),
                agent_provider: None,
                last_run_event_id: None,
                owner_decisions: vec![],
                pending_run_result: None,
                objective_history: vec![],
            })
            .unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success();
        let block = AdvanceCampaign::new(
            agent.clone(),
            FakeShellGateway::success(),
            registry,
            store_path.clone(),
        );
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                campaign: "c".to_string(),
                run_event_id: Some("run-1".to_string()),
                run_result: Some(TaskRunCompletedPayload {
                    project: "p".to_string(),
                    success: true,
                    landed: false,
                    summary: "required no landing".to_string(),
                    preservation_ref: None,
                    verdict: TaskVerdict::Complete,
                    context: LoopContext {
                        campaign: Some("c".to_string()),
                        ..LoopContext::default()
                    },
                }),
            })
            .unwrap(),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.events.is_empty());
        assert!(agent.invocations().is_empty());
        let stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Paused);
        assert_eq!(campaign.cycles_landed, 0);
        assert_eq!(campaign.last_run_event_id.as_deref(), Some("run-1"));
    }

    fn campaign_for_accumulation_test() -> Campaign {
        Campaign {
            name: "c".to_string(),
            project: "p".to_string(),
            mission: "ship".to_string(),
            intent_refs: vec![],
            context_paths: vec![],
            done_evidence: vec![DoneEvidence::Review {
                statement: "shipped".to_string(),
            }],
            budget: CampaignBudget { max_cycles: 6 },
            escalation: vec![],
            status: CampaignStatus::Active,
            cycles_completed: 2,
            cycles_landed: 0,
            authorized_by: Some("owner".to_string()),
            agent_provider: None,
            last_run_event_id: None,
            owner_decisions: vec![],
            pending_run_result: None,
            objective_history: vec![],
        }
    }

    fn shell_result(stdout: &str) -> foundry_sdk::gateway::CommandResult {
        foundry_sdk::gateway::CommandResult {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        }
    }

    /// A non-landing cycle preserves its branch and chains forward, so the
    /// decision agent must see that branch's work or it re-cuts objectives the
    /// accumulated commits already satisfy.
    #[tokio::test]
    async fn decision_prompt_carries_accumulated_work_from_the_preserved_branch() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store.add(campaign_for_accumulation_test()).unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Land the accumulated converter work.\",\"reason\":\"unmerged chain carries the mission\"}\n```",
        );
        // status, log -8, then the two accumulated-work probes.
        let shell = FakeShellGateway::sequence(vec![
            shell_result("## main\n"),
            shell_result("2d4a2a7 trunk commit\n"),
            shell_result("d170a17 add parite-converterd\n1da793c add NVENC profile\n"),
            shell_result(" 48 files changed, 5734 insertions(+), 297 deletions(-)\n"),
        ]);
        let block =
            AdvanceCampaign::new(agent.clone(), shell.clone(), registry, store_path.clone());

        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                campaign: "c".to_string(),
                run_event_id: Some("run-9".to_string()),
                run_result: Some(TaskRunCompletedPayload {
                    project: "p".to_string(),
                    success: false,
                    landed: false,
                    summary: "task stopped with a typed non-complete verdict; work preserved"
                        .to_string(),
                    preservation_ref: Some("foundry-task/parite-61ef680dcf08".to_string()),
                    verdict: TaskVerdict::Remainder {
                        gaps: vec!["deployment evidence".to_string()],
                    },
                    context: LoopContext {
                        campaign: Some("c".to_string()),
                        ..LoopContext::default()
                    },
                }),
            })
            .unwrap(),
        );
        block.execute(&trigger).await.unwrap();

        let git_args: Vec<Vec<String>> = shell
            .invocations()
            .into_iter()
            .filter(|inv| inv.command == "git")
            .map(|inv| inv.args)
            .collect();
        assert!(
            git_args.contains(&vec![
                "log".to_string(),
                "--oneline".to_string(),
                "HEAD..foundry-task/parite-61ef680dcf08".to_string(),
            ]),
            "expected a commit-range probe against the preserved ref, got {git_args:?}"
        );
        assert!(
            git_args.contains(&vec![
                "diff".to_string(),
                "--stat".to_string(),
                "HEAD...foundry-task/parite-61ef680dcf08".to_string(),
            ]),
            "expected a diffstat probe against the preserved ref, got {git_args:?}"
        );

        let prompt = &agent.invocations()[0].prompt;
        assert!(prompt.contains("NEXT CYCLE BASE: foundry-task/parite-61ef680dcf08"));
        assert!(prompt.contains("d170a17 add parite-converterd"));
        assert!(prompt.contains("48 files changed, 5734 insertions(+)"));
        // The trunk snapshot must stay distinguishable from the accumulated work.
        assert!(prompt.contains("LIVE REPO SNAPSHOT (delivered trunk state)"));
        assert!(prompt.contains("ACCUMULATED UNMERGED WORK"));
    }

    #[tokio::test]
    async fn decision_prompt_states_no_carry_forward_without_a_preserved_ref() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store.add(campaign_for_accumulation_test()).unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Cut the first slice.\",\"reason\":\"nothing built yet\"}\n```",
        );
        let shell = FakeShellGateway::success();
        let block =
            AdvanceCampaign::new(agent.clone(), shell.clone(), registry, store_path.clone());

        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                campaign: "c".to_string(),
                run_event_id: None,
                run_result: None,
            })
            .unwrap(),
        );
        block.execute(&trigger).await.unwrap();

        let prompt = &agent.invocations()[0].prompt;
        assert!(prompt.contains("no preserved work carries forward"));
        assert!(
            !shell
                .invocations()
                .iter()
                .any(|inv| inv.args.iter().any(|arg| arg.contains("HEAD.."))),
            "must not probe a commit range when no ref carries forward"
        );
    }

    #[tokio::test]
    async fn resumed_campaign_forms_from_pending_result_and_landed_commit_ref() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let pending = TaskRunCompletedPayload {
            project: "p".to_string(),
            success: true,
            landed: true,
            summary: "first slice landed with one boundary test gap".to_string(),
            preservation_ref: Some("4a855db".to_string()),
            verdict: TaskVerdict::Remainder {
                gaps: vec!["exercise the generated gRPC boundary".to_string()],
            },
            context: LoopContext {
                campaign: Some("c".to_string()),
                ..LoopContext::default()
            },
        };
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                budget: CampaignBudget { max_cycles: 3 },
                status: CampaignStatus::Paused,
                cycles_completed: 1,
                ..campaign_for_accumulation_test()
            })
            .unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Add the generated gRPC boundary test.\",\"reason\":\"typed remainder\"}\n```",
        );
        let block = AdvanceCampaign::new(
            agent.clone(),
            FakeShellGateway::success(),
            registry,
            store_path.clone(),
        );

        let paused_trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                campaign: "c".to_string(),
                run_event_id: Some("run-1".to_string()),
                run_result: Some(pending.clone()),
            })
            .unwrap(),
        );
        let paused_result = block.execute(&paused_trigger).await.unwrap();
        assert!(paused_result.events.is_empty());
        let mut stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find_mut("c").unwrap();
        assert_eq!(campaign.pending_run_result.as_ref(), Some(&pending));
        campaign.status = CampaignStatus::Active;
        stored.save(&store_path).unwrap();

        let manual_trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                campaign: "c".to_string(),
                run_event_id: None,
                run_result: None,
            })
            .unwrap(),
        );
        let result = block.execute(&manual_trigger).await.unwrap();

        let execution = result
            .events
            .iter()
            .find(|event| event.event_type == EventType::ExecutionRequested)
            .expect("next task dispatched");
        assert_eq!(
            execution.payload.get("base_ref").and_then(serde_json::Value::as_str),
            Some("4a855db")
        );
        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert!(invocations[0].prompt.contains("exercise the generated gRPC boundary"));
        assert!(invocations[0].prompt.contains("first slice landed with one boundary test gap"));

        let stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find("c").unwrap();
        assert_eq!(campaign.last_run_event_id.as_deref(), Some("run-1"));
        assert_eq!(campaign.cycles_landed, 1);
        assert_eq!(campaign.cycles_completed, 2);
        assert!(campaign.pending_run_result.is_none());
    }

    #[tokio::test]
    async fn recorded_owner_decision_appears_in_next_formation_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        std::fs::write(dir.path().join("CHARTER.md"), "charter").unwrap();
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                name: "c".to_string(),
                project: "p".to_string(),
                mission: "ship".to_string(),
                intent_refs: vec![],
                context_paths: vec![],
                done_evidence: vec![DoneEvidence::Review {
                    statement: "shipped".to_string(),
                }],
                budget: CampaignBudget { max_cycles: 3 },
                escalation: vec![],
                status: CampaignStatus::Active,
                cycles_completed: 1,
                cycles_landed: 1,
                authorized_by: Some("owner".to_string()),
                agent_provider: None,
                last_run_event_id: Some("run-1".to_string()),
                owner_decisions: vec![OwnerDecision {
                    decision: "Prefer the generated gRPC client path; do not add raw JSON shims."
                        .to_string(),
                    authorized_by: "owner".to_string(),
                    decided_at: chrono::DateTime::parse_from_rfc3339("2026-07-18T12:00:00Z")
                        .unwrap()
                        .with_timezone(&chrono::Utc),
                }],
                pending_run_result: None,
                objective_history: vec![],
            })
            .unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Add the generated client test.\",\"reason\":\"owner policy recorded\"}\n```",
        );
        let block =
            AdvanceCampaign::new(agent.clone(), FakeShellGateway::success(), registry, store_path);
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                campaign: "c".to_string(),
                run_event_id: None,
                run_result: None,
            })
            .unwrap(),
        );

        let _ = block.execute(&trigger).await.unwrap();

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert!(
            invocations[0]
                .prompt
                .contains("Prefer the generated gRPC client path; do not add raw JSON shims.")
        );
        assert!(invocations[0].prompt.contains("OWNER DECISIONS"));
        assert!(invocations[0].prompt.contains("2026-07-18T12:00:00+00:00 [owner]"));
    }

    // --- objective history makes a repeat visible -------------------------

    /// `foundry-daemon-authoritative-state-v1` ran eleven cycles and landed
    /// once, restating the same reason from cycle 1 through cycle 9. Shown one
    /// cycle of history, the agent had no way to see that.
    #[tokio::test]
    async fn decision_prompt_carries_every_objective_this_campaign_has_cut() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut campaign = campaign_for_accumulation_test();
        campaign.objective_history = vec![
            CampaignCycle {
                cycle: 1,
                objective: "Move registry list/show onto daemon-owned gRPC.".to_string(),
                outcome: Some(CycleOutcome {
                    verdict: TaskVerdict::Remainder {
                        gaps: vec!["RegistryCommands::List still reads local files".to_string()],
                    },
                    landed: true,
                }),
            },
            CampaignCycle {
                cycle: 2,
                objective: "Add the RegistryList RPC to proto/foundry.proto.".to_string(),
                outcome: None,
            },
        ];
        let mut store = CampaignStore::default();
        store.add(campaign).unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Route the CLI at the new RPC.\",\"reason\":\"the RPC exists but nothing calls it\"}\n```",
        );
        let block =
            AdvanceCampaign::new(agent.clone(), FakeShellGateway::success(), registry, store_path);

        block.execute(&manual_advance_trigger()).await.unwrap();

        let prompt = &agent.invocations()[0].prompt;
        assert!(prompt.contains("OBJECTIVE HISTORY"));
        assert!(
            prompt.contains(
                "- cycle 1 [remainder, landed; gaps: RegistryCommands::List still reads local files] Move registry list/show onto daemon-owned gRPC."
            ),
            "history entry lost its objective or its verdict: {prompt}"
        );
        assert!(
            prompt.contains(
                "- cycle 2 [dispatched, no result recorded] Add the RegistryList RPC to proto/foundry.proto."
            ),
            "an in-flight cycle must still be visible: {prompt}"
        );
        // The history is only useful if the agent is told what to do with it,
        // and the wrong-tree reading stays the first branch to check.
        assert!(prompt.contains("NO RESTATEMENT"));
        assert!(prompt.contains("you are reading the wrong tree"));
        assert!(prompt.contains("A third identical objective is never the answer."));
    }

    #[tokio::test]
    async fn first_formation_states_that_nothing_has_been_cut_yet() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Cut the first slice.\",\"reason\":\"nothing built yet\"}\n```",
        );
        let block =
            AdvanceCampaign::new(agent.clone(), FakeShellGateway::success(), registry, store_path);

        block.execute(&manual_advance_trigger()).await.unwrap();

        assert!(
            agent.invocations()[0]
                .prompt
                .contains("none — this is the first objective this campaign has cut.")
        );
    }

    /// The dispatch and its result arrive on different advances, so the loop is
    /// only closed if the objective survives from one to the other and the next
    /// formation can see both halves.
    #[tokio::test]
    async fn a_dispatched_objective_and_its_verdict_reach_the_following_formation() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let agent = FakeAgentGateway::sequence(vec![
            AgentResponse::success(
                "```json\n{\"decision\":\"advance\",\"objective\":\"Move registry list/show onto daemon-owned gRPC.\",\"reason\":\"the CLI still reads local files\"}\n```",
            ),
            AgentResponse::success(
                "```json\n{\"decision\":\"advance\",\"objective\":\"Route registry init at the new RPC.\",\"reason\":\"one command still bypasses the daemon\"}\n```",
            ),
        ]);
        let block = AdvanceCampaign::new(
            agent.clone(),
            FakeShellGateway::success(),
            registry,
            store_path.clone(),
        );

        block.execute(&manual_advance_trigger()).await.unwrap();
        let result_trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                campaign: "c".to_string(),
                run_event_id: Some("run-1".to_string()),
                run_result: Some(TaskRunCompletedPayload {
                    project: "p".to_string(),
                    success: false,
                    landed: true,
                    summary: "the RPCs exist; the CLI still routes locally".to_string(),
                    preservation_ref: None,
                    verdict: TaskVerdict::Remainder {
                        gaps: vec!["registry init still writes the local file".to_string()],
                    },
                    context: LoopContext {
                        campaign: Some("c".to_string()),
                        ..LoopContext::default()
                    },
                }),
            })
            .unwrap(),
        );
        block.execute(&result_trigger).await.unwrap();

        let second_prompt = &agent.invocations()[1].prompt;
        assert!(
            second_prompt.contains(
                "- cycle 3 [remainder, landed; gaps: registry init still writes the local file] Move registry list/show onto daemon-owned gRPC."
            ),
            "the previous objective and its verdict did not reach formation: {second_prompt}"
        );

        let stored = CampaignStore::load(&store_path).unwrap();
        let history = &stored.find("c").unwrap().objective_history;
        assert_eq!(history.len(), 2);
        assert_eq!(history[1].objective, "Route registry init at the new RPC.");
        assert!(history[1].outcome.is_none(), "the in-flight cycle has no result yet");
    }

    /// A synthesized gate objective runs to several paragraphs; rendered whole,
    /// eight of them would crowd out the repository state the decision needs.
    #[tokio::test]
    async fn a_long_objective_is_condensed_to_one_line_in_the_history() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut campaign = campaign_for_accumulation_test();
        campaign.objective_history = vec![CampaignCycle {
            cycle: 1,
            objective: format!("Make every required gate pass.\n\n{}", "detail ".repeat(200)),
            outcome: None,
        }];
        let mut store = CampaignStore::default();
        store.add(campaign).unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Next slice.\",\"reason\":\"gap\"}\n```",
        );
        let block =
            AdvanceCampaign::new(agent.clone(), FakeShellGateway::success(), registry, store_path);

        block.execute(&manual_advance_trigger()).await.unwrap();

        let entry = agent.invocations()[0]
            .prompt
            .lines()
            .find(|line| line.starts_with("- cycle 1 "))
            .expect("history entry rendered")
            .to_string();
        assert!(entry.contains("Make every required gate pass. detail"));
        assert!(entry.ends_with('…'), "a long objective must be truncated: {entry}");
        assert!(entry.chars().count() < 400, "history entry is not bounded: {entry}");
    }

    // --- transient decision-agent failures --------------------------------

    /// An active campaign with a live store, ready to be advanced.
    fn staged_active_campaign(
        dir: &std::path::Path,
    ) -> (
        std::path::PathBuf,
        std::sync::Arc<std::sync::RwLock<foundry_sdk::registry::Registry>>,
    ) {
        let store_path = dir.join("campaigns.json");
        let mut store = CampaignStore::default();
        store.add(campaign_for_accumulation_test()).unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.to_str().unwrap());
        (store_path, registry)
    }

    /// A manual advance request carrying no run result.
    fn manual_advance_trigger() -> Event {
        Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                campaign: "c".to_string(),
                run_event_id: None,
                run_result: None,
            })
            .unwrap(),
        )
    }

    fn run_result_with(verdict: TaskVerdict) -> TaskRunCompletedPayload {
        TaskRunCompletedPayload {
            project: "p".to_string(),
            success: false,
            landed: false,
            summary: "runner stopped".to_string(),
            preservation_ref: Some("foundry-task/preserved".to_string()),
            verdict,
            context: LoopContext {
                campaign: Some("c".to_string()),
                ..LoopContext::default()
            },
        }
    }

    fn terminal_reason(
        result: &foundry_sdk::task_block::TaskBlockResult,
        ty: &EventType,
    ) -> String {
        result
            .events
            .iter()
            .find(|event| event.event_type == *ty)
            .and_then(|event| event.payload.get("reason"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("no {ty} event in {:?}", result.events))
            .to_string()
    }

    /// The campaign with the best landing rate on record was ended twice by a
    /// single transport failure, both times right after a landed cycle.
    #[tokio::test(start_paused = true)]
    async fn transient_decision_agent_failure_is_retried_instead_of_ending_the_campaign() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let agent = FakeAgentGateway::sequence(vec![
            AgentResponse::failure(""),
            AgentResponse::failure("connection reset by peer"),
            AgentResponse::success(
                "```json\n{\"decision\":\"advance\",\"objective\":\"Land the next slice.\",\"reason\":\"gap remains\"}\n```",
            ),
        ]);
        let block = AdvanceCampaign::new(
            agent.clone(),
            FakeShellGateway::success(),
            registry,
            store_path.clone(),
        );

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert_eq!(
            agent.invocations().len(),
            3,
            "a transient failure must be re-asked, not converted straight to escalation"
        );
        assert!(result.events.iter().any(|e| e.event_type == EventType::ExecutionRequested));
        assert_eq!(
            CampaignStore::load(&store_path).unwrap().find("c").unwrap().status,
            CampaignStatus::Active
        );
    }

    /// The observed escalations carried an empty reason, so nobody could tell
    /// what had failed. Every attempt now appears in the escalation.
    #[tokio::test(start_paused = true)]
    async fn exhausted_decision_agent_retries_escalate_with_every_attempt_reported() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let agent = FakeAgentGateway::failure("");
        let block = AdvanceCampaign::new(
            agent.clone(),
            FakeShellGateway::success(),
            registry,
            store_path.clone(),
        );

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert_eq!(agent.invocations().len(), DECISION_ATTEMPTS as usize);
        let reason = terminal_reason(&result, &EventType::CampaignEscalated);
        assert!(reason.contains("3 attempts"), "reason omits the attempt count: {reason}");
        for attempt in 1..=DECISION_ATTEMPTS {
            assert!(
                reason.contains(&format!("attempt {attempt}")),
                "reason omits attempt {attempt}: {reason}"
            );
        }
        assert!(reason.contains("no diagnostic output"), "empty stderr left no trace: {reason}");
        assert_eq!(
            CampaignStore::load(&store_path).unwrap().find("c").unwrap().status,
            CampaignStatus::Escalated
        );
    }

    /// Re-asking an agent that already answered only burns identical
    /// invocations — the answer will be malformed again.
    #[tokio::test(start_paused = true)]
    async fn malformed_decision_escalates_without_burning_retries() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let agent = FakeAgentGateway::success_with("I think we should keep going.");
        let block = AdvanceCampaign::new(
            agent.clone(),
            FakeShellGateway::success(),
            registry,
            store_path.clone(),
        );

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert_eq!(agent.invocations().len(), 1, "a permanent failure must not be retried");
        assert!(
            terminal_reason(&result, &EventType::CampaignEscalated)
                .contains("campaign formation failed")
        );
    }

    // --- provider outages pause rather than escalate ----------------------

    /// A tripped breaker killed a campaign at cycle 0, twice, before it had
    /// done any work. The breaker stays open, so retrying is pointless and
    /// escalating spends the campaign on something outside its own work.
    #[tokio::test(start_paused = true)]
    async fn open_provider_circuit_breaker_pauses_the_campaign_without_retrying() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let agent = FakeAgentGateway::always(AgentResponse::terminal_failure(
            AgentProvider::Claude,
            AgentFailureKind::AccountLimit,
            "You've hit your monthly spend limit (circuit breaker open for provider claude)",
        ));
        let block = AdvanceCampaign::new(
            agent.clone(),
            FakeShellGateway::success(),
            registry,
            store_path.clone(),
        );

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert_eq!(agent.invocations().len(), 1, "an open breaker returns the same refusal");
        // Observable, but not terminal: an automatic pause has no operator
        // watching it, so it must reach the event stream — while leaving no
        // escalation or completion to resurrect from.
        assert_eq!(
            result.events.iter().map(|event| &event.event_type).collect::<Vec<_>>(),
            vec![&EventType::CampaignPaused],
            "an automatic pause must be visible in the stream and nothing more: {:?}",
            result.events
        );
        assert!(
            result.events[0].payload["reason"]
                .as_str()
                .is_some_and(|reason| reason.contains("monthly spend limit")),
            "pause event must explain itself: {:?}",
            result.events[0].payload
        );
        assert!(result.summary.contains("paused"), "pause reason lost: {}", result.summary);
        assert!(result.summary.contains("monthly spend limit"));
        let stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Paused);
        assert_eq!(campaign.cycles_completed, 2, "a pause must not consume a cycle");
    }

    /// The same breaker text also arrives as the executor's typed verdict.
    #[tokio::test(start_paused = true)]
    async fn runner_error_from_an_open_breaker_pauses_and_keeps_the_run_pending() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let agent = FakeAgentGateway::success();
        let block = AdvanceCampaign::new(
            agent.clone(),
            FakeShellGateway::success(),
            registry,
            store_path.clone(),
        );
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                campaign: "c".to_string(),
                run_event_id: Some("run-1".to_string()),
                run_result: Some(run_result_with(TaskVerdict::RunnerError {
                    detail: "agent account limit reached: You've hit your monthly spend limit \
                             (circuit breaker open for provider claude)"
                        .to_string(),
                })),
            })
            .unwrap(),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(agent.invocations().is_empty());
        assert_eq!(
            result.events.iter().map(|event| &event.event_type).collect::<Vec<_>>(),
            vec![&EventType::CampaignPaused],
            "the verdict path must be as observable as the decision path: {:?}",
            result.events
        );
        let stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Paused);
        assert_eq!(
            campaign
                .pending_run_result
                .as_ref()
                .and_then(|result| result.preservation_ref.as_deref()),
            Some("foundry-task/preserved"),
            "the resumed advance must still see the preserved work"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn runner_error_unrelated_to_the_provider_still_escalates() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let block = AdvanceCampaign::new(
            FakeAgentGateway::success(),
            FakeShellGateway::success(),
            registry,
            store_path.clone(),
        );
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                campaign: "c".to_string(),
                run_event_id: Some("run-1".to_string()),
                run_result: Some(run_result_with(TaskVerdict::RunnerError {
                    detail: "task review missing isolated worktree".to_string(),
                })),
            })
            .unwrap(),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(
            terminal_reason(&result, &EventType::CampaignEscalated)
                .contains("runner/provider unavailable")
        );
        assert_eq!(
            CampaignStore::load(&store_path).unwrap().find("c").unwrap().status,
            CampaignStatus::Escalated
        );
    }

    // --- red done-gates dispatch an actionable objective ------------------

    fn failing_clippy_gate() -> foundry_sdk::gates::GateResult {
        foundry_sdk::gates::GateResult {
            name: "campaign_done_1".to_string(),
            command: "cargo clippy --workspace --all-targets -- -D warnings".to_string(),
            passed: false,
            required: true,
            output: "error: this function has too many lines (145/100)\n  \
                     --> crates/foundry-cli/tests/registry_cli.rs:349:1"
                .to_string(),
            exit_code: 1,
            duration_ms: Some(10),
            fix_applied: false,
        }
    }

    /// Dispatched with nothing but a gate command, cycles were observed
    /// rewriting the very test file the lint named.
    #[test]
    fn red_done_gate_objective_carries_the_mission_and_the_failing_output() {
        let mut campaign = campaign_for_accumulation_test();
        campaign.mission =
            "Expose campaign state over gRPC with a live CLI integration test.".to_string();
        let gate = failing_clippy_gate();

        let decision = enforce_done_gate_truth(
            &campaign,
            CampaignDecision::Done {
                reason: "the mission looks shipped".to_string(),
            },
            std::slice::from_ref(&gate),
        );

        let CampaignDecision::Advance { objective, .. } = decision else {
            panic!("a red required gate must rewrite done into advance");
        };
        assert!(objective.contains(&campaign.mission), "objective lost the mission: {objective}");
        assert!(
            objective.contains(&gate.command),
            "objective lost the gate command: {objective}"
        );
        assert!(
            objective.contains("crates/foundry-cli/tests/registry_cli.rs:349"),
            "objective lost the gate output: {objective}"
        );
        assert!(objective.contains("too many lines"));
    }

    #[test]
    fn passing_required_gates_leave_a_done_decision_alone() {
        let campaign = campaign_for_accumulation_test();
        let gate = foundry_sdk::gates::GateResult {
            passed: true,
            ..failing_clippy_gate()
        };
        let decision = CampaignDecision::Done {
            reason: "every gate is green".to_string(),
        };

        assert_eq!(
            enforce_done_gate_truth(&campaign, decision.clone(), std::slice::from_ref(&gate)),
            decision
        );
    }
}
