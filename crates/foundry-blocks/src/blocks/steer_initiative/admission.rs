use foundry_sdk::campaign::CampaignStore;
use foundry_sdk::initiative::{Initiative, SteeringDecision};
use foundry_sdk::registry::Registry;
use foundry_sdk::work_item::WorkItemStore;

pub(super) fn validate_member_pin(
    pin: Option<&str>,
    decision: &SteeringDecision,
) -> anyhow::Result<()> {
    if let Some(pin) = pin {
        let chosen = match decision {
            SteeringDecision::Advance { member, .. } => Some(member.as_str()),
            SteeringDecision::Dispatch { member, .. } => member.as_deref(),
            _ => return Ok(()),
        };
        if chosen != Some(pin) {
            anyhow::bail!(
                "owner requested member '{pin}'; steering cannot choose another work member"
            );
        }
    }
    Ok(())
}

pub(super) fn action_project<'a>(
    decision: &'a SteeringDecision,
    store: &'a CampaignStore,
    _initiative: &'a Initiative,
) -> anyhow::Result<Option<&'a str>> {
    match decision {
        SteeringDecision::Advance { member, project } => Ok(Some(
            store
                .find(member)
                .map(|campaign| campaign.project.as_str())
                .or(project.as_deref())
                .ok_or_else(|| {
                    anyhow::anyhow!("member requires an explicitly registered project")
                })?,
        )),
        SteeringDecision::Dispatch { project, .. } => Ok(Some(project)),
        _ => Ok(None),
    }
}

pub(super) fn ensure_action_repository_free(
    decision: &SteeringDecision,
    store: &CampaignStore,
    initiative: &Initiative,
    ledger: &WorkItemStore,
    registry: &Registry,
) -> anyhow::Result<()> {
    let Some(project) = action_project(decision, store, initiative)? else {
        return Ok(());
    };
    if !initiative.projects.iter().any(|allowed| allowed == project) {
        anyhow::bail!("action project is outside initiative scope");
    }
    let entry = registry
        .find_project(project)
        .ok_or_else(|| anyhow::anyhow!("action project is not registered"))?;
    if let Some(reason) = &entry.skip {
        anyhow::bail!("action project is blocked: {reason}");
    }
    if repository_busy(registry, ledger, project) {
        anyhow::bail!("action repository still has an execution or cleanup owner");
    }
    let active = store.ledger_active_members(ledger);
    match decision {
        SteeringDecision::Advance { member, .. }
        | SteeringDecision::Dispatch {
            member: Some(member),
            ..
        } => {
            store.enforce_member_eligibility(&initiative.name, member, &active)?;
        }
        SteeringDecision::Dispatch { member: None, .. }
            if store.occupied_member_slots(&initiative.name, &active)?.len()
                >= initiative.budget.max_active_members =>
        {
            anyhow::bail!("initiative has reached its active member limit");
        }
        _ => {}
    }
    Ok(())
}

/// Whether `project`'s repository has an execution or cleanup owner in `ledger`.
fn repository_busy(registry: &Registry, ledger: &WorkItemStore, project: &str) -> bool {
    let key = foundry_sdk::pacing::repository_key(registry, project);
    ledger.items.iter().any(|item| {
        item.occupies_slot()
            && (foundry_sdk::pacing::repository_key(registry, &item.project) == key
                || item
                    .reserved_projects
                    .iter()
                    .any(|project| foundry_sdk::pacing::repository_key(registry, project) == key))
    })
}

/// The initiative projects whose repository has an execution or cleanup owner.
pub(super) fn busy_projects(
    initiative: &Initiative,
    ledger: &WorkItemStore,
    registry: &Registry,
) -> Vec<String> {
    initiative
        .projects
        .iter()
        .filter(|project| repository_busy(registry, ledger, project))
        .cloned()
        .collect()
}

/// The members a steering pass of initiative `name` could advance now.
///
/// This is the precheck the pass itself runs, so whoever decides to request a
/// pass and the pass agree on eligibility. Assessments of the initiative in
/// `ledger` are left out: a requested pass runs after them, and each pass
/// leaves itself out of its own precheck.
pub fn steering_eligible_members(
    store: &CampaignStore,
    name: &str,
    ledger: &WorkItemStore,
    registry: &Registry,
) -> anyhow::Result<Vec<String>> {
    Ok(steering_readiness(store, name, ledger, registry)?.eligible)
}

/// The same typed readiness used by steering and its read-only projections.
/// Task receipts are derived into a copy; inspection changes no durable state.
pub fn steering_readiness(
    store: &CampaignStore,
    name: &str,
    ledger: &WorkItemStore,
    registry: &Registry,
) -> anyhow::Result<foundry_sdk::initiative::SteeringPrecheck> {
    let mut checked = store.clone();
    let initiative = checked.initiative_view(name)?;
    if !initiative.status.is_terminal()
        && initiative.authorized_by.as_ref().is_some_and(|owner| !owner.trim().is_empty())
    {
        record_task_evidence(&mut checked, name, ledger)?;
    }
    let mut others = ledger.clone();
    others.items.retain(|item| {
        ![item.started_root.as_ref(), item.pending_root.as_ref()]
            .into_iter()
            .flatten()
            .any(|root| {
                root.event_type == foundry_sdk::event::EventType::InitiativeAdvanceRequested
                    && root.payload_str("initiative") == Some(name)
            })
    });
    let active = checked.ledger_active_members(&others);
    let busy = busy_projects(&initiative, &others, registry);
    let mut precheck = checked.steering_precheck(name, &active, &busy)?;
    let available =
        |project: &str| registry.find_project(project).is_some_and(|entry| entry.skip.is_none());
    for readiness in &mut precheck.readiness {
        let projects = checked
            .find(&readiness.member)
            .map_or_else(|| initiative.projects.clone(), |campaign| vec![campaign.project.clone()]);
        if !projects.iter().any(|project| available(project)) {
            readiness.block(
                foundry_sdk::initiative::ReadinessBlockerKind::ProjectUnavailable,
                "no registered, enabled project is available for this member".into(),
            );
        } else if !projects.iter().any(|project| available(project) && !busy.contains(project))
            && !readiness.blockers.iter().any(|blocker| {
                blocker.kind == foundry_sdk::initiative::ReadinessBlockerKind::RepositoryBusy
            })
        {
            readiness.block(
                foundry_sdk::initiative::ReadinessBlockerKind::RepositoryBusy,
                "its repository has an execution or cleanup owner".into(),
            );
        }
    }
    precheck.eligible.retain(|member| {
        precheck
            .readiness
            .iter()
            .any(|readiness| readiness.member == *member && readiness.ready)
    });
    precheck.blocked = precheck
        .readiness
        .iter()
        .filter(|readiness| {
            !readiness.ready
                && initiative.members.iter().any(|member| {
                    member.name == readiness.member
                        && member.kind != foundry_sdk::initiative::MemberKind::Task
                })
        })
        .map(|readiness| foundry_sdk::initiative::BlockedMember {
            member: readiness.member.clone(),
            reason: readiness
                .blockers
                .iter()
                .map(|blocker| blocker.detail.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        })
        .collect();
    precheck.dispatch_open &= initiative
        .projects
        .iter()
        .any(|project| available(project) && !busy.contains(project));
    Ok(precheck)
}

pub(super) fn live_operations(items: &WorkItemStore) -> Vec<String> {
    let mut live = Vec::new();
    for item in &items.items {
        if item.state.is_waiting() || item.occupies_slot() {
            live.push(item.id.clone());
            for root in
                [item.pending_root.as_ref(), item.started_root.as_ref()].into_iter().flatten()
            {
                live.push(root.id.clone());
                if let Some(action) =
                    root.payload.get("steering_action_id").and_then(serde_json::Value::as_str)
                {
                    live.push(action.into());
                }
            }
        }
    }
    live
}

/// The integration steer owns its trunks until it emits the next action. That
/// reservation protects evidence collection, and must not block its own action.
pub(super) fn without_steering_owner(items: &WorkItemStore, id: &str) -> WorkItemStore {
    let mut items = items.clone();
    items.items.retain(|item| {
        !item.started_root.as_ref().is_some_and(|root| {
            root.id == id
                && root.event_type == foundry_sdk::event::EventType::InitiativeAdvanceRequested
        })
    });
    items
}

pub(super) fn outcome_has_live_work(initiative: &Initiative, items: &WorkItemStore) -> bool {
    items.items.iter().any(|item| {
        (item.state.is_waiting() || item.state.is_open() || item.occupies_slot())
            && (initiative.projects.contains(&item.project)
                || item
                    .reserved_projects
                    .iter()
                    .any(|project| initiative.projects.contains(project))
                || item.source.as_ref().is_some_and(|source| {
                    source.kind == foundry_sdk::work_source::WorkSourceKind::Initiative
                        && source.reference == initiative.name
                }))
    })
}

pub(super) fn record_task_evidence(
    store: &mut CampaignStore,
    name: &str,
    ledger: &WorkItemStore,
) -> anyhow::Result<()> {
    let initiative = store.initiative_view(name)?;
    for receipt in &initiative.task_dispatches {
        let Some(item) = ledger.find(&receipt.work_item) else {
            continue;
        };
        if !item.execution_completed {
            continue;
        }
        let Some(commit) = item
            .disposition
            .as_ref()
            .and_then(|disposition| disposition.landed_commit.as_deref())
        else {
            continue;
        };
        let ids = item
            .started_root
            .as_ref()
            .map(|root| foundry_sdk::payload::ChainContext::extract_from(&root.payload).finding_ids)
            .unwrap_or_default();
        store.consume_findings(name, &receipt.member, &item.id, &ids)?;
        store.record_milestone(
            name,
            &receipt.member,
            "landed",
            vec![format!("work item {} landed {commit}", item.id)],
        )?;
        if item.disposition.as_ref().and_then(|disposition| disposition.verdict.as_deref())
            == Some("complete")
        {
            store.record_milestone(
                name,
                &receipt.member,
                "completed",
                vec![format!("work item {} completed at {commit}", item.id)],
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn owner_pin_cannot_be_replaced_by_another_work_choice() {
        assert!(
            validate_member_pin(
                Some("one"),
                &SteeringDecision::Advance {
                    member: "two".into(),
                    project: None
                }
            )
            .is_err()
        );
        assert!(
            validate_member_pin(
                Some("one"),
                &SteeringDecision::Dispatch {
                    member: None,
                    project: "sample".into(),
                    objective: "work".into()
                }
            )
            .is_err()
        );
        assert!(
            validate_member_pin(
                Some("one"),
                &SteeringDecision::Advance {
                    member: "one".into(),
                    project: None
                }
            )
            .is_ok()
        );
        assert!(
            validate_member_pin(
                Some("one"),
                &SteeringDecision::Escalate {
                    reason: "already realised".into()
                }
            )
            .is_ok()
        );
    }
}
