use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use foundry_sdk::campaign::escalation::REESCALATION_REASON;
use foundry_sdk::campaign::{
    Campaign, CampaignEscalation, CampaignStatus, CampaignStore, CampaignStoreGuard, CycleOutcome,
    DoneEvidence,
};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gateway::{AgentStage, StageProviders};
use foundry_sdk::initiative::{Finding, FindingKind, FindingSource, InitiativeContext};
use foundry_sdk::payload::{
    CampaignAdvanceCompletedPayload, CampaignAdvanceOutcome, CampaignAdvanceRequestedPayload,
    CampaignDecision, CampaignStandDown, CampaignTerminalPayload, LandBlocked,
    TaskRunCompletedPayload, TaskVerdict,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::{WorkItemKind, WorkItemState, WorkItemStore};
use foundry_sdk::work_source::WorkSource;

use crate::gateway::{
    AgentAccess, AgentFailureMetadata, AgentGateway, AgentOutcome, AgentProvider, ModelTier,
    ReasoningEffort, ShellGateway,
};

use super::{AgentBlockSpec, SimulatedSuccess, invoke_agent};

pub struct AdvanceCampaign {
    agent: Arc<dyn AgentGateway>,
    shell: Arc<dyn ShellGateway>,
    registry: Arc<std::sync::RwLock<Registry>>,
    store_path: PathBuf,
    work_items_path: PathBuf,
    formation_locks: Arc<FormationLocks>,
}

impl AdvanceCampaign {
    /// Advance campaigns held in the store at `store_path`.
    ///
    /// `work_items_path` is the work-item ledger. Formation reads it to learn
    /// whether the preserved work a cycle would continue from has since been
    /// settled by its owner or found on trunk; it never writes it.
    pub fn new(
        agent: Arc<dyn AgentGateway>,
        shell: Arc<dyn ShellGateway>,
        registry: Arc<std::sync::RwLock<Registry>>,
        store_path: PathBuf,
        work_items_path: PathBuf,
    ) -> Self {
        Self {
            agent,
            shell,
            registry,
            store_path,
            work_items_path,
            formation_locks: Arc::new(FormationLocks::default()),
        }
    }
}

/// One async mutex per campaign name.
///
/// A campaign never forms twice at once: a second advance for the same
/// campaign waits here, then reads the record the first one left. Different
/// campaigns hold different mutexes, so their formations run side by side.
/// This lock is in-process only and is held across the agent call; the
/// cross-process campaign store flock is not (see
/// [`execute_campaign_advance`]).
///
/// Entries are never removed. A campaign name costs one small entry for the
/// life of the daemon, and removing one safely would need the same
/// coordination this map exists to provide.
#[derive(Default)]
struct FormationLocks(std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>);

impl FormationLocks {
    fn for_campaign(&self, campaign: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.0.lock().unwrap_or_else(|poisoned| {
            // The map holds no invariant beyond membership, so a panic while
            // it was held cannot have left it inconsistent. Recover rather
            // than take the daemon down.
            tracing::warn!("campaign formation lock map poisoned; recovering");
            poisoned.into_inner()
        });
        Arc::clone(locks.entry(campaign.to_string()).or_default())
    }
}

/// Keep normative files available without copying them into every decision.
fn read_context_files(repo: &Path, paths: &[String]) -> anyhow::Result<String> {
    let repo = repo.canonicalize()?;
    let mut lines = Vec::new();
    for relative in paths {
        let path = repo.join(relative).canonicalize()?;
        if !path.starts_with(&repo) {
            anyhow::bail!("campaign context path escapes project: {relative}");
        }
        let bytes = std::fs::metadata(&path)?.len();
        lines.push(format!(
            "- {relative} ({bytes} bytes; read relevant contract sections before deciding)"
        ));
    }
    Ok(format!(
        "READ THESE YOURSELF; preserve normative wording and owner policy.\n{}",
        lines.join("\n")
    ))
}

/// Bound orienting Git output. Keep the exact command so formation can read
/// the complete evidence selectively; mission and owner policy are untouched.
fn compact_git(text: String, limit: usize, command: &str) -> String {
    if text.len() <= limit {
        return text;
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[{} bytes omitted; read full evidence with `{command}`]",
        &text[..end],
        text.len() - end
    )
}

async fn repo_snapshot(shell: &dyn ShellGateway, repo: &Path) -> String {
    let status = shell.run(repo, "git", &["status", "--short", "--branch"], None, None).await;
    let log = shell.run(repo, "git", &["log", "-8", "--oneline"], None, None).await;
    let changed = shell
        .run(repo, "git", &["show", "--format=", "--name-status", "HEAD"], None, None)
        .await;
    format!(
        "STATUS:\n{}\n\nRECENT COMMITS:\n{}\n\nLATEST COMMIT PATHS:\n{}",
        compact_git(
            status.map_or_else(|e| e.to_string(), |r| format!("{}{}", r.stdout, r.stderr)),
            1024,
            "git status --short --branch"
        ),
        compact_git(
            log.map_or_else(|e| e.to_string(), |r| format!("{}{}", r.stdout, r.stderr)),
            1024,
            "git log -8 --oneline"
        ),
        compact_git(
            changed.map_or_else(|e| e.to_string(), |r| format!("{}{}", r.stdout, r.stderr)),
            2048,
            "git show --format= --name-status HEAD"
        )
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

/// The work item holding the preserved work a cycle would continue from, when
/// that item has since been settled outside the campaign.
///
/// An owner who reconciles preserved work onto trunk by hand closes its item
/// (`foundry queue close`, settled `cancelled`), and supersession detection
/// settles an item `landed` once trunk carries its work. Neither writes the
/// campaign store, so the campaign's pending run result still names the ref.
/// Continuing from it then replays work trunk already has, and the
/// continuation guard refuses the cycle before any agent runs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SettledBase {
    /// The preservation ref the cycle would have continued from.
    reference: String,
    item_id: String,
    state: WorkItemState,
    reason: String,
    /// The owner command that settled the item, when a person did.
    owner_command: Option<String>,
}

impl SettledBase {
    /// How the item was settled, for formation: `settled cancelled by an
    /// owner close: <reason>` or `settled landed: <reason>`.
    fn describe(&self) -> String {
        match &self.owner_command {
            Some(command) => {
                format!("settled {} by an owner {command}: {}", self.state.tag(), self.reason)
            }
            None => format!("settled {}: {}", self.state.tag(), self.reason),
        }
    }
}

/// Find the cycle item of `campaign` in `project` whose disposition preserves
/// `reference`, and report it when it is settled `cancelled` or `landed`.
///
/// When several items record the same ref, the latest submitted decides. An
/// item still open (`preserved`, `needs_decision`, `failed`) or not yet settled
/// keeps carrying its work forward, as does a ref no item records.
fn settled_base(
    ledger: &WorkItemStore,
    campaign: &str,
    project: &str,
    reference: &str,
) -> Option<SettledBase> {
    let item = ledger
        .items
        .iter()
        .filter(|item| {
            item.kind == WorkItemKind::CampaignCycle
                && item.project == project
                && item.campaign_name() == Some(campaign)
                && item
                    .disposition
                    .as_ref()
                    .and_then(|disposition| disposition.preservation_ref.as_deref())
                    == Some(reference)
        })
        .max_by_key(|item| item.submitted_at)?;
    matches!(item.state, WorkItemState::Cancelled | WorkItemState::Landed).then(|| SettledBase {
        reference: reference.to_string(),
        item_id: item.id.clone(),
        state: item.state,
        reason: item.reason.clone(),
        owner_command: item.operator_action.as_ref().map(|action| action.command.clone()),
    })
}

/// Read the ledger at `work_items_path` and report whether the ref `request`
/// would continue from belongs to a settled item.
///
/// The ledger is read off the runtime, as the other ledger readers do: on a
/// long-lived daemon it holds every item ever dispatched.
async fn resolve_settled_base(
    work_items_path: &Path,
    campaign: &Campaign,
    request: &CampaignAdvanceRequestedPayload,
) -> Option<SettledBase> {
    let reference = next_base_ref(request)?;
    let path = work_items_path.to_path_buf();
    let loaded = tokio::task::spawn_blocking(move || WorkItemStore::load(&path))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result.map_err(anyhow::Error::from));
    let ledger = match loaded {
        Ok(ledger) => ledger,
        Err(error) => {
            // Best-effort: without the ledger nobody can say the work was
            // settled, so the cycle continues from the ref exactly as it did
            // before this check existed. The warn carries the fault.
            tracing::warn!(
                campaign = %campaign.name,
                path = %work_items_path.display(),
                %error,
                "could not read the work-item ledger; continuing from the preserved ref"
            );
            return None;
        }
    };
    settled_base(&ledger, &campaign.name, &campaign.project, reference)
}

/// Whether the last cycle was a continuation refused before its agent session
/// because its preserved work conflicts with trunk.
///
/// Such a result carries no preservation ref, so the next cycle starts from
/// trunk instead of re-trying the same refused base.
fn continuation_refused(request: &CampaignAdvanceRequestedPayload) -> bool {
    request
        .run_result
        .as_ref()
        .is_some_and(|result| result.land_blocked == Some(LandBlocked::ContinuationConflict))
}

/// Work reachable from the next cycle's base ref but absent from the canonical
/// checkout.
///
/// Non-landing cycles (`remainder`, `defect`) preserve their branch and chain
/// forward, so from the second such cycle onward the canonical checkout no
/// longer reflects what the executor will actually start from. Without this the
/// decision agent re-cuts objectives the accumulated work already satisfies.
///
/// `settled` names preserved work whose item was settled outside the campaign.
/// The request then carries no preservation ref, and formation is told why, so
/// it does not treat that work as still pending.
async fn accumulated_work(
    shell: &dyn ShellGateway,
    repo: &Path,
    request: &CampaignAdvanceRequestedPayload,
    settled: Option<&SettledBase>,
) -> String {
    if let Some(settled) = settled {
        return format!(
            "NEXT CYCLE BASE: the live checkout above. The preserved work at {} (work item {}) \
             was {}. It no longer carries forward: the next cycle starts from trunk, not from \
             that ref. Do not assume that work is still pending. Where it was reconciled onto \
             trunk, the live snapshot above shows it; cut the next objective from the mission \
             minus trunk, and reapply nothing from that ref unless trunk lacks something the \
             mission still needs.",
            settled.reference,
            settled.item_id,
            settled.describe()
        );
    }
    let Some(reference) = next_base_ref(request) else {
        if continuation_refused(request) {
            return "NEXT CYCLE BASE: the live checkout above. The last cycle was refused before \
                    its agent session: the preserved work it was to continue is behind trunk and \
                    conflicts with it (LAST TYPED RUN RESULT names the ref, the commit and the \
                    conflicting files). That work is still durable at its ref but no longer \
                    carries forward. If it is still needed, cut an objective that reapplies it \
                    onto trunk, resolving those files (the commit is readable with git show); \
                    otherwise cut the next objective from trunk, or escalate."
                .to_string();
        }
        return "NEXT CYCLE BASE: the live checkout above — no preserved work carries forward."
            .to_string();
    };
    let commit_range = format!("HEAD..{reference}");
    let diff_range = format!("HEAD...{reference}");
    let commits = shell.run(repo, "git", &["log", "--oneline", &commit_range], None, None).await;
    let stat = shell.run(repo, "git", &["diff", "--stat", &diff_range], None, None).await;
    format!(
        "NEXT CYCLE BASE: {reference}\n\nCOMMITS ON THAT REF ABSENT FROM THE LIVE CHECKOUT:\n{}\n\nFILES IT CHANGES vs THE LIVE CHECKOUT:\n{}",
        compact_git(
            or_none(
                commits.map_or_else(|e| e.to_string(), |r| format!("{}{}", r.stdout, r.stderr))
            ),
            1024,
            &format!("git log --oneline {commit_range}")
        ),
        compact_git(
            or_none(stat.map_or_else(|e| e.to_string(), |r| format!("{}{}", r.stdout, r.stderr))),
            2048,
            &format!("git diff --stat {diff_range}")
        )
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
    let landing = match (outcome.landed, outcome.land_blocked) {
        (true, _) => "landed".to_string(),
        (false, Some(reason)) => format!("did not land ({reason}), preserved for the next cycle"),
        (false, None) => "did not land".to_string(),
    };
    let detail = match &outcome.verdict {
        TaskVerdict::Complete => format!("complete, {landing}"),
        TaskVerdict::Remainder { gaps } => {
            format!("remainder, {landing}; gaps: {}", gaps.join("; "))
        }
        TaskVerdict::Defect { diagnosis } => format!("defect, {landing}; {diagnosis}"),
        TaskVerdict::BlockedOnDecision { finding, .. } => format!("blocked on decision; {finding}"),
        TaskVerdict::BlockedOnRecovery { kind, detail } => format!("recovery {kind:?}; {detail}"),
        TaskVerdict::RunnerError { detail } => format!("runner error; {detail}"),
    };
    condense(&detail)
}

/// One line telling formation whether the last run's agent session started
/// and why it ended.
///
/// Without it formation reads a verdict and a reason and cannot tell a runner
/// outage from a work defect: on 2026-10-19 it judged that repeating an
/// objective whose Codex session never started would fail again, and
/// recorded `capability_unavailable` recoveries for a provider at capacity.
fn last_run_line(result: &TaskRunCompletedPayload) -> String {
    if result.failure.is_provider_unavailable() {
        return format!(
            "last run: did not start ({}); no agent worked on the objective",
            result.failure.execution_summary()
        );
    }
    if result.land_blocked == Some(LandBlocked::ContinuationConflict) {
        return format!(
            "last run: did not start (its continuation was refused before any agent session: {})",
            condense(&result.summary)
        );
    }
    if result.failure.is_terminal_provider_failure() {
        return format!(
            "last run: stopped by its provider ({})",
            result.failure.execution_summary()
        );
    }
    let detail = match &result.verdict {
        TaskVerdict::Complete => String::new(),
        TaskVerdict::Remainder { gaps } => gaps.join("; "),
        TaskVerdict::Defect { diagnosis } => diagnosis.clone(),
        TaskVerdict::BlockedOnDecision { finding, .. } => finding.clone(),
        TaskVerdict::BlockedOnRecovery { kind, detail } => format!("{kind}: {detail}"),
        TaskVerdict::RunnerError { detail } => {
            return format!(
                "last run: ended with a runner error before review: {}",
                condense(detail)
            );
        }
    };
    let tag = result.verdict.tag();
    if detail.is_empty() {
        format!("last run: started, ended with verdict {tag}")
    } else {
        format!("last run: started, ended with verdict {tag}: {}", condense(&detail))
    }
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
    #[allow(
        clippy::expect_used,
        reason = "GateResult contains no non-string map keys or non-finite floats"
    )]
    let gates = serde_json::to_string_pretty(&formation_gate_results(gate_results))
        .expect("gate results are infallibly serializable");
    let last_result = request.run_result.as_ref().map_or_else(
        || "none (initial/manual advance)".to_string(),
        |result| {
            let started = last_run_line(result);
            #[allow(
                clippy::expect_used,
                reason = "TaskRunCompletedPayload contains only strings, bools, and typed \
                          sub-structs — no non-string map keys or non-finite floats"
            )]
            let typed = serde_json::to_string_pretty(&serde_json::json!({
                "summary": result.summary,
                "verdict": result.verdict,
                "landed": result.landed,
                "preservation_ref": result.preservation_ref,
                "land_blocked": result.land_blocked,
                "trunk_arrivals": result.trunk_arrivals,
            }))
            .expect("task run result is infallibly serializable");
            format!("{started}\n{typed}")
        },
    );
    let history = objective_history(campaign);
    super::with_single_turn_discipline(&format!(
        "You are advancing a durable engineering campaign. Inspect the repository yourself; descriptive metadata is never current state. Decide exactly one of done, advance, recover, or escalate. You return a decision; the daemon dispatches it. Never invoke foundry task/campaign, probe localhost, or infer daemon availability from this sandbox. WRITABLE REPOSITORY: this project only; sibling repositories are read-only.\n\n\
         CAMPAIGN: {}\nMISSION: {}\nINTENT REFS: {}\nCYCLES: {} completed / {} landed / {} max\nESCALATION RULES:\n- {}\n\n\
         OWNER DECISIONS (binding policy for this and future advances):\n{}\n\nREQUIRED REVIEW EVIDENCE:\n{}\n\nMECHANICAL DONE-GATE RESULTS (run here at formation, against the delivered trunk — the task does NOT run these):\n{}\n\nOBJECTIVE HISTORY (what this campaign has already asked for, oldest first, with the typed verdict each returned):\n{}\n\nLAST TYPED RUN RESULT:\n{}\n\nLIVE REPO SNAPSHOT (delivered trunk state):\n{}\n\nACCUMULATED UNMERGED WORK:\n{}\n\nCONTEXT ARTIFACTS (wording is binding and must be threaded into acceptance criteria):\n{}\n\n\
         TWO TREES. The live snapshot is the delivered trunk state. The accumulated section describes a preserved branch carrying earlier cycles of THIS campaign that did not land; the next cycle starts from that ref, not from the trunk. Its work is real and already written—it is invisible to a trunk-only inspection, and any tool you run in the working directory sees the trunk, not it.\n\n\
         READ-ONLY SANDBOX. Do not run builds, tests, linters, or anything else that writes target/, caches or lock files: they cannot run here and prove nothing. Test and gate evidence comes from MECHANICAL DONE-GATE RESULTS, which the daemon ran against the delivered trunk, and from LAST TYPED RUN RESULT: a run with landed: true reached trunk only after every required gate in the project's gate file passed in its worktree, re-run after any rebase onto trunk. Judge review statements about passing tests from that evidence and the code you read. Being unable to run a command here is never a reason to escalate.\n\n\
         DONE only when every required gate passes and every review statement is true against the delivered trunk state. Accumulated unmerged work does not make a campaign done. ADVANCE must cut exactly ONE objective from mission minus current state, where current state is the trunk PLUS the accumulated work. Never re-cut an objective the accumulated commits already satisfy; if the accumulated work appears to carry the mission but has not landed, the objective is to reconcile and land it—finish the remaining gaps, get the required gates green on that branch—not to rebuild it from scratch. NO RESTATEMENT. Check the objective you are about to cut against OBJECTIVE HISTORY before you commit to it. If it substantially restates an entry there, do not re-dispatch it; decide which of exactly two things is true. Either that earlier work exists and you are reading the wrong tree—re-read the accumulated section, and the objective becomes reconcile-and-land, not rebuild. Or the earlier cycle returned `remainder` and its gaps are genuinely still open on a tree you can see, in which case asking for the same thing has already failed once and repeating it is not the correction: name the single sub-gap that blocked it, change the approach, or escalate for an owner decision. A third identical objective is never the answer. Constraints, change licenses, scope guards, and forbidden moves are gates—not co-equal objectives. DO NOT RESTATE THE MECHANICAL DONE-GATE COMMANDS AS TASK EVIDENCE. Those run here, at formation, against the delivered trunk; the task runs the project's own discovered gates in an isolated worktree and cannot produce a result for a command it is never given. Asking it to prove one creates a gap it cannot close no matter what it does. Require only evidence the task itself can produce in its worktree. State concrete proof capable of rejecting masked IDs, count-only checks, or tests that bypass the project's public API. Do not prescribe implementation mechanism. Before removal/refactor packets, perform cheap live structural probes for callers and cross-module coupling and encode discoveries as scope guards. For migrations, name each licensed behavior change with its intent ref; all other characterized behavior is frozen. Build characterization before migration. RECOVER on a missing operational capability, access, environment or evidence. State the exact prerequisite and its typed kind; this records recovery without owner judgement or extra cycles. ESCALATE only on a human judgment, fired escalation rule requiring owner authority, or a policy choice that invalidates a campaign assumption. An internal seam that TEST DESIGN pre-authorizes is never an owner question; never escalate for it.\n\n\
         {}\n\n\
         End with exactly one fenced JSON object:\n\
         {{\"decision\":\"done\",\"reason\":\"evidence\"}}\n\
         {{\"decision\":\"advance\",\"objective\":\"single objective plus forbidden moves and task-producible evidence\",\"reason\":\"gap from mission minus state\"}}\n\
         {{\"decision\":\"recover\",\"kind\":\"capability_unavailable\",\"detail\":\"exact operational prerequisite\"}}\n\
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
        super::agent_helpers::TEST_DESIGN_GUIDANCE,
    ))
}

/// Keep every gate's identity and truth, but spend diagnostic space only on
/// failures. Full results remain in the durable advance event, and a failed
/// gate's complete output is in the files its `logs` names (serialized with
/// it). Shortening keeps a tail of every output section, so a flood of stderr
/// cannot hide the stdout line that names the failure.
fn formation_gate_results(
    results: &[foundry_sdk::gates::GateResult],
) -> Vec<foundry_sdk::gates::GateResult> {
    let mut remaining: usize = 2 * 1024;
    results
        .iter()
        .map(|gate| {
            let mut compact = gate.clone();
            compact.output = if gate.passed {
                String::new()
            } else {
                let limit = remaining.min(512);
                remaining -= gate.output.len().min(limit);
                let shortened = crate::gate_runner::output::compact_output(&gate.output, limit);
                if shortened == gate.output {
                    shortened
                } else {
                    format!(
                        "[diagnostic output shortened; full output in advance event]\n{shortened}"
                    )
                }
            };
            compact
        })
        .collect()
}

fn parse_decision(output: &str) -> anyhow::Result<CampaignDecision> {
    let decision: CampaignDecision = serde_json::from_str(&super::extract_json(output))
        .map_err(|e| anyhow::anyhow!("campaign agent returned no valid decision: {e}"))?;

    match &decision {
        CampaignDecision::Recover { detail, .. } if detail.trim().is_empty() => {
            anyhow::bail!("campaign recovery requires an exact prerequisite");
        }
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
/// in minutes. No jitter: one campaign forms at a time, and the few campaigns
/// forming side by side retry independent invocations, so there is no herd to
/// spread.
const DECISION_RETRY_DELAY: Duration = Duration::from_secs(5);

/// Separate operational recovery from campaign decisions and owner questions.
enum AdvanceOutcome {
    Recovery {
        kind: foundry_sdk::recovery::RecoveryKind,
        detail: String,
    },
    Decided(CampaignDecision),
    /// A task stopped with `blocked_on_decision`. It escalates like any other
    /// escalation, but keeps the agent's question and choices apart so the
    /// campaign record can show them to the owner as they were asked.
    OwnerQuestion {
        finding: String,
        options: Vec<String>,
    },
    /// Run the cycle's objective again; see [`Redispatch`].
    Redispatch(Redispatch),
}

impl AdvanceOutcome {
    /// Apply formation rules to policy decisions, preserving operational
    /// recovery and the exact question from a blocked task.
    fn map_decision(self, rule: impl FnOnce(CampaignDecision) -> CampaignDecision) -> Self {
        match self {
            Self::Decided(decision) => Self::Decided(rule(decision)),
            Self::Recovery { .. } | Self::OwnerQuestion { .. } | Self::Redispatch(_) => self,
        }
    }
}

/// What formation was actually shown and measured, recorded onto
/// `CampaignAdvanceCompleted` so a cycle can be reconstructed from the event
/// stream alone rather than inferred from the decision's prose.
///
/// Empty for decisions forced without asking an agent (unauthorized campaign,
/// already-terminal status, a verdict that decides on its own).
#[derive(Default)]
struct FormationRecord {
    initiative_context: Option<InitiativeContext>,
    selected_providers: Option<StageProviders>,
    prompt: Option<String>,
    agent_provider: Option<String>,
    gate_results: Vec<foundry_sdk::gates::GateResult>,
}

/// The fixed part of a decision-agent invocation. Every attempt re-sends
/// exactly this; only the attempt number differs.
struct DecisionRequest {
    prompt: String,
    working_dir: PathBuf,
    agent_file: Option<PathBuf>,
    provider: Option<AgentProvider>,
    timeout: Duration,
    trace_id: Option<String>,
}

impl DecisionRequest {
    fn spec(&self) -> AgentBlockSpec {
        AgentBlockSpec {
            prompt: self.prompt.clone(),
            working_dir: self.working_dir.clone(),
            access: AgentAccess::ReadOnly,
            tier: ModelTier::Balanced,
            effort: ReasoningEffort::Medium,
            agent_file: self.agent_file.clone(),
            provider: self.provider,
            env: vec![("FOUNDRY_AGENT_STAGE".into(), "formation".into())],
            timeout: self.timeout,
            trace_id: self.trace_id.clone(),
            requires_json: true,
        }
    }
}

/// Pauses before each re-dispatch of a cycle whose agent session the provider
/// refused as unavailable, after execution's own retries ran out. About two
/// hours in all; the 2026-10-19 Codex capacity outage lasted 1 h 42 min. An
/// outage that outlasts them records a `runner_unavailable` recovery.
const PROVIDER_REDISPATCH_DELAYS: [Duration; 4] = [
    Duration::from_secs(5 * 60),
    Duration::from_secs(15 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(60 * 60),
];

/// What a campaign does with a cycle the provider refused as unavailable.
#[derive(Debug, PartialEq, Eq)]
enum ProviderOutage {
    /// Dispatch the same objective again, under the same cycle number.
    Redispatch {
        objective: String,
        reason: String,
        not_before: Option<chrono::DateTime<chrono::Utc>>,
    },
    /// The re-dispatches ran out: record a `runner_unavailable` recovery.
    Exhausted { detail: String },
}

/// Whether the campaign's recovery was recorded for the run `request` names
/// and has since been rechecked.
fn recovery_rechecked_for_run(
    campaign: &Campaign,
    request: &CampaignAdvanceRequestedPayload,
) -> bool {
    campaign.recovery.as_ref().is_some_and(|recovery| {
        !recovery.is_pending()
            && request
                .run_event_id
                .as_ref()
                .is_some_and(|event| recovery.id.starts_with(&format!("recovery_{event}_")))
    })
}

/// The next step for a run the provider refused as unavailable, or `None`
/// when `result` is not one, or when the objective to repeat is unknown.
///
/// The cycle did no work, so it is repeated rather than re-formed: formation
/// would only see a failed run and judge the objective again. Each repeat
/// keeps the cycle number, so no budget is spent; the cycle's entries in the
/// objective history count the repeats. A recovery recheck dispatches at
/// once; otherwise the next pause in [`PROVIDER_REDISPATCH_DELAYS`] applies,
/// and when they are spent the outage becomes a typed recovery.
fn provider_outage(
    campaign: &Campaign,
    result: &TaskRunCompletedPayload,
    rechecked: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<ProviderOutage> {
    if !result.provider_unavailable() {
        return None;
    }
    let cycle = campaign.cycles_completed;
    let objective = campaign
        .objective_history
        .iter()
        .rev()
        .find(|entry| entry.cycle == cycle)
        .map(|entry| entry.objective.clone())
        .or_else(|| {
            result
                .context
                .prompt
                .as_ref()
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })?;
    let detail = result.failure.execution_summary();
    if rechecked {
        return Some(ProviderOutage::Redispatch {
            objective,
            reason: format!(
                "recovery rechecked; re-dispatching cycle {cycle} after a provider outage without forming: {detail}"
            ),
            not_before: None,
        });
    }
    let repeats = campaign
        .objective_history
        .iter()
        .filter(|entry| entry.cycle == cycle)
        .count()
        .saturating_sub(1);
    Some(match PROVIDER_REDISPATCH_DELAYS.get(repeats) {
        Some(pause) => ProviderOutage::Redispatch {
            objective,
            reason: format!(
                "cycle {cycle} did not start ({detail}); re-dispatching it in {} minutes, repeat {} of {}, no cycle budget spent",
                pause.as_secs() / 60,
                repeats + 1,
                PROVIDER_REDISPATCH_DELAYS.len()
            ),
            not_before: chrono::Duration::from_std(*pause).ok().map(|pause| now + pause),
        },
        None => ProviderOutage::Exhausted {
            detail: format!(
                "provider unavailable through {} re-dispatches of cycle {cycle}: {detail}. A recovery recheck dispatches the same objective again without forming",
                PROVIDER_REDISPATCH_DELAYS.len()
            ),
        },
    })
}

/// The re-dispatch for an advance whose last run the provider refused, with
/// the formation record that dispatch needs, or `None` when the advance is
/// to form as usual.
fn provider_redispatch(
    execution: &AdvanceExecution,
    campaign: &Campaign,
) -> Option<(AdvanceOutcome, FormationRecord)> {
    let request = &execution.request;
    let result = request.run_result.as_ref()?;
    let rechecked = recovery_rechecked_for_run(campaign, request);
    match provider_outage(campaign, result, rechecked, chrono::Utc::now())? {
        ProviderOutage::Redispatch {
            objective,
            reason,
            not_before,
        } => Some((
            AdvanceOutcome::Redispatch(Redispatch {
                objective,
                reason,
                not_before,
            }),
            FormationRecord {
                initiative_context: execution.initiative_context.clone(),
                selected_providers: Some(cycle_providers(
                    campaign,
                    request,
                    execution.initiative_context.as_ref(),
                )),
                ..FormationRecord::default()
            },
        )),
        ProviderOutage::Exhausted { .. } => None,
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

/// Ask the decision agent, retrying a transport failure before giving up.
///
/// Only failures to *get* an answer are retried. A malformed decision is the
/// agent answering badly rather than failing to answer, so it propagates on the
/// first attempt — re-asking would burn identical invocations. A terminal
/// provider failure records recovery immediately for the same reason.
async fn ask_decision_agent(
    agent: &dyn AgentGateway,
    request: &DecisionRequest,
    project: &str,
) -> anyhow::Result<AdvanceOutcome> {
    let mut diagnostics = Vec::new();
    let mut delay = DECISION_RETRY_DELAY;
    for attempt in 1..=DECISION_ATTEMPTS {
        let spec = request.spec();
        let (context, detail) = match invoke_agent(agent, spec, "campaign advance", project).await {
            AgentOutcome::Success { stdout } => {
                return Ok(match parse_decision(&stdout)? {
                    CampaignDecision::Recover { kind, detail } => {
                        AdvanceOutcome::Recovery { kind, detail }
                    }
                    decision => AdvanceOutcome::Decided(decision),
                });
            }
            AgentOutcome::AgentFailed { stderr, failure } => {
                if let Some(reason) = provider_outage_reason(failure.as_ref()) {
                    return Ok(AdvanceOutcome::Recovery {
                        kind: foundry_sdk::recovery::RecoveryKind::RunnerUnavailable,
                        detail: reason,
                    });
                }
                let detail = failure
                    .as_ref()
                    .and_then(|metadata| metadata.message.as_ref())
                    .filter(|message| !message.trim().is_empty())
                    .cloned()
                    .unwrap_or(stderr);
                if detail.to_ascii_lowercase().contains("model is not supported") {
                    return Ok(AdvanceOutcome::Recovery {
                        kind: foundry_sdk::recovery::RecoveryKind::EnvironmentUnavailable,
                        detail: format!("campaign model configuration rejected: {detail}"),
                    });
                }
                ("failed", detail)
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
    Ok(AdvanceOutcome::Recovery {
        kind: foundry_sdk::recovery::RecoveryKind::RunnerUnavailable,
        detail: format!(
            "campaign decision agent did not answer in {DECISION_ATTEMPTS} attempts ({})",
            diagnostics.join("; ")
        ),
    })
}

fn completed_event(
    campaign: &Campaign,
    throttle: foundry_sdk::throttle::Throttle,
    outcome: CampaignDecision,
    record: &FormationRecord,
) -> Event {
    advance_completed_event(campaign, throttle, CampaignAdvanceOutcome::Decided(outcome), record)
}

/// The completion of an advance that applied nothing because the campaign was
/// stopped or settled by someone else.
///
/// It is a success: the advance did what the stop asked of it. The work item
/// therefore settles `landed` instead of waiting in the open group for a
/// person. It carries no objective, changes no status and counts no cycle, and
/// its `decision` is `stood_down`, so no reader can take it for a dispatch or
/// for the campaign completing. `record` is the formation that ran and was
/// discarded, when one did; it explains the agent spend.
fn stood_down_event(
    campaign: &Campaign,
    throttle: foundry_sdk::throttle::Throttle,
    reason: String,
    record: &FormationRecord,
) -> Event {
    advance_completed_event(
        campaign,
        throttle,
        CampaignAdvanceOutcome::StoodDown(CampaignStandDown::new(campaign.status, reason)),
        record,
    )
}

fn advance_completed_event(
    campaign: &Campaign,
    throttle: foundry_sdk::throttle::Throttle,
    outcome: CampaignAdvanceOutcome,
    record: &FormationRecord,
) -> Event {
    super::event_from_infallible_payload(
        EventType::CampaignAdvanceCompleted,
        &campaign.project,
        throttle,
        &CampaignAdvanceCompletedPayload {
            last_task_run_event_id: campaign.last_run_event_id.clone(),
            campaign: campaign.name.clone(),
            project: campaign.project.clone(),
            cycles_completed: campaign.cycles_completed,
            cycles_landed: campaign.cycles_landed,
            success: true,
            outcome,
            prompt: record.prompt.clone(),
            agent_provider: record.agent_provider.clone(),
            gate_results: record.gate_results.clone(),
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
    operator_origin: Option<&str>,
    initiative_context: Option<&InitiativeContext>,
    selected_providers: Option<StageProviders>,
) -> Event {
    // `cycles_completed` was incremented for this dispatch just above, so it is
    // the number of the cycle this task *is* — and it matches the count the
    // sibling `CampaignAdvanceCompleted` reports. It rides the chain and loop
    // context from here, so every event the task emits can name its cycle
    // instead of leaving consumers to infer boundaries from timestamps.
    let mut payload = serde_json::json!({
        "project": campaign.project,
        "workflow": "task",
        "prompt": objective,
        "campaign": campaign.name,
        "campaign_cycle": campaign.cycles_completed,
        "campaign_limits": campaign.budget.stages,
    });
    payload["agent_provider"] = serde_json::json!(
        campaign
            .agent_provider
            .as_deref()
            .or_else(|| initiative_context.and_then(|context| context.agent_provider.as_deref()))
            .unwrap_or("codex")
    );
    payload["providers"] =
        serde_json::json!(selected_providers.unwrap_or_else(|| initiative_context.map_or_else(
            || campaign.providers,
            |context| campaign.providers.inherit(&context.providers)
        )));
    if let Some(context) = initiative_context {
        payload["initiative"] = serde_json::json!(context.initiative);
        payload["initiative_brief"] = serde_json::json!(context.brief);
        payload["initiative_findings"] = serde_json::json!(context.findings);
        payload["finding_ids"] = serde_json::json!(context.finding_ids());
    }
    if let Some(reference) = base_ref {
        payload["base_ref"] = serde_json::json!(reference);
    }
    // Opaque operator context, forwarded verbatim from the manual advance that
    // asked for this cycle. Absent for the automatic post-result advance.
    if let Some(origin) = operator_origin {
        payload["operator_origin"] = serde_json::json!(origin);
    }
    // The initiative is the dispatcher for a member cycle; otherwise the
    // campaign is. The member name and cycle stay explicit in the payload.
    // A manual advance's operator source must never replace that ownership.
    Event::new(EventType::ExecutionRequested, campaign.project.clone(), throttle, payload)
        .with_source(Some(campaign.initiative.as_ref().map_or_else(
            || WorkSource::campaign(campaign.name.clone(), campaign.cycles_completed),
            |initiative| WorkSource::initiative(initiative.clone(), campaign.cycles_completed),
        )))
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
            last_task_run_event_id: None,
            campaign: campaign.to_string(),
            project: project.to_string(),
            cycles_completed,
            cycles_landed,
            // A fault, not a decision the campaign reached: the work item
            // settles `failed` on this.
            success: false,
            outcome: CampaignAdvanceOutcome::Decided(CampaignDecision::Escalate {
                reason: reason.clone(),
            }),
            // A store failure aborts before any agent is asked, so there is no
            // formation to record.
            prompt: None,
            agent_provider: None,
            gate_results: Vec::new(),
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
    /// Trace this advance belongs to, so the decision agent's spend lands in
    /// the campaign's own workflow.
    trace_id: Option<String>,
    agent: Arc<dyn AgentGateway>,
    shell: Arc<dyn ShellGateway>,
    registry: Arc<std::sync::RwLock<Registry>>,
    store_path: PathBuf,
    work_items_path: PathBuf,
    formation_locks: Arc<FormationLocks>,
    request: CampaignAdvanceRequestedPayload,
    /// Set by [`release_settled_base`] when the preserved work `request`
    /// would have continued from was settled outside the campaign.
    settled_base: Option<SettledBase>,
    initiative_context: Option<InitiativeContext>,
    project: String,
    throttle: foundry_sdk::throttle::Throttle,
}

fn persist_or_terminal(
    execution: &AdvanceExecution,
    guard: &mut CampaignStoreGuard,
    events: Vec<Event>,
    success_summary: String,
    failure_context: &str,
    cycles_completed: u64,
    cycles_landed: u64,
) -> TaskBlockResult {
    if let Some(initiative) = guard
        .store
        .find(&execution.request.campaign)
        .and_then(|campaign| campaign.initiative.clone())
        && let Err(error) = (|| -> anyhow::Result<()> {
            if guard.store.find_initiative(&initiative).is_some_and(|value| {
                !value.status.is_terminal()
                    && value.authorized_by.as_ref().is_some_and(|owner| !owner.trim().is_empty())
            }) && let Some(completed) =
                events.iter().find(|event| event.event_type == EventType::CampaignCompleted)
            {
                guard.store.record_milestone(
                    &initiative,
                    &execution.request.campaign,
                    "completed",
                    vec![format!("event:{}", completed.id)],
                )?;
            }
            guard.store.refresh_questions(&initiative)?;
            Ok(())
        })()
    {
        return terminal_error_result(
            &execution.request.campaign,
            &execution.project,
            execution.throttle,
            cycles_completed,
            cycles_landed,
            format!("initiative questions could not be refreshed: {error}"),
        );
    }
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

/// Record the arriving run and return the outcome it forces, if any.
///
/// `redispatch_on_outage` lets a run the provider refused as unavailable go
/// on to be re-dispatched (see [`provider_outage`]) instead of forcing a
/// recovery; an initiative that steers its members keeps that choice itself.
fn update_run_and_forced_outcome(
    campaign: &mut Campaign,
    request: &CampaignAdvanceRequestedPayload,
    redispatch_on_outage: bool,
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
            reason: REESCALATION_REASON.to_string(),
        }));
    }
    if let Some(recovery) = campaign.recovery.as_ref().filter(|recovery| recovery.is_pending()) {
        return Some(AdvanceOutcome::Recovery {
            kind: recovery.kind,
            detail: recovery.detail.clone(),
        });
    }
    let recovered_run = recovery_rechecked_for_run(campaign, request);
    if let Some(result) = request.run_result.as_ref().filter(|_| !recovered_run) {
        match &result.verdict {
            TaskVerdict::BlockedOnDecision { finding, options } => {
                return Some(AdvanceOutcome::OwnerQuestion {
                    finding: finding.clone(),
                    options: options.clone(),
                });
            }
            TaskVerdict::BlockedOnRecovery { kind, detail } => {
                return Some(AdvanceOutcome::Recovery {
                    kind: *kind,
                    detail: detail.clone(),
                });
            }
            TaskVerdict::RunnerError { .. } if continuation_refused(request) => {}
            TaskVerdict::RunnerError { detail }
                if redispatch_on_outage && result.provider_unavailable() =>
            {
                // The provider refused the session: the cycle is repeated
                // after a pause, and the budget check below does not apply to
                // a cycle that did no work.
                let detail = match provider_outage(campaign, result, false, chrono::Utc::now()) {
                    Some(ProviderOutage::Redispatch { .. }) => return None,
                    Some(ProviderOutage::Exhausted { detail }) => detail,
                    None => detail.clone(),
                };
                return Some(AdvanceOutcome::Recovery {
                    kind: foundry_sdk::recovery::RecoveryKind::RunnerUnavailable,
                    detail,
                });
            }
            TaskVerdict::RunnerError { detail } => {
                return Some(AdvanceOutcome::Recovery {
                    kind: foundry_sdk::recovery::RecoveryKind::RunnerUnavailable,
                    detail: detail.clone(),
                });
            }
            TaskVerdict::Complete | TaskVerdict::Remainder { .. } | TaskVerdict::Defect { .. } => {}
        }
    }
    if campaign.cycles_completed >= campaign.budget.max_cycles
        && let Some(result) = request
            .run_result
            .as_ref()
            .filter(|r| !r.landed && !matches!(r.verdict, TaskVerdict::Complete))
    {
        let reason = if continuation_refused(request) {
            refused_at_budget_reason(campaign, result)
        } else {
            format!(
                "campaign cycle budget exhausted ({}) after an unlanded non-complete result",
                campaign.budget.max_cycles
            )
        };
        return Some(AdvanceOutcome::Decided(CampaignDecision::Escalate { reason }));
    }
    None
}

/// The escalation reason when the last authorized cycle was a refused
/// continuation.
///
/// The cycle is not refunded: a refused continuation is the campaign's own
/// decision to continue from that ref, and repeating it would be refused
/// again. (A reused cycle number is kept for repeating a cycle the provider
/// refused as unavailable, which did no work.) Instead
/// the owner is told that the cycle ran no agent, which ref was refused and
/// why (the refusal summary names the ref, the preserved commit, how far
/// behind trunk it was and the conflicting files), and what to do next.
fn refused_at_budget_reason(campaign: &Campaign, result: &TaskRunCompletedPayload) -> String {
    format!(
        "campaign cycle budget exhausted ({budget}): the last cycle was refused before any agent \
         session ran, so it did no work. {refusal}. To go on, add a cycle (`foundry campaign \
         resume {name} --add-cycles 1`) and advance: the next cycle starts from trunk, and \
         formation sees this refusal. If that preserved work was already reconciled onto trunk \
         by hand, close its work item with `foundry queue close` so it leaves the open list",
        budget = campaign.budget.max_cycles,
        refusal = result.summary.trim().trim_end_matches('.'),
        name = campaign.name,
    )
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
        .map(|gate| {
            let logs = crate::gate_runner::full_logs_note(&gate.logs)
                .map(|note| format!("\n{note}"))
                .unwrap_or_default();
            format!("$ {}\n{}{logs}", gate.command, or_none(gate.output.clone()))
        })
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

/// Member overrides precede the admitted pairing; preserved work keeps its pairing.
fn cycle_providers(
    campaign: &Campaign,
    request: &CampaignAdvanceRequestedPayload,
    context: Option<&InitiativeContext>,
) -> StageProviders {
    let inherited = context.map(|context| context.providers).unwrap_or_default();
    let admitted = if next_base_ref(request).is_some() {
        request
            .run_result
            .as_ref()
            .map(|result| result.context.providers)
            .unwrap_or_default()
            .inherit(&request.providers)
    } else {
        request.providers
    };
    campaign.providers.inherit(&admitted.inherit(&inherited))
}

async fn derive_advance_outcome(
    execution: &AdvanceExecution,
    entry: &foundry_sdk::registry::ProjectEntry,
    campaign: &Campaign,
) -> anyhow::Result<(AdvanceOutcome, FormationRecord)> {
    let agent = &*execution.agent;
    let shell = &*execution.shell;
    let request = &execution.request;
    let settled = execution.settled_base.as_ref();
    let trace_id = execution.trace_id.clone();
    let initiative_context = execution.initiative_context.as_ref();
    campaign.validate()?;
    let providers = cycle_providers(campaign, request, initiative_context);
    let fallback = campaign
        .agent_provider
        .as_deref()
        .or_else(|| initiative_context.and_then(|context| context.agent_provider.as_deref()))
        .and_then(|value| super::parse_agent_provider(Some(value)))
        .or(Some(AgentProvider::Codex));
    let execution_provider = providers.resolve(AgentStage::Execution, fallback);
    if !execution_provider.is_some_and(AgentProvider::enforces_writable_root) {
        anyhow::bail!("campaign execution provider does not enforce a writable root");
    }
    let formation_provider = providers.resolve(AgentStage::Formation, fallback);
    let repo = Path::new(&entry.path);
    let gate_results =
        crate::campaign_gates::run_done_gates(shell, repo, &campaign.done_evidence).await?;
    let mut context = read_context_files(repo, &campaign.context_paths)?;
    if let Some(initiative) = initiative_context {
        let _ = write!(context, "\n\nINITIATIVE: {}\n", initiative.initiative);
        let _ = writeln!(context, "{}", super::agent_helpers::INITIATIVE_CONTRACT_GUIDANCE);
        if let Some(brief) = &initiative.brief {
            let _ = write!(
                context,
                "BINDING INITIATIVE BRIEF: {} (SHA-256 {})\nRead this brief before deciding.\n",
                brief.path, brief.hash
            );
        }
        context.push_str(&initiative.render_findings());
    }
    let snapshot = repo_snapshot(shell, repo).await;
    let accumulated = accumulated_work(shell, repo, request, settled).await;
    // The gates, the snapshot, and the prompt are all deterministic for this
    // advance, so a retry re-asks the same question rather than re-deriving it.
    let decision_request = DecisionRequest {
        trace_id: trace_id.clone(),
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
        provider: formation_provider,
        timeout: Duration::ZERO,
    };
    if decision_request.prompt.len() > campaign.budget.stages.formation_prompt_bytes {
        anyhow::bail!(
            "formation prompt is {} bytes, above its {} byte stage budget; shorten mission/owner decisions or raise budget.stages.formation_prompt_bytes",
            decision_request.prompt.len(),
            campaign.budget.stages.formation_prompt_bytes
        );
    }
    let outcome = ask_decision_agent(agent, &decision_request, &campaign.project).await?;
    let record = FormationRecord {
        prompt: Some(decision_request.prompt),
        agent_provider: formation_provider.map(|provider| provider.to_string()),
        initiative_context: initiative_context.cloned(),
        selected_providers: Some(providers),
        gate_results: gate_results.clone(),
    };
    Ok((
        outcome.map_decision(|decision| enforce_done_gate_truth(campaign, decision, &gate_results)),
        record,
    ))
}

fn record_recovery_outcome(
    campaign: &mut Campaign,
    request: &CampaignAdvanceRequestedPayload,
    throttle: foundry_sdk::throttle::Throttle,
    kind: foundry_sdk::recovery::RecoveryKind,
    detail: String,
    record: &FormationRecord,
) -> Vec<Event> {
    let decision = CampaignDecision::Recover {
        kind,
        detail: detail.clone(),
    };
    if !campaign
        .recovery
        .as_ref()
        .is_some_and(foundry_sdk::recovery::RecoveryState::is_pending)
    {
        campaign.recovery = Some(foundry_sdk::recovery::RecoveryState {
            id: request.run_event_id.as_ref().map_or_else(
                || format!("recovery_{}", uuid::Uuid::new_v4()),
                |event| format!("recovery_{event}_{}", uuid::Uuid::new_v4()),
            ),
            kind,
            detail,
            recorded_at: chrono::Utc::now(),
            resolution: None,
        });
    }
    vec![
        completed_event(campaign, throttle, decision, record),
        super::event_from_infallible_payload(
            EventType::CampaignRecoveryRecorded,
            &campaign.project,
            throttle,
            &serde_json::json!({"campaign":campaign.name,"recovery":campaign.recovery}),
        ),
    ]
}

fn apply_advance_outcome(
    campaign: &mut Campaign,
    request: &CampaignAdvanceRequestedPayload,
    throttle: foundry_sdk::throttle::Throttle,
    outcome: AdvanceOutcome,
    record: &FormationRecord,
) -> Vec<Event> {
    let (decision, question) = match outcome {
        AdvanceOutcome::Recovery { kind, detail } => {
            return record_recovery_outcome(campaign, request, throttle, kind, detail, record);
        }
        AdvanceOutcome::Redispatch(redispatch) => {
            return redispatch_events(campaign, request, throttle, redispatch, record);
        }
        AdvanceOutcome::Decided(decision) => (decision, None),
        AdvanceOutcome::OwnerQuestion { finding, options } => (
            CampaignDecision::Escalate {
                reason: format!("{finding}; options: {}", options.join(" | ")),
            },
            Some((finding, options)),
        ),
    };
    match decision {
        CampaignDecision::Recover { kind, detail } => {
            record_recovery_outcome(campaign, request, throttle, kind, detail, record)
        }
        CampaignDecision::Done { reason } => {
            campaign.status = CampaignStatus::Completed;
            campaign.open_escalation = None;
            campaign.pending_run_result = None;
            vec![
                completed_event(
                    campaign,
                    throttle,
                    CampaignDecision::Done {
                        reason: reason.clone(),
                    },
                    record,
                ),
                terminal_event(EventType::CampaignCompleted, campaign, throttle, reason),
            ]
        }
        CampaignDecision::Escalate { reason } => {
            // The first reason stands while the campaign stays escalated: a
            // later forced re-escalation only restates the status, and
            // replacing the record with it would hide why the campaign stopped.
            if campaign.status != CampaignStatus::Escalated {
                let (question, choices) = question
                    .map_or((None, Vec::new()), |(finding, options)| (Some(finding), options));
                campaign.open_escalation = Some(CampaignEscalation {
                    reason: reason.clone(),
                    escalated_at: chrono::Utc::now(),
                    cycles_completed: campaign.cycles_completed,
                    cycles_landed: campaign.cycles_landed,
                    question,
                    choices,
                    from_event_log: false,
                });
            }
            campaign.status = CampaignStatus::Escalated;
            // Resumption must retain the reviewed result and preserved base.
            // Escalation waits for the owner; it does not discard learning.
            vec![
                completed_event(
                    campaign,
                    throttle,
                    CampaignDecision::Escalate {
                        reason: reason.clone(),
                    },
                    record,
                ),
                terminal_event(EventType::CampaignEscalated, campaign, throttle, reason),
            ]
        }
        CampaignDecision::Advance { objective, reason } => {
            campaign.status = CampaignStatus::Active;
            campaign.open_escalation = None;
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
                    record,
                ),
                execution_event(
                    campaign,
                    throttle,
                    &objective,
                    base_ref,
                    request.operator_origin.as_deref(),
                    record.initiative_context.as_ref(),
                    record.selected_providers,
                ),
            ]
        }
    }
}

/// A cycle repeated after a provider outage: the same objective under the
/// same cycle number, because the provider refused its agent session as
/// unavailable. No agent forms it and it spends no cycle budget, since the
/// cycle did no work. `not_before` holds the dispatch in the queue until the
/// provider has had time to recover; `None` dispatches at once (a recovery
/// recheck).
struct Redispatch {
    objective: String,
    reason: String,
    not_before: Option<chrono::DateTime<chrono::Utc>>,
}

/// Dispatch the cycle's objective again, under the cycle number it already
/// has, from the same base it would have continued from.
///
/// `cycles_completed` does not move: the refused run did no work, so it
/// spends no budget. The objective is recorded again for the same cycle, so
/// the history shows each repeat, and the advance records it as an `advance`
/// whose reason names the outage. `not_before` rides on the dispatch, and the
/// pacing scheduler holds the queued cycle until then.
fn redispatch_events(
    campaign: &mut Campaign,
    request: &CampaignAdvanceRequestedPayload,
    throttle: foundry_sdk::throttle::Throttle,
    redispatch: Redispatch,
    record: &FormationRecord,
) -> Vec<Event> {
    let Redispatch {
        objective,
        reason,
        not_before,
    } = redispatch;
    campaign.status = CampaignStatus::Active;
    campaign.open_escalation = None;
    campaign.record_dispatched_objective(objective.clone());
    let base_ref = next_base_ref(request).map(ToString::to_string);
    campaign.pending_run_result = None;
    let mut dispatch = execution_event(
        campaign,
        throttle,
        &objective,
        base_ref,
        request.operator_origin.as_deref(),
        record.initiative_context.as_ref(),
        record.selected_providers,
    );
    if let Some(at) = not_before {
        dispatch.payload["not_before"] = serde_json::json!(at);
    }
    vec![
        completed_event(
            campaign,
            throttle,
            CampaignDecision::Advance {
                objective: objective.clone(),
                reason,
            },
            record,
        ),
        dispatch,
    ]
}

async fn choose_advance_outcome(
    execution: &AdvanceExecution,
    campaign: &Campaign,
    forced: Option<AdvanceOutcome>,
) -> (AdvanceOutcome, FormationRecord) {
    if let Some(outcome) = forced {
        return (outcome, FormationRecord::default());
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
            return (
                AdvanceOutcome::Recovery {
                    kind: foundry_sdk::recovery::RecoveryKind::RunnerUnavailable,
                    detail: format!("campaign formation failed: {error}"),
                },
                FormationRecord::default(),
            );
        }
    };
    derive_advance_outcome(execution, &entry, campaign)
        .await
        .unwrap_or_else(|error| {
            (
                AdvanceOutcome::Recovery {
                    kind: foundry_sdk::recovery::RecoveryKind::RunnerUnavailable,
                    detail: format!("campaign formation failed: {error}"),
                },
                FormationRecord::default(),
            )
        })
}

/// The advance to actually form from.
///
/// A manual advance carries no run result, so it replays whatever the campaign
/// recorded while it was paused or escalated — formation then sees the reviewer gaps and the
/// executor continues from the preserved ref.
fn replay_pending_run(execution: &AdvanceExecution, campaign: &Campaign) -> AdvanceExecution {
    let mut request = execution.request.clone();
    if request.run_result.is_none() {
        request.run_result.clone_from(&campaign.pending_run_result);
        request.run_event_id.clone_from(&campaign.last_run_event_id);
    }
    AdvanceExecution {
        request,
        trace_id: execution.trace_id.clone(),
        agent: Arc::clone(&execution.agent),
        shell: Arc::clone(&execution.shell),
        registry: Arc::clone(&execution.registry),
        store_path: execution.store_path.clone(),
        work_items_path: execution.work_items_path.clone(),
        formation_locks: Arc::clone(&execution.formation_locks),
        settled_base: execution.settled_base.clone(),
        initiative_context: execution.initiative_context.clone(),
        project: execution.project.clone(),
        throttle: execution.throttle,
    }
}

/// Stop the advance continuing from preserved work that was settled outside
/// the campaign.
///
/// When the work item holding the ref `effective` would continue from is
/// settled `cancelled` (an owner closed it) or `landed` (trunk
/// already carries it), the request drops that ref, so formation's prompt and
/// the dispatched cycle both start from the registered trunk, and the
/// settlement is kept for the prompt. Resolved once per formation, after the
/// pending run is replayed, because a manual advance replays a ref that may
/// have been settled since it was recorded.
async fn release_settled_base(
    mut effective: AdvanceExecution,
    campaign: &Campaign,
) -> AdvanceExecution {
    effective.settled_base =
        resolve_settled_base(&effective.work_items_path, campaign, &effective.request).await;
    if let Some(settled) = &effective.settled_base {
        tracing::info!(
            campaign = %campaign.name,
            preservation_ref = %settled.reference,
            work_item = %settled.item_id,
            state = %settled.state.tag(),
            "preserved work was settled outside the campaign; the next cycle starts from trunk"
        );
        if let Some(result) = effective.request.run_result.as_mut() {
            result.preservation_ref = None;
        }
    }
    effective
}

/// The block summary for a formed outcome.
///
/// Operational recovery carries its exact prerequisite into the run trace.
fn outcome_summary(campaign: &str, outcome: &AdvanceOutcome) -> String {
    match outcome {
        AdvanceOutcome::Decided(_) | AdvanceOutcome::OwnerQuestion { .. } => {
            format!("campaign '{campaign}' advanced")
        }
        AdvanceOutcome::Recovery { detail, .. } => {
            format!("campaign '{campaign}' awaiting recovery: {detail}")
        }
        AdvanceOutcome::Redispatch(Redispatch { reason, .. }) => {
            format!("campaign '{campaign}' re-dispatched its cycle: {reason}")
        }
    }
}

/// Take the campaign store's cross-process flock off the runtime.
///
/// RPCs, offline CLI commands and other formations can hold it. Their critical
/// sections are short, but a runtime worker must never park on a flock.
pub(super) async fn lock_store(store_path: &Path) -> anyhow::Result<CampaignStoreGuard> {
    let store_path = store_path.to_path_buf();
    tokio::task::spawn_blocking(move || CampaignStore::lock_exclusive(&store_path))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|result| result.map_err(anyhow::Error::from))
}

/// Save the run result recorded on a `stopped` campaign and stand down
/// without forming.
fn stand_down_before_forming(
    execution: &AdvanceExecution,
    guard: &mut CampaignStoreGuard,
    stopped: &Campaign,
) -> TaskBlockResult {
    let summary = format!(
        "campaign '{}' is {}; run recorded without advancing",
        stopped.name, stopped.status
    );
    let events = vec![stood_down_event(
        stopped,
        execution.throttle,
        summary.clone(),
        &FormationRecord::default(),
    )];
    persist_or_terminal(
        execution,
        guard,
        events,
        summary,
        "stopped campaign result could not be saved",
        stopped.cycles_completed,
        stopped.cycles_landed,
    )
}

/// What the first critical section decided.
enum Prepared {
    /// The advance ended without asking an agent.
    Finished(TaskBlockResult),
    /// Form from this record, which is also what was saved.
    Form(Box<Campaign>, Option<InitiativeContext>, Vec<Event>),
}

/// Whether a formed outcome may still be applied to the campaign.
#[derive(Debug, PartialEq, Eq)]
enum Recheck {
    /// The record is exactly what formation read.
    Unchanged,
    /// Someone stopped or settled the campaign while it formed.
    Stopped(CampaignStatus),
    /// The campaign can still advance, but its record changed.
    Changed,
}

/// Compare the record formation read with the record now in the store.
///
/// Any difference means someone else wrote the campaign while the agent ran:
/// an operator's `pause`, `resume`, `decide`, `complete` or `cancel`, or an
/// `--offline` edit. The outcome was formed from a record that no longer
/// exists, so it is never applied over that change.
fn recheck(formed_from: &Campaign, current: &Campaign) -> Recheck {
    if same_record(formed_from, current) {
        Recheck::Unchanged
    } else if current.check_advanceable().is_err() {
        Recheck::Stopped(current.status)
    } else {
        Recheck::Changed
    }
}

fn same_record(left: &Campaign, right: &Campaign) -> bool {
    match (serde_json::to_value(left), serde_json::to_value(right)) {
        (Ok(left), Ok(right)) => left == right,
        (Err(error), _) | (_, Err(error)) => {
            // Best-effort: a record that cannot be serialised cannot be
            // compared. Treating it as changed discards the formed outcome
            // instead of risking an overwrite; the warn carries the fault.
            tracing::warn!(campaign = %left.name, %error, "campaign record could not be compared after formation");
            false
        }
    }
}

/// The first critical section: record the arriving run result and decide
/// whether an agent is needed at all.
///
/// Everything here is quick, so it runs under the store flock. A forced
/// outcome (unauthorized, already settled, a verdict that decides on its own)
/// asks no agent and is applied before the flock is released. Otherwise the
/// recorded run result is saved now, so it survives a formation that fails or
/// is discarded, and the saved record is returned to form from.
/// Turn task-result learning into durable initiative facts under the campaign guard.
fn record_initiative_result(
    store: &mut CampaignStore,
    execution: &AdvanceExecution,
    work_item: &str,
) -> anyhow::Result<Vec<Event>> {
    let Some(run_id) = &execution.request.run_event_id else {
        return Ok(Vec::new());
    };
    let Some(result) = &execution.request.run_result else {
        return Ok(Vec::new());
    };
    let Some(campaign) = store.find(&execution.request.campaign) else {
        return Ok(Vec::new());
    };
    if campaign.last_run_event_id.as_ref() == Some(run_id) {
        return Ok(Vec::new());
    }
    let Some(initiative) = campaign.initiative.clone() else {
        return Ok(Vec::new());
    };
    let mut events = Vec::new();
    if result.landed {
        record_landing_milestone(store, &initiative, execution, work_item)?;
        for finding in store.consume_findings(
            &initiative,
            &execution.request.campaign,
            work_item,
            &result.context.finding_ids,
        )? {
            events.push(Event::new(EventType::FindingConsumed, result.project.clone(), execution.throttle, serde_json::json!({
                "initiative": initiative, "finding_id": finding.id, "member": execution.request.campaign,
                "work_item_id": work_item, "finding": finding,
            })).with_source(Some(WorkSource::initiative(&initiative, result.context.campaign_cycle.unwrap_or(0)))));
        }
    }
    let learned: Vec<(FindingKind, &str)> = match &result.verdict {
        TaskVerdict::Remainder { gaps } if result.landed => {
            gaps.iter().map(|gap| (FindingKind::Blocker, gap.as_str())).collect()
        }
        TaskVerdict::Defect { diagnosis } => vec![(FindingKind::Regression, diagnosis.as_str())],
        _ => Vec::new(),
    };
    for (index, (kind, text)) in learned.into_iter().enumerate() {
        let mut finding = Finding::new(
            &initiative,
            FindingSource {
                kind: if work_item.starts_with("wi_") {
                    "campaign_cycle"
                } else {
                    "task_result_event"
                }
                .into(),
                id: work_item.to_string(),
            },
            Vec::new(),
            kind,
            text,
            vec![
                format!("campaign:{}", execution.request.campaign),
                format!("event:{run_id}"),
            ],
        );
        // A replay has the same result identity, so it must never duplicate learning.
        finding.id = format!("fnd_{run_id}_{index}");
        if store.find_finding(&finding.id).is_some() {
            continue;
        }
        let finding = store.add_finding(finding)?;
        let mut payload = serde_json::to_value(&finding)?;
        payload["finding_id"] = serde_json::json!(finding.id);
        events.push(
            Event::new(
                EventType::FindingRecorded,
                result.project.clone(),
                execution.throttle,
                payload,
            )
            .with_source(Some(WorkSource::initiative(
                &initiative,
                result.context.campaign_cycle.unwrap_or(0),
            ))),
        );
    }
    Ok(events)
}

fn record_landing_milestone(
    store: &mut CampaignStore,
    initiative: &str,
    execution: &AdvanceExecution,
    work_item: &str,
) -> anyhow::Result<()> {
    if store.find_initiative(initiative).is_some_and(|value| {
        !value.status.is_terminal()
            && value.authorized_by.as_ref().is_some_and(|owner| !owner.trim().is_empty())
    }) {
        let mut proof = vec![
            format!("event:{}", execution.request.run_event_id.as_deref().unwrap_or_default()),
            format!("work_item:{work_item}"),
        ];
        if let Some(cycle) = execution
            .request
            .run_result
            .as_ref()
            .and_then(|result| result.context.campaign_cycle)
        {
            proof.push(format!("cycle:{cycle}"));
        }
        store.record_milestone(initiative, &execution.request.campaign, "landed", proof)?;
    }
    Ok(())
}

async fn result_work_item(execution: &AdvanceExecution) -> String {
    let fallback = execution.request.run_event_id.clone().unwrap_or_default();
    let Some(result) = &execution.request.run_result else {
        return fallback;
    };
    let path = execution.work_items_path.clone();
    let Ok(Ok(ledger)) = tokio::task::spawn_blocking(move || WorkItemStore::load(&path)).await
    else {
        return fallback;
    };
    let member_items = || {
        ledger.items.iter().filter(|item| {
            item.kind == WorkItemKind::CampaignCycle
                && item.campaign_name() == Some(execution.request.campaign.as_str())
        })
    };
    let exact = member_items()
        .filter(|item| {
            result.context.campaign_cycle.is_some()
                && item.campaign_cycle() == result.context.campaign_cycle
        })
        .max_by_key(|item| item.submitted_at);
    let found = exact.or_else(|| {
        member_items()
            .filter(|item| execution.trace_id.is_some() && item.trace_id == execution.trace_id)
            .max_by_key(|item| item.submitted_at)
    });
    found.map_or(fallback, |item| item.id.clone())
}

async fn active_member_names(
    work_items_path: &Path,
    store: &CampaignStore,
) -> anyhow::Result<Vec<String>> {
    let path = work_items_path.to_path_buf();
    let ledger = tokio::task::spawn_blocking(move || WorkItemStore::load(&path)).await??;
    Ok(store.ledger_active_members(&ledger))
}

async fn lock_formation_store(
    execution: &AdvanceExecution,
) -> anyhow::Result<(CampaignStoreGuard, Vec<String>)> {
    let guard = lock_store(&execution.store_path).await?;
    let members = if guard
        .store
        .find(&execution.request.campaign)
        .is_some_and(|campaign| campaign.initiative.is_some())
    {
        active_member_names(&execution.work_items_path, &guard.store).await?
    } else {
        Vec::new()
    };
    Ok((guard, members))
}

async fn prepare_formation_inner(
    execution: &AdvanceExecution,
    recorded: &mut Vec<Event>,
) -> Prepared {
    let work_item = result_work_item(execution).await;
    let (mut guard, active_members) = match lock_formation_store(execution).await {
        Ok(locked) => locked,
        Err(error) => {
            return Prepared::Finished(terminal_error_result(
                &execution.request.campaign,
                &execution.project,
                execution.throttle,
                0,
                0,
                format!("campaign store unavailable: {error}"),
            ));
        }
    };
    match record_initiative_result(&mut guard.store, execution, &work_item) {
        Ok(events) => *recorded = events,
        Err(error) => {
            return Prepared::Finished(terminal_error_result(
                &execution.request.campaign,
                &execution.project,
                execution.throttle,
                0,
                0,
                format!("initiative result could not be recorded: {error}"),
            ));
        }
    }
    let initiative_steers = initiative_steering_enabled(&guard.store, &execution.request.campaign);
    let Some(campaign) = guard.store.find_mut(&execution.request.campaign) else {
        return Prepared::Finished(terminal_error_result(
            &execution.request.campaign,
            &execution.project,
            execution.throttle,
            0,
            0,
            format!("campaign '{}' not found", execution.request.campaign),
        ));
    };

    if let Err(error) = campaign.validate() {
        let events = apply_advance_outcome(
            campaign,
            &execution.request,
            execution.throttle,
            AdvanceOutcome::Decided(CampaignDecision::Escalate {
                reason: error.to_string(),
            }),
            &FormationRecord::default(),
        );
        let cycles_completed = campaign.cycles_completed;
        let cycles_landed = campaign.cycles_landed;
        return Prepared::Finished(persist_or_terminal(
            execution,
            &mut guard,
            events,
            format!("campaign '{}' escalated", execution.request.campaign),
            "campaign invalid and escalation could not be saved",
            cycles_completed,
            cycles_landed,
        ));
    }

    let forced = update_run_and_forced_outcome(campaign, &execution.request, !initiative_steers);
    // Both a pause and a cancellation stop the loop here, by recording the
    // arriving run result and forming nothing further. For a cancellation this
    // is the whole graceful-stop mechanism: the in-flight cycle runs to
    // completion, `FinalizeTask` commits and preserves its work as usual, and
    // the result lands in the store with no successor dispatched. A paused
    // campaign keeps that result pending, and the advance after `resume`
    // replays it.
    //
    // The only event is the advance's own `stood_down` completion, so its work
    // item settles `landed`: stopping here is what the operator asked for.
    // Nothing else is emitted and no decision is formed. `CampaignCancelled`
    // was already emitted by the operator's `cancel`, and forming any decision
    // here would move the status off `Cancelled` — `Done` in particular would
    // record the abandoned campaign as completed, which is the false evidence
    // claim the separate status exists to prevent. A stand-down changes no
    // status.
    if campaign.status.halts_formation() {
        let stopped = campaign.clone();
        return Prepared::Finished(stand_down_before_forming(execution, &mut guard, &stopped));
    }

    if let Some(outcome) = forced {
        // No ledger read here: a forced outcome never dispatches a cycle
        // (`enforce_campaign_budget` only turns an advance into an
        // escalation), so no base ref is read. The pending run result stays
        // as recorded, and the advance that later forms from it checks the
        // ledger then.
        let effective_execution = replay_pending_run(execution, campaign);
        let outcome = outcome.map_decision(|decision| enforce_campaign_budget(campaign, decision));
        let summary = outcome_summary(&execution.request.campaign, &outcome);
        let events = apply_advance_outcome(
            campaign,
            &effective_execution.request,
            execution.throttle,
            outcome,
            &FormationRecord::default(),
        );
        let cycles_completed = campaign.cycles_completed;
        let cycles_landed = campaign.cycles_landed;
        return Prepared::Finished(persist_or_terminal(
            execution,
            &mut guard,
            events,
            summary,
            "campaign decision could not be saved",
            cycles_completed,
            cycles_landed,
        ));
    }

    if initiative_steers && execution.request.run_result.is_some() {
        let accounted = campaign.clone();
        return Prepared::Finished(accounted_for_initiative(execution, &mut guard, &accounted));
    }

    let formed_from = campaign.clone();
    save_formation_snapshot(execution, &guard, formed_from, &active_members)
}

fn initiative_steering_enabled(store: &CampaignStore, member: &str) -> bool {
    store
        .find(member)
        .and_then(|campaign| campaign.initiative.as_deref())
        .and_then(|name| store.find_initiative(name))
        .is_some_and(|initiative| initiative.steering_enabled)
}

fn accounted_for_initiative(
    execution: &AdvanceExecution,
    guard: &mut CampaignStoreGuard,
    campaign: &Campaign,
) -> TaskBlockResult {
    let reason = format!(
        "campaign '{}' result accounted; initiative chooses the next action",
        campaign.name
    );
    let events = vec![stood_down_event(
        campaign,
        execution.throttle,
        reason.clone(),
        &FormationRecord::default(),
    )];
    persist_or_terminal(
        execution,
        guard,
        events,
        reason,
        "initiative member result could not be saved",
        campaign.cycles_completed,
        campaign.cycles_landed,
    )
}

fn save_formation_snapshot(
    execution: &AdvanceExecution,
    guard: &CampaignStoreGuard,
    formed_from: Campaign,
    active_members: &[String],
) -> Prepared {
    if let Err(error) = guard.save() {
        return Prepared::Finished(terminal_error_result(
            &execution.request.campaign,
            &execution.project,
            execution.throttle,
            formed_from.cycles_completed,
            formed_from.cycles_landed,
            format!("campaign run result could not be saved before formation: {error}"),
        ));
    }
    if let Err(error) = guard.store.enforce_can_form(&formed_from.name, active_members) {
        let reason = format!("initiative member stood down before formation: {error}");
        return Prepared::Finished(TaskBlockResult::success(
            reason.clone(),
            vec![stood_down_event(
                &formed_from,
                execution.throttle,
                reason,
                &FormationRecord::default(),
            )],
        ));
    }
    let context = match guard.store.formation_context(&formed_from.name, FINDINGS_PACKET_BYTES) {
        Ok(context) => context,
        Err(error) => {
            return Prepared::Finished(terminal_error_result(
                &formed_from.name,
                &formed_from.project,
                execution.throttle,
                formed_from.cycles_completed,
                formed_from.cycles_landed,
                format!("initiative context unavailable: {error}"),
            ));
        }
    };
    Prepared::Form(Box::new(formed_from), context, Vec::new())
}

async fn prepare_formation(execution: &AdvanceExecution) -> Prepared {
    let mut recorded = Vec::new();
    match prepare_formation_inner(execution, &mut recorded).await {
        Prepared::Finished(mut result) => {
            if result.success {
                recorded.append(&mut result.events);
                result.events = recorded;
            }
            Prepared::Finished(result)
        }
        Prepared::Form(campaign, context, _) => Prepared::Form(campaign, context, recorded),
    }
}

/// How many times one advance forms before it gives up on a campaign that
/// keeps changing under it.
///
/// Each formation that finds the record edited forms again from the edited
/// record, so an owner's budget or mission edit never stalls the campaign. An
/// owner edits a campaign a handful of times at most; a record that changes
/// during three formations in a row is something rewriting it continuously,
/// and the advance fails visibly instead of spending more agent calls.
const FORMATIONS_PER_ADVANCE: usize = 3;
const FINDINGS_PACKET_BYTES: usize = 4096;

/// What the second critical section did with a formed outcome.
enum Applied {
    /// The advance is over: the outcome was applied, or the campaign was
    /// stopped and the advance stood down, or a fault ended it.
    Settled(TaskBlockResult),
    /// The campaign can still advance but its record changed, so the outcome
    /// was discarded and nothing was written. Form again.
    Changed,
}

/// Recheck initiative admission under the store lock before dispatching a cycle.
fn enforce_member_dispatch(
    store: &CampaignStore,
    execution: &AdvanceExecution,
    formed_from: &Campaign,
    outcome: AdvanceOutcome,
    record: &FormationRecord,
) -> Result<AdvanceOutcome, TaskBlockResult> {
    if !matches!(
        outcome,
        AdvanceOutcome::Decided(CampaignDecision::Advance { .. }) | AdvanceOutcome::Redispatch(_)
    ) {
        return Ok(outcome);
    }
    let name = &execution.request.campaign;
    if let Err(error) = store.enforce_can_form(name, &[]) {
        let reason = format!("initiative member stood down before dispatch: {error}");
        return Err(TaskBlockResult::success(
            reason.clone(),
            vec![stood_down_event(
                formed_from,
                execution.throttle,
                reason,
                record,
            )],
        ));
    }
    Ok(match store.enforce_can_advance(name, &[]) {
        Ok(()) => outcome,
        Err(error) => AdvanceOutcome::Decided(CampaignDecision::Escalate {
            reason: error.to_string(),
        }),
    })
}

/// The second critical section: apply the formed outcome, unless the campaign
/// changed while its formation ran.
///
/// A changed record is never overwritten. When the campaign was stopped or
/// settled (paused, cancelled, completed, escalated) the outcome is discarded
/// and the advance stands down: it emits a `stood_down` completion and
/// succeeds, because the operator asked for exactly that, and a paused
/// campaign's run result stays pending for the advance after `resume`. When
/// the campaign could still advance but its record changed (a budget or
/// mission edit), the outcome is discarded and the caller forms again from the
/// current record.
async fn apply_unless_changed(
    execution: &AdvanceExecution,
    request: &CampaignAdvanceRequestedPayload,
    formed_from: &Campaign,
    outcome: AdvanceOutcome,
    record: &FormationRecord,
) -> Applied {
    let name = &execution.request.campaign;
    let mut guard = match lock_store(&execution.store_path).await {
        Ok(guard) => guard,
        Err(error) => {
            return Applied::Settled(terminal_error_result(
                name,
                &execution.project,
                execution.throttle,
                formed_from.cycles_completed,
                formed_from.cycles_landed,
                format!("campaign store unavailable after formation: {error}"),
            ));
        }
    };
    let current_context = match guard.store.formation_context(name, FINDINGS_PACKET_BYTES) {
        Ok(context) => context,
        Err(error) => {
            return Applied::Settled(terminal_error_result(
                name,
                &execution.project,
                execution.throttle,
                formed_from.cycles_completed,
                formed_from.cycles_landed,
                format!("initiative context unavailable after formation: {error}"),
            ));
        }
    };
    // Observations do not change authority or invalidate an in-flight assessment.
    let policy_context = |context: &Option<foundry_sdk::initiative::InitiativeContext>| {
        let mut context = context.clone();
        if let Some(context) = &mut context {
            context.status_notes.clear();
        }
        serde_json::to_value(context).ok()
    };
    let context_changed =
        policy_context(&current_context) != policy_context(&record.initiative_context);
    if context_changed {
        return Applied::Changed;
    }
    let outcome =
        match enforce_member_dispatch(&guard.store, execution, formed_from, outcome, record) {
            Ok(outcome) => outcome,
            Err(result) => return Applied::Settled(result),
        };
    let Some(campaign) = guard.store.find_mut(name) else {
        return Applied::Settled(terminal_error_result(
            name,
            &execution.project,
            execution.throttle,
            formed_from.cycles_completed,
            formed_from.cycles_landed,
            format!("campaign '{name}' not found after formation"),
        ));
    };
    match recheck(formed_from, campaign) {
        Recheck::Unchanged => {
            let summary = outcome_summary(name, &outcome);
            let events =
                apply_advance_outcome(campaign, request, execution.throttle, outcome, record);
            let cycles_completed = campaign.cycles_completed;
            let cycles_landed = campaign.cycles_landed;
            Applied::Settled(persist_or_terminal(
                execution,
                &mut guard,
                events,
                summary,
                "campaign decision could not be saved",
                cycles_completed,
                cycles_landed,
            ))
        }
        Recheck::Stopped(status) => {
            tracing::info!(campaign = %name, %status, "campaign stopped while it formed; advance stood down");
            let summary = format!(
                "campaign '{name}' became {status} while it formed; the formed outcome was discarded"
            );
            // Nothing is written: the record stays exactly as the stop left it.
            Applied::Settled(TaskBlockResult::success(
                summary.clone(),
                vec![stood_down_event(
                    campaign,
                    execution.throttle,
                    summary,
                    record,
                )],
            ))
        }
        Recheck::Changed => {
            tracing::warn!(campaign = %name, "campaign changed while it formed; forming again from the current record");
            Applied::Changed
        }
    }
}

/// The advance's result when the campaign changed during every formation.
///
/// A fault, not a decision: no outcome was applied and no event is emitted, so
/// the work item settles `failed` and lands in `foundry queue open` with this
/// summary in its trace.
fn formations_exhausted(execution: &AdvanceExecution) -> TaskBlockResult {
    let name = &execution.request.campaign;
    tracing::warn!(campaign = %name, formations = FORMATIONS_PER_ADVANCE, "campaign changed during every formation; advance failed");
    TaskBlockResult {
        success: false,
        summary: format!(
            "campaign '{name}' changed while it formed, {FORMATIONS_PER_ADVANCE} formations in a row; no outcome was applied and every change was kept. Run `foundry campaign advance {name}` once the campaign is no longer being edited"
        ),
        ..Default::default()
    }
}

/// Advance one campaign.
///
/// The campaign store's flock is held only for short critical sections, never
/// across the agent: formation runs the done gates and a decision agent for
/// minutes, and the flock serialises every campaign mutation in every process.
/// Holding it there made every `AdvanceCampaign` RPC and every other campaign
/// control wait on whichever formation was running (the 2026-10-11 production
/// hang). The per-campaign [`FormationLocks`] entry is held across the whole
/// advance instead, so one campaign never forms twice at once while different
/// campaigns form side by side.
///
/// A formation whose campaign was edited meanwhile is discarded and the
/// advance prepares again from the current record, up to
/// [`FORMATIONS_PER_ADVANCE`] formations. Preparing again re-runs every check
/// the first pass made (status, authorization, forced outcomes, budget) against
/// the edited record. It records the arriving run result at most once, because
/// recording is keyed on the run's event id, so a cycle is never counted twice.
async fn execute_campaign_advance(mut execution: AdvanceExecution) -> TaskBlockResult {
    let formation = execution.formation_locks.for_campaign(&execution.request.campaign);
    let _formation_guard = formation.lock().await;

    let mut recorded_events = Vec::new();
    for _ in 0..FORMATIONS_PER_ADVANCE {
        let formed_from = match prepare_formation(&execution).await {
            Prepared::Finished(mut result) => {
                recorded_events.append(&mut result.events);
                result.events = recorded_events;
                return result;
            }
            Prepared::Form(campaign, context, mut events) => {
                recorded_events.append(&mut events);
                execution.initiative_context = context;
                *campaign
            }
        };
        let effective_execution =
            release_settled_base(replay_pending_run(&execution, &formed_from), &formed_from).await;
        // A cycle the provider refused is repeated, not re-formed.
        let (outcome, record) = match provider_redispatch(&effective_execution, &formed_from) {
            Some(redispatch) => redispatch,
            None => choose_advance_outcome(&effective_execution, &formed_from, None).await,
        };
        let outcome =
            outcome.map_decision(|decision| enforce_campaign_budget(&formed_from, decision));
        match apply_unless_changed(
            &execution,
            &effective_execution.request,
            &formed_from,
            outcome,
            &record,
        )
        .await
        {
            Applied::Settled(mut result) => {
                recorded_events.append(&mut result.events);
                result.events = recorded_events;
                return result;
            }
            Applied::Changed => {}
        }
    }
    let mut result = formations_exhausted(&execution);
    result.events = recorded_events;
    result
}

impl SimulatedSuccess for AdvanceCampaign {
    type Outcome = Vec<Event>;

    fn simulate(&self, trigger: &Event) -> Vec<Event> {
        let Ok(request) = trigger.parse_payload::<CampaignAdvanceRequestedPayload>() else {
            return vec![];
        };
        // Best-effort: dry-run simulation must not fail the caller when the
        // campaign store is unreadable — it degrades to "no events" the same
        // as an unparseable trigger payload.
        let campaign = CampaignStore::load(&self.store_path)
            .map_err(|e| {
                tracing::warn!(
                    error = %e,
                    path = %self.store_path.display(),
                    "campaign store unreadable during dry-run simulation"
                );
                e
            })
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
            // A dry run asks no agent and runs no gates, so it records no
            // formation — the absence is itself the honest signal.
            completed_event(&campaign, trigger.throttle, outcome, &FormationRecord::default()),
            execution_event(
                &campaign,
                foundry_sdk::throttle::Throttle::DryRun,
                &format!("Dry-run next objective for campaign '{}'.", campaign.name),
                None,
                request.operator_origin.as_deref(),
                None,
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
        let trace_id = trigger.trace_id.clone();
        let throttle = trigger.throttle;
        let agent = Arc::clone(&self.agent);
        let shell = Arc::clone(&self.shell);
        let registry = Arc::clone(&self.registry);
        let store_path = self.store_path.clone();
        let work_items_path = self.work_items_path.clone();
        let formation_locks = Arc::clone(&self.formation_locks);

        Box::pin(async move {
            Ok(execute_campaign_advance(AdvanceExecution {
                trace_id,
                agent,
                shell,
                registry,
                store_path,
                work_items_path,
                formation_locks,
                request,
                settled_base: None,
                initiative_context: None,
                project,
                throttle,
            })
            .await)
        })
    }
}

#[cfg(test)]
mod tests {
    include!("advance_campaign/initiative_tests.rs");
    include!("advance_campaign/slice1_tests.rs");
    #[test]
    fn git_packet_compaction_is_bounded_unicode_safe_and_names_full_evidence() {
        let input = "界".repeat(4000);
        let result = super::compact_git(input.clone(), 1024, "git show HEAD");
        assert!(result.len() < 1150);
        assert!(result.starts_with("界"));
        assert!(result.contains("git show HEAD"));
        assert!(result.contains(&format!("{} bytes omitted", input.len() - 1023)));
        assert_eq!(super::compact_git("unchanged".into(), 1024, "git status"), "unchanged");
    }

    use foundry_sdk::campaign::{
        Campaign, CampaignBudget, CampaignCycle, CampaignStatus, CampaignStore, CycleOutcome,
        DoneEvidence, OwnerDecision,
    };
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::payload::{
        CampaignAdvanceCompletedPayload, CampaignAdvanceOutcome, CampaignAdvanceRequestedPayload,
        CampaignDecision, LoopContext, TaskRunCompletedPayload, TaskVerdict,
    };
    use foundry_sdk::task_block::{TaskBlock, TaskBlockResult};
    use foundry_sdk::throttle::Throttle;
    use foundry_sdk::work_source::WorkSource;

    use crate::gateway::fakes::{FakeAgentGateway, FakeShellGateway};
    use crate::gateway::{AgentFailureKind, AgentProvider, AgentResponse};

    use super::{
        AdvanceCampaign, AdvanceOutcome, DECISION_ATTEMPTS, Recheck, enforce_campaign_budget,
        enforce_done_gate_truth, formation_gate_results, last_run_line, outcome_label,
        parse_decision, recheck, settled_base, update_run_and_forced_outcome,
    };
    use foundry_sdk::work_item::WorkItemStore;

    /// The block under test, reading its work-item ledger from
    /// `work_items.json` beside the campaign store. A test that never writes
    /// that file runs against an empty ledger.
    fn advance_block(
        agent: std::sync::Arc<dyn crate::gateway::AgentGateway>,
        shell: std::sync::Arc<dyn crate::gateway::ShellGateway>,
        registry: std::sync::Arc<std::sync::RwLock<foundry_sdk::registry::Registry>>,
        store_path: std::path::PathBuf,
    ) -> AdvanceCampaign {
        let work_items_path = ledger_path_beside(&store_path);
        AdvanceCampaign::new(agent, shell, registry, store_path, work_items_path)
    }

    fn ledger_path_beside(store_path: &std::path::Path) -> std::path::PathBuf {
        store_path.with_file_name("work_items.json")
    }

    #[test]
    fn an_untouched_record_takes_its_formed_outcome() {
        let formed_from = campaign_for_accumulation_test();
        assert_eq!(recheck(&formed_from, &formed_from.clone()), Recheck::Unchanged);
    }

    #[test]
    fn a_campaign_stopped_while_it_formed_reports_its_new_status() {
        let formed_from = campaign_for_accumulation_test();
        for status in [
            CampaignStatus::Paused,
            CampaignStatus::Cancelled,
            CampaignStatus::Completed,
            CampaignStatus::Escalated,
        ] {
            let mut current = formed_from.clone();
            current.status = status;
            assert_eq!(recheck(&formed_from, &current), Recheck::Stopped(status));
        }
    }

    #[test]
    fn any_other_edit_while_it_formed_counts_as_a_change() {
        let formed_from = campaign_for_accumulation_test();
        let mut budget = formed_from.clone();
        budget.budget.max_cycles += 5;
        assert_eq!(recheck(&formed_from, &budget), Recheck::Changed);
        let mut decided = formed_from.clone();
        decided.owner_decisions.push(foundry_sdk::campaign::OwnerDecision {
            done_evidence_amendment: None,
            decision: "prefer the smaller slice".to_string(),
            authorized_by: "owner".to_string(),
            decided_at: chrono::Utc::now(),
        });
        assert_eq!(recheck(&formed_from, &decided), Recheck::Changed);
    }

    #[test]
    fn formation_keeps_gate_truth_and_failure_tails_without_passing_noise() {
        let mut gates = vec![failing_clippy_gate(); 8];
        for (index, gate) in gates.iter_mut().enumerate() {
            gate.name = format!("gate-{index}");
            gate.required = index % 2 == 0;
            gate.output = format!("{}FINAL ERROR", "é".repeat(20_000));
        }
        gates[0].passed = true;
        let compact = formation_gate_results(&gates);
        assert!(
            compact[0].output.is_empty(),
            "expected `compact[0].output` to be empty: {:?}",
            compact[0].output
        );
        assert!(compact[1].output.ends_with("FINAL ERROR"));
        assert!(compact[1].output.contains("full output in advance event"));
        assert!(compact.iter().map(|g| g.output.len()).sum::<usize>() < 17 * 1024);
        for (original, compact) in gates.iter().zip(&compact) {
            let mut expected = original.clone();
            expected.output.clone_from(&compact.output);
            assert_eq!(
                serde_json::to_value(&expected).unwrap(),
                serde_json::to_value(compact).unwrap(),
                "only diagnostics may change"
            );
        }
        assert_eq!(gates[0].output.len(), 40_011, "original audit evidence is intact");
    }

    #[test]
    fn formation_keeps_the_stdout_failure_line_when_stderr_is_noisy() {
        let stderr = (0..300)
            .map(|i| format!("   Compiling noisy-crate-{i} v1.0.0"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut gate = failing_clippy_gate();
        gate.output = format!(
            "test foo::bar ... FAILED\ntest result: FAILED. 1 failed\n{}\n{stderr}",
            crate::gate_runner::output::STDERR_MARKER
        );
        gate.logs = vec!["/home/u/.foundry/evidence/gate-logs/r/test.stdout.log".to_string()];

        let compact = formation_gate_results(std::slice::from_ref(&gate));

        let output = &compact[0].output;
        assert!(output.contains("test foo::bar ... FAILED"), "{output}");
        assert!(output.contains("Compiling noisy-crate-299"));
        assert!(output.contains("full output in advance event"));
        assert_eq!(compact[0].logs, gate.logs, "the log paths reach the formation prompt");
    }

    #[test]
    fn red_done_gate_objective_names_the_full_logs() {
        let mut gate = failing_clippy_gate();
        gate.logs = vec!["/logs/campaign_done_1.stderr.log".to_string()];

        let decision = enforce_done_gate_truth(
            &campaign_for_accumulation_test(),
            CampaignDecision::Done {
                reason: "looks shipped".to_string(),
            },
            std::slice::from_ref(&gate),
        );

        let CampaignDecision::Advance { objective, .. } = decision else {
            panic!("a red required gate must rewrite done into advance");
        };
        assert!(objective.contains("full logs: /logs/campaign_done_1.stderr.log"), "{objective}");
    }

    #[tokio::test]
    async fn rejected_model_records_environment_recovery_once() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let message = "The 'configured-model' model is not supported when using Codex with a ChatGPT account.";
        let agent = FakeAgentGateway::always(AgentResponse::failure_with_metadata(
            "",
            1,
            crate::gateway::AgentFailureMetadata::new(AgentProvider::Codex).with_message(message),
        ));
        let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path);
        let result = block.execute(&manual_advance_trigger()).await.unwrap();
        assert_eq!(agent.invocations().len(), 1);
        assert!(terminal_reason(&result, &EventType::CampaignRecoveryRecorded).contains(message));
    }

    /// The decision agent must read an unlanded complete as preserved work
    /// carried forward, not as finished-and-integrated.
    #[test]
    fn an_unlanded_complete_is_labelled_with_its_land_block() {
        let blocked = CycleOutcome {
            verdict: TaskVerdict::Complete,
            landed: false,
            land_blocked: Some(foundry_sdk::payload::LandBlocked::TrunkMovedConflict),
        };
        assert_eq!(
            outcome_label(Some(&blocked)),
            "complete, did not land (trunk_moved_conflict), preserved for the next cycle"
        );
        let landed = CycleOutcome {
            verdict: TaskVerdict::Complete,
            landed: true,
            land_blocked: None,
        };
        assert_eq!(outcome_label(Some(&landed)), "complete, landed");
    }

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
            recovery: None,
            name: "c".to_string(),
            project: "p".to_string(),
            mission: "ship".to_string(),
            intent_refs: vec![],
            context_paths: vec![],
            done_evidence: vec![DoneEvidence::Review {
                statement: "shipped".to_string(),
            }],
            budget: CampaignBudget {
                max_cycles: 2,
                ..Default::default()
            },
            escalation: vec![],
            status: CampaignStatus::Active,
            cycles_completed: 2,
            cycles_landed: 2,
            authorized_by: Some("owner".to_string()),
            agent_provider: None,
            providers: foundry_sdk::gateway::StageProviders::default(),
            initiative: None,
            last_run_event_id: Some("run-2".to_string()),
            owner_decisions: vec![],
            pending_run_result: None,
            objective_history: vec![],
            open_escalation: None,
            writable_repositories: vec![],
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
    async fn blocked_on_decision_escalates_without_asking_agent() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                recovery: None,
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
                providers: foundry_sdk::gateway::StageProviders::default(),
                initiative: None,
                last_run_event_id: None,
                owner_decisions: vec![],
                pending_run_result: None,
                objective_history: vec![],
                open_escalation: None,
                writable_repositories: vec![],
            })
            .unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success();
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());
        let mut run_result = run_result_with(TaskVerdict::BlockedOnDecision {
            finding: "boundaries differ".to_string(),
            options: vec!["A".to_string(), "B".to_string()],
        });
        run_result.summary = "decision needed".to_string();
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
                run_event_id: Some("run-1".to_string()),
                run_result: Some(run_result),
            })
            .unwrap(),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(result.events.iter().any(|e| e.event_type == EventType::CampaignEscalated));
        // The blocked objective is the owner's question, not the next cycle.
        assert!(!result.events.iter().any(|e| e.event_type == EventType::ExecutionRequested));
        assert!(agent.invocations().is_empty());
        let stored = CampaignStore::load(&store_path).unwrap().find("c").unwrap().clone();
        assert_eq!(stored.status, CampaignStatus::Escalated);
        // The owner reads why from the campaign itself, with the agent's own
        // question and choices kept apart from the flattened reason.
        let open = stored.open_escalation.expect("the escalation is recorded on the campaign");
        assert_eq!(open.reason, "boundaries differ; options: A | B");
        assert_eq!(open.question.as_deref(), Some("boundaries differ"));
        assert_eq!(open.choices, ["A", "B"]);
        assert_eq!((open.cycles_completed, open.cycles_landed), (1, 0));
        assert!(!open.from_event_log);

        // A later result reaching the still-escalated campaign restates the
        // status; it must not replace the question the owner has to answer.
        let again = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
                run_event_id: None,
                run_result: None,
            })
            .unwrap(),
        );
        let result = block.execute(&again).await.unwrap();
        assert!(terminal_reason(&result, &EventType::CampaignEscalated).contains("requires owner"));
        assert!(!result.events.iter().any(|e| e.event_type == EventType::ExecutionRequested));
        let stored = CampaignStore::load(&store_path).unwrap().find("c").unwrap().clone();
        assert_eq!(stored.open_escalation.unwrap().reason, open.reason);
    }

    #[tokio::test]
    async fn paused_campaign_records_run_without_escalating_or_advancing() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                recovery: None,
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
                providers: foundry_sdk::gateway::StageProviders::default(),
                initiative: None,
                last_run_event_id: None,
                owner_decisions: vec![],
                pending_run_result: None,
                objective_history: vec![],
                open_escalation: None,
                writable_repositories: vec![],
            })
            .unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success();
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
                run_event_id: Some("run-1".to_string()),
                run_result: Some(TaskRunCompletedPayload {
                    project: "p".to_string(),
                    success: true,
                    landed: true,
                    summary: "landed".to_string(),
                    preservation_ref: None,
                    land_blocked: None,
                    proof_evidence: None,
                    uncommitted_artifacts: None,
                    trunk_arrivals: Vec::new(),
                    verdict: TaskVerdict::Complete,
                    context: LoopContext {
                        campaign: Some("c".to_string()),
                        ..LoopContext::default()
                    },
                    failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
                }),
            })
            .unwrap(),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert_only_stand_down(&result, CampaignStatus::Paused);
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

    /// The graceful half of `foundry campaign cancel`: the in-flight cycle
    /// runs to completion and its result arrives here, but nothing further is
    /// formed. Critically the status must stay `Cancelled` — forming a `Done`
    /// decision would rewrite it to `Completed` and put a false evidence claim
    /// into the store and the ops digest.
    #[tokio::test]
    async fn cancelled_campaign_records_run_without_advancing_or_completing() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                recovery: None,
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
                status: CampaignStatus::Cancelled,
                cycles_completed: 1,
                cycles_landed: 0,
                authorized_by: Some("owner".to_string()),
                agent_provider: None,
                providers: foundry_sdk::gateway::StageProviders::default(),
                initiative: None,
                last_run_event_id: None,
                owner_decisions: vec![],
                pending_run_result: None,
                objective_history: vec![],
                open_escalation: None,
                writable_repositories: vec![],
            })
            .unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success();
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
                run_event_id: Some("run-1".to_string()),
                run_result: Some(TaskRunCompletedPayload {
                    project: "p".to_string(),
                    success: true,
                    landed: true,
                    summary: "landed".to_string(),
                    preservation_ref: None,
                    land_blocked: None,
                    proof_evidence: None,
                    uncommitted_artifacts: None,
                    trunk_arrivals: Vec::new(),
                    verdict: TaskVerdict::Complete,
                    context: LoopContext {
                        campaign: Some("c".to_string()),
                        ..LoopContext::default()
                    },
                    failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
                }),
            })
            .unwrap(),
        );

        let result = block.execute(&trigger).await.unwrap();

        // No successor dispatched, no formation agent consulted, and above all
        // no terminal event — `cancel` already emitted `CampaignCancelled`.
        // The only event is the advance's own stand-down, which moves no
        // status.
        assert_only_stand_down(&result, CampaignStatus::Cancelled);
        assert!(agent.invocations().is_empty());
        let stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Cancelled);
        assert_eq!(campaign.cycles_landed, 1);
        assert_eq!(campaign.last_run_event_id.as_deref(), Some("run-1"));
    }

    #[tokio::test]
    async fn paused_campaign_does_not_count_complete_result_that_did_not_land() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                recovery: None,
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
                providers: foundry_sdk::gateway::StageProviders::default(),
                initiative: None,
                last_run_event_id: None,
                owner_decisions: vec![],
                pending_run_result: None,
                objective_history: vec![],
                open_escalation: None,
                writable_repositories: vec![],
            })
            .unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success();
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
                run_event_id: Some("run-1".to_string()),
                run_result: Some(TaskRunCompletedPayload {
                    project: "p".to_string(),
                    success: true,
                    landed: false,
                    summary: "required no landing".to_string(),
                    preservation_ref: None,
                    land_blocked: None,
                    proof_evidence: None,
                    uncommitted_artifacts: None,
                    trunk_arrivals: Vec::new(),
                    verdict: TaskVerdict::Complete,
                    context: LoopContext {
                        campaign: Some("c".to_string()),
                        ..LoopContext::default()
                    },
                    failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
                }),
            })
            .unwrap(),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert_only_stand_down(&result, CampaignStatus::Paused);
        assert!(agent.invocations().is_empty());
        let stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Paused);
        assert_eq!(campaign.cycles_landed, 0);
        assert_eq!(campaign.last_run_event_id.as_deref(), Some("run-1"));
    }

    fn campaign_for_accumulation_test() -> Campaign {
        Campaign {
            recovery: None,
            name: "c".to_string(),
            project: "p".to_string(),
            mission: "ship".to_string(),
            intent_refs: vec![],
            context_paths: vec![],
            done_evidence: vec![DoneEvidence::Review {
                statement: "shipped".to_string(),
            }],
            budget: CampaignBudget {
                max_cycles: 6,
                ..Default::default()
            },
            escalation: vec![],
            status: CampaignStatus::Active,
            cycles_completed: 2,
            cycles_landed: 0,
            authorized_by: Some("owner".to_string()),
            agent_provider: None,
            providers: foundry_sdk::gateway::StageProviders::default(),
            initiative: None,
            last_run_event_id: None,
            owner_decisions: vec![],
            pending_run_result: None,
            objective_history: vec![],
            open_escalation: None,
            writable_repositories: vec![],
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
        let block = advance_block(agent.clone(), shell.clone(), registry, store_path.clone());

        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
                run_event_id: Some("run-9".to_string()),
                run_result: Some(TaskRunCompletedPayload {
                    project: "p".to_string(),
                    success: false,
                    landed: false,
                    summary: "task stopped with a typed non-complete verdict; work preserved"
                        .to_string(),
                    preservation_ref: Some("foundry-task/parite-61ef680dcf08".to_string()),
                    land_blocked: None,
                    proof_evidence: None,
                    uncommitted_artifacts: None,
                    trunk_arrivals: Vec::new(),
                    verdict: TaskVerdict::Remainder {
                        gaps: vec!["deployment evidence".to_string()],
                    },
                    context: LoopContext {
                        campaign: Some("c".to_string()),
                        ..LoopContext::default()
                    },
                    failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
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
        // Formation writes the objective and its evidence, so it must not ask
        // for tests through a real network.
        assert!(prompt.contains(super::super::agent_helpers::TEST_DESIGN_GUIDANCE));
        // Formation decides escalation, so it carries the seam
        // pre-authorization and says plainly that a seam is no owner question.
        for phrase in super::super::agent_helpers::SEAM_AUTHORIZATION_PHRASES {
            assert!(prompt.contains(phrase), "missing {phrase}");
        }
        assert!(prompt.contains(
            "An internal seam that TEST DESIGN pre-authorizes is never an owner question; never escalate for it."
        ));
        assert!(!prompt.contains("bypass the real boundary"));
        assert!(prompt.contains("tests that bypass the project's public API"));
        // A formation agent that backgrounds a command and ends its turn
        // waiting for a notification returns prose, and the campaign
        // escalates on a non-decision.
        assert!(
            prompt.trim_end().ends_with(super::super::SINGLE_TURN_JSON_DISCIPLINE),
            "formation prompt must close with the single-turn rule: {prompt}"
        );
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
        let block = advance_block(agent.clone(), shell.clone(), registry, store_path.clone());

        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
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
            land_blocked: None,
            proof_evidence: None,
            uncommitted_artifacts: None,
            trunk_arrivals: Vec::new(),
            verdict: TaskVerdict::Remainder {
                gaps: vec!["exercise the generated gRPC boundary".to_string()],
            },
            context: LoopContext {
                campaign: Some("c".to_string()),
                ..LoopContext::default()
            },
            failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
        };
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                budget: CampaignBudget {
                    max_cycles: 3,
                    ..Default::default()
                },
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
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());

        let paused = block.execute(&automatic_advance_trigger(pending.clone())).await.unwrap();
        assert_only_stand_down(&paused, CampaignStatus::Paused);
        let mut stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find_mut("c").unwrap();
        assert_eq!(campaign.pending_run_result.as_ref(), Some(&pending));
        campaign.status = CampaignStatus::Active;
        stored.save(&store_path).unwrap();

        let manual_trigger =
            manual_advance_trigger().with_source(Some(WorkSource::operator("workbench")));
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
        assert_eq!(
            execution.source,
            Some(WorkSource::campaign("c", 2)),
            "the cycle names the campaign and its number, not the operator who advanced it"
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
                recovery: None,
                name: "c".to_string(),
                project: "p".to_string(),
                mission: "ship".to_string(),
                intent_refs: vec![],
                context_paths: vec![],
                done_evidence: vec![DoneEvidence::Review {
                    statement: "shipped".to_string(),
                }],
                budget: CampaignBudget {
                    max_cycles: 3,
                    ..Default::default()
                },
                escalation: vec![],
                status: CampaignStatus::Active,
                cycles_completed: 1,
                cycles_landed: 1,
                authorized_by: Some("owner".to_string()),
                agent_provider: None,
                providers: foundry_sdk::gateway::StageProviders::default(),
                initiative: None,
                last_run_event_id: Some("run-1".to_string()),
                owner_decisions: vec![OwnerDecision {
                    done_evidence_amendment: None,
                    decision: "Prefer the generated gRPC client path; do not add raw JSON shims."
                        .to_string(),
                    authorized_by: "owner".to_string(),
                    decided_at: chrono::DateTime::parse_from_rfc3339("2026-07-18T12:00:00Z")
                        .unwrap()
                        .with_timezone(&chrono::Utc),
                }],
                pending_run_result: None,
                objective_history: vec![],
                open_escalation: None,
                writable_repositories: vec![],
            })
            .unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Add the generated client test.\",\"reason\":\"owner policy recorded\"}\n```",
        );
        let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path);
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
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
                    land_blocked: None,
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
        let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path);

        block.execute(&manual_advance_trigger()).await.unwrap();

        let prompt = &agent.invocations()[0].prompt;
        assert_eq!(agent.invocations()[0].tier, crate::gateway::ModelTier::Balanced);
        assert_eq!(agent.invocations()[0].effort, crate::gateway::ReasoningEffort::Medium);
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
        let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path);

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
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());

        block.execute(&manual_advance_trigger()).await.unwrap();
        let result_trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
                run_event_id: Some("run-1".to_string()),
                run_result: Some(TaskRunCompletedPayload {
                    project: "p".to_string(),
                    success: false,
                    landed: true,
                    summary: "the RPCs exist; the CLI still routes locally".to_string(),
                    preservation_ref: None,
                    land_blocked: None,
                    proof_evidence: None,
                    uncommitted_artifacts: None,
                    trunk_arrivals: Vec::new(),
                    verdict: TaskVerdict::Remainder {
                        gaps: vec!["registry init still writes the local file".to_string()],
                    },
                    context: LoopContext {
                        campaign: Some("c".to_string()),
                        ..LoopContext::default()
                    },
                    failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
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
        let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path);

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

    // --- formation audit trail ---------------------------------------------

    /// A cycle must be reconstructible from the event stream alone. Formation
    /// was the only agent-invoking block that recorded no prompt, so what the
    /// decision agent actually saw could only be guessed at from its prose.
    #[tokio::test]
    async fn advance_completed_records_the_prompt_provider_and_done_gates() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut campaign = campaign_for_accumulation_test();
        campaign.agent_provider = Some("codex".to_string());
        campaign.done_evidence.push(DoneEvidence::Gate {
            command: "cargo test --workspace".to_string(),
            required: true,
            artifacts: vec![],
        });
        let mut store = CampaignStore::default();
        store.add(campaign).unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Cut one slice.\",\"reason\":\"gap\"}\n```",
        );
        let noise = "PASSING-GATE-NOISE".repeat(1_000);
        let shell = FakeShellGateway::sequence(vec![
            crate::gateway::CommandResult {
                stdout: noise.clone(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            crate::gateway::CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = advance_block(agent.clone(), shell, registry, store_path.clone());

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        let completed = result
            .events
            .iter()
            .find(|event| event.event_type == EventType::CampaignAdvanceCompleted)
            .expect("advance completed event");
        let payload: CampaignAdvanceCompletedPayload = completed.parse_payload().unwrap();

        let prompt = payload.prompt.expect("formation prompt must be recorded");
        assert_eq!(
            prompt,
            agent.invocations()[0].prompt,
            "the recorded prompt must be the one the agent was actually sent"
        );
        // The audit trail must show whether the agent saw the two-tree context.
        assert!(prompt.contains("ACCUMULATED UNMERGED WORK"));
        assert_eq!(payload.agent_provider.as_deref(), Some("codex"));
        assert_eq!(
            payload.gate_results.len(),
            1,
            "the done-gate result that governs `done` must be queryable, not prose"
        );
        assert_eq!(payload.gate_results[0].command, "cargo test --workspace");
        assert!(payload.gate_results[0].required);
        assert!(payload.gate_results[0].output.contains(&noise));
        assert!(!prompt.contains("PASSING-GATE-NOISE"));
    }

    /// Source context reaches formation as a path, not as its contents.
    ///
    /// One campaign declared 21 context paths totalling 1,039,114 bytes, 17 of
    /// them source. Inlined, the prompt exceeded `ARG_MAX` and the agent could
    /// not be spawned at all. The agent holds `Read`/`Glob`/`Grep` over the same
    /// checkout, so a path is all it needs — and it reads the part it wants.
    #[tokio::test]
    async fn all_context_is_listed_without_copying_document_bodies() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        std::fs::write(repo.join("CHARTER.md"), "BINDING-CHARTER-WORDING").unwrap();
        std::fs::write(repo.join("big.rs"), "fn f() { /* SOURCE-BODY */ }").unwrap();
        std::fs::write(repo.join("AGENTS.md"), "AGENT-GUIDANCE-BODY").unwrap();
        std::fs::write(repo.join("api.proto"), "PROTO-BODY").unwrap();
        std::fs::create_dir_all(repo.join("book/src/guide")).unwrap();
        std::fs::write(repo.join("book/src/guide/api.md"), "REFERENCE-GUIDE-BODY").unwrap();
        let store_path = repo.join("campaigns.json");
        let mut campaign = campaign_for_accumulation_test();
        campaign.context_paths = [
            "CHARTER.md",
            "big.rs",
            "AGENTS.md",
            "api.proto",
            "book/src/guide/api.md",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let mut store = CampaignStore::default();
        store.add(campaign).unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", repo.to_str().unwrap());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Cut one slice.\",\"reason\":\"gap\"}\n```",
        );
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());

        block.execute(&manual_advance_trigger()).await.unwrap();

        let prompt = &agent.invocations()[0].prompt;
        assert!(
            !prompt.contains("BINDING-CHARTER-WORDING"),
            "contract files must be read selectively"
        );
        assert!(
            !prompt.contains("SOURCE-BODY"),
            "source context must not be inlined into the prompt"
        );
        assert!(prompt.contains("big.rs"), "source context must still be listed by path");
        for (path, body) in [
            ("AGENTS.md", "AGENT-GUIDANCE-BODY"),
            ("api.proto", "PROTO-BODY"),
            ("book/src/guide/api.md", "REFERENCE-GUIDE-BODY"),
        ] {
            assert!(prompt.contains(path));
            assert!(!prompt.contains(body));
        }
        assert!(
            prompt.contains("READ THESE YOURSELF"),
            "the agent must be told the listed paths are its to open"
        );
    }

    /// Formation must not ask the task to prove a gate the task never runs.
    ///
    /// The campaign's mechanical done-gates run at formation against the
    /// delivered trunk; the task runs the project's own discovered gates in an
    /// isolated worktree. Restating the former as task acceptance evidence
    /// creates a gap the executor cannot close no matter what it does —
    /// observed consuming a reviewer gap slot in three of one campaign's five
    /// cycles, including as the sole remaining item on an otherwise satisfied
    /// final cycle.
    #[tokio::test]
    async fn formation_is_told_not_to_restate_done_gates_as_task_evidence() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut campaign = campaign_for_accumulation_test();
        campaign.done_evidence.push(DoneEvidence::Gate {
            command: "mdbook build book".to_string(),
            required: true,
            artifacts: vec![],
        });
        let mut store = CampaignStore::default();
        store.add(campaign).unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Cut one slice.\",\"reason\":\"gap\"}\n```",
        );
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());

        block.execute(&manual_advance_trigger()).await.unwrap();

        let prompt = &agent.invocations()[0].prompt;
        assert!(
            prompt.contains("the task does NOT run these"),
            "the done-gate section must say where those gates actually run"
        );
        assert!(
            prompt.contains("DO NOT RESTATE THE MECHANICAL DONE-GATE COMMANDS AS TASK EVIDENCE"),
            "formation must be told not to demand evidence the task cannot produce"
        );
        // The JSON shape hint used to literally ask for "gates" in the objective.
        assert!(
            !prompt.contains("single objective plus gates/"),
            "the response shape must stop inviting gate commands into the objective"
        );
        // The rendered MECHANICAL DONE-GATE RESULTS section must actually carry
        // the gate data (via serde_json::to_string_pretty), not an empty
        // fallback — this campaign declared one done gate.
        assert!(
            prompt.contains("mdbook build book"),
            "gate section must render the declared done-gate command, not come back empty: {prompt}"
        );
    }

    /// Formation runs in a read-only sandbox, so it cannot build or test. On
    /// 2026-10-11 a campaign escalated after a landed cycle because clippy and
    /// `cargo test` could not open `target/debug/.cargo-build-lock`, while the
    /// required done-gate had passed and the task had landed only after its
    /// required gates passed. The prompt must say where test evidence comes
    /// from and that a command it cannot run here is no reason to escalate.
    #[tokio::test]
    async fn decision_prompt_says_the_sandbox_cannot_build_and_where_test_evidence_comes_from() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store.add(campaign_for_accumulation_test()).unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Cut one slice.\",\"reason\":\"gap\"}\n```",
        );
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());

        block.execute(&manual_advance_trigger()).await.unwrap();

        let prompt = &agent.invocations()[0].prompt;
        assert!(
            prompt.contains("READ-ONLY SANDBOX"),
            "formation must be told its sandbox is read-only: {prompt}"
        );
        assert!(
            prompt.contains("Do not run builds, tests, linters"),
            "formation must be told not to attempt commands that write target/ or caches"
        );
        assert!(
            prompt.contains("landed: true")
                && prompt
                    .contains("only after every required gate in the project's gate file passed"),
            "formation must be told what a landed run already proves"
        );
        assert!(
            prompt.contains("never a reason to escalate"),
            "a command this sandbox cannot run must not become an escalation"
        );
    }

    /// The dispatched task must name its cycle, and name the same one its
    /// sibling `CampaignAdvanceCompleted` reports — otherwise a consumer
    /// joining the two gets contradictory answers about which attempt it is
    /// looking at.
    #[tokio::test]
    async fn a_dispatched_task_names_the_cycle_it_belongs_to() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        // Seeded at 2 completed, so this dispatch is cycle 3.
        store.add(campaign_for_accumulation_test()).unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Cut one slice.\",\"reason\":\"gap\"}\n```",
        );
        let block = advance_block(agent, FakeShellGateway::success(), registry, store_path.clone());

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        let execution = result
            .events
            .iter()
            .find(|event| event.event_type == EventType::ExecutionRequested)
            .expect("next task dispatched");
        assert_eq!(
            execution.payload.get("campaign_cycle").and_then(serde_json::Value::as_u64),
            Some(3),
            "the dispatched task must carry its cycle number"
        );
        assert_eq!(
            execution.payload.get("campaign").and_then(serde_json::Value::as_str),
            Some("c")
        );

        let completed = result
            .events
            .iter()
            .find(|event| event.event_type == EventType::CampaignAdvanceCompleted)
            .expect("advance completed event");
        let payload: CampaignAdvanceCompletedPayload = completed.parse_payload().unwrap();
        assert_eq!(
            payload.cycles_completed, 3,
            "the two sides of one cycle must agree on its number"
        );
    }

    /// A `CampaignAdvanceRequested` carrying an operator origin, ready to run
    /// through the real block.
    fn advance_trigger_with_origin(
        origin: Option<&str>,
        throttle: foundry_sdk::throttle::Throttle,
    ) -> Event {
        Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            throttle,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: origin.map(ToString::to_string),
                run_event_id: None,
                run_result: None,
            })
            .unwrap(),
        )
    }

    /// A staged active campaign plus a block wired to fakes, for the
    /// operator-origin forwarding proofs below.
    fn origin_forwarding_block(dir: &std::path::Path) -> AdvanceCampaign {
        let store_path = dir.join("campaigns.json");
        let mut store = CampaignStore::default();
        store.add(campaign_for_accumulation_test()).unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.to_str().unwrap());
        advance_block(
            FakeAgentGateway::success_with(
                "```json\n{\"decision\":\"advance\",\"objective\":\"Cut one slice.\",\"reason\":\"gap\"}\n```",
            ),
            FakeShellGateway::success(),
            registry,
            store_path,
        )
    }

    /// The operator's note on a manual advance must reach the cycle that advance
    /// dispatches. It is the only carrier of *why a person asked* — drop it here
    /// and the ledger item for that cycle can never name its human origin, with
    /// nothing downstream able to recover it.
    #[tokio::test]
    async fn a_manual_advance_forwards_its_operator_origin_to_the_dispatched_cycle() {
        let dir = tempfile::tempdir().unwrap();
        let block = origin_forwarding_block(dir.path());

        let result = block
            .execute(&advance_trigger_with_origin(Some("host workbench: by hand"), Throttle::Full))
            .await
            .unwrap();

        let execution = result
            .events
            .iter()
            .find(|event| event.event_type == EventType::ExecutionRequested)
            .expect("next task dispatched");
        assert_eq!(
            execution.payload.get("operator_origin").and_then(serde_json::Value::as_str),
            Some("host workbench: by hand"),
            "the dispatched cycle must carry the advance's operator origin verbatim"
        );
    }

    /// An advance with no operator note must dispatch the payload it always did
    /// — an absent key, not an empty string, so a consumer can tell "no note"
    /// from "a note that says nothing".
    #[tokio::test]
    async fn a_manual_advance_without_an_origin_dispatches_no_operator_origin_key() {
        let dir = tempfile::tempdir().unwrap();
        let block = origin_forwarding_block(dir.path());

        let result =
            block.execute(&advance_trigger_with_origin(None, Throttle::Full)).await.unwrap();

        let execution = result
            .events
            .iter()
            .find(|event| event.event_type == EventType::ExecutionRequested)
            .expect("next task dispatched");
        assert!(
            execution.payload.get("operator_origin").is_none(),
            "an advance with no operator origin must leave the key absent"
        );
    }

    /// The dry-run preview must show the operator origin the real dispatch would
    /// carry — a preview that silently omits it misreports what the run does.
    #[test]
    fn a_dry_run_advance_previews_the_operator_origin_it_would_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let block = origin_forwarding_block(dir.path());

        let with_origin = block.dry_run_events(&advance_trigger_with_origin(
            Some("host workbench: by hand"),
            Throttle::DryRun,
        ));
        let execution = with_origin
            .iter()
            .find(|event| event.event_type == EventType::ExecutionRequested)
            .expect("dry run must preview the dispatched task");
        assert_eq!(
            execution.payload.get("operator_origin").and_then(serde_json::Value::as_str),
            Some("host workbench: by hand"),
            "the previewed cycle must carry the advance's operator origin verbatim"
        );

        let without_origin =
            block.dry_run_events(&advance_trigger_with_origin(None, Throttle::DryRun));
        let execution = without_origin
            .iter()
            .find(|event| event.event_type == EventType::ExecutionRequested)
            .expect("dry run must preview the dispatched task");
        assert!(
            execution.payload.get("operator_origin").is_none(),
            "a preview with no operator origin must leave the key absent"
        );
    }

    /// A decision made without asking an agent has no formation to record, and
    /// must not imply otherwise.
    #[tokio::test]
    async fn a_forced_decision_records_no_formation() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut campaign = campaign_for_accumulation_test();
        campaign.authorized_by = None;
        let mut store = CampaignStore::default();
        store.campaigns.push(campaign);
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = FakeAgentGateway::success();
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert!(agent.invocations().is_empty(), "no agent should have been asked");
        let completed = result
            .events
            .iter()
            .find(|event| event.event_type == EventType::CampaignAdvanceCompleted)
            .expect("advance completed event");
        let payload: CampaignAdvanceCompletedPayload = completed.parse_payload().unwrap();
        assert!(payload.prompt.is_none());
        assert!(payload.agent_provider.is_none());
        assert!(payload.gate_results.is_empty());
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
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
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
            land_blocked: None,
            proof_evidence: None,
            uncommitted_artifacts: None,
            trunk_arrivals: Vec::new(),
            verdict,
            context: LoopContext {
                campaign: Some("c".to_string()),
                ..LoopContext::default()
            },
            failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
        }
    }

    #[test]
    fn rechecked_failed_run_is_not_replayed_as_another_recovery() {
        use foundry_sdk::recovery::{RecoveryKind, RecoveryResolution, RecoveryState};
        let mut campaign = campaign_for_accumulation_test();
        campaign.authorized_by = Some("owner".into());
        let request = CampaignAdvanceRequestedPayload {
            campaign: campaign.name.clone(),
            run_event_id: Some("failed-run".into()),
            run_result: Some(run_result_with(TaskVerdict::RunnerError {
                detail: "runner unavailable".into(),
            })),
            operator_origin: None,
            providers: foundry_sdk::gateway::StageProviders::default(),
        };
        assert!(matches!(
            update_run_and_forced_outcome(&mut campaign, &request, true),
            Some(AdvanceOutcome::Recovery { .. })
        ));
        let preserved = campaign.pending_run_result.clone();
        campaign.recovery = Some(RecoveryState {
            id: "recovery_failed-run_unique".into(),
            kind: RecoveryKind::RunnerUnavailable,
            detail: "runner unavailable".into(),
            recorded_at: chrono::Utc::now(),
            resolution: Some(RecoveryResolution {
                evidence: vec!["runner probe passed".into()],
                resolved_at: chrono::Utc::now(),
            }),
        });
        assert!(
            update_run_and_forced_outcome(&mut campaign, &request, true).is_none(),
            "formation must now reassess the objective"
        );
        assert_eq!(
            campaign.pending_run_result, preserved,
            "preserved work remains available to formation"
        );
        let mut new_failure = request.clone();
        new_failure.run_event_id = Some("another-run".into());
        assert!(
            matches!(
                update_run_and_forced_outcome(&mut campaign, &new_failure, true),
                Some(AdvanceOutcome::Recovery { .. })
            ),
            "a recheck cannot exempt a new failure"
        );
    }

    /// A decision agent that answers `advance`, and on its first ask edits
    /// the campaign it is forming, as an owner would mid-formation.
    struct EditsOnFirstAsk {
        store: std::path::PathBuf,
        edit: fn(&mut Campaign),
        asked: std::sync::atomic::AtomicUsize,
        prompts: std::sync::Mutex<Vec<String>>,
    }

    impl crate::gateway::AgentGateway for EditsOnFirstAsk {
        fn invoke<'a>(
            &'a self,
            request: &'a crate::gateway::AgentRequest,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<AgentResponse>> + Send + 'a>,
        > {
            Box::pin(async move {
                self.prompts.lock().unwrap().push(request.prompt.clone());
                if self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    let mut guard = CampaignStore::lock_exclusive(&self.store).unwrap();
                    (self.edit)(guard.store.find_mut("c").unwrap());
                    guard.save().unwrap();
                }
                Ok(AgentResponse::success(
                    r#"{"decision":"advance","objective":"next slice","reason":"gap remains"}"#,
                ))
            })
        }
    }

    fn reforming_block(
        dir: &std::path::Path,
        edit: fn(&mut Campaign),
    ) -> (AdvanceCampaign, std::sync::Arc<EditsOnFirstAsk>, std::path::PathBuf) {
        let (store_path, registry) = staged_active_campaign(dir);
        let agent = std::sync::Arc::new(EditsOnFirstAsk {
            store: store_path.clone(),
            edit,
            asked: std::sync::atomic::AtomicUsize::new(0),
            prompts: std::sync::Mutex::new(Vec::new()),
        });
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());
        (block, agent, store_path)
    }

    fn automatic_advance_trigger(run_result: TaskRunCompletedPayload) -> Event {
        Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
                run_event_id: Some("run-1".to_string()),
                run_result: Some(run_result),
            })
            .unwrap(),
        )
    }

    /// An edit during formation re-forms from the edited record. The arriving
    /// run result was recorded by the first pass and is not counted again by
    /// the second, and the cycle is counted once, for the outcome applied.
    #[tokio::test]
    async fn an_edit_during_formation_reforms_without_counting_the_run_twice() {
        let dir = tempfile::tempdir().unwrap();
        let (block, agent, store_path) =
            reforming_block(dir.path(), |campaign| campaign.mission = "edited".to_string());
        let mut landed = run_result_with(TaskVerdict::Complete);
        landed.success = true;
        landed.landed = true;

        let result = block.execute(&automatic_advance_trigger(landed)).await.unwrap();

        assert!(result.success, "{}", result.summary);
        assert_eq!(agent.asked.load(std::sync::atomic::Ordering::SeqCst), 2, "formed twice");
        let stored = CampaignStore::load(&store_path).unwrap().find("c").unwrap().clone();
        assert_eq!(stored.mission, "edited", "the edit was kept");
        assert_eq!(stored.cycles_landed, 1, "the run result was recorded once");
        assert_eq!(stored.cycles_completed, 3, "one cycle dispatched");
        assert_eq!(stored.last_run_event_id.as_deref(), Some("run-1"));
        let kinds: Vec<_> = result.events.iter().map(|e| e.event_type.clone()).collect();
        assert_eq!(
            kinds,
            [
                EventType::CampaignAdvanceCompleted,
                EventType::ExecutionRequested
            ]
        );
    }

    /// An owner decision recorded through `decide` on the active campaign
    /// while it forms is not overwritten by the formation's write-back. The
    /// record changed, so the campaign is formed again, the second prompt
    /// carries the decision as binding, and the campaign stays active with the
    /// decision stored once.
    #[tokio::test]
    async fn a_decision_recorded_on_an_active_campaign_during_formation_survives_and_reforms() {
        let dir = tempfile::tempdir().unwrap();
        let (block, agent, store_path) = reforming_block(dir.path(), |campaign| {
            campaign
                .record_owner_decision(
                    "Do not remove the v1 endpoint.".to_string(),
                    chrono::Utc::now(),
                    0,
                )
                .unwrap();
        });

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert!(result.success, "{}", result.summary);
        assert_eq!(agent.asked.load(std::sync::atomic::Ordering::SeqCst), 2, "formed twice");
        let prompts = agent.prompts.lock().unwrap();
        assert!(!prompts[0].contains("Do not remove the v1 endpoint."));
        assert!(
            prompts[1].contains("Do not remove the v1 endpoint."),
            "the reformation sees the decision"
        );
        let stored = CampaignStore::load(&store_path).unwrap().find("c").unwrap().clone();
        assert_eq!(stored.status, CampaignStatus::Active);
        assert_eq!(stored.owner_decisions.len(), 1, "the decision survived formation");
        assert_eq!(stored.owner_decisions[0].decision, "Do not remove the v1 endpoint.");
    }

    /// The second formation is held to the edited budget: an owner who cut the
    /// budget mid-formation gets the budget escalation, not a dispatch.
    #[tokio::test]
    async fn a_budget_cut_during_formation_is_enforced_on_the_reform() {
        let dir = tempfile::tempdir().unwrap();
        let (block, agent, store_path) =
            reforming_block(dir.path(), |campaign| campaign.budget.max_cycles = 2);

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert_eq!(agent.asked.load(std::sync::atomic::Ordering::SeqCst), 2);
        let stored = CampaignStore::load(&store_path).unwrap().find("c").unwrap().clone();
        assert_eq!(stored.status, CampaignStatus::Escalated);
        assert_eq!(stored.cycles_completed, 2, "nothing dispatched past the edited budget");
        assert!(
            terminal_reason(&result, &EventType::CampaignEscalated).contains("budget exhausted")
        );
        assert!(!result.events.iter().any(|e| e.event_type == EventType::ExecutionRequested));
    }

    /// The advance stood down: it succeeded, and its only event is a
    /// `stood_down` completion naming `status`, with no objective.
    fn assert_only_stand_down(result: &TaskBlockResult, status: CampaignStatus) {
        assert!(result.success, "{}", result.summary);
        assert_eq!(result.events.len(), 1, "{:?}", result.events);
        let event = &result.events[0];
        assert_eq!(event.event_type, EventType::CampaignAdvanceCompleted);
        let payload: CampaignAdvanceCompletedPayload = event.parse_payload().unwrap();
        assert!(payload.success);
        match payload.outcome {
            CampaignAdvanceOutcome::StoodDown(stand_down) => {
                assert_eq!(stand_down.campaign_status, status);
            }
            CampaignAdvanceOutcome::Decided(decision) => {
                panic!("a stopped campaign formed a decision: {decision:?}")
            }
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
            .and_then(|event| {
                event.payload.get("reason").or_else(|| {
                    event.payload.get("recovery").and_then(|recovery| recovery.get("detail"))
                })
            })
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
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());

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
    async fn exhausted_decision_agent_retries_record_recovery_with_every_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let agent = FakeAgentGateway::failure("");
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert_eq!(agent.invocations().len(), DECISION_ATTEMPTS as usize);
        let reason = terminal_reason(&result, &EventType::CampaignRecoveryRecorded);
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
            CampaignStatus::Active
        );
    }

    /// Re-asking an agent that already answered only burns identical
    /// invocations — the answer will be malformed again.
    #[tokio::test(start_paused = true)]
    async fn malformed_decision_records_recovery_without_burning_retries() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let agent = FakeAgentGateway::success_with("I think we should keep going.");
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert_eq!(agent.invocations().len(), 1, "a permanent failure must not be retried");
        assert!(
            terminal_reason(&result, &EventType::CampaignRecoveryRecorded)
                .contains("campaign formation failed")
        );
    }

    // --- provider outages pause rather than escalate ----------------------

    /// A tripped breaker killed a campaign at cycle 0, twice, before it had
    /// done any work. The breaker stays open, so retrying is pointless and
    /// escalating spends the campaign on something outside its own work.
    #[tokio::test(start_paused = true)]
    async fn open_provider_circuit_breaker_records_recovery_without_retrying() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let agent = FakeAgentGateway::always(AgentResponse::terminal_failure(
            AgentProvider::Claude,
            AgentFailureKind::AccountLimit,
            "You've hit your monthly spend limit (circuit breaker open for provider claude)",
        ));
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert_eq!(agent.invocations().len(), 1, "an open breaker returns the same refusal");
        // Formation settles while the campaign waits for recovery evidence.
        // No owner escalation or campaign completion is emitted.
        assert_eq!(
            result.events.iter().map(|event| &event.event_type).collect::<Vec<_>>(),
            vec![
                &EventType::CampaignAdvanceCompleted,
                &EventType::CampaignRecoveryRecorded
            ],
            "formation settlement and operational recovery must both be visible: {:?}",
            result.events
        );
        assert!(
            result.events[1].payload["recovery"]["detail"]
                .as_str()
                .is_some_and(|reason| reason.contains("monthly spend limit")),
            "recovery event must explain itself: {:?}",
            result.events[1].payload
        );
        assert!(result.summary.contains("recovery"), "recovery reason lost: {}", result.summary);
        assert!(result.summary.contains("monthly spend limit"));
        let stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Active);
        assert_eq!(campaign.cycles_completed, 2, "recovery must not consume a cycle");
    }

    /// The same breaker text also arrives as the executor's typed verdict.
    #[tokio::test(start_paused = true)]
    async fn runner_error_records_recovery_and_keeps_preserved_work() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let agent = FakeAgentGateway::success();
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::Full,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
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
            vec![
                &EventType::CampaignAdvanceCompleted,
                &EventType::CampaignRecoveryRecorded
            ],
            "the verdict path must be as observable as the decision path: {:?}",
            result.events
        );
        let stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Active);
        assert_eq!(
            campaign
                .pending_run_result
                .as_ref()
                .and_then(|result| result.preservation_ref.as_deref()),
            Some("foundry-task/preserved"),
            "the resumed advance must still see the preserved work"
        );
    }

    const CAPACITY: &str = "Selected model is at capacity. Please try a different model.";

    /// A cycle whose Codex session the provider refused at capacity: exit 1,
    /// zero tokens, settled as a runner error that names the outage.
    fn provider_refused_run() -> TaskRunCompletedPayload {
        let mut result = run_result_with(TaskVerdict::RunnerError {
            detail: format!("provider unavailable: {CAPACITY} (4 attempts)"),
        });
        result.failure = foundry_sdk::gateway::AgentFailureMetadata::new(AgentProvider::Codex)
            .classified(AgentFailureKind::ProviderUnavailable)
            .with_message(format!("{CAPACITY} (4 attempts)"));
        result
    }

    /// A campaign whose cycle 2 dispatched `objective`, `repeats` times over,
    /// with its cycle budget already spent.
    fn campaign_at_its_last_cycle(
        dir: &std::path::Path,
        repeats: usize,
    ) -> (
        std::path::PathBuf,
        std::sync::Arc<std::sync::RwLock<foundry_sdk::registry::Registry>>,
    ) {
        let store_path = dir.join("campaigns.json");
        let mut campaign = campaign_for_accumulation_test();
        campaign.budget.max_cycles = 2;
        for _ in 0..repeats {
            campaign
                .record_dispatched_objective("Implement the parser behind the public API".into());
            campaign.record_cycle_outcome(&provider_refused_run());
        }
        campaign.record_dispatched_objective("Implement the parser behind the public API".into());
        let mut store = CampaignStore::default();
        store.add(campaign).unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.to_str().unwrap());
        (store_path, registry)
    }

    fn dispatched(result: &foundry_sdk::task_block::TaskBlockResult) -> &Event {
        result
            .events
            .iter()
            .find(|event| event.event_type == EventType::ExecutionRequested)
            .unwrap_or_else(|| panic!("no dispatch in {:?}", result.events))
    }

    /// Scenario 3 of the 2026-10-19 outage: a campaign cycle the provider
    /// refused is dispatched again, the same objective from the same base and
    /// under the same cycle number, after a pause. No agent forms it, no cycle
    /// budget is spent (this campaign has none left), and no recovery is
    /// recorded, so nobody has to act.
    #[tokio::test(start_paused = true)]
    async fn a_provider_refused_cycle_is_redispatched_without_forming_or_spending_budget() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = campaign_at_its_last_cycle(dir.path(), 0);
        let agent = FakeAgentGateway::success();
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());
        let before = chrono::Utc::now();

        let result =
            block.execute(&automatic_advance_trigger(provider_refused_run())).await.unwrap();

        let after = chrono::Utc::now();
        assert!(result.success, "{}", result.summary);
        assert!(agent.invocations().is_empty(), "formation must not be asked");
        assert_eq!(
            result.events.iter().map(|event| &event.event_type).collect::<Vec<_>>(),
            vec![
                &EventType::CampaignAdvanceCompleted,
                &EventType::ExecutionRequested
            ],
        );
        let dispatch = dispatched(&result);
        assert_eq!(dispatch.payload["prompt"], "Implement the parser behind the public API");
        assert_eq!(dispatch.payload["campaign_cycle"], 2);
        assert_eq!(dispatch.payload["base_ref"], "foundry-task/preserved");
        let not_before: chrono::DateTime<chrono::Utc> =
            serde_json::from_value(dispatch.payload["not_before"].clone()).unwrap();
        let pause = chrono::Duration::minutes(5);
        assert!(not_before >= before + pause && not_before <= after + pause, "{not_before}");
        let reason = result.events[0].payload["reason"].as_str().unwrap_or_default().to_string();
        assert!(
            reason.contains("did not start") && reason.contains("no cycle budget spent"),
            "{reason}"
        );

        let stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find("c").unwrap();
        assert_eq!(campaign.cycles_completed, 2, "the repeat spends no cycle");
        assert_eq!(campaign.status, CampaignStatus::Active);
        assert!(campaign.recovery.is_none(), "no recovery, no owner action");
        let cycle_two = campaign.objective_history.iter().filter(|entry| entry.cycle == 2).count();
        assert_eq!(cycle_two, 2, "the history shows the refused attempt and its repeat");
    }

    /// An outage that outlasts every re-dispatch records a typed
    /// `runner_unavailable` recovery, never `capability_unavailable`, and its
    /// recheck dispatches the same objective at once instead of re-forming.
    #[tokio::test(start_paused = true)]
    async fn an_outage_past_every_redispatch_records_runner_unavailable_and_its_recheck_dispatches()
    {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) =
            campaign_at_its_last_cycle(dir.path(), super::PROVIDER_REDISPATCH_DELAYS.len());
        let agent = FakeAgentGateway::success();
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());

        let result =
            block.execute(&automatic_advance_trigger(provider_refused_run())).await.unwrap();

        assert!(agent.invocations().is_empty());
        let recorded = result
            .events
            .iter()
            .find(|event| event.event_type == EventType::CampaignRecoveryRecorded)
            .expect("a typed recovery");
        assert_eq!(recorded.payload["recovery"]["kind"], "runner_unavailable");
        let detail = terminal_reason(&result, &EventType::CampaignRecoveryRecorded);
        assert!(detail.contains("provider unavailable"), "{detail}");

        let mut store = CampaignStore::load(&store_path).unwrap();
        let id = store.find("c").unwrap().recovery.as_ref().unwrap().id.clone();
        store
            .recover_campaign(
                "c",
                &id,
                &["codex capacity restored".to_string()],
                chrono::Utc::now(),
            )
            .unwrap();
        store.save(&store_path).unwrap();

        let rechecked = block.execute(&manual_advance_trigger()).await.unwrap();

        assert!(agent.invocations().is_empty(), "the recheck does not re-form");
        let dispatch = dispatched(&rechecked);
        assert_eq!(dispatch.payload["prompt"], "Implement the parser behind the public API");
        assert_eq!(dispatch.payload["campaign_cycle"], 2);
        assert!(dispatch.payload.get("not_before").is_none(), "a recheck dispatches at once");
        let campaign = CampaignStore::load(&store_path).unwrap().find("c").unwrap().clone();
        assert_eq!(campaign.cycles_completed, 2);
    }

    /// Formation is told whether the last run started and why it ended.
    #[test]
    fn formation_is_told_whether_the_last_run_started() {
        assert_eq!(
            last_run_line(&provider_refused_run()),
            format!(
                "last run: did not start (provider unavailable: {CAPACITY} (4 attempts)); no agent worked on the objective"
            )
        );
        let defect = run_result_with(TaskVerdict::Defect {
            diagnosis: "the parser drops trailing fields".into(),
        });
        assert_eq!(
            last_run_line(&defect),
            "last run: started, ended with verdict defect: the parser drops trailing fields"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn runner_error_unrelated_to_provider_records_recovery() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let block = advance_block(
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
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
                run_event_id: Some("run-1".to_string()),
                run_result: Some(run_result_with(TaskVerdict::RunnerError {
                    detail: "task review missing isolated worktree".to_string(),
                })),
            })
            .unwrap(),
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(
            terminal_reason(&result, &EventType::CampaignRecoveryRecorded)
                .contains("task review missing isolated worktree")
        );
        assert_eq!(
            CampaignStore::load(&store_path).unwrap().find("c").unwrap().status,
            CampaignStatus::Active
        );
    }

    /// A continuation refused because its preserved work conflicts with trunk
    /// is not a runner fault: formation decides, from trunk, with the refusal
    /// and its files in front of it, and the next cycle does not retry the
    /// refused base.
    #[tokio::test(start_paused = true)]
    async fn a_refused_continuation_reaches_formation_and_the_next_cycle_starts_from_trunk() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Reapply the preserved slice onto trunk.\",\"reason\":\"refused continuation\"}\n```",
        );
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());
        let refusal = "continuation refused before any agent session: the preserved work at \
                       foundry-task/p-c14 (637e470) is 3 commit(s) behind trunk (a57afdf) and \
                       replaying it onto trunk conflicts in: Package.swift, README.md";
        let mut refused = run_result_with(TaskVerdict::RunnerError {
            detail: refusal.to_string(),
        });
        refused.summary = refusal.to_string();
        refused.preservation_ref = None;
        refused.land_blocked = Some(super::LandBlocked::ContinuationConflict);

        let result = block.execute(&automatic_advance_trigger(refused)).await.unwrap();

        assert!(
            result
                .events
                .iter()
                .all(|event| event.event_type != EventType::CampaignEscalated),
            "a refused continuation must not escalate as a runner fault: {:?}",
            result.events
        );
        let execution = result
            .events
            .iter()
            .find(|event| event.event_type == EventType::ExecutionRequested)
            .expect("formation dispatched the next cycle");
        assert!(
            execution.payload.get("base_ref").is_none(),
            "the next cycle must start from trunk, not the refused base"
        );
        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1, "formation ran once");
        let prompt = &invocations[0].prompt;
        assert!(prompt.contains("continuation_conflict"), "{prompt}");
        assert!(prompt.contains("Package.swift, README.md"), "{prompt}");
        assert!(prompt.contains("no longer carries forward"), "{prompt}");
    }

    // --- preserved work settled outside the campaign ----------------------

    const PRESERVED_REF: &str = "foundry-task/p-c-c2-a38a60";

    /// Cycle 2's result: a remainder whose work is preserved at
    /// [`PRESERVED_REF`].
    fn preserved_run() -> TaskRunCompletedPayload {
        let mut run = run_result_with(TaskVerdict::Remainder {
            gaps: vec!["conformance table".to_string()],
        });
        run.summary = "task stopped with a typed non-complete verdict; work preserved".to_string();
        run.preservation_ref = Some(PRESERVED_REF.to_string());
        run
    }

    /// Cycle 2's work item as the ledger settles it from [`preserved_run`].
    fn preserved_cycle_item() -> foundry_sdk::work_item::WorkItem {
        use foundry_sdk::work_item::{WorkItem, WorkItemKind, WorkItemSpec, WorkLane};
        let at = chrono::Utc::now();
        let mut item = WorkItem::dispatched(
            WorkItemSpec {
                project: "p".to_string(),
                objective: "Write the recovery conformance table.".to_string(),
                kind: WorkItemKind::CampaignCycle,
                lane: WorkLane::Campaign,
                origin: "campaign c".to_string(),
                trace_id: None,
            },
            at,
        )
        .with_source(Some(WorkSource::campaign("c", 2)));
        item.settle_from_task_run(&preserved_run(), Some(true), at);
        item
    }

    /// What `foundry queue close` does to an open item.
    fn close_by_owner(item: &mut foundry_sdk::work_item::WorkItem, reason: &str) {
        item.operator_action = Some(foundry_sdk::work_item::WorkItemOperatorAction {
            command: "close".to_string(),
            origin: "max-headroom".to_string(),
            previous_state: item.state,
            previous_reason: item.reason.clone(),
            previous_settled_at: item.settled_at,
        });
        item.settle_cancelled(reason, item.disposition.clone(), chrono::Utc::now());
    }

    /// A campaign whose cycle 2 preserved its work and which now waits for
    /// the owner's manual advance, beside a ledger holding cycle 2's item as
    /// `settle` leaves it. Returns the item's id.
    fn campaign_awaiting_owner_advance(
        dir: &std::path::Path,
        settle: impl FnOnce(&mut foundry_sdk::work_item::WorkItem),
    ) -> (
        std::path::PathBuf,
        std::sync::Arc<std::sync::RwLock<foundry_sdk::registry::Registry>>,
        String,
    ) {
        let store_path = dir.join("campaigns.json");
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                pending_run_result: Some(preserved_run()),
                last_run_event_id: Some("run-2".to_string()),
                ..campaign_for_accumulation_test()
            })
            .unwrap();
        store.save(&store_path).unwrap();
        let mut item = preserved_cycle_item();
        assert_eq!(
            item.disposition.as_ref().and_then(|d| d.preservation_ref.as_deref()),
            Some(PRESERVED_REF),
            "a preserved cycle's disposition records the ref its run result names"
        );
        settle(&mut item);
        let id = item.id.clone();
        foundry_sdk::work_item::WorkItemStore {
            items: vec![item],
            ..Default::default()
        }
        .save(&ledger_path_beside(&store_path))
        .unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.to_str().unwrap());
        (store_path, registry, id)
    }

    fn advance_decision_agent() -> std::sync::Arc<FakeAgentGateway> {
        FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Cut the next slice from trunk.\",\"reason\":\"gap remains\"}\n```",
        )
    }

    fn dispatched_base_ref(result: &TaskBlockResult) -> Option<String> {
        result
            .events
            .iter()
            .find(|event| event.event_type == EventType::ExecutionRequested)
            .expect("formation dispatched the next cycle")
            .payload
            .get("base_ref")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string)
    }

    fn probed_the_ref(shell: &FakeShellGateway) -> bool {
        shell
            .invocations()
            .iter()
            .any(|inv| inv.args.iter().any(|arg| arg.contains(PRESERVED_REF)))
    }

    /// The owner reconciled cycle 2's work onto trunk by hand and closed its
    /// item. The next cycle must start from trunk, and formation must be told
    /// why, not shown the closed ref as the base it continues from.
    #[tokio::test]
    async fn an_owner_closed_preserved_item_no_longer_carries_forward() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry, item_id) = campaign_awaiting_owner_advance(dir.path(), |item| {
            close_by_owner(item, "cherry-picked onto main by hand");
        });
        let agent = advance_decision_agent();
        let shell = FakeShellGateway::success();
        let block = advance_block(agent.clone(), shell.clone(), registry, store_path);

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert_eq!(dispatched_base_ref(&result), None, "the next cycle starts from trunk");
        let prompt = &agent.invocations()[0].prompt;
        assert!(!prompt.contains(&format!("NEXT CYCLE BASE: {PRESERVED_REF}")), "{prompt}");
        assert!(prompt.contains("NEXT CYCLE BASE: the live checkout above"), "{prompt}");
        assert!(prompt.contains(PRESERVED_REF), "the settlement names the ref: {prompt}");
        assert!(prompt.contains(&item_id), "the settlement names the item: {prompt}");
        assert!(prompt.contains("settled cancelled by an owner close"), "{prompt}");
        assert!(prompt.contains("cherry-picked onto main by hand"), "{prompt}");
        assert!(!probed_the_ref(&shell), "a settled ref is not probed as accumulated work");
    }

    /// Supersession detection found cycle 2's work on trunk and settled its
    /// item `landed`. The next cycle starts from trunk.
    #[tokio::test]
    async fn a_preserved_item_superseded_on_trunk_no_longer_carries_forward() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry, item_id) = campaign_awaiting_owner_advance(dir.path(), |item| {
            item.settle_landed("superseded by 9f3c2e1", chrono::Utc::now());
        });
        let agent = advance_decision_agent();
        let shell = FakeShellGateway::success();
        let block = advance_block(agent.clone(), shell.clone(), registry, store_path);

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert_eq!(dispatched_base_ref(&result), None, "the next cycle starts from trunk");
        let prompt = &agent.invocations()[0].prompt;
        assert!(!prompt.contains(&format!("NEXT CYCLE BASE: {PRESERVED_REF}")), "{prompt}");
        assert!(prompt.contains(&item_id), "{prompt}");
        assert!(prompt.contains("settled landed: superseded by 9f3c2e1"), "{prompt}");
        assert!(!probed_the_ref(&shell));
    }

    /// Control: an item still `preserved` carries its work forward as before.
    #[tokio::test]
    async fn a_still_preserved_item_keeps_carrying_forward() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry, _) = campaign_awaiting_owner_advance(dir.path(), |_| {});
        let agent = advance_decision_agent();
        let shell = FakeShellGateway::success();
        let block = advance_block(agent.clone(), shell.clone(), registry, store_path);

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert_eq!(dispatched_base_ref(&result).as_deref(), Some(PRESERVED_REF));
        let prompt = &agent.invocations()[0].prompt;
        assert!(prompt.contains(&format!("NEXT CYCLE BASE: {PRESERVED_REF}")), "{prompt}");
        assert!(probed_the_ref(&shell));
    }

    /// An unreadable ledger cannot say the work was settled, so the cycle
    /// continues from the ref as it always has.
    #[tokio::test]
    async fn an_unreadable_ledger_keeps_continuing_from_the_preserved_ref() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry, _) = campaign_awaiting_owner_advance(dir.path(), |item| {
            close_by_owner(item, "cherry-picked onto main by hand");
        });
        std::fs::write(ledger_path_beside(&store_path), "{ not json").unwrap();
        let agent = advance_decision_agent();
        let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path);

        let result = block.execute(&manual_advance_trigger()).await.unwrap();

        assert_eq!(dispatched_base_ref(&result).as_deref(), Some(PRESERVED_REF));
    }

    fn ledger_of(items: Vec<foundry_sdk::work_item::WorkItem>) -> WorkItemStore {
        WorkItemStore {
            items,
            ..Default::default()
        }
    }

    #[test]
    fn only_a_cancelled_or_landed_item_settles_its_ref() {
        use foundry_sdk::work_item::WorkItemState;
        let preserved = preserved_cycle_item();
        assert_eq!(
            settled_base(&ledger_of(vec![preserved.clone()]), "c", "p", PRESERVED_REF),
            None
        );
        for state in [
            WorkItemState::NeedsDecision,
            WorkItemState::Failed,
            WorkItemState::Running,
        ] {
            let mut open = preserved.clone();
            open.state = state;
            assert_eq!(
                settled_base(&ledger_of(vec![open]), "c", "p", PRESERVED_REF),
                None,
                "{state:?} still carries its work"
            );
        }
        let mut closed = preserved.clone();
        close_by_owner(&mut closed, "reconciled by hand");
        let settled = settled_base(&ledger_of(vec![closed.clone()]), "c", "p", PRESERVED_REF)
            .expect("an owner close settles the ref");
        assert_eq!(settled.item_id, closed.id);
        assert_eq!(settled.describe(), "settled cancelled by an owner close: reconciled by hand");
        let mut superseded = preserved;
        superseded.settle_landed("superseded by 9f3c2e1", chrono::Utc::now());
        assert_eq!(
            settled_base(&ledger_of(vec![superseded]), "c", "p", PRESERVED_REF)
                .map(|settled| settled.describe()),
            Some("settled landed: superseded by 9f3c2e1".to_string())
        );
    }

    #[test]
    fn a_settled_ref_matches_only_its_own_campaign_cycle() {
        use foundry_sdk::work_item::WorkItemKind;
        let mut closed = preserved_cycle_item();
        close_by_owner(&mut closed, "reconciled by hand");
        let ledger = ledger_of(vec![closed.clone()]);
        assert_eq!(settled_base(&ledger, "other", "p", PRESERVED_REF), None, "another campaign");
        assert_eq!(settled_base(&ledger, "c", "q", PRESERVED_REF), None, "another project");
        assert_eq!(settled_base(&ledger, "c", "p", "foundry-task/other"), None, "another ref");
        let mut task = closed;
        task.kind = WorkItemKind::Task;
        assert_eq!(
            settled_base(&ledger_of(vec![task]), "c", "p", PRESERVED_REF),
            None,
            "only a campaign cycle item counts"
        );
    }

    /// Two items recording the same ref: the latest submitted decides.
    #[test]
    fn the_latest_item_for_a_ref_decides_whether_it_is_settled() {
        let mut older = preserved_cycle_item();
        close_by_owner(&mut older, "reconciled by hand");
        older.submitted_at -= chrono::Duration::hours(1);
        let newer = preserved_cycle_item();
        assert_eq!(
            settled_base(&ledger_of(vec![older.clone(), newer.clone()]), "c", "p", PRESERVED_REF),
            None,
            "the newer item is still preserved"
        );
        let mut newer_closed = newer;
        close_by_owner(&mut newer_closed, "closed again");
        older.state = foundry_sdk::work_item::WorkItemState::Preserved;
        assert_eq!(
            settled_base(&ledger_of(vec![newer_closed.clone(), older]), "c", "p", PRESERVED_REF)
                .map(|settled| settled.item_id),
            Some(newer_closed.id),
            "order in the file does not matter"
        );
    }

    /// The last authorized cycle was refused before its agent ran. The
    /// escalation must say so and name the stale ref, so the owner knows the
    /// cycle bought nothing and what to do next.
    #[tokio::test]
    async fn an_exhausted_budget_after_a_refused_continuation_names_the_stale_ref() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                cycles_completed: 6,
                ..campaign_for_accumulation_test()
            })
            .unwrap();
        store.save(&store_path).unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let agent = advance_decision_agent();
        let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path);
        let refusal = format!(
            "continuation refused before any agent session: the preserved work at \
             {PRESERVED_REF} (637e470) is 4 commit(s) behind trunk (a57afdf) and replaying it \
             onto trunk conflicts in: RECOVERY-CONFORMANCE.md. The preserved ref is untouched; \
             carry the work forward by reapplying it onto trunk"
        );
        let mut refused = run_result_with(TaskVerdict::RunnerError {
            detail: refusal.clone(),
        });
        refused.summary.clone_from(&refusal);
        refused.preservation_ref = None;
        refused.land_blocked = Some(super::LandBlocked::ContinuationConflict);

        let result = block.execute(&automatic_advance_trigger(refused)).await.unwrap();

        assert!(agent.invocations().is_empty(), "an exhausted budget forms nothing");
        let reason = terminal_reason(&result, &EventType::CampaignEscalated);
        assert!(reason.contains("budget exhausted (6)"), "{reason}");
        assert!(reason.contains("before any agent session ran"), "{reason}");
        assert!(reason.contains(PRESERVED_REF), "the reason names the stale ref: {reason}");
        assert!(reason.contains("RECOVERY-CONFORMANCE.md"), "{reason}");
        assert!(
            reason.contains("add a cycle"),
            "the reason says what the owner can do: {reason}"
        );
        assert!(reason.contains("foundry queue close"), "{reason}");
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
            logs: Vec::new(),
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

    #[test]
    fn simulate_returns_no_events_when_payload_unparseable() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let block = advance_block(
            FakeAgentGateway::success(),
            FakeShellGateway::success(),
            registry,
            store_path,
        );
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::DryRun,
            serde_json::json!({"not": "a valid campaign advance payload shape"}),
        );

        let events = block.dry_run_events(&trigger);

        assert!(events.is_empty(), "expected `events` to be empty: {events:?}");
        assert!(events.iter().all(|e| !format!("{e:?}").contains("unknown")));
    }

    #[test]
    fn simulate_returns_no_events_when_store_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        std::fs::write(&store_path, "not valid json{{{").unwrap();
        let registry =
            super::super::test_helpers::registry_with_project("p", dir.path().to_str().unwrap());
        let block = advance_block(
            FakeAgentGateway::success(),
            FakeShellGateway::success(),
            registry,
            store_path,
        );
        let trigger = Event::new(
            EventType::CampaignAdvanceRequested,
            "p".to_string(),
            Throttle::DryRun,
            Event::serialize_payload(&CampaignAdvanceRequestedPayload {
                providers: foundry_sdk::gateway::StageProviders::default(),
                campaign: "c".to_string(),
                operator_origin: None,
                run_event_id: None,
                run_result: None,
            })
            .unwrap(),
        );

        let events = block.dry_run_events(&trigger);

        assert!(events.is_empty(), "expected `events` to be empty: {events:?}");
        assert!(events.iter().all(|e| !format!("{e:?}").contains("unknown")));
    }
    #[tokio::test]
    async fn exhausted_defect_invokes_neither_agent_nor_gates() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let mut store = CampaignStore::load(&store_path).unwrap();
        let campaign = &mut store.campaigns[0];
        campaign.cycles_completed = campaign.budget.max_cycles;
        store.save(&store_path).unwrap();
        let agent = FakeAgentGateway::success_with("should never be asked");
        let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path);
        let mut trigger = manual_advance_trigger();
        trigger.payload["run_event_id"] = serde_json::json!("defect-result");
        trigger.payload["run_result"] = serde_json::json!({
            "project":"p", "success":false, "landed":false, "summary":"rejected",
            "verdict":"defect", "diagnosis":"marker checks", "trunk_arrivals":[]
        });
        let result = block.execute(&trigger).await.unwrap();
        assert!(agent.invocations().is_empty());
        assert!(
            terminal_reason(&result, &EventType::CampaignEscalated).contains("budget exhausted")
        );
        let completed = result
            .events
            .iter()
            .find(|e| e.event_type == EventType::CampaignAdvanceCompleted)
            .unwrap();
        assert!(
            completed
                .parse_payload::<CampaignAdvanceCompletedPayload>()
                .unwrap()
                .gate_results
                .is_empty()
        );
    }

    #[tokio::test]
    async fn prompt_budget_rejects_before_agent_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let mut store = CampaignStore::load(&store_path).unwrap();
        store.campaigns[0].budget.stages.formation_prompt_bytes = 128;
        store.save(&store_path).unwrap();
        let agent = FakeAgentGateway::success_with("should never be asked");
        let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path);
        let result = block.execute(&manual_advance_trigger()).await.unwrap();
        assert!(agent.invocations().is_empty());
        assert!(
            terminal_reason(&result, &EventType::CampaignRecoveryRecorded).contains("stage budget")
        );
    }
    #[tokio::test]
    async fn exhausted_campaign_retains_result_through_extension_and_resume() {
        let dir = tempfile::tempdir().unwrap();
        let (store_path, registry) = staged_active_campaign(dir.path());
        let mut store = CampaignStore::load(&store_path).unwrap();
        store.campaigns[0].cycles_completed = 1;
        store.campaigns[0].budget.max_cycles = 1;
        store.save(&store_path).unwrap();
        let agent = FakeAgentGateway::success_with(
            "```json\n{\"decision\":\"advance\",\"objective\":\"Repair the validator path diagnostics.\",\"reason\":\"preserved reviewer finding\"}\n```",
        );
        let block =
            advance_block(agent.clone(), FakeShellGateway::success(), registry, store_path.clone());
        let mut trigger = manual_advance_trigger();
        trigger.payload["run_event_id"] = serde_json::json!("preserved-result");
        trigger.payload["run_result"] = serde_json::json!({
            "project":"p", "success":false, "landed":false, "summary":"source work preserved",
            "verdict":"defect", "diagnosis":"validator path diagnostics lost the filename",
            "preservation_ref":"foundry-task/p/preserved", "trunk_arrivals":[]
        });
        let result = block.execute(&trigger).await.unwrap();
        assert!(agent.invocations().is_empty());
        assert!(
            terminal_reason(&result, &EventType::CampaignEscalated).contains("budget exhausted")
        );
        let mut stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find_mut("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Escalated);
        let open = campaign.open_escalation.as_ref().expect("the budget stop is recorded");
        assert!(open.reason.contains("budget exhausted"), "{}", open.reason);
        assert!(open.question.is_none(), "a budget stop asks no question");
        assert_eq!(
            campaign.pending_run_result.as_ref().unwrap().preservation_ref.as_deref(),
            Some("foundry-task/p/preserved")
        );
        campaign.resume(1).unwrap();
        stored.save(&store_path).unwrap();
        let resumed = block.execute(&manual_advance_trigger()).await.unwrap();
        let execution = resumed
            .events
            .iter()
            .find(|e| e.event_type == EventType::ExecutionRequested)
            .unwrap();
        assert_eq!(execution.payload["base_ref"], "foundry-task/p/preserved");
        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert!(invocations[0].prompt.contains("validator path diagnostics lost the filename"));
        assert!(invocations[0].prompt.contains("source work preserved"));
        let stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find("c").unwrap();
        assert_eq!(campaign.cycles_completed, 2);
        assert!(campaign.pending_run_result.is_none());
    }
}
