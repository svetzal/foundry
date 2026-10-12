//! Owner-confirmed outcome completion and resumable Tracker closure.

use std::path::Path;

use chrono::Utc;
use foundry_sdk::campaign::{CampaignStore, Transition};
use foundry_sdk::initiative::derive::{IntentProjection, derive_from_projection};
use foundry_sdk::initiative::{Initiative, InitiativeStatus, OutcomeSnapshot};
use foundry_sdk::registry::Registry;
use tonic::Status;

use super::initiative_ops;

trait Tracker {
    fn read(&self, repo: &Path) -> Result<IntentProjection, Status>;
    fn close(&self, repo: &Path, reference: &str, expected: &str) -> Result<(), Status>;
}

struct TrackerCli;

fn et_json(repo: &Path, args: &[&str]) -> Result<serde_json::Value, Status> {
    let output = std::process::Command::new("et")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|_| Status::unavailable("could not invoke Tracker in its source repository"))?;
    if !output.status.success() {
        // Tracker output may contain configuration diagnostics. Keep it out of
        // Foundry's persisted receipts and RPC errors.
        return Err(Status::unavailable("Tracker command failed; retry completion after recovery"));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| Status::failed_precondition("Tracker returned invalid JSON"))
}

impl Tracker for TrackerCli {
    fn read(&self, repo: &Path) -> Result<IntentProjection, Status> {
        Ok(IntentProjection {
            goals: serde_json::from_value(et_json(repo, &["list", "goals", "--all", "--json"])?)
                .map_err(|_| Status::failed_precondition("invalid Tracker goals"))?,
            interactions: serde_json::from_value(et_json(
                repo,
                &["list", "interactions", "--all", "--json"],
            )?)
            .map_err(|_| Status::failed_precondition("invalid Tracker interactions"))?,
            journeys: serde_json::from_value(et_json(
                repo,
                &["list", "journeys", "--all", "--json"],
            )?)
            .map_err(|_| Status::failed_precondition("invalid Tracker journeys"))?,
        })
    }

    fn close(&self, repo: &Path, reference: &str, expected: &str) -> Result<(), Status> {
        let (kind, id) = close_reference(reference)?;
        let result = et_json(repo, &["close", kind, id, "--json"])?;
        if result["success"].as_bool() != Some(true)
            || result["data"]["id"].as_str() != Some(id)
            || result["data"]["state"].as_str() != Some(expected)
        {
            return Err(Status::failed_precondition(
                "Tracker did not verify the requested reality state",
            ));
        }
        Ok(())
    }
}

fn close_reference(reference: &str) -> Result<(&str, &str), Status> {
    let mut parts = reference.split(':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("et"), Some(kind @ ("goal" | "interaction")), Some(id), None) if !id.is_empty() => {
            Ok((kind, id))
        }
        _ => Err(Status::failed_precondition("invalid Tracker completion reference")),
    }
}

fn reality(state: &str) -> Option<&'static str> {
    match state {
        "create" | "update" | "created" => Some("created"),
        "delete" | "deleted" => Some("deleted"),
        _ => None,
    }
}

pub(super) fn complete(
    path: &Path,
    ledger: &Path,
    registry: &Registry,
    name: &str,
    reason: &str,
) -> Result<(crate::proto::Initiative, bool), Status> {
    complete_with(path, ledger, registry, name, reason, &TrackerCli)
}

/// Confirmation is durable before any external mutation. Restart may resume
/// only those proposals the owner already confirmed, and retains failed ones.
pub(super) fn recover(
    path: &Path,
    ledger: &Path,
    registry: &Registry,
) -> Result<Vec<foundry_sdk::event::Event>, Status> {
    recover_with(path, ledger, registry, &TrackerCli)
}

fn recover_with(
    path: &Path,
    ledger: &Path,
    registry: &Registry,
    tracker: &dyn Tracker,
) -> Result<Vec<foundry_sdk::event::Event>, Status> {
    use foundry_sdk::event::{Event, EventType};
    let store = CampaignStore::load(path).map_err(|error| Status::internal(error.to_string()))?;
    let mut events = Vec::new();
    for initiative in &store.initiatives {
        let Some(confirmation) = &initiative.completion_confirmation else {
            continue;
        };
        if matches!(initiative.status, InitiativeStatus::Completed | InitiativeStatus::Cancelled) {
            continue;
        }
        match complete_with(path, ledger, registry, &initiative.name, &confirmation.reason, tracker)
        {
            Ok((view, changed)) => {
                if !changed {
                    continue;
                }
                let members = view
                    .members
                    .iter()
                    .filter(|member| member.kind == "campaign")
                    .map(|member| member.name.clone())
                    .collect::<Vec<_>>();
                let cancelled = super::work_item_ops::cancel_waiting_initiative(
                    ledger,
                    &initiative.name,
                    &members,
                    &confirmation.reason,
                )?;
                for item in cancelled {
                    events.push(Event::new(
                        EventType::WorkItemCancelled,
                        item.project.clone(),
                        foundry_sdk::throttle::Throttle::Full,
                        Event::serialize_payload(
                            &foundry_sdk::payload::WorkItemEventPayload::from_item(&item),
                        )
                        .map_err(|error| Status::internal(error.to_string()))?,
                    ));
                }
                events.push(Event::new(EventType::InitiativeCompleted, "system".into(), foundry_sdk::throttle::Throttle::Full,
                    serde_json::json!({"initiative":view.name,"status":view.status,"reason":confirmation.reason,"recovered":true})));
            }
            Err(error) => {
                tracing::warn!(initiative = %initiative.name, message = %error.message(), "confirmed completion remains held after restart");
            }
        }
    }
    Ok(events)
}

fn complete_with(
    path: &Path,
    ledger: &Path,
    registry: &Registry,
    name: &str,
    reason: &str,
    tracker: &dyn Tracker,
) -> Result<(crate::proto::Initiative, bool), Status> {
    let initial = CampaignStore::load(path).map_err(|error| Status::internal(error.to_string()))?;
    let initiative = initial.initiative_view(name).map_err(initiative_ops::error)?;
    if initiative.status == InitiativeStatus::Completed {
        return Ok((initiative_ops::to_proto(&initial, &initiative), false));
    }
    if initiative.status == InitiativeStatus::Cancelled {
        return Err(Status::failed_precondition("a cancelled initiative cannot complete"));
    }
    if initiative.completion_proposal.is_none() {
        return Err(Status::failed_precondition(
            "initiative has no done proposal awaiting owner confirmation; advance it to review outcome evidence",
        ));
    }
    require_origin_trunks(&initiative, registry)?;
    let context =
        foundry_blocks::blocks::capture_initiative_external_context(&initiative, registry)
            .map_err(|error| Status::failed_precondition(error.to_string()))?;
    if initiative.completion_confirmation.is_none()
        && let Some(intent) = &context.intent
    {
        let projection: IntentProjection = serde_json::from_str(intent)
            .map_err(|_| Status::failed_precondition("invalid current intent projection"))?;
        let mut current = initial.clone();
        current
            .refresh_derived_intent(name, &projection)
            .map_err(initiative_ops::error)?;
        if current.initiative_view(name).map_err(initiative_ops::error)?.intent_revision
            != initiative.intent_revision
        {
            return Err(Status::failed_precondition(
                "intent projection changed since outcome review; steer again",
            ));
        }
    }
    let snapshot = initial
        .outcome_snapshot(name, context.repositories.clone())
        .map_err(initiative_ops::error)?;
    require_trunk_checkouts(&snapshot)?;
    let mut guard =
        CampaignStore::lock_exclusive(path).map_err(|error| Status::internal(error.to_string()))?;
    require_no_live_work(&guard.store, ledger, name)?;
    guard
        .store
        .confirm_completion(name, reason, &snapshot, Utc::now())
        .map_err(initiative_ops::error)?;
    guard.save().map_err(|error| Status::internal(error.to_string()))?;
    drop(guard);
    let result = close_and_finish(path, ledger, registry, name, reason, &snapshot, tracker);
    if let Err(failure) = &result {
        let mut guard = CampaignStore::lock_exclusive(path)
            .map_err(|error| Status::internal(error.to_string()))?;
        guard
            .store
            .record_completion_error(name, failure.message())
            .map_err(initiative_ops::error)?;
        guard.save().map_err(|error| Status::internal(error.to_string()))?;
    }
    result
}

fn close_and_finish(
    path: &Path,
    ledger: &Path,
    registry: &Registry,
    name: &str,
    reason: &str,
    snapshot: &OutcomeSnapshot,
    tracker: &dyn Tracker,
) -> Result<(crate::proto::Initiative, bool), Status> {
    let store = CampaignStore::load(path).map_err(|error| Status::internal(error.to_string()))?;
    let initiative = store.initiative_view(name).map_err(initiative_ops::error)?;
    if let Some(source) = &initiative.derive_source {
        let repo = Path::new(&source.repo);
        let current = tracker.read(repo)?;
        let baseline = if let Some(json) = initiative
            .completion_confirmation
            .as_ref()
            .and_then(|confirmation| confirmation.intent_json.as_deref())
        {
            serde_json::from_str(json)
                .map_err(|_| Status::internal("invalid saved completion intent"))?
        } else {
            validate_initial_intent(&initiative, &current)?;
            let json = serde_json::to_string(&current)
                .map_err(|error| Status::internal(error.to_string()))?;
            let mut guard = CampaignStore::lock_exclusive(path)
                .map_err(|error| Status::internal(error.to_string()))?;
            require_no_live_work(&guard.store, ledger, name)?;
            guard.store.validate_completion(name, snapshot).map_err(initiative_ops::error)?;
            guard.store.set_completion_intent(name, &json).map_err(initiative_ops::error)?;
            guard.save().map_err(|error| Status::internal(error.to_string()))?;
            current.clone()
        };
        let targets = close_targets(&initiative, &baseline)?;
        for reference in &targets {
            ensure_current(path, ledger, registry, name, snapshot)?;
            let current = tracker.read(repo)?;
            validate_retry_intent(&baseline, &current, &targets)?;
            let (kind, id) = close_reference(reference)?;
            if kind == "goal"
                && current
                    .interactions
                    .iter()
                    .filter(|child| {
                        child.goals.iter().any(|goal| goal == id)
                            || child.goal.as_deref() == Some(id)
                    })
                    .any(|child| matches!(child.state.as_str(), "create" | "update" | "delete"))
            {
                return Err(Status::failed_precondition(
                    "the Tracker goal still has children outside reality",
                ));
            }
            let state = entity_state(&current, kind, id)?;
            let expected = reality(entity_state(&baseline, kind, id)?)
                .ok_or_else(|| Status::failed_precondition("Tracker intent was withdrawn"))?;
            if state != expected {
                tracker.close(repo, reference, expected)?;
            }
            let verified = tracker.read(repo)?;
            validate_retry_intent(&baseline, &verified, &targets)?;
            if entity_state(&verified, kind, id)? != expected {
                return Err(Status::failed_precondition(
                    "Tracker closure is not present in the current product state",
                ));
            }
            let mut guard = CampaignStore::lock_exclusive(path)
                .map_err(|error| Status::internal(error.to_string()))?;
            require_no_live_work(&guard.store, ledger, name)?;
            guard.store.validate_completion(name, snapshot).map_err(initiative_ops::error)?;
            guard
                .store
                .record_et_close(name, reference, Utc::now())
                .map_err(initiative_ops::error)?;
            guard.save().map_err(|error| Status::internal(error.to_string()))?;
        }
    }
    ensure_current(path, ledger, registry, name, snapshot)?;
    let mut guard =
        CampaignStore::lock_exclusive(path).map_err(|error| Status::internal(error.to_string()))?;
    require_no_live_work(&guard.store, ledger, name)?;
    guard.store.validate_completion(name, snapshot).map_err(initiative_ops::error)?;
    let changed = guard
        .store
        .complete_initiative(name, reason, Utc::now())
        .map_err(initiative_ops::error)?
        == Transition::Applied;
    guard.save().map_err(|error| Status::internal(error.to_string()))?;
    let view = guard.store.initiative_view(name).map_err(initiative_ops::error)?;
    Ok((initiative_ops::to_proto(&guard.store, &view), changed))
}

fn ensure_current(
    path: &Path,
    ledger: &Path,
    registry: &Registry,
    name: &str,
    expected: &OutcomeSnapshot,
) -> Result<(), Status> {
    let store = CampaignStore::load(path).map_err(|error| Status::internal(error.to_string()))?;
    let initiative = store.initiative_view(name).map_err(initiative_ops::error)?;
    require_origin_trunks(&initiative, registry)?;
    let context =
        foundry_blocks::blocks::capture_initiative_external_context(&initiative, registry)
            .map_err(|error| Status::failed_precondition(error.to_string()))?;
    let actual = store
        .outcome_snapshot(name, context.repositories)
        .map_err(initiative_ops::error)?;
    require_trunk_checkouts(&actual)?;
    if &actual != expected {
        return Err(Status::failed_precondition(
            "outcome evidence is stale; steer again before completion",
        ));
    }
    let guard =
        CampaignStore::lock_exclusive(path).map_err(|error| Status::internal(error.to_string()))?;
    require_no_live_work(&guard.store, ledger, name)?;
    guard.store.validate_completion(name, &actual).map_err(initiative_ops::error)
}

fn entity_state<'a>(
    projection: &'a IntentProjection,
    kind: &str,
    id: &str,
) -> Result<&'a str, Status> {
    let state = if kind == "goal" {
        projection
            .goals
            .iter()
            .find(|entity| entity.id == id)
            .map(|entity| entity.state.as_str())
    } else {
        projection
            .interactions
            .iter()
            .find(|entity| entity.id == id)
            .map(|entity| entity.state.as_str())
    };
    state.ok_or_else(|| Status::failed_precondition("Tracker completion intent was removed"))
}

fn require_no_live_work(store: &CampaignStore, path: &Path, name: &str) -> Result<(), Status> {
    use foundry_sdk::work_source::WorkSourceKind;
    let initiative = store.initiative_view(name).map_err(initiative_ops::error)?;
    let _gate = foundry_sdk::work_item::ledger_write_gate()
        .lock()
        .map_err(|_| Status::internal("ledger gate poisoned"))?;
    let ledger = super::work_item_ops::load_store(path)?;
    if ledger.items.iter().any(|item| {
        (item.state.is_waiting() || item.occupies_slot())
            && (item.source.as_ref().is_some_and(|source| {
                source.kind == WorkSourceKind::Initiative && source.reference == name
            }) || item.campaign_name().is_some_and(|campaign| {
                initiative.members.iter().any(|member| member.name == campaign)
            }) || initiative
                .task_dispatches
                .iter()
                .any(|dispatch| dispatch.work_item == item.id)
                || item
                    .pending_root
                    .iter()
                    .chain(item.started_root.iter())
                    .any(|root| root.payload_str("initiative") == Some(name)))
    }) {
        return Err(Status::failed_precondition(
            "initiative work is queued, held or running; settle it before completion",
        ));
    }
    Ok(())
}

fn require_trunk_checkouts(snapshot: &OutcomeSnapshot) -> Result<(), Status> {
    if snapshot.repositories.iter().any(|repo| repo.head.trim() != repo.trunk.trim()) {
        return Err(Status::failed_precondition(
            "outcome completion must inspect each registered trunk checkout",
        ));
    }
    Ok(())
}

fn require_origin_trunks(initiative: &Initiative, registry: &Registry) -> Result<(), Status> {
    for name in &initiative.projects {
        let project = registry.find_project(name).ok_or_else(|| {
            Status::failed_precondition("initiative project is no longer registered")
        })?;
        let root = Path::new(&project.path);
        let fetched = std::process::Command::new("git")
            .args(["fetch", "--quiet", "origin"])
            .current_dir(root)
            .output()
            .map_err(|_| {
                Status::unavailable(
                    "cannot fetch initiative origin to verify current outcome evidence",
                )
            })?;
        if !fetched.status.success() {
            return Err(Status::unavailable(
                "initiative origin fetch failed; completion requires current trunk evidence",
            ));
        }
        let rev = |reference: String| -> Result<Vec<u8>, Status> {
            let output = std::process::Command::new("git")
                .args(["rev-parse", "--verify", "--end-of-options", &reference])
                .current_dir(root)
                .output()
                .map_err(|_| {
                    Status::failed_precondition("cannot inspect current initiative trunk")
                })?;
            if !output.status.success() {
                return Err(Status::failed_precondition(
                    "initiative origin does not contain its registered trunk",
                ));
            }
            Ok(output.stdout)
        };
        if rev(format!("refs/heads/{}", project.branch))?
            != rev(format!("refs/remotes/origin/{}", project.branch))?
        {
            return Err(Status::failed_precondition(
                "initiative origin has changed since outcome review; update the trunk and steer again",
            ));
        }
    }
    Ok(())
}

fn validate_initial_intent(
    initiative: &Initiative,
    projection: &IntentProjection,
) -> Result<(), Status> {
    let source = initiative
        .derive_source
        .as_ref()
        .ok_or_else(|| Status::internal("missing derivation source"))?;
    let derived = derive_from_projection(projection, &source.from, initiative.projects.clone())
        .map_err(initiative_ops::error)?;
    if derived.outcome != initiative.outcome
        || derived.members.iter().any(|current| {
            !initiative.members.iter().any(|saved| {
                saved.intent_ref == current.intent_ref
                    && saved.description == current.description
                    && saved.intent_state == current.intent_state
            })
        })
    {
        return Err(Status::failed_precondition(
            "Tracker intent changed since outcome review; steer again",
        ));
    }
    for member in &initiative.members {
        if member.intent_state.as_deref() == Some("revoked") {
            continue;
        }
        if let Some(reference) = &member.intent_ref {
            let (kind, id) = close_reference(reference)?;
            let state = entity_state(projection, kind, id)?;
            if member.intent_state.as_deref() != Some(state) {
                return Err(Status::failed_precondition(
                    "Tracker member intent changed or was withdrawn; steer again",
                ));
            }
        }
    }
    Ok(())
}

fn close_targets(
    initiative: &Initiative,
    projection: &IntentProjection,
) -> Result<Vec<String>, Status> {
    let mut targets = Vec::new();
    let proposal = initiative
        .completion_proposal
        .as_ref()
        .ok_or_else(|| Status::failed_precondition("outcome has not been reviewed"))?;
    for member in &initiative.members {
        if member.intent_state.as_deref() == Some("revoked") {
            continue;
        }
        let Some(reference) = &member.intent_ref else {
            continue;
        };
        let (kind, id) = close_reference(reference)?;
        let state = entity_state(projection, kind, id)?;
        if matches!(state, "create" | "update" | "delete") {
            if !proposal.reviews.iter().any(|review| {
                review.affirmed
                    && Some(&review.statement) == member.description.as_ref()
                    && !review.evidence.is_empty()
            }) {
                return Err(Status::failed_precondition(
                    "a Tracker interaction has no affirmed outcome review",
                ));
            }
            targets.push(reference.clone());
        } else if !matches!(state, "created" | "deleted") {
            return Err(Status::failed_precondition("Tracker interaction intent was withdrawn"));
        }
    }
    let source = initiative
        .derive_source
        .as_ref()
        .ok_or_else(|| Status::failed_precondition("derived initiative has no Tracker source"))?;
    let derived = derive_from_projection(projection, &source.from, initiative.projects.clone())
        .map_err(initiative_ops::error)?;
    let goal = derived
        .intent_refs
        .iter()
        .find(|reference| reference.starts_with("et:goal:"))
        .ok_or_else(|| Status::failed_precondition("derived initiative has no Tracker goal"))?;
    let (_, id) = close_reference(goal)?;
    if reality(entity_state(projection, "goal", id)?).is_none() {
        return Err(Status::failed_precondition("Tracker goal intent was withdrawn"));
    }
    targets.push(goal.clone());
    Ok(targets)
}

fn validate_retry_intent(
    baseline: &IntentProjection,
    current: &IntentProjection,
    targets: &[String],
) -> Result<(), Status> {
    let mut normalized = current.clone();
    for reference in targets {
        let (kind, id) = close_reference(reference)?;
        let prior = entity_state(baseline, kind, id)?;
        let state = entity_state(current, kind, id)?;
        if state != prior && Some(state) != reality(prior) {
            return Err(Status::failed_precondition("Tracker intent changed during completion"));
        }
        if kind == "goal" {
            if let Some(goal) = normalized.goals.iter_mut().find(|goal| goal.id == id) {
                goal.state = prior.into();
            }
        } else if let Some(interaction) =
            normalized.interactions.iter_mut().find(|interaction| interaction.id == id)
        {
            interaction.state = prior.into();
        }
    }
    // Ordering is immaterial, but every entity description and relationship is
    // part of the confirmation. A new approved child requires another review.
    let mut expected = baseline.clone();
    for projection in [&mut normalized, &mut expected] {
        projection.goals.sort_by(|a, b| a.id.cmp(&b.id));
        projection.interactions.sort_by(|a, b| a.id.cmp(&b.id));
        projection.journeys.sort_by(|a, b| a.id.cmp(&b.id));
    }
    if serde_json::to_value(normalized).map_err(|error| Status::internal(error.to_string()))?
        != serde_json::to_value(expected).map_err(|error| Status::internal(error.to_string()))?
    {
        return Err(Status::failed_precondition(
            "Tracker intent changed during completion; outcome remains uncompleted",
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "initiative_completion_tests.rs"]
mod tests;
