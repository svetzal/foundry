mod action;
mod admission;
mod external;
mod integration;
pub use external::capture as capture_initiative_external_context;
pub use external::validate_steering_external_context;
mod packet;
mod request;
mod task_result;
pub use action::ApplyInitiativeAction;
use admission::{
    ensure_action_repository_free, live_operations, record_task_evidence, validate_member_pin,
};
pub use admission::{steering_eligible_members, steering_readiness};
pub use request::RequestInitiativeAdvance;
pub use task_result::RecordInitiativeTaskResult;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_sdk::campaign::CampaignStore;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gateway::AgentStage;
use foundry_sdk::initiative::derive::{IntentProjection, read_projection};
use foundry_sdk::initiative::{
    Initiative, InitiativeError, SteeringCommit, SteeringDecision, SteeringSnapshot,
};
use foundry_sdk::payload::{
    InitiativeActionRequestedPayload, InitiativeAdvanceRequestedPayload, InitiativeSteeredPayload,
    SteeringOutcome, WorkItemEventPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::{WorkItemStore, ledger_write_gate};
use foundry_sdk::work_source::WorkSource;

use super::advance_campaign::lock_store;
use super::{AgentBlockSpec, SimulatedSuccess, invoke_agent};
use crate::gateway::{
    AgentAccess, AgentGateway, AgentOutcome, AgentProvider, ModelTier, ReasoningEffort,
    ShellGateway,
};
use packet::{RepositorySnapshot, SteeringResponse};

/// One read-only assessment. No store or ledger lock crosses provider, Git or Tracker work.
pub struct SteerInitiative {
    agent: Arc<dyn AgentGateway>,
    shell: Arc<dyn ShellGateway>,
    registry: Arc<RwLock<Registry>>,
    store_path: PathBuf,
    work_items_path: PathBuf,
}

impl SteerInitiative {
    #[must_use]
    pub fn new(
        agent: Arc<dyn AgentGateway>,
        shell: Arc<dyn ShellGateway>,
        registry: Arc<RwLock<Registry>>,
        store_path: PathBuf,
        work_items_path: PathBuf,
    ) -> Self {
        Self {
            agent,
            shell,
            registry,
            store_path,
            work_items_path,
        }
    }

    fn registry(&self) -> anyhow::Result<Registry> {
        Ok(super::read_registry(&self.registry)?.clone())
    }

    async fn prepare(
        &self,
        request: &InitiativeAdvanceRequestedPayload,
        trigger: &Event,
    ) -> anyhow::Result<Preparation> {
        let initial = CampaignStore::load(&self.store_path)?;
        let initiative = initial.initiative_view(&request.initiative)?;
        let projection = intent(&initiative).await?;
        let (evidence_store, mut events) =
            self.refresh_intent(&initiative, projection.as_ref()).await?;
        let registry = self.registry()?;
        let integration = integration::run(
            self.shell.as_ref(),
            &registry,
            &evidence_store,
            &initiative.name,
            &self.work_items_path,
            &self.store_path,
            &trigger.id,
        )
        .await?;
        let repos =
            packet::repositories(&registry, &initiative.projects, self.shell.as_ref()).await?;
        let mut guard = lock_store(&self.store_path).await?;
        let previous_findings = guard.store.list_findings(&initiative.name)?;
        for result in integration.results {
            guard.store.record_integration_result(&initiative.name, result)?;
        }
        for finding in guard.store.list_findings(&initiative.name)? {
            if !previous_findings.iter().any(|previous| previous == &finding) {
                events.push(finding_event(trigger, &finding)?);
            }
        }
        if guard.store.initiative_view(&initiative.name)?.completion_proposal.is_some() {
            let snapshot =
                guard.store.outcome_snapshot(&initiative.name, repository_identities(&repos))?;
            guard.store.invalidate_completion(&initiative.name, &snapshot)?;
        }
        let items = WorkItemStore::load(&self.work_items_path)?;
        let ledger = ledger_snapshot(&items, &guard.store.initiative_view(&initiative.name)?)?;
        let precheck = {
            let others = admission::without_steering_owner(&items, &trigger.id);
            steering_readiness(&guard.store, &initiative.name, &others, &registry)?
        };
        guard.save()?;
        if let Some(reason) = steering_wait_reason(request, &initiative, &precheck) {
            return Ok(Preparation::Skip { reason, events });
        }
        let mut snapshot = guard.store.begin_steering(&initiative.name, &trigger.id)?;
        snapshot.follow_up_of.clone_from(&request.follow_up_of);
        snapshot.integration_skipped = integration.skipped;
        guard.save()?;
        let store = guard.store.clone();
        drop(guard);
        if repos.is_empty() {
            anyhow::bail!("initiative has no registered repository");
        }
        let brief = brief_content(&store, &snapshot.initiative)?;
        let provider = snapshot
            .initiative
            .providers
            .resolve(
                AgentStage::Formation,
                super::parse_agent_provider(snapshot.initiative.agent_provider.as_deref()),
            )
            .unwrap_or(AgentProvider::Codex);
        if !provider.enforces_writable_root() {
            anyhow::bail!("initiative steering provider must enforce the read-only boundary");
        }
        let prompt = packet::prompt(
            &snapshot,
            &store,
            &repos,
            request.member.as_deref(),
            &ledger,
            &precheck,
            self.store_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("store path has no evidence parent"))?,
        )?;
        Ok(Preparation::Assess(Box::new(Prepared {
            snapshot,
            repos,
            projection: projection
                .map(|projection| serde_json::to_string(&projection))
                .transpose()?,
            ledger,
            brief,
            provider,
            artifact: prompt.artifact,
            prompt: prompt.text,
            events,
            requested_member: request.member.clone(),
            eligible: precheck.eligible,
        })))
    }

    async fn refresh_intent(
        &self,
        initiative: &Initiative,
        projection: Option<&IntentProjection>,
    ) -> anyhow::Result<(CampaignStore, Vec<Event>)> {
        let mut guard = lock_store(&self.store_path).await?;
        if guard.store.initiative_view(&initiative.name)?.derive_source != initiative.derive_source
        {
            anyhow::bail!("initiative derivation source changed before steering");
        }
        let mut events = Vec::new();
        {
            let _gate = ledger_write_gate()
                .lock()
                .map_err(|_| anyhow::anyhow!("ledger write gate poisoned"))?;
            let mut items = WorkItemStore::load(&self.work_items_path)?;
            if let Some(projection) = projection {
                let cancelled = guard.store.refresh_derived_intent(&initiative.name, projection)?;
                // Intent cancellation is durable before its ledger obligations are discharged.
                guard.save()?;
                for item in &mut items.items {
                    let task =
                        guard.store.find_initiative(&initiative.name).is_some_and(|initiative| {
                            initiative.task_dispatches.iter().any(|receipt| {
                                receipt.work_item == item.id && cancelled.contains(&receipt.member)
                            })
                        });
                    if (item
                        .campaign_name()
                        .is_some_and(|member| cancelled.iter().any(|name| name == member))
                        || task)
                        && (item.state.is_open() || item.state.is_waiting())
                    {
                        item.settle_cancelled(
                            "initiative intent was discarded or revoked",
                            item.disposition.clone(),
                            chrono::Utc::now(),
                        );
                        events.push(Event::new(
                            EventType::WorkItemCancelled,
                            item.project.clone(),
                            foundry_sdk::throttle::Throttle::Full,
                            Event::serialize_payload(&WorkItemEventPayload::from_item(item))?,
                        ));
                    }
                }
                items.save(&self.work_items_path)?;
            }
            record_task_evidence(&mut guard.store, &initiative.name, &items)?;
            let live = live_operations(&items);
            guard.store.recover_steering(&initiative.name, &live)?;
        }
        guard.save()?;
        let evidence_store = guard.store.clone();
        drop(guard);
        Ok((evidence_store, events))
    }

    async fn unchanged(&self, prepared: &Prepared) -> anyhow::Result<bool> {
        let registry = self.registry()?;
        let repos = packet::repositories(
            &registry,
            &prepared.snapshot.initiative.projects,
            self.shell.as_ref(),
        )
        .await?;
        let projection = intent(&prepared.snapshot.initiative)
            .await?
            .map(|projection| serde_json::to_string(&projection))
            .transpose()?;
        let store = CampaignStore::load(&self.store_path)?;
        let artifact_unchanged = prepared
            .artifact
            .as_ref()
            .map(|(path, hash)| {
                std::fs::read_to_string(path).map(|text| packet::context_hash(&text) == *hash)
            })
            .transpose()?
            .unwrap_or(true);
        Ok(artifact_unchanged
            && repos == prepared.repos
            && projection == prepared.projection
            && brief_content(&store, &prepared.snapshot.initiative)? == prepared.brief)
    }

    async fn release(&self, name: &str, id: &str) -> anyhow::Result<()> {
        let mut guard = lock_store(&self.store_path).await?;
        guard.store.finish_steering(name, id)?;
        guard.save()?;
        Ok(())
    }

    async fn run(&self, trigger: &Event) -> anyhow::Result<TaskBlockResult> {
        let request = trigger.parse_payload::<InitiativeAdvanceRequestedPayload>()?;
        let mut prepared = match self.prepare(&request, trigger).await {
            Ok(Preparation::Assess(prepared)) => prepared,
            Ok(Preparation::Skip { reason, events }) => {
                return skipped(trigger, &request.initiative, reason, events);
            }
            Err(error) => {
                self.release(&request.initiative, &trigger.id).await?;
                return steering_result(
                    trigger,
                    &request.initiative,
                    SteeringOutcome::Failed,
                    error.to_string(),
                    String::new(),
                    None,
                    None,
                    None,
                    &[],
                );
            }
        };
        let outcome = invoke_agent(
            self.agent.as_ref(),
            AgentBlockSpec {
                prompt: prepared.prompt.clone(),
                working_dir: prepared
                    .repos
                    .first()
                    .ok_or_else(|| anyhow::anyhow!("initiative has no repository"))?
                    .root
                    .clone(),
                access: AgentAccess::ReadOnly,
                tier: ModelTier::Balanced,
                effort: ReasoningEffort::Medium,
                agent_file: None,
                provider: Some(prepared.provider),
                env: vec![("FOUNDRY_AGENT_STAGE".into(), "formation".into())],
                timeout: Duration::ZERO,
                trace_id: trigger.trace_id.clone(),
                requires_json: true,
            },
            "initiative steering",
            "system",
        )
        .await;
        let response = match outcome {
            AgentOutcome::Success { stdout } => packet::parse_response(&stdout),
            AgentOutcome::AgentFailed { stderr, .. } => {
                Err(anyhow::anyhow!("steering agent failed: {stderr}"))
            }
            AgentOutcome::Unavailable { error } => {
                Err(anyhow::anyhow!("steering agent unavailable: {error}"))
            }
        };
        let result = match response {
            Ok(response) => self.apply_response(trigger, &prepared, response).await,
            Err(error) => steering_result(
                trigger,
                &request.initiative,
                SteeringOutcome::Failed,
                error.to_string(),
                prepared.prompt.clone(),
                Some(prepared.provider),
                None,
                None,
                &prepared.eligible,
            ),
        };
        self.release(&request.initiative, &trigger.id).await?;
        let mut result = match result {
            Ok(result) => result,
            Err(error) => steering_result(
                trigger,
                &request.initiative,
                outcome_of(&error),
                error.to_string(),
                prepared.prompt.clone(),
                Some(prepared.provider),
                None,
                None,
                &prepared.eligible,
            )?,
        };
        prepared.events.append(&mut result.events);
        result.events = prepared.events;
        Ok(result)
    }

    async fn apply_response(
        &self,
        trigger: &Event,
        prepared: &Prepared,
        mut response: SteeringResponse,
    ) -> anyhow::Result<TaskBlockResult> {
        let checked = validate_member_pin(prepared.requested_member.as_deref(), &response.action)
            .and_then(|()| {
                packet::validate_audits(&response.audits, &prepared.snapshot, &prepared.repos)
            });
        if let Err(error) = checked {
            return prepared.fact(
                trigger,
                SteeringOutcome::Rejected,
                error.to_string(),
                response.action,
            );
        }
        let mut rejected = None;
        if matches!(response.action, foundry_sdk::initiative::SteeringDecision::Done { .. })
            && let Err(error) = self.validate_done(trigger, prepared, &response.action)
        {
            rejected =
                Some(serde_json::json!({"decision":response.action,"reason":error.to_string()}));
            response.action = self.repair_action(prepared, &response, &error.to_string())?;
        }
        if !self.unchanged(prepared).await? {
            return prepared.fact(
                trigger,
                SteeringOutcome::Stale,
                "repository, brief or intent changed during assessment".into(),
                response.action,
            );
        }
        let mut result = self.commit_response(trigger, prepared, response).await?;
        if let Some(rejected) = rejected {
            for event in &mut result.events {
                if event.event_type == EventType::InitiativeSteered {
                    event.payload["rejected_completion"] = rejected.clone();
                }
            }
        }
        Ok(result)
    }

    fn validate_done(
        &self,
        trigger: &Event,
        prepared: &Prepared,
        decision: &foundry_sdk::initiative::SteeringDecision,
    ) -> anyhow::Result<()> {
        if let foundry_sdk::initiative::SteeringDecision::Done { reviews } = decision {
            for review in reviews {
                for reference in &review.evidence {
                    if prepared
                        .snapshot
                        .initiative
                        .integration_results
                        .iter()
                        .any(|gate| gate.id == *reference)
                    {
                        continue;
                    }
                    let (project, path) = reference.split_once('/').ok_or_else(|| anyhow::anyhow!("review evidence must name a gate result or project-relative checked path"))?;
                    let repo =
                        prepared.repos.iter().find(|repo| repo.project == project).ok_or_else(
                            || {
                                anyhow::anyhow!(
                                    "review evidence names a project outside the initiative"
                                )
                            },
                        )?;
                    packet::checked_evidence_path(&repo.root, path)?;
                }
            }
            let items = admission::without_steering_owner(
                &WorkItemStore::load(&self.work_items_path)?,
                &trigger.id,
            );
            if admission::outcome_has_live_work(&prepared.snapshot.initiative, &items) {
                anyhow::bail!("initiative members still have live or waiting work");
            }
            let mut store = CampaignStore::load(&self.store_path)?;
            let snapshot = store.outcome_snapshot(
                &prepared.snapshot.initiative.name,
                repository_identities(&prepared.repos),
            )?;
            store.propose_completion(
                &prepared.snapshot.initiative.name,
                &trigger.id,
                snapshot,
                reviews.clone(),
            )?;
        }
        Ok(())
    }

    fn repair_action(
        &self,
        prepared: &Prepared,
        response: &SteeringResponse,
        reason: &str,
    ) -> anyhow::Result<foundry_sdk::initiative::SteeringDecision> {
        let store = CampaignStore::load(&self.store_path)?;
        let items = admission::without_steering_owner(
            &WorkItemStore::load(&self.work_items_path)?,
            &prepared.snapshot.id,
        );
        let registry = self.registry()?;
        for member in &prepared.snapshot.initiative.members {
            use foundry_sdk::initiative::{AuditClassification, MemberKind};
            let project = match member.kind {
                MemberKind::Campaign => None,
                // A task slot needs a newly derived objective; repeating its description can duplicate a landing.
                MemberKind::Task => continue,
                MemberKind::Candidate => {
                    let audit = response
                        .audits
                        .iter()
                        .find(|audit| Some(&audit.intent_ref) == member.intent_ref.as_ref())
                        .or_else(|| {
                            member
                                .audit_finding_id
                                .as_ref()
                                .and_then(|id| store.find_finding(id))
                                .and_then(|finding| finding.audit.as_ref())
                        });
                    if let Some(audit) = audit {
                        if !matches!(
                            audit.classification,
                            AuditClassification::Partial | AuditClassification::Absent
                        ) {
                            continue;
                        }
                        Some(audit.project.clone())
                    } else if prepared.snapshot.initiative.derive_source.is_none()
                        && prepared.snapshot.initiative.projects.len() == 1
                    {
                        prepared.snapshot.initiative.projects.first().cloned()
                    } else {
                        continue;
                    }
                }
            };
            let decision = foundry_sdk::initiative::SteeringDecision::Advance {
                member: member.name.clone(),
                project,
            };
            if validate_member_pin(prepared.requested_member.as_deref(), &decision).is_err()
                || ensure_action_repository_free(
                    &decision,
                    &store,
                    &prepared.snapshot.initiative,
                    &items,
                    &registry,
                )
                .is_err()
            {
                continue;
            }
            let mut candidate = store.clone();
            if candidate
                .commit_steering(
                    &prepared.snapshot,
                    &prepared.snapshot.id,
                    decision.clone(),
                    response.audits.clone(),
                    prepared.prompt.clone(),
                    Some(prepared.provider),
                )
                .is_ok()
            {
                return Ok(decision);
            }
        }
        if reason == "open question budget exhausted" {
            return Ok(foundry_sdk::initiative::SteeringDecision::Escalate {
                reason: "waiting on owner; open question budget exhausted".into(),
            });
        }
        Ok(foundry_sdk::initiative::SteeringDecision::Recover {
            kind: foundry_sdk::recovery::RecoveryKind::UnmetPrecondition,
            detail: reason.into(),
        })
    }

    fn enforce_question_budget(
        &self,
        prepared: &Prepared,
        response: &mut SteeringResponse,
    ) -> anyhow::Result<()> {
        let question = match &response.action {
            foundry_sdk::initiative::SteeringDecision::Ask { question, .. } => question,
            foundry_sdk::initiative::SteeringDecision::Escalate { reason } => reason,
            _ => return Ok(()),
        };
        if prepared.snapshot.initiative.open_questions.len()
            >= prepared.snapshot.initiative.budget.max_open_questions
            && !prepared
                .snapshot
                .initiative
                .open_questions
                .iter()
                .chain(&prepared.snapshot.initiative.deferred_questions)
                .any(|existing| existing.question.trim() == question.trim())
        {
            response.action =
                self.repair_action(prepared, response, "open question budget exhausted")?;
        }
        Ok(())
    }

    async fn commit_response(
        &self,
        trigger: &Event,
        prepared: &Prepared,
        mut response: SteeringResponse,
    ) -> anyhow::Result<TaskBlockResult> {
        self.enforce_question_budget(prepared, &mut response)?;
        let initiative = &prepared.snapshot.initiative.name;
        let mut guard = lock_store(&self.store_path).await?;
        let _gate = ledger_write_gate()
            .lock()
            .map_err(|_| anyhow::anyhow!("ledger write gate poisoned"))?;
        let ledger = WorkItemStore::load(&self.work_items_path)?;
        if guard.store.validate_steering(&prepared.snapshot).is_err()
            || ledger_snapshot(&ledger, &prepared.snapshot.initiative)? != prepared.ledger
        {
            return prepared.fact(
                trigger,
                SteeringOutcome::Stale,
                "owner, member or ledger state changed during assessment".into(),
                response.action,
            );
        }
        if let Err(error) = ensure_action_repository_free(
            &response.action,
            &guard.store,
            &prepared.snapshot.initiative,
            &admission::without_steering_owner(&ledger, &trigger.id),
            &self.registry()?,
        ) {
            return prepared.fact(
                trigger,
                SteeringOutcome::Rejected,
                error.to_string(),
                response.action,
            );
        }
        let previous_findings = guard.store.list_findings(initiative)?;
        let committed = guard.store.commit_steering(
            &prepared.snapshot,
            &trigger.id,
            response.action.clone(),
            response.audits,
            prepared.prompt.clone(),
            Some(prepared.provider),
        );
        let (outcome, detail, action) = match committed {
            Ok(committed) => committed_outcome(committed),
            Err(error) => {
                let outcome = if matches!(error, InitiativeError::Persistence(_)) {
                    SteeringOutcome::Failed
                } else {
                    SteeringOutcome::Rejected
                };
                return prepared.fact(trigger, outcome, error.to_string(), response.action);
            }
        };
        let action = match action {
            Some(action) if needs_external_evidence(&action.decision) => {
                Some(guard.store.bind_external_context(
                    initiative,
                    &action.id,
                    prepared.external_context(),
                )?)
            }
            other => other,
        };
        guard.save()?;
        if outcome == SteeringOutcome::Waited && !prepared.eligible.is_empty() {
            tracing::info!(
                initiative = %initiative,
                eligible = ?prepared.eligible,
                reason = %detail,
                "steering chose to wait while members were eligible to advance"
            );
        }
        let decision = action.as_ref().map_or(response.action, |action| action.decision.clone());
        let mut result = steering_result(
            trigger,
            initiative,
            outcome,
            detail,
            prepared.prompt.clone(),
            Some(prepared.provider),
            Some(decision),
            action.clone(),
            &prepared.eligible,
        )?;
        for finding in guard.store.list_findings(initiative)? {
            if !previous_findings.iter().any(|previous| previous == &finding) {
                result.events.push(finding_event(trigger, &finding)?);
            }
        }
        result.events.extend(self.next_request(
            trigger,
            initiative,
            outcome,
            action,
            &ledger,
            &guard.store,
        )?);
        Ok(result)
    }

    /// What a committed pass requests next.
    ///
    /// A decision that needs an action requests it. A wait or a decision
    /// already in force needs no action. A wait requests nothing more. A
    /// decision already in force dispatches nothing, like an applied hold or
    /// relay, so it may request the same one follow-up.
    fn next_request(
        &self,
        trigger: &Event,
        initiative: &str,
        outcome: SteeringOutcome,
        action: Option<foundry_sdk::initiative::DurableSteeringAction>,
        ledger: &WorkItemStore,
        store: &CampaignStore,
    ) -> anyhow::Result<Option<Event>> {
        if let Some(action) = action {
            let mut requested = trigger.with_payload(
                EventType::InitiativeActionRequested,
                &InitiativeActionRequestedPayload { action },
            )?;
            requested.id = format!("evt_{}_action", trigger.id);
            requested.source = Some(WorkSource::initiative(initiative, 0));
            return Ok(Some(requested));
        }
        if outcome != SteeringOutcome::NoOp {
            return Ok(None);
        }
        action::follow_up_request(
            trigger,
            initiative,
            &trigger.id,
            ledger,
            &self.registry()?,
            store,
        )
    }
}

struct Prepared {
    snapshot: SteeringSnapshot,
    repos: Vec<RepositorySnapshot>,
    projection: Option<String>,
    ledger: String,
    brief: Option<String>,
    provider: AgentProvider,
    prompt: String,
    events: Vec<Event>,
    requested_member: Option<String>,
    artifact: Option<(PathBuf, String)>,
    /// Members the precheck found eligible to advance, recorded on the fact.
    eligible: Vec<String>,
}

/// What preparation decided: ask the agent, or record that no action could apply.
enum Preparation {
    Assess(Box<Prepared>),
    Skip { reason: String, events: Vec<Event> },
}

/// Foundry refused a steering decision with a guard: the member is held, the
/// active-member limit is reached, the repository is busy, the owner pinned
/// another member, and so on. The decision took no effect and nothing is owed,
/// so the steer settles `cancelled`, not `failed`.
#[derive(Debug)]
struct SteeringRejected(String);
impl std::fmt::Display for SteeringRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SteeringRejected {}

/// Mark a guard's refusal of a steering decision.
fn rejected(error: impl std::fmt::Display) -> anyhow::Error {
    SteeringRejected(error.to_string()).into()
}

/// A store refusal is a guard rejection; a persistence fault is a failure.
fn store_refusal(error: InitiativeError) -> anyhow::Error {
    match error {
        InitiativeError::Persistence(_) => error.into(),
        refusal => rejected(refusal),
    }
}

/// `rejected` for a guard's refusal, `failed` for every other error.
fn outcome_of(error: &anyhow::Error) -> SteeringOutcome {
    if error.is::<SteeringRejected>() {
        SteeringOutcome::Rejected
    } else {
        SteeringOutcome::Failed
    }
}

impl Prepared {
    /// The steering fact for an assessment the agent answered without an action.
    fn fact(
        &self,
        trigger: &Event,
        outcome: SteeringOutcome,
        detail: String,
        decision: SteeringDecision,
    ) -> anyhow::Result<TaskBlockResult> {
        steering_result(
            trigger,
            &self.snapshot.initiative.name,
            outcome,
            detail,
            self.prompt.clone(),
            Some(self.provider),
            Some(decision),
            None,
            &self.eligible,
        )
    }

    fn external_context(&self) -> foundry_sdk::initiative::SteeringExternalContext {
        foundry_sdk::initiative::SteeringExternalContext {
            repositories: repository_identities(&self.repos),
            derive_source: self.snapshot.initiative.derive_source.clone(),
            intent: self.projection.clone(),
            brief: self.snapshot.initiative.brief.clone(),
            brief_content: self.brief.clone(),
        }
    }
}

fn repository_identities(
    repos: &[RepositorySnapshot],
) -> Vec<foundry_sdk::initiative::SteeringRepositoryIdentity> {
    repos
        .iter()
        .map(|repo| foundry_sdk::initiative::SteeringRepositoryIdentity {
            project: repo.project.clone(),
            root: repo.root.to_string_lossy().into_owned(),
            registry: repo.registry.clone(),
            trunk: repo.trunk.clone(),
            head: repo.head.clone(),
            status: repo.status.clone(),
            diff: repo.diff.clone(),
        })
        .collect()
}

async fn intent(initiative: &Initiative) -> anyhow::Result<Option<IntentProjection>> {
    match &initiative.derive_source {
        Some(source) => Ok(Some(read_projection(std::path::Path::new(&source.repo)).await?)),
        None => Ok(None),
    }
}

fn brief_content(store: &CampaignStore, initiative: &Initiative) -> anyhow::Result<Option<String>> {
    initiative
        .brief
        .as_ref()
        .map(|_| store.show_brief(&initiative.name).map_err(anyhow::Error::from))
        .transpose()
}

fn ledger_snapshot(ledger: &WorkItemStore, initiative: &Initiative) -> anyhow::Result<String> {
    let items: Vec<_> = ledger
        .items
        .iter()
        .filter(|item| {
            initiative.projects.contains(&item.project)
                || item.source.as_ref().is_some_and(|source| {
                    source.kind == foundry_sdk::work_source::WorkSourceKind::Initiative
                        && source.reference == initiative.name
                })
        })
        .cloned()
        .map(|mut item| {
            item.coalesced_root = None;
            item
        })
        .collect();
    Ok(serde_json::to_string(&items)?)
}

/// The outcome, its detail and the durable action, when the decision needs one.
fn committed_outcome(
    committed: SteeringCommit,
) -> (SteeringOutcome, String, Option<foundry_sdk::initiative::DurableSteeringAction>) {
    match committed {
        SteeringCommit::Action(action) => (
            SteeringOutcome::Applied,
            "one steering action recorded".to_owned(),
            Some(*action),
        ),
        SteeringCommit::Wait { reason } => (SteeringOutcome::Waited, reason, None),
        SteeringCommit::NoOp { reason } => (SteeringOutcome::NoOp, reason, None),
    }
}

/// Avoid reassessing an unchanged wait, while allowing new facts and independent work.
fn steering_wait_reason(
    request: &InitiativeAdvanceRequestedPayload,
    initiative: &foundry_sdk::initiative::Initiative,
    precheck: &foundry_sdk::initiative::SteeringPrecheck,
) -> Option<String> {
    if request.member.is_some() {
        return None;
    }
    if request.trigger_id.is_none()
        && precheck.eligible.is_empty()
        && let Some(recovery) =
            initiative.recovery.as_ref().filter(|recovery| recovery.is_pending())
    {
        return Some(format!(
            "recovery '{}' awaits new evidence: {}",
            recovery.id, recovery.detail
        ));
    }
    // An explicit request is assessed unless it repeats an unresolved recovery.
    request.trigger_id.as_ref().and_then(|_| precheck.skip_reason())
}

/// Work, staging and completion actions are admitted against the repository,
/// intent and brief evidence the assessment saw.
fn needs_external_evidence(decision: &SteeringDecision) -> bool {
    matches!(
        decision,
        SteeringDecision::Advance { .. }
            | SteeringDecision::Dispatch { .. }
            | SteeringDecision::Done { .. }
            | SteeringDecision::Stage { .. }
    )
}

/// No steering action could apply, so no agent ran. Findings recorded while
/// preparing still precede the fact.
fn skipped(
    trigger: &Event,
    initiative: &str,
    reason: String,
    mut events: Vec<Event>,
) -> anyhow::Result<TaskBlockResult> {
    let mut result = steering_result(
        trigger,
        initiative,
        SteeringOutcome::Skipped,
        reason,
        String::new(),
        None,
        None,
        None,
        &[],
    )?;
    events.append(&mut result.events);
    result.events = events;
    Ok(result)
}

#[allow(
    clippy::too_many_arguments,
    reason = "the small event builder mirrors the typed steering fact fields"
)]
fn steering_result(
    trigger: &Event,
    initiative: &str,
    outcome: SteeringOutcome,
    detail: String,
    prompt: String,
    provider: Option<AgentProvider>,
    decision: Option<SteeringDecision>,
    action: Option<foundry_sdk::initiative::DurableSteeringAction>,
    eligible: &[String],
) -> anyhow::Result<TaskBlockResult> {
    let success = outcome != SteeringOutcome::Failed;
    // The fact, and a fresh request after a stale pass, keep the follow-up
    // cause, so a stale follow-up cannot reset the one-follow-up bound.
    let follow_up_of = trigger
        .parse_payload::<InitiativeAdvanceRequestedPayload>()
        .ok()
        .and_then(|request| request.follow_up_of);
    let (error, reason) = match outcome {
        SteeringOutcome::Applied => (None, None),
        SteeringOutcome::Waited | SteeringOutcome::NoOp | SteeringOutcome::Skipped => {
            (None, Some(detail.clone()))
        }
        SteeringOutcome::Rejected | SteeringOutcome::Stale | SteeringOutcome::Failed => {
            (Some(detail.clone()), None)
        }
    };
    let payload = InitiativeSteeredPayload {
        success,
        outcome,
        error,
        initiative: initiative.into(),
        steer_id: trigger.id.clone(),
        prompt,
        decision,
        provider,
        action,
        reason,
        eligible_members: eligible.to_vec(),
        follow_up_of: follow_up_of.clone(),
    };
    let mut event = trigger.with_payload(EventType::InitiativeSteered, &payload)?;
    event.source = Some(WorkSource::initiative(initiative, 0));
    let mut events = vec![event];
    if outcome == SteeringOutcome::Stale {
        let mut fresh = trigger.with_payload(
            EventType::InitiativeAdvanceRequested,
            &InitiativeAdvanceRequestedPayload {
                initiative: initiative.into(),
                member: trigger
                    .payload
                    .get("member")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                operator_origin: None,
                trigger_id: Some(trigger.id.clone()),
                follow_up_of,
            },
        )?;
        fresh.id = format!("evt_{}_fresh", trigger.id);
        events.push(fresh);
    }
    Ok(TaskBlockResult {
        success,
        summary: detail,
        events,
        ..Default::default()
    })
}

impl SimulatedSuccess for SteerInitiative {
    type Outcome = ();
    fn simulate(&self, _trigger: &Event) {}
    fn success_events(&self, _trigger: &Event, _outcome: &()) -> Vec<Event> {
        vec![]
    }
}

impl TaskBlock for SteerInitiative {
    task_block_meta! { name: "Steer Initiative", kind: Mutator, sinks_on: [InitiativeAdvanceRequested], }
    dry_run_via_simulation!();
    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        {
            let trigger = trigger.clone();
            Box::pin(async move { self.run(&trigger).await })
        }
    }
}

#[cfg(test)]
mod follow_up_tests;
#[cfg(test)]
mod outcome_tests;
#[cfg(test)]
mod precheck_tests;
#[cfg(test)]
mod tests;

pub(super) fn finding_event(
    trigger: &Event,
    finding: &foundry_sdk::initiative::Finding,
) -> anyhow::Result<Event> {
    let mut payload = serde_json::to_value(finding)?;
    payload["finding_id"] = serde_json::json!(finding.id);
    let mut event = trigger.with_payload(EventType::FindingRecorded, &payload)?;
    event.id = format!("evt_{}_finding_{}", trigger.id, finding.id);
    event.source = Some(WorkSource::initiative(&finding.initiative, 0));
    Ok(event)
}
