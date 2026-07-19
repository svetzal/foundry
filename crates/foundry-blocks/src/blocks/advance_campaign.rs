use std::path::{Path, PathBuf};
use std::sync::Arc;

use foundry_sdk::campaign::{
    Campaign, CampaignStatus, CampaignStore, CampaignStoreGuard, DoneEvidence,
};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gates::GateDefinition;
use foundry_sdk::payload::{
    CampaignAdvanceCompletedPayload, CampaignAdvanceRequestedPayload, CampaignDecision,
    CampaignTerminalPayload, TaskVerdict,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::{AgentAccess, AgentGateway, ModelTier, ReasoningEffort, ShellGateway};

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
    format!(
        "You are advancing a durable engineering campaign. Inspect the live repository yourself; descriptive metadata is never current state. Decide exactly one of done, advance, or escalate.\n\n\
         CAMPAIGN: {}\nMISSION: {}\nINTENT REFS: {}\nCYCLES: {} completed / {} landed / {} max\nESCALATION RULES:\n- {}\n\n\
         OWNER DECISIONS (binding policy for this and future advances):\n{}\n\nREQUIRED REVIEW EVIDENCE:\n{}\n\nMECHANICAL DONE-GATE RESULTS:\n{}\n\nLAST TYPED RUN RESULT:\n{}\n\nLIVE REPO SNAPSHOT:\n{}\n\nCONTEXT ARTIFACTS (wording is binding and must be threaded into acceptance criteria):\n{}\n\n\
         DONE only when every required gate passes and every review statement is true against the repo itself. ADVANCE must cut exactly ONE objective from mission minus current state. Constraints, change licenses, scope guards, and forbidden moves are gates—not co-equal objectives. State concrete proof capable of rejecting masked IDs, count-only checks, or tests that bypass the real boundary. Do not prescribe implementation mechanism. Before removal/refactor packets, perform cheap live structural probes for callers and cross-module coupling and encode discoveries as scope guards. For migrations, name each licensed behavior change with its intent ref; all other characterized behavior is frozen. Build characterization before migration. ESCALATE on a human judgment, fired escalation rule, unusable provider, or invalidated campaign assumption.\n\n\
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
        last_result,
        snapshot,
        context,
    )
}

fn parse_decision(output: &str) -> anyhow::Result<CampaignDecision> {
    serde_json::from_str(&super::extract_json(output))
        .map_err(|e| anyhow::anyhow!("campaign agent returned no valid decision: {e}"))
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

fn update_run_and_forced_decision(
    campaign: &mut Campaign,
    request: &CampaignAdvanceRequestedPayload,
) -> Option<CampaignDecision> {
    if let Some(run_event_id) = &request.run_event_id
        && Some(run_event_id) != campaign.last_run_event_id.as_ref()
    {
        if let Some(run_result) = &request.run_result {
            if run_result.landed {
                campaign.cycles_landed += 1;
            }
            campaign.pending_run_result = Some(run_result.clone());
        }
        campaign.last_run_event_id = Some(run_event_id.clone());
    }

    if campaign.authorized_by.is_none() {
        return Some(CampaignDecision::Escalate {
            reason: "campaign has not been authorized by its owner".to_string(),
        });
    }
    if campaign.status == CampaignStatus::Completed {
        return Some(CampaignDecision::Done {
            reason: "campaign is already completed".to_string(),
        });
    }
    if campaign.status == CampaignStatus::Escalated {
        return Some(CampaignDecision::Escalate {
            reason: format!("campaign is {} and requires owner resumption", campaign.status),
        });
    }
    if let Some(result) = &request.run_result {
        match &result.verdict {
            TaskVerdict::BlockedOnDecision { finding, options } => {
                return Some(CampaignDecision::Escalate {
                    reason: format!("{finding}; options: {}", options.join(" | ")),
                });
            }
            TaskVerdict::RunnerError { detail } => {
                return Some(CampaignDecision::Escalate {
                    reason: format!("runner/provider unavailable: {detail}"),
                });
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

fn enforce_done_gate_truth(
    decision: CampaignDecision,
    gate_results: &[foundry_sdk::gates::GateResult],
) -> CampaignDecision {
    let required_pass = gate_results.iter().filter(|gate| gate.required).all(|gate| gate.passed);
    if !matches!(decision, CampaignDecision::Done { .. }) || required_pass {
        return decision;
    }
    let failed = gate_results
        .iter()
        .filter(|gate| gate.required && !gate.passed)
        .map(|gate| gate.command.clone())
        .collect::<Vec<_>>()
        .join(", ");
    CampaignDecision::Advance {
        objective: format!("Restore the required campaign done-evidence gates: {failed}"),
        reason: "review proposed done while required mechanical evidence was failing".to_string(),
    }
}

async fn derive_campaign_decision(
    agent: &dyn AgentGateway,
    shell: &dyn ShellGateway,
    entry: &foundry_sdk::registry::ProjectEntry,
    campaign: &Campaign,
    request: &CampaignAdvanceRequestedPayload,
) -> anyhow::Result<CampaignDecision> {
    let repo = Path::new(&entry.path);
    let gate_results = run_done_gates(shell, repo, &campaign.done_evidence).await?;
    let context = read_context_files(repo, &campaign.context_paths)?;
    let snapshot = repo_snapshot(shell, repo).await;
    let prompt = decision_prompt(campaign, &context, &snapshot, &gate_results, request);
    let outcome = invoke_agent(
        agent,
        AgentBlockSpec {
            prompt,
            working_dir: repo.to_path_buf(),
            access: AgentAccess::ReadOnly,
            tier: ModelTier::Deep,
            effort: ReasoningEffort::High,
            agent_file: super::resolve_agent_file(&entry.agent),
            provider: campaign
                .agent_provider
                .as_deref()
                .and_then(|provider| super::parse_agent_provider(Some(provider))),
            env: Vec::new(),
            timeout: entry.timeout(),
        },
        "campaign advance",
        &campaign.project,
    )
    .await;
    let decision = match outcome {
        crate::gateway::AgentOutcome::Success { stdout } => parse_decision(&stdout)?,
        crate::gateway::AgentOutcome::AgentFailed { stderr, .. } => {
            CampaignDecision::Escalate { reason: stderr }
        }
        crate::gateway::AgentOutcome::Unavailable { error } => {
            CampaignDecision::Escalate { reason: error }
        }
    };
    Ok(enforce_done_gate_truth(decision, &gate_results))
}

fn apply_campaign_decision(
    campaign: &mut Campaign,
    request: &CampaignAdvanceRequestedPayload,
    throttle: foundry_sdk::throttle::Throttle,
    decision: CampaignDecision,
) -> Vec<Event> {
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
            let base_ref =
                request.run_result.as_ref().and_then(|result| result.preservation_ref.clone());
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

async fn choose_campaign_decision(
    execution: &AdvanceExecution,
    campaign: &Campaign,
    forced: Option<CampaignDecision>,
) -> CampaignDecision {
    if let Some(decision) = forced {
        return decision;
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
            return CampaignDecision::Escalate {
                reason: format!("campaign formation failed: {error}"),
            };
        }
    };
    derive_campaign_decision(
        &*execution.agent,
        &*execution.shell,
        &entry,
        campaign,
        &execution.request,
    )
    .await
    .unwrap_or_else(|error| CampaignDecision::Escalate {
        reason: format!("campaign formation failed: {error}"),
    })
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
        let events = apply_campaign_decision(
            campaign,
            &execution.request,
            execution.throttle,
            CampaignDecision::Escalate {
                reason: error.to_string(),
            },
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

    let forced = update_run_and_forced_decision(campaign, &execution.request);
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

    let mut effective_request = execution.request.clone();
    if effective_request.run_result.is_none() {
        effective_request.run_result.clone_from(&campaign.pending_run_result);
        effective_request.run_event_id.clone_from(&campaign.last_run_event_id);
    }
    let effective_execution = AdvanceExecution {
        request: effective_request,
        agent: Arc::clone(&execution.agent),
        shell: Arc::clone(&execution.shell),
        registry: Arc::clone(&execution.registry),
        store_path: execution.store_path.clone(),
        lock: Arc::clone(&execution.lock),
        project: execution.project.clone(),
        throttle: execution.throttle,
    };
    let decision = choose_campaign_decision(&effective_execution, campaign, forced).await;
    let decision = enforce_campaign_budget(campaign, decision);
    let events = apply_campaign_decision(
        campaign,
        &effective_execution.request,
        execution.throttle,
        decision,
    );
    let cycles_completed = campaign.cycles_completed;
    let cycles_landed = campaign.cycles_landed;
    persist_or_terminal(
        &execution,
        &guard,
        events,
        format!("campaign '{}' advanced", execution.request.campaign),
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
        Campaign, CampaignBudget, CampaignStatus, CampaignStore, DoneEvidence, OwnerDecision,
    };
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::payload::{
        CampaignAdvanceRequestedPayload, CampaignDecision, LoopContext, TaskRunCompletedPayload,
        TaskVerdict,
    };
    use foundry_sdk::task_block::TaskBlock;
    use foundry_sdk::throttle::Throttle;

    use crate::gateway::fakes::{FakeAgentGateway, FakeShellGateway};

    use super::{AdvanceCampaign, enforce_campaign_budget, parse_decision, run_done_gates};

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
                status: CampaignStatus::Paused,
                cycles_completed: 1,
                cycles_landed: 0,
                authorized_by: Some("owner".to_string()),
                agent_provider: None,
                last_run_event_id: None,
                owner_decisions: vec![],
                pending_run_result: None,
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
}
