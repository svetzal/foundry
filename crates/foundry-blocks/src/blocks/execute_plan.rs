use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gateway::AgentFailureMetadata;
use foundry_sdk::payload::{
    ExecutionCompletedPayload, LandBlocked, LoopContext, PlanCompletedPayload,
    TaskRunCompletedPayload, TaskVerdict,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::workflow::WorkflowType;

use crate::gateway::{AgentGateway, ProcessShellGateway, ShellGateway};

use super::{ExecutionContext, SimulatedSuccess, TriggerContext};

/// Every task execution, standalone or campaign cycle, gets this. Task
/// finalization commits nothing under `.foundry/` (see
/// `crate::workspace::FOUNDRY_DIR`), so the executor must know where durable
/// work belongs.
const TASK_SCRATCH_PROMPT: &str = "

FOUNDRY SCRATCH: files under .foundry/ are Foundry scratch for execution evidence (logs, screenshots, harness output, proof). They never land in the repository; Foundry archives them to its evidence store. Put anything the project must keep in a normal project path, for example docs/ or the test tree, and do not make repository files reference .foundry/ paths as if they were committed.
";

/// Every task execution gets this: Foundry, not the agent, owns what happens
/// to the work after it lands, including the branch's CI.
const TRUNK_CI_PROMPT: &str = "
TRUNK CI: Foundry lands your work and then watches the branch's CI itself, so do not push, open pull requests, or poll or wait on CI.
";

const EARLY_PROOF_PROMPT: &str = r#"

PROOF FIRST: before expanding fixtures, documentation, or running the full quality suite, exercise the hardest acceptance behavior through the project's public API, with its I/O boundary stubbed by an in-process test double as TEST DESIGN describes. Observe its rejecting case and corrected passing case. A marker toggle or count-only test is insufficient.
Record .foundry/proof.json with this exact behavioral shape:
{
  "kind": "behavioral",
  "source_change": "src/core.rs: describe the source/input change",
  "rejecting": {"command": "test command", "exit_code": 1, "log": ".foundry/logs/rejecting.log"},
  "corrected": {"command": "test command", "exit_code": 0, "log": ".foundry/logs/corrected.log"}
}
source_change is a single non-empty string. rejecting and corrected are top-level objects. Each log must exist; rejecting must be nonzero and corrected zero. Record actual probe exit codes.
For a non-behavioral objective, use this exact direct shape with the smallest passing acceptance probe:
{
  "kind": "direct",
  "reason": "Explain why this objective is non-behavioral",
  "corrected": {"command": "acceptance command", "exit_code": 0, "log": ".foundry/logs/corrected.log"}
}
Validate the file's JSON, field types, actual exit codes, and existence of each log before finishing. The reviewer must inspect this evidence.
BLOCKED: if you cannot reach the acceptance boundary because a capability, access or owner decision is missing (for example, the objective needs an operating system, service or credential that no available host provides), do not fabricate evidence and do not write .foundry/proof.json. Record .foundry/blocker.json with this exact shape instead, then stop:
{
  "kind": "blocked",
  "tried": "what was attempted",
  "missing": "the capability, access or decision that is absent",
  "question": "the one question for the owner",
  "options": ["...", "..."]
}
For an operational prerequisite, add "recovery_kind" with one of "capability_unavailable", "access_unavailable", "environment_unavailable", "missing_evidence", "unmet_precondition" or "runner_unavailable". Foundry records a recovery and rechecks after recovery evidence; this does not request owner judgement. Omit recovery_kind only for a real choice that needs owner authority.
An internal seam that TEST DESIGN pre-authorizes is never a missing owner decision: add it and continue, do not write a blocker for it.
tried and missing are non-empty strings. For typed operational recovery, question and options may be omitted. Otherwise question is required and non-empty; options is optional and holds non-empty concrete owner choices. Only an untyped owner-decision blocker asks the owner a question and waits for their answer.
WRITABLE REPOSITORY: this isolated worktree only; do not edit sibling checkouts, dispatch nested Foundry work, commit, or push.
"#;

agent_execution_block! {
    /// Applies the correction plan to the project.
    ///
    /// Mutator — sinks on `PlanCompleted`.
    /// Uses `AgentGateway` with `Coding` capability and `Full` access.
    /// Emits `ExecutionCompleted` with success status and `changes_detected` flag.
    pub struct ExecutePlan with sleeper
}

/// Outcome of an execute-plan dry-run simulation.
pub(crate) enum ExecutePlanOutcome {
    /// Plan decided no correction is needed — emit no-correction `ExecutionCompleted`.
    NoCorrection,
    /// Plan requires execution — emit standard dry-run `ExecutionCompleted`.
    Executed,
}

impl SimulatedSuccess for ExecutePlan {
    type Outcome = ExecutePlanOutcome;

    fn simulate(&self, trigger: &Event) -> ExecutePlanOutcome {
        let correction_needed = trigger
            .parse_payload::<PlanCompletedPayload>()
            .map_or(true, |p| p.correction_needed);
        if correction_needed {
            ExecutePlanOutcome::Executed
        } else {
            ExecutePlanOutcome::NoCorrection
        }
    }

    fn success_events(&self, trigger: &Event, outcome: &ExecutePlanOutcome) -> Vec<Event> {
        let workflow = foundry_sdk::workflow::WorkflowType::from_payload(&trigger.payload);
        match outcome {
            ExecutePlanOutcome::Executed => super::dry_run_execution_event(trigger, workflow, None),
            ExecutePlanOutcome::NoCorrection => {
                let workflow_str = workflow.to_string();
                let context = LoopContext::extract_from(&trigger.payload);
                vec![super::execution_completed_event(
                    &trigger.project,
                    trigger.throttle,
                    &ExecutionCompletedPayload {
                        project: trigger.project.clone(),
                        workflow: workflow_str,
                        success: true,
                        summary: String::new(),
                        execution_output: None,
                        dry_run: Some(true),
                        retry_count: None,
                        changes_detected: Some(false),
                        files_changed: vec![],
                        failure: AgentFailureMetadata::default(),
                        session_failure: None,
                        context,
                    },
                )]
            }
        }
    }
}

impl TaskBlock for ExecutePlan {
    task_block_meta! {
        name: "Execute Plan",
        kind: Mutator,
        sinks_on: [PlanCompleted],
    }

    dry_run_via_simulation!();

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let TriggerContext {
            project,
            throttle,
            payload,
            trace_id,
        } = TriggerContext::from_trigger(trigger);

        let entry = require_project!(self, project);
        let agent = Arc::clone(&self.agent);
        let shell = Arc::clone(&self.shell);
        let sleeper = Arc::clone(&self.sleeper);

        let plan_payload = parse_payload!(trigger, PlanCompletedPayload);

        if !plan_payload.correction_needed {
            let result = build_no_correction_result(
                &project,
                trigger,
                &payload,
                &plan_payload.correction_reason,
            );
            return Box::pin(async move { result });
        }

        // Read the workflow from the trigger payload so that non-iterate callers
        // (e.g. the prompt workflow) receive the correct WorkflowType.  This is
        // important because the silent no-op override only fires for WorkflowType::Iterate.
        let workflow = WorkflowType::from_payload(&payload);
        let run_id = trigger.id.clone();

        Box::pin(async move {
            let mut execution_payload = payload;
            let project_path = if workflow == WorkflowType::Task {
                let base_ref =
                    execution_payload.get("base_ref").and_then(serde_json::Value::as_str);
                // A campaign cycle's workspace is named from the campaign and
                // cycle it serves, not from this run's event id. Cancellation
                // knows the campaign but cannot know the run — nothing in the
                // event stream names the worktree until the agent has already
                // finished — so this is what lets a killed cycle's orphaned
                // worktree be found and disposed of afterwards.
                let workspace_id = match (
                    execution_payload.get("campaign").and_then(serde_json::Value::as_str),
                    execution_payload.get("campaign_cycle").and_then(serde_json::Value::as_u64),
                ) {
                    (Some(campaign), Some(cycle)) => {
                        crate::workspace::campaign_workspace_id(campaign, cycle, &run_id)
                    }
                    _ => crate::workspace::run_workspace_id(&run_id),
                };
                let prepared = prepare_with_reconciliation(
                    &*shell,
                    &entry,
                    &workspace_id,
                    base_ref.map(str::to_owned),
                    &mut execution_payload,
                )
                .await;
                match prepared {
                    Ok(workspace) => {
                        execution_payload["task_worktree"] =
                            serde_json::json!(workspace.path.to_string_lossy());
                        execution_payload["task_branch"] = serde_json::json!(workspace.branch);
                        execution_payload["workspace_base"] = serde_json::json!(workspace.base);
                        workspace.path
                    }
                    Err(error) => {
                        return workspace_failure_result(
                            &project,
                            throttle,
                            &execution_payload,
                            &error,
                        );
                    }
                }
            } else {
                std::path::PathBuf::from(&entry.path)
            };

            let mut execution_entry = entry.clone();
            execution_entry.path = project_path.to_string_lossy().to_string();
            let plan = &plan_payload.plan;
            let principle = &plan_payload.principle;
            let gates = plan_payload.chain.gates.as_ref();
            let mut prompt = build_execution_prompt(&project, plan, principle, gates);
            prompt.push_str(&task_prompt_suffix(
                workflow,
                execution_payload.get("campaign").is_some()
                    || execution_payload.get("reconciliation").is_some(),
            ));
            append_reconciliation_prompt(&mut prompt, &execution_payload);
            let ctx = ExecutionContext {
                trace_id: trace_id.clone(),
                project: &project,
                workflow,
                payload: &execution_payload,
                throttle,
                label: "plan execution",
                retry_count: None,
                correction_needed: plan_payload.correction_needed,
            };

            Ok(super::execute_agent_block(
                &*agent,
                &*shell,
                &*sleeper,
                &execution_entry,
                &ctx,
                prompt,
            )
            .await)
        })
    }
}

async fn prepare_with_reconciliation(
    shell: &dyn ShellGateway,
    entry: &foundry_sdk::registry::ProjectEntry,
    workspace_id: &str,
    base_ref: Option<String>,
    payload: &mut serde_json::Value,
) -> anyhow::Result<super::task_workspace::TaskWorkspace> {
    match super::task_workspace::prepare_task_workspace(
        shell,
        entry,
        workspace_id,
        base_ref.as_deref(),
    )
    .await
    {
        Err(error) if payload.get("reconciliation").is_none() => {
            let Some(conflict) =
                error.downcast_ref::<super::task_workspace::ContinuationConflict>()
            else {
                return Err(error);
            };
            payload["reconciliation"] = serde_json::json!(foundry_sdk::payload::Reconciliation {
                source_ref: conflict.base_ref.clone(),
                preserved: conflict.preserved.clone(),
                trunk: conflict.trunk.clone(),
                files: conflict.files.clone(),
            });
            super::task_workspace::prepare_task_workspace(shell, entry, workspace_id, None).await
        }
        other => other,
    }
}

fn append_reconciliation_prompt(prompt: &mut String, payload: &serde_json::Value) {
    use std::fmt::Write as _;
    if let Some(reconciliation) = payload.get("reconciliation") {
        // Writing to String is infallible.
        let _ = write!(
            prompt,
            "\nBOUNDED RECONCILIATION: one attempt in this task, starting on current trunk. Immutable source evidence: {reconciliation}\nRead the original objective and review findings above. Inspect the preserved commit with git show and compare its changes with current trunk. Reapply the remaining intended behavior in this worktree, retaining trunk changes and omitting source changes already satisfied by trunk. Explain each adaptation or omission in your result. Do not checkout, reset to, rebase, rewrite or delete the source ref; do not edit another worktree. Do not commit or push. Run acceptance proof and normal gates. If a product choice is needed, write .foundry/blocker.json with the exact question. If reconciliation cannot be completed, retain useful work and report the obstacle; do not dispatch another attempt.\n"
        );
    }
}

/// The terminal result for a task whose workspace could not be prepared.
///
/// A refused continuation is typed so the ledger and the campaign can tell it
/// from a runner fault: no agent ran, and the preserved work is still at its
/// ref.
fn workspace_failure_result(
    project: &str,
    throttle: foundry_sdk::throttle::Throttle,
    payload: &serde_json::Value,
    error: &anyhow::Error,
) -> anyhow::Result<TaskBlockResult> {
    let land_blocked = error
        .downcast_ref::<super::task_workspace::ContinuationConflict>()
        .map(|_| LandBlocked::ContinuationConflict);
    let headline = if land_blocked.is_some() {
        "continuation refused before its agent session"
    } else {
        "task workspace preparation failed"
    };
    super::emit_event_result(
        format!("{project}: {headline}"),
        false,
        EventType::TaskRunCompleted,
        project,
        throttle,
        &TaskRunCompletedPayload {
            project: project.to_string(),
            success: false,
            landed: false,
            summary: error.to_string(),
            preservation_ref: None,
            land_blocked,
            proof_evidence: None,
            uncommitted_artifacts: None,
            trunk_arrivals: Vec::new(),
            verdict: TaskVerdict::RunnerError {
                detail: error.to_string(),
            },
            context: LoopContext::extract_from(payload),
            failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
        },
    )
}

fn build_no_correction_result(
    project: &str,
    trigger: &Event,
    payload: &serde_json::Value,
    reason: &str,
) -> anyhow::Result<TaskBlockResult> {
    let summary = "no correction needed — codebase satisfies assessed principle".to_string();
    let context = LoopContext::extract_from(payload);
    let workflow = WorkflowType::from_payload(payload);
    let exec_payload = ExecutionCompletedPayload {
        project: project.to_string(),
        workflow: workflow.to_string(),
        success: true,
        summary: summary.clone(),
        execution_output: if reason.is_empty() {
            None
        } else {
            Some(reason.to_string())
        },
        dry_run: None,
        retry_count: None,
        changes_detected: Some(false),
        files_changed: vec![],
        failure: AgentFailureMetadata::default(),
        session_failure: None,
        context,
    };
    tracing::info!(
        project = %project,
        "iterate plan concluded no correction needed; short-circuiting before agent invocation"
    );
    let event = trigger.with_payload(EventType::ExecutionCompleted, &exec_payload)?;
    Ok(TaskBlockResult {
        events: vec![event],
        success: true,
        summary: format!("{project}: {summary}"),
        ..Default::default()
    })
}

/// What a task adds to the plan prompt: the `.foundry` scratch rule, the
/// trunk CI rule and the shared test-design rule for every task, then early proof for a campaign
/// cycle. Iterate, maintain and prompt runs commit through other paths and get
/// none of them.
fn task_prompt_suffix(workflow: WorkflowType, campaign: bool) -> String {
    let mut suffix = String::new();
    if workflow == WorkflowType::Task {
        suffix.push_str(TASK_SCRATCH_PROMPT);
        suffix.push_str(TRUNK_CI_PROMPT);
        suffix.push('\n');
        suffix.push_str(super::agent_helpers::TEST_DESIGN_GUIDANCE);
    }
    if campaign {
        suffix.push_str(EARLY_PROOF_PROMPT);
    }
    suffix
}

fn build_execution_prompt(
    project: &str,
    plan: &str,
    principle: &str,
    gates: Option<&serde_json::Value>,
) -> String {
    let gates_context = super::format_gates_context(gates);
    format!(
        "You are executing a correction plan for project '{project}'.\n\n\
         Principle being addressed: {principle}\n\n\
         Plan:\n{plan}\n\n\
         REQUIREMENTS:\n\
         - The plan above describes mutations that MUST be applied to the source files. Apply them now.\n\
         - Do NOT skip the plan because quality gates currently pass. Passing gates is necessary, not sufficient — the purpose of this run is to apply the plan, not to re-verify a clean tree.\n\
         - After applying the plan, the working tree MUST contain modifications to the files named or implied by the plan. If `git status --porcelain` would be empty when you finish, you have not done the job and the run has failed.\n\
         {git_rule}\
         - Make only the changes the plan describes; do not expand scope.\n\n\
         {rules}\n\
         - Once the plan is applied, the following quality gates must still pass:{gates_context}",
        rules = super::suppression_guard::ADVISORY_RULES,
        git_rule = super::push_guard::FOUNDRY_OWNS_GIT_RULE,
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
    use super::ExecutePlan;

    assert_block_meta!(
        ExecutePlan::new(
            FakeAgentGateway::success(),
            Arc::new(RwLock::new(Registry { version: 2, projects: vec![] })),
        ),
        kind: Mutator,
        sinks_on: [PlanCompleted],
    );

    #[tokio::test]
    async fn executes_plan_successfully() {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Changes applied successfully");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        // Simulate real file changes so the iterate override does not fire
        let shell = FakeShellGateway::always(CommandResult {
            stdout: "M  src/lib.rs\n".to_string(),
            stderr: String::new(),
            exit_code: 0,
            success: true,
        });
        let block = ExecutePlan::with_gateways(agent.clone(), registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(result.events[0].payload["workflow"], "iterate");
        assert_eq!(result.events[0].payload["success"], true);

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].access, AgentAccess::Full);
        assert_eq!(invocations[0].tier, ModelTier::Balanced);
        assert_eq!(invocations[0].effort, ReasoningEffort::Medium);
        assert!(invocations[0].prompt.contains("DRY"));
    }

    #[tokio::test]
    async fn agent_failure_emits_execution_completed_with_failure() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::failure("compilation error");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ExecutePlan::new(agent, registry);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });
        test_helpers::assert_agent_failure_emits_failure(&block, &trigger).await;
    }

    #[tokio::test]
    async fn forwards_actions_from_payload() {
        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let block = ExecutePlan::new(agent, registry);
        let trigger = Event::new(
            EventType::PlanCompleted,
            "my-project".to_string(),
            Throttle::Full,
            serde_json::json!({
                "project": "my-project",
                "plan": "1. Do things",
                "principle": "SRP",
                "workflow": "iterate",
                "actions": {"maintain": true},
            }),
        );
        test_helpers::assert_forwards_actions(&block, &trigger).await;
    }

    #[test]
    fn dry_run_emits_simulated_success() {
        let agent = FakeAgentGateway::success();
        let block = ExecutePlan::new(
            agent,
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });

        let events = block.dry_run_events(&trigger);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(events[0].payload["dry_run"], true);
        assert_eq!(events[0].payload["success"], true);
        assert_eq!(events[0].payload["workflow"], "iterate");
    }

    #[test]
    fn dry_run_matches_execute_for_no_correction_plan() {
        let agent = FakeAgentGateway::success();
        let block = ExecutePlan::new(
            agent,
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "No changes needed.",
            "principle": "DRY",
            "workflow": "iterate",
            "correction_needed": false,
        });

        let events = block.dry_run_events(&trigger);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(events[0].payload["dry_run"], true);
        assert_eq!(events[0].payload["success"], true);
        assert_eq!(
            events[0].payload["changes_detected"], false,
            "no-correction path must set changes_detected=false"
        );
        assert_eq!(events[0].payload["workflow"], "iterate");
    }

    /// A conflict starts one adaptation session on trunk. Finalization lands
    /// the adapted behavior without rewriting the original preserved branch.
    #[tokio::test]
    async fn a_conflicting_continuation_gets_one_session_on_trunk_and_retains_source() {
        use crate::blocks::test_helpers::git_repo::{CleanProcessShellGateway, commit, git, repo};

        let fixture = repo();
        git(&fixture.work, &["checkout", "-q", "-b", "foundry-task/refused"]);
        commit(&fixture.work, "README.md", "preserved edit", "preserved slice");
        git(&fixture.work, &["checkout", "-q", "main"]);
        commit(&fixture.work, "README.md", "trunk edit", "trunk moved on");
        git(&fixture.work, &["push", "-q", "origin", "main"]);

        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_project(
            "execute-plan-refusal",
            fixture.work.to_str().unwrap(),
        );
        let block =
            ExecutePlan::with_gateways(agent.clone(), registry, Arc::new(CleanProcessShellGateway));
        let trigger = test_event!(EventType::PlanCompleted, "execute-plan-refusal", {
            "project": "execute-plan-refusal",
            "plan": "continue the preserved work",
            "principle": "",
            "category": "",
            "workflow": "task",
            "correction_needed": true,
            "base_ref": "foundry-task/refused",
        });

        let result = block.execute(&trigger).await.unwrap();

        let invocations = agent.invocations();
        assert_eq!(invocations.len(), 1);
        assert!(invocations[0].prompt.contains("BOUNDED RECONCILIATION"));
        assert!(invocations[0].prompt.contains("foundry-task/refused"));
        let payload = &result.events[0].payload;
        assert_eq!(result.events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(payload["reconciliation"]["source_ref"], "foundry-task/refused");
        assert_eq!(payload["reconciliation"]["files"], serde_json::json!(["README.md"]));
        let path = std::path::Path::new(payload["task_worktree"].as_str().unwrap());
        let trunk = git(&fixture.work, &["rev-parse", "main"]);
        assert_eq!(git(path, &["rev-parse", "HEAD"]), trunk);
        assert_eq!(std::fs::read_to_string(path.join("README.md")).unwrap(), "trunk edit");
        let preserved = git(&fixture.work, &["show", "foundry-task/refused:README.md"]);
        assert_eq!(preserved, "preserved edit");
        assert_eq!(git(&fixture.work, &["status", "--porcelain"]), "");
        // Model the coding gateway's adaptation, then exercise the real finalizer.
        std::fs::write(path.join("README.md"), "trunk edit\npreserved behavior adapted\n").unwrap();
        let review = Event::new(
            EventType::TaskReviewed,
            "execute-plan-refusal".into(),
            foundry_sdk::throttle::Throttle::Full,
            Event::serialize_payload(&foundry_sdk::payload::TaskReviewedPayload {
                project: "execute-plan-refusal".into(),
                objective: "continue the preserved work".into(),
                review: "adapted preserved behavior while retaining trunk".into(),
                gate_results: vec![],
                verdict: foundry_sdk::payload::TaskVerdict::Complete,
                context: foundry_sdk::payload::LoopContext::extract_from(payload),
                failure: foundry_sdk::gateway::AgentFailureMetadata::default(),
            })
            .unwrap(),
        );
        let finalizer = super::super::FinalizeTask::with_gateways(
            test_helpers::registry_with_project(
                "execute-plan-refusal",
                fixture.work.to_str().unwrap(),
            ),
            Arc::new(CleanProcessShellGateway),
        );
        let landed = finalizer.execute(&review).await.unwrap();
        assert_eq!(landed.events[0].payload["landed"], true);
        assert_eq!(landed.events[0].payload["reconciliation"], payload["reconciliation"]);
        assert_eq!(
            git(&fixture.work, &["show", "main:README.md"]),
            "trunk edit\npreserved behavior adapted"
        );
        assert_eq!(
            git(&fixture.work, &["show", "foundry-task/refused:README.md"]),
            "preserved edit"
        );
        assert!(!path.exists());
    }

    /// The start preparation recorded travels with the run, so finalize can
    /// tell a trunk that moved during the run from a stale start.
    #[tokio::test]
    async fn a_task_execution_carries_where_its_workspace_started() {
        use crate::blocks::test_helpers::git_repo::{CleanProcessShellGateway, git, repo};

        let fixture = repo();
        let trunk = git(&fixture.work, &["rev-parse", "HEAD"]);
        let agent = FakeAgentGateway::success();
        let registry = test_helpers::registry_with_project(
            "execute-plan-start",
            fixture.work.to_str().unwrap(),
        );
        let block =
            ExecutePlan::with_gateways(agent.clone(), registry, Arc::new(CleanProcessShellGateway));
        let trigger = test_event!(EventType::PlanCompleted, "execute-plan-start", {
            "project": "execute-plan-start",
            "plan": "make a change",
            "principle": "",
            "category": "",
            "workflow": "task",
            "correction_needed": true,
        });

        let result = block.execute(&trigger).await.unwrap();

        assert_eq!(agent.invocations().len(), 1);
        let payload = &result.events[0].payload;
        assert_eq!(payload["workspace_base"]["trunk"], trunk);
        assert_eq!(payload["workspace_base"]["commit"], trunk);
        let worktree = std::path::PathBuf::from(payload["task_worktree"].as_str().unwrap());
        // Test fixture owns the worktree and branch it created.
        git(&fixture.work, &["worktree", "remove", "--force", worktree.to_str().unwrap()]);
        git(&fixture.work, &["branch", "-D", payload["task_branch"].as_str().unwrap()]);
        let _ = std::fs::remove_dir(worktree.parent().unwrap());
    }

    #[tokio::test]
    async fn project_not_in_registry_returns_failure() {
        let block = ExecutePlan::new(
            FakeAgentGateway::success(),
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
        );
        test_helpers::assert_missing_project_fails(&block, EventType::PlanCompleted).await;
    }

    #[tokio::test]
    async fn detects_changes_when_tree_dirty() {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Changes applied");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        // Shell sequence:
        // 1. `git rev-parse HEAD` (pre-execution sha capture) → returns sha
        // 2. `git diff --name-only <sha>` (post-execution detection) → returns files
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: "abc123\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: "src/lib.rs\nnew.txt\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = ExecutePlan::with_gateways(agent, registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });
        test_helpers::assert_detects_changes_when_dirty(
            &block,
            &trigger,
            &["src/lib.rs", "new.txt"],
        )
        .await;
    }

    #[tokio::test]
    async fn reports_no_changes_when_tree_clean() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success(); // empty stdout
        let block = ExecutePlan::with_gateways(agent, registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });
        // Iterate + clean tree → overridden to failure (silent no-op)
        test_helpers::assert_reports_no_changes_when_clean(&block, &trigger, false).await;
    }

    #[tokio::test]
    async fn iterate_clean_tree_overrides_to_failure() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success(); // empty stdout → no changes
        let block = ExecutePlan::with_gateways(agent, registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
            "correction_needed": true,
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success, "expected failure for iterate + clean tree");
        assert!(
            result.summary.contains("silent no-op"),
            "expected 'silent no-op' in summary, got: {}",
            result.summary
        );
    }

    #[tokio::test]
    async fn iterate_clean_tree_no_correction_needed_succeeds() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("THIS SHOULD NEVER RUN");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success();
        let block = ExecutePlan::with_gateways(agent.clone(), registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "No changes needed.",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
            "correction_needed": false,
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(
            result.success,
            "expected success for legitimate no-op (correction_needed=false)"
        );
        assert!(
            result.summary.contains("no correction needed"),
            "expected 'no correction needed' in summary, got: {}",
            result.summary
        );
        assert_eq!(result.events[0].payload["success"], true);
        assert!(
            agent.invocations().is_empty(),
            "agent must not be invoked when correction_needed=false"
        );
    }

    #[tokio::test]
    async fn iterate_no_correction_needed_emits_reason_in_execution_output() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("THIS SHOULD NEVER RUN");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::success();
        let block = ExecutePlan::with_gateways(agent.clone(), registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "No changes needed.",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
            "correction_needed": false,
            "correction_reason": "Codebase already extracts shared validation into a helper.",
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(agent.invocations().is_empty());
        assert_eq!(result.events.len(), 1);
        assert_eq!(result.events[0].event_type, EventType::ExecutionCompleted);
        assert_eq!(
            result.events[0].payload["execution_output"],
            "Codebase already extracts shared validation into a helper."
        );
        assert_eq!(result.events[0].payload["changes_detected"], false);
        assert_eq!(result.events[0].payload["workflow"], "iterate");
    }

    #[tokio::test]
    async fn iterate_aux_only_changes_treated_as_clean() {
        use crate::gateway::fakes::FakeShellGateway;
        use crate::shell::CommandResult;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success();
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        // Shell sequence:
        // 1. `git rev-parse HEAD` → sha
        // 2. `git diff --name-only <sha>` → only a .claude/worktrees/ path
        let shell = FakeShellGateway::sequence(vec![
            CommandResult {
                stdout: "abc123\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            CommandResult {
                stdout: ".claude/worktrees/abc/session.jsonl\n".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let block = ExecutePlan::with_gateways(agent, registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });

        let result = block.execute(&trigger).await.unwrap();

        assert!(!result.success, "all-auxiliary files must be treated as a no-op");
        assert!(result.summary.contains("silent no-op"));
    }

    #[tokio::test]
    async fn tolerates_git_status_failure() {
        use crate::gateway::fakes::FakeShellGateway;

        let dir = tempfile::tempdir().unwrap();
        let agent = FakeAgentGateway::success_with("Changes applied");
        let registry =
            test_helpers::registry_with_project("my-project", dir.path().to_str().unwrap());
        let shell = FakeShellGateway::failure("fatal: not a git repository");
        let block = ExecutePlan::with_gateways(agent, registry, shell);
        let trigger = test_event!(EventType::PlanCompleted, "my-project", {
            "project": "my-project",
            "plan": "1. Extract helper\n2. Update callers",
            "principle": "DRY",
            "category": "duplication",
            "workflow": "iterate",
        });
        // Git status failure → changes_detected=false → iterate override fires
        test_helpers::assert_tolerates_git_failure(&block, &trigger, false).await;
    }

    #[test]
    fn prompt_contains_imperative_requirements() {
        let gates = serde_json::json!({"cargo test": "pass"});
        let prompt = super::build_execution_prompt(
            "my-project",
            "1. Extract helper\n2. Update callers",
            "DRY",
            Some(&gates),
        );

        // Imperative: plan application is mandatory
        assert!(
            prompt.contains("MUST be applied"),
            "expected imperative phrase 'MUST be applied' in:\n{prompt}"
        );
        // Invalidation of "gates already pass" as a stopping condition
        assert!(
            prompt.contains("necessary, not sufficient"),
            "expected invalidation phrase 'necessary, not sufficient' in:\n{prompt}"
        );
        // Empty working tree means failure
        assert!(
            prompt.contains("git status --porcelain"),
            "expected 'git status --porcelain' in:\n{prompt}"
        );
        assert!(
            prompt.contains("empty"),
            "expected 'empty' (empty-tree-means-failure) in:\n{prompt}"
        );
        assert!(
            prompt.contains("Do NOT commit, push, merge, rebase, tag, or modify refs"),
            "expected Foundry-owned Git finalization requirement in:\n{prompt}"
        );
        assert!(
            prompt.contains("Repository guidance") && prompt.contains("does not apply"),
            "expected repository-guidance override in:\n{prompt}"
        );
        // Core content: principle and plan still present
        assert!(prompt.contains("DRY"), "expected principle 'DRY' in:\n{prompt}");
        assert!(prompt.contains("1. Extract helper"), "expected plan content in:\n{prompt}");
        // Gates section present when gates are provided
        assert!(
            prompt.contains("quality gates"),
            "expected gates section when gates are provided, in:\n{prompt}"
        );
    }

    #[test]
    fn prompt_without_gates_still_contains_requirements() {
        let prompt = super::build_execution_prompt("other-project", "1. Do the thing", "SRP", None);

        assert!(prompt.contains("MUST be applied"), "expected 'MUST be applied' in:\n{prompt}");
        assert!(
            prompt.contains("necessary, not sufficient"),
            "expected 'necessary, not sufficient' in:\n{prompt}"
        );
        assert!(
            prompt.contains("git status --porcelain"),
            "expected 'git status --porcelain' in:\n{prompt}"
        );
        assert!(prompt.contains("SRP"), "expected principle 'SRP' in:\n{prompt}");
        assert!(prompt.contains("1. Do the thing"), "expected plan content in:\n{prompt}");
    }

    /// Task finalization commits nothing under `.foundry/`, so every task,
    /// standalone or campaign, is told so; other workflows are not.
    #[test]
    fn every_task_prompt_says_dot_foundry_is_scratch_and_only_campaigns_require_proof() {
        use foundry_sdk::workflow::WorkflowType;

        let standalone = super::task_prompt_suffix(WorkflowType::Task, false);
        assert!(standalone.contains("files under .foundry/ are Foundry scratch"), "{standalone}");
        assert!(standalone.contains("never land in the repository"), "{standalone}");
        assert!(standalone.contains("docs/ or the test tree"), "{standalone}");
        assert!(!standalone.contains("PROOF FIRST"), "{standalone}");
        assert!(
            standalone.contains("watches the branch's CI itself, so do not push, open pull requests, or poll or wait on CI"),
            "{standalone}"
        );

        let campaign = super::task_prompt_suffix(WorkflowType::Task, true);
        assert!(campaign.contains("TRUNK CI:"), "{campaign}");
        assert!(campaign.contains("files under .foundry/ are Foundry scratch"), "{campaign}");
        assert!(campaign.contains("PROOF FIRST"), "{campaign}");
        // A campaign executor that cannot reach the boundary has a recorded
        // way to say so instead of leaving proof absent.
        assert!(campaign.contains("Record .foundry/blocker.json"), "{campaign}");
        assert!(campaign.contains(r#""kind": "blocked""#), "{campaign}");
        assert!(campaign.contains("do not fabricate evidence"), "{campaign}");
        assert!(!standalone.contains("blocker.json"), "{standalone}");
        assert!(
            !campaign.contains("committed with your change"),
            "no prompt may say .foundry files land: {campaign}"
        );

        for workflow in [
            WorkflowType::Iterate,
            WorkflowType::Maintain,
            WorkflowType::Prompt,
        ] {
            assert!(super::task_prompt_suffix(workflow, false).is_empty(), "{workflow}");
        }
    }

    /// "Through the real boundary" was read as "through a real network", and
    /// campaign executors built test suites on local HTTP servers. Every task
    /// executor gets the shared test-design rule, and early proof names the
    /// project's public API with a stubbed I/O boundary. The proof and blocker
    /// shapes are unchanged.
    #[test]
    fn every_task_executor_gets_the_test_design_rule_and_proof_names_the_public_api() {
        use foundry_sdk::workflow::WorkflowType;

        let guidance = super::super::agent_helpers::TEST_DESIGN_GUIDANCE;
        for campaign in [false, true] {
            let suffix = super::task_prompt_suffix(WorkflowType::Task, campaign);
            assert!(suffix.contains(guidance), "{suffix}");
            assert!(!suffix.contains("through the real boundary"), "{suffix}");
        }

        let campaign = super::task_prompt_suffix(WorkflowType::Task, true);
        assert!(
            campaign.contains(
                "exercise the hardest acceptance behavior through the project's public API, with its I/O boundary stubbed by an in-process test double"
            ),
            "{campaign}"
        );
        assert!(campaign.contains(r#""kind": "behavioral""#), "{campaign}");
        assert!(campaign.contains(r#""kind": "direct""#), "{campaign}");
        assert!(campaign.contains(r#""kind": "blocked""#), "{campaign}");
        assert!(
            campaign.find(guidance) < campaign.find("PROOF FIRST"),
            "early proof refers to TEST DESIGN, so the rule must come first: {campaign}"
        );
    }

    /// Executors stopped to ask whether they may add a clock or transport
    /// seam. Every task executor carries the pre-authorization, and a campaign
    /// executor's blocker rule says a seam is never a missing owner decision.
    #[test]
    fn every_task_executor_carries_the_seam_pre_authorization() {
        use foundry_sdk::workflow::WorkflowType;

        for campaign in [false, true] {
            let suffix = super::task_prompt_suffix(WorkflowType::Task, campaign);
            for phrase in super::super::agent_helpers::SEAM_AUTHORIZATION_PHRASES {
                assert!(suffix.contains(phrase), "missing {phrase}: {suffix}");
            }
        }
        let campaign = super::task_prompt_suffix(WorkflowType::Task, true);
        assert!(
            campaign.contains(
                "An internal seam that TEST DESIGN pre-authorizes is never a missing owner decision: add it and continue, do not write a blocker for it."
            ),
            "{campaign}"
        );
    }

    #[test]
    fn the_task_and_iterate_prompt_carries_the_advisory_rules() {
        let prompt = super::build_execution_prompt("p", "Upgrade axios", "security", None);
        assert!(
            prompt.contains(
                "Never suppress, ignore or allowlist an advisory that has a fixed release"
            )
        );
        assert!(prompt.contains("unless you cite"));
    }
}
