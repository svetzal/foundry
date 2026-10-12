use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::initiative::SteeringDecision;
use foundry_sdk::payload::{
    CampaignAdvanceRequestedPayload, ChainContext, ExecutionRequestedPayload,
    InitiativeActionRequestedPayload,
};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::{WorkItemStore, ledger_write_gate};
use foundry_sdk::work_source::WorkSource;

use super::super::{SimulatedSuccess, advance_campaign::lock_store};
use super::admission::{action_project, active_members, ensure_action_repository_free};

/// Converts a recorded action into one durable, paced child admission.
pub struct ApplyInitiativeAction {
    registry: Arc<RwLock<Registry>>,
    store_path: PathBuf,
    work_items_path: PathBuf,
}
impl ApplyInitiativeAction {
    #[must_use]
    pub fn new(
        registry: Arc<RwLock<Registry>>,
        store_path: PathBuf,
        work_items_path: PathBuf,
    ) -> Self {
        Self {
            registry,
            store_path,
            work_items_path,
        }
    }

    async fn run(&self, trigger: &Event) -> anyhow::Result<TaskBlockResult> {
        match self.apply(trigger).await {
            Ok(result) => Ok(result),
            Err(error) => self.discharge_failed_action(trigger, error).await,
        }
    }
    async fn discharge_failed_action(
        &self,
        trigger: &Event,
        error: anyhow::Error,
    ) -> anyhow::Result<TaskBlockResult> {
        let supplied = trigger.parse_payload::<InitiativeActionRequestedPayload>()?.action;
        let mut guard = lock_store(&self.store_path).await?;
        let _gate = ledger_write_gate()
            .lock()
            .map_err(|_| anyhow::anyhow!("ledger gate poisoned"))?;
        let items = WorkItemStore::load(&self.work_items_path)?;
        let admitted = items.items.iter().any(|item| {
            [item.started_root.as_ref(), item.pending_root.as_ref()]
                .into_iter()
                .flatten()
                .any(|root| {
                    root.payload.get("steering_action_id").and_then(serde_json::Value::as_str)
                        == Some(supplied.id.as_str())
                })
        });
        if !admitted
            && let Some(pending) =
                guard.store.pending_steering_action(&supplied.initiative, &supplied.id)?
            && pending.id == supplied.id
            && pending.decision == supplied.decision
            && pending.providers == supplied.providers
        {
            guard.store.discard_steering_action(&supplied.initiative, &supplied.id)?;
            guard.save()?;
        }
        let initiative = guard.store.initiative_view(&supplied.initiative)?;
        let record = initiative.steering_history.iter().find(|record| record.id == supplied.id);
        let stale = error.is::<StaleExternal>();
        let fact = foundry_sdk::payload::InitiativeSteeredPayload {
            success: stale,
            outcome: if stale { "stale" } else { "failed" }.into(),
            error: Some(error.to_string()),
            initiative: supplied.initiative.clone(),
            steer_id: supplied.id.clone(),
            prompt: record.map(|record| record.prompt.clone()).unwrap_or_default(),
            decision: Some(supplied.decision),
            provider: record.and_then(|record| record.provider),
            action: None,
        };
        let mut events = vec![trigger.with_payload(EventType::InitiativeSteered, &fact)?];
        if stale && !admitted && initiative.steering_enabled {
            let mut fresh = trigger.with_payload(
                EventType::InitiativeAdvanceRequested,
                &foundry_sdk::payload::InitiativeAdvanceRequestedPayload {
                    initiative: initiative.name,
                    member: None,
                    operator_origin: None,
                    trigger_id: Some(trigger.id.clone()),
                },
            )?;
            fresh.id = format!("evt_{}_fresh", trigger.id);
            fresh.project = "system".into();
            events.push(fresh);
        }
        Ok(TaskBlockResult {
            success: stale,
            summary: error.to_string(),
            events,
            ..Default::default()
        })
    }
    async fn validate_external(
        &self,
        supplied: &foundry_sdk::initiative::DurableSteeringAction,
    ) -> anyhow::Result<Option<foundry_sdk::initiative::SteeringExternalContext>> {
        let initial = foundry_sdk::campaign::CampaignStore::load(&self.store_path)?;
        let initial_action =
            initial.pending_steering_action(&supplied.initiative, &supplied.id)?.cloned();
        let registry = super::super::read_registry(&self.registry)?.clone();
        Ok(if let Some(recorded) = &initial_action {
            if matches!(
                recorded.decision,
                SteeringDecision::Advance { .. } | SteeringDecision::Dispatch { .. }
            ) {
                let expected = recorded.external_context.clone();
                let initiative = initial.initiative_view(&recorded.initiative)?;
                Some(
                    tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
                        if let Some(expected) = expected {
                            if !super::external::validate_steering_external_context(
                                &expected, &registry,
                            )? {
                                return Err(StaleExternal.into());
                            }
                            Ok(expected)
                        } else {
                            super::external::capture(&initiative, &registry)
                        }
                    })
                    .await??,
                )
            } else {
                None
            }
        } else {
            None
        })
    }
    async fn apply(&self, trigger: &Event) -> anyhow::Result<TaskBlockResult> {
        let payload = trigger.parse_payload::<InitiativeActionRequestedPayload>()?;
        let supplied = payload.action;
        let external = self.validate_external(&supplied).await?;
        let mut guard = lock_store(&self.store_path).await?;
        let _gate = ledger_write_gate()
            .lock()
            .map_err(|_| anyhow::anyhow!("ledger gate poisoned"))?;
        let Some(mut action) = guard
            .store
            .pending_steering_action(&supplied.initiative, &supplied.id)?
            .cloned()
        else {
            return Ok(TaskBlockResult::success("initiative action already acknowledged", vec![]));
        };
        let mut comparison = action.clone();
        comparison.external_context.clone_from(&supplied.external_context);
        if comparison != supplied {
            anyhow::bail!("action differs from durable steering decision");
        }
        if !guard.store.pending_action_is_current(&action.initiative, &action.id)? {
            return Err(StaleExternal.into());
        }
        if let Some(external) = external {
            action = guard.store.bind_external_context(&action.initiative, &action.id, external)?;
            guard.save()?;
        }
        let initiative = guard.store.initiative_view(&action.initiative)?;
        let items = WorkItemStore::load(&self.work_items_path)?;
        let registry = super::super::read_registry(&self.registry)?.clone();
        guard.store.enforce_steering_action(
            &initiative.name,
            &action.id,
            &active_members(&initiative, &items),
        )?;
        ensure_action_repository_free(
            &action.decision,
            &guard.store,
            &initiative,
            &items,
            &registry,
        )?;
        enforce_task_providers(&action, &initiative)?;
        let kind = match &action.decision {
            SteeringDecision::Advance { .. } => EventType::CampaignAdvanceRequested,
            SteeringDecision::Dispatch { .. } => EventType::ExecutionRequested,
            _ => {
                guard.store.finish_steering_action(&initiative.name, &action.id)?;
                guard.save()?;
                let facts = if let SteeringDecision::Relay { finding, .. } = &action.decision {
                    let current = guard
                        .store
                        .find_finding(&finding.id)
                        .ok_or_else(|| anyhow::anyhow!("durable relay finding is missing"))?;
                    if guard
                        .store
                        .initiative_view(&initiative.name)?
                        .steering_history
                        .iter()
                        .any(|record| record.id == action.id && record.provider.is_some())
                    {
                        vec![]
                    } else {
                        vec![super::finding_event(trigger, current)?]
                    }
                } else {
                    vec![]
                };
                return Ok(TaskBlockResult::success("initiative idle decision applied", facts));
            }
        };
        let project = action_project(&action.decision, &guard.store, &initiative)?
            .ok_or_else(|| anyhow::anyhow!("work action lacks project"))?
            .to_owned();
        let mut event = child_event(trigger, &action, &initiative, &guard.store, &project, &kind)?;
        event.project = project;
        event.id = format!(
            "evt_{}_{}",
            action.id,
            if kind == EventType::CampaignAdvanceRequested {
                "advance"
            } else {
                "dispatch"
            }
        );
        event.source = Some(WorkSource::initiative(
            &initiative.name,
            guard.store.shared_cycles_used(&initiative.name)?,
        ));
        event.payload["initiative"] = serde_json::json!(initiative.name);
        event.payload["steering_action_id"] = serde_json::json!(action.id);
        event.payload["steering_external_context"] =
            serde_json::to_value(&action.external_context)?;
        // Admission acknowledges this action only after the exact child is durably queued.
        Ok(TaskBlockResult::success(
            "initiative work action submitted for durable admission",
            vec![event],
        ))
    }
}
impl SimulatedSuccess for ApplyInitiativeAction {
    type Outcome = ();
    fn simulate(&self, _trigger: &Event) {}
    fn success_events(&self, _trigger: &Event, _outcome: &()) -> Vec<Event> {
        vec![]
    }
}
impl TaskBlock for ApplyInitiativeAction {
    task_block_meta! { name: "Apply Initiative Action", kind: Mutator, sinks_on: [InitiativeActionRequested], }
    dry_run_via_simulation!();
    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        {
            let trigger = trigger.clone();
            Box::pin(async move { self.run(&trigger).await })
        }
    }
}

fn child_event(
    trigger: &Event,
    action: &foundry_sdk::initiative::DurableSteeringAction,
    initiative: &foundry_sdk::initiative::Initiative,
    store: &foundry_sdk::campaign::CampaignStore,
    project: &str,
    kind: &EventType,
) -> anyhow::Result<Event> {
    Ok(match &action.decision {
        SteeringDecision::Advance { member, .. } => trigger.with_payload(
            kind.clone(),
            &CampaignAdvanceRequestedPayload {
                campaign: member.clone(),
                run_event_id: None,
                run_result: None,
                operator_origin: None,
                providers: action.providers,
            },
        )?,
        SteeringDecision::Dispatch {
            objective, member, ..
        } => {
            let member = member
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("recorded dispatch has no assigned task slot"))?;
            let mut findings = Vec::new();
            let mut bytes = 0;
            for finding in store.list_findings(&initiative.name)? {
                if !(finding.to.is_empty() || finding.to.iter().any(|to| to == member))
                    || finding.consumed_by.iter().any(|consumed| consumed.member == member)
                {
                    continue;
                }
                let size = serde_json::to_vec(&finding)?.len();
                if bytes + size > 8192 {
                    continue;
                }
                bytes += size;
                findings.push(finding);
            }
            let mut event = trigger.with_payload(
                kind.clone(),
                &ExecutionRequestedPayload {
                    project: project.to_owned(),
                    prompt: objective.clone(),
                    operator_origin: None,
                    depends_on: vec![],
                    not_before: None,
                    continues: vec![],
                    chain: ChainContext {
                        agent_provider: initiative.agent_provider.clone(),
                        providers: action.providers,
                        initiative: Some(initiative.name.clone()),
                        campaign_limits: Some(foundry_sdk::campaign::StageBudget::default()),
                        initiative_brief: initiative.brief.clone(),
                        finding_ids: findings.iter().map(|finding| finding.id.clone()).collect(),
                        initiative_findings: findings,
                        ..Default::default()
                    },
                },
            )?;
            event.payload["workflow"] =
                serde_json::json!(foundry_sdk::workflow::WorkflowType::Task);
            event
        }
        _ => anyhow::bail!("non-work action cannot construct a work child"),
    })
}

#[derive(Debug)]
struct StaleExternal;
impl std::fmt::Display for StaleExternal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("external repository, intent, registry or brief changed after steering")
    }
}
impl std::error::Error for StaleExternal {}

fn enforce_task_providers(
    action: &foundry_sdk::initiative::DurableSteeringAction,
    initiative: &foundry_sdk::initiative::Initiative,
) -> anyhow::Result<()> {
    if matches!(action.decision, SteeringDecision::Dispatch { .. }) {
        for stage in [
            foundry_sdk::gateway::AgentStage::Execution,
            foundry_sdk::gateway::AgentStage::Review,
        ] {
            let fallback = super::super::parse_agent_provider(initiative.agent_provider.as_deref())
                .or(Some(foundry_sdk::gateway::AgentProvider::Codex));
            if !action
                .providers
                .resolve(stage, fallback)
                .is_some_and(foundry_sdk::gateway::AgentProvider::enforces_writable_root)
            {
                anyhow::bail!(
                    "initiative task provider must enforce its execution and review boundaries"
                );
            }
        }
    }
    Ok(())
}
