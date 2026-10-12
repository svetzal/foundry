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
pub use request::RequestInitiativeAdvance;
pub use task_result::RecordInitiativeTaskResult;

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use foundry_sdk::campaign::CampaignStore;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::gateway::AgentStage;
use foundry_sdk::initiative::derive::{IntentProjection, read_projection};
use foundry_sdk::initiative::{Initiative, SteeringSnapshot};
use foundry_sdk::payload::{
    InitiativeActionRequestedPayload, InitiativeAdvanceRequestedPayload, InitiativeSteeredPayload,
    WorkItemEventPayload,
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
    ) -> anyhow::Result<Prepared> {
        let initial = CampaignStore::load(&self.store_path)?;
        let initiative = initial.initiative_view(&request.initiative)?;
        let projection = intent(&initiative).await?;
        let (evidence_store, mut events) =
            self.refresh_intent(&initiative, projection.as_ref()).await?;
        let registry = self.registry()?;
        let evidence = integration::run(
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
        for result in evidence {
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
        let ledger = ledger_snapshot(
            &WorkItemStore::load(&self.work_items_path)?,
            &guard.store.initiative_view(&initiative.name)?,
        )?;
        guard.save()?;
        let snapshot = guard.store.begin_steering(&initiative.name, &trigger.id)?;
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
            self.store_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("store path has no evidence parent"))?,
        )?;
        Ok(Prepared {
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
        })
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
            Ok(prepared) => prepared,
            Err(error) => {
                self.release(&request.initiative, &trigger.id).await?;
                return steering_result(
                    trigger,
                    &request.initiative,
                    "failed",
                    error.to_string(),
                    String::new(),
                    None,
                    None,
                    None,
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
                "failed",
                error.to_string(),
                prepared.prompt.clone(),
                Some(prepared.provider),
                None,
                None,
            ),
        };
        self.release(&request.initiative, &trigger.id).await?;
        let mut result = match result {
            Ok(result) => result,
            Err(error) => steering_result(
                trigger,
                &request.initiative,
                "failed",
                error.to_string(),
                prepared.prompt.clone(),
                Some(prepared.provider),
                None,
                None,
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
        let initiative = &prepared.snapshot.initiative.name;
        let checked = validate_member_pin(prepared.requested_member.as_deref(), &response.action)
            .and_then(|()| {
                packet::validate_audits(&response.audits, &prepared.snapshot, &prepared.repos)
            });
        if let Err(error) = checked {
            return steering_result(
                trigger,
                initiative,
                "failed",
                error.to_string(),
                prepared.prompt.clone(),
                Some(prepared.provider),
                Some(response.action),
                None,
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
            return steering_result(
                trigger,
                initiative,
                "stale",
                "repository, brief or intent changed during assessment".into(),
                prepared.prompt.clone(),
                Some(prepared.provider),
                Some(response.action),
                None,
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
            let decision = foundry_sdk::initiative::SteeringDecision::Advance {
                member: member.name.clone(),
                project: None,
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
        Ok(foundry_sdk::initiative::SteeringDecision::Ask {
            question: format!("Outcome completion lacks evidence: {reason}"),
            members: vec![],
        })
    }
    async fn commit_response(
        &self,
        trigger: &Event,
        prepared: &Prepared,
        response: SteeringResponse,
    ) -> anyhow::Result<TaskBlockResult> {
        let initiative = &prepared.snapshot.initiative.name;
        let mut guard = lock_store(&self.store_path).await?;
        let _gate = ledger_write_gate()
            .lock()
            .map_err(|_| anyhow::anyhow!("ledger write gate poisoned"))?;
        let ledger = WorkItemStore::load(&self.work_items_path)?;
        if guard.store.validate_steering(&prepared.snapshot).is_err()
            || ledger_snapshot(&ledger, &prepared.snapshot.initiative)? != prepared.ledger
        {
            return steering_result(
                trigger,
                initiative,
                "stale",
                "owner, member or ledger state changed during assessment".into(),
                prepared.prompt.clone(),
                Some(prepared.provider),
                Some(response.action),
                None,
            );
        }
        ensure_action_repository_free(
            &response.action,
            &guard.store,
            &prepared.snapshot.initiative,
            &admission::without_steering_owner(&ledger, &trigger.id),
            &self.registry()?,
        )?;
        let previous_findings = guard.store.list_findings(initiative)?;
        let mut action = match guard.store.commit_steering(
            &prepared.snapshot,
            &trigger.id,
            response.action.clone(),
            response.audits,
            prepared.prompt.clone(),
            Some(prepared.provider),
        ) {
            Ok(action) => action,
            Err(error) => {
                return steering_result(
                    trigger,
                    initiative,
                    "failed",
                    error.to_string(),
                    prepared.prompt.clone(),
                    Some(prepared.provider),
                    Some(response.action),
                    None,
                );
            }
        };
        if matches!(
            action.decision,
            foundry_sdk::initiative::SteeringDecision::Advance { .. }
                | foundry_sdk::initiative::SteeringDecision::Dispatch { .. }
                | foundry_sdk::initiative::SteeringDecision::Done { .. }
                | foundry_sdk::initiative::SteeringDecision::Stage { .. }
        ) {
            action = guard.store.bind_external_context(
                initiative,
                &action.id,
                prepared.external_context(),
            )?;
        }
        guard.save()?;
        let mut result = steering_result(
            trigger,
            initiative,
            "applied",
            "one steering action recorded".into(),
            prepared.prompt.clone(),
            Some(prepared.provider),
            Some(action.decision.clone()),
            Some(action.clone()),
        )?;
        for finding in guard.store.list_findings(initiative)? {
            if !previous_findings.iter().any(|previous| previous == &finding) {
                result.events.push(finding_event(trigger, &finding)?);
            }
        }
        let mut requested = trigger.with_payload(
            EventType::InitiativeActionRequested,
            &InitiativeActionRequestedPayload { action },
        )?;
        requested.id = format!("evt_{}_action", trigger.id);
        requested.source = Some(WorkSource::initiative(initiative, 0));
        result.events.push(requested);
        Ok(result)
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
}

impl Prepared {
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

#[allow(
    clippy::too_many_arguments,
    reason = "the small event builder mirrors the typed steering fact fields"
)]
fn steering_result(
    trigger: &Event,
    initiative: &str,
    outcome: &str,
    detail: String,
    prompt: String,
    provider: Option<AgentProvider>,
    decision: Option<foundry_sdk::initiative::SteeringDecision>,
    action: Option<foundry_sdk::initiative::DurableSteeringAction>,
) -> anyhow::Result<TaskBlockResult> {
    let success = outcome != "failed";
    let payload = InitiativeSteeredPayload {
        success,
        outcome: outcome.into(),
        error: (outcome != "applied").then_some(detail.clone()),
        initiative: initiative.into(),
        steer_id: trigger.id.clone(),
        prompt,
        decision,
        provider,
        action,
    };
    let mut event = trigger.with_payload(EventType::InitiativeSteered, &payload)?;
    event.source = Some(WorkSource::initiative(initiative, 0));
    let mut events = vec![event];
    if outcome == "stale" {
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
mod outcome_tests;
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
