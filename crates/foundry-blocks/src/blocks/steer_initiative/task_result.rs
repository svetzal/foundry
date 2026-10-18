use super::super::{SimulatedSuccess, advance_campaign::lock_store};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::initiative::{Finding, FindingKind, FindingSource};
use foundry_sdk::payload::{TaskRunCompletedPayload, TaskVerdict};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::{WorkItemStore, ledger_write_gate};
use std::path::PathBuf;

/// Records task learning before the post-cleanup steering admission can observe it.
pub struct RecordInitiativeTaskResult {
    store_path: PathBuf,
    work_items_path: PathBuf,
}
impl RecordInitiativeTaskResult {
    #[must_use]
    pub fn new(store_path: PathBuf, work_items_path: PathBuf) -> Self {
        Self {
            store_path,
            work_items_path,
        }
    }
    async fn run(&self, trigger: &Event) -> anyhow::Result<TaskBlockResult> {
        let result = trigger.parse_payload::<TaskRunCompletedPayload>()?;
        let name = result
            .context
            .initiative
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("initiative task lacks owner"))?;
        let mut guard = lock_store(&self.store_path).await?;
        let _gate = ledger_write_gate()
            .lock()
            .map_err(|_| anyhow::anyhow!("ledger gate poisoned"))?;
        let ledger = WorkItemStore::load(&self.work_items_path)?;
        let initiative = guard.store.initiative_view(name)?;
        let receipt = initiative
            .task_dispatches
            .iter()
            .filter(|receipt| {
                ledger.find(&receipt.work_item).is_some_and(|item| {
                    item.project == result.project
                        && item.trace_id.is_some()
                        && item.trace_id == trigger.trace_id
                        && result.context.task_worktree.as_ref().is_none_or(|worktree| {
                            item.disposition
                                .as_ref()
                                .and_then(|disposition| disposition.worktree.as_ref())
                                == Some(worktree)
                        })
                })
            })
            .max_by_key(|receipt| {
                ledger
                    .find(&receipt.work_item)
                    .map(|item| item.started_at.unwrap_or(item.submitted_at))
            })
            .ok_or_else(|| {
                anyhow::anyhow!("task result has no authoritative initiative dispatch receipt")
            })?;
        let mut events = Vec::new();
        if result.landed {
            for finding in guard.store.consume_findings(
                name,
                &receipt.member,
                &receipt.work_item,
                &result.context.finding_ids,
            )? {
                events.push(trigger.with_payload(
                    EventType::FindingConsumed,
                    &serde_json::json!({"initiative":name,"finding_id":finding.id,
                    "member":receipt.member,"work_item_id":receipt.work_item,"finding":finding}),
                )?);
            }
        }
        let learned: Vec<(FindingKind, &str)> = match &result.verdict {
            TaskVerdict::Remainder { gaps } if result.landed => {
                gaps.iter().map(|gap| (FindingKind::Blocker, gap.as_str())).collect()
            }
            TaskVerdict::Defect { diagnosis } => vec![(FindingKind::Regression, diagnosis)],
            TaskVerdict::BlockedOnDecision { finding, .. } => vec![(FindingKind::Blocker, finding)],
            TaskVerdict::BlockedOnRecovery { detail, .. } => vec![(FindingKind::Blocker, detail)],
            _ => vec![],
        };
        for (index, (kind, text)) in learned.into_iter().enumerate() {
            let mut finding = Finding::new(
                name,
                FindingSource {
                    kind: "initiative_task".into(),
                    id: receipt.work_item.clone(),
                },
                vec![],
                kind,
                text,
                vec![format!("event:{}", trigger.id)],
            );
            finding.id = format!("fnd_{}_{index}", trigger.id);
            if guard.store.find_finding(&finding.id).is_some() {
                continue;
            }
            let finding = guard.store.add_finding(finding)?;
            events.push(super::finding_event(trigger, &finding)?);
        }
        if let TaskVerdict::BlockedOnDecision { finding, options } = &result.verdict {
            guard.store.record_required_question(
                name,
                &format!("{finding}\nOptions: {}", options.join("; ")),
                std::slice::from_ref(&receipt.member),
            )?;
        }
        guard.save()?;
        Ok(TaskBlockResult::success(
            "initiative task learning recorded; steering waits for cleanup",
            events,
        ))
    }
}
impl SimulatedSuccess for RecordInitiativeTaskResult {
    type Outcome = ();
    fn simulate(&self, _trigger: &Event) {}
    fn success_events(&self, _trigger: &Event, _outcome: &()) -> Vec<Event> {
        vec![]
    }
}
impl TaskBlock for RecordInitiativeTaskResult {
    task_block_meta! { name:"Record Initiative Task Result", kind:Mutator, sinks_on:[TaskRunCompleted], }
    dry_run_via_simulation!();
    fn accepts(&self, trigger: &Event) -> bool {
        trigger.throttle == foundry_sdk::throttle::Throttle::Full
            && trigger.parse_payload::<TaskRunCompletedPayload>().is_ok_and(|result| {
                result.context.initiative.is_some() && result.context.campaign.is_none()
            })
    }
    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let trigger = trigger.clone();
        Box::pin(async move { self.run(&trigger).await })
    }
}
