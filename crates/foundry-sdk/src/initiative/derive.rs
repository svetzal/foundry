//! Read-only derivation of staged initiative candidates from Epilogue Tracker.

use std::path::Path;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use super::{
    DoneEvidence, Initiative, InitiativeBudget, InitiativeError, InitiativeMember,
    InitiativeStatus, MemberKind,
};

/// Neutral committed projection accepted before consulting the et CLI.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntentProjection {
    pub goals: Vec<Goal>,
    pub interactions: Vec<Interaction>,
    pub journeys: Vec<Journey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    #[serde(default)]
    pub state: String,
    pub description: String,
    #[serde(default)]
    pub success_criteria: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interaction {
    pub id: String,
    pub description: String,
    pub state: String,
    #[serde(default)]
    pub priority: i64,
    #[serde(default)]
    pub goals: Vec<String>,
    #[serde(default)]
    pub goal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journey {
    pub id: String,
    pub goal: String,
    #[serde(default)]
    pub steps: Vec<String>,
}

/// Read committed `.et/projection.json` or `.alloy/projections/et.json`.
/// Without one, invoke only et's list commands at the repository root.
/// No credentials are read by Foundry and no et state is changed.
pub async fn derive(
    repo: &Path,
    from: &str,
    projects: Vec<String>,
) -> Result<Initiative, InitiativeError> {
    let projection = read_projection(repo).await?;
    let mut initiative = derive_from_projection(&projection, from, projects)?;
    initiative.derive_source = Some(super::DeriveSource {
        repo: repo.to_string_lossy().into_owned(),
        from: from.into(),
    });
    Ok(initiative)
}

/// Read current intent without changing Tracker state or exposing its credentials.
pub async fn read_projection(repo: &Path) -> Result<IntentProjection, InitiativeError> {
    let mut projection = None;
    for path in [
        ".et/projection.json",
        ".alloy/projections/et.json",
        ".alloy/projections/epilogue.json",
    ] {
        let output = Command::new("git")
            .args(["show", &format!("HEAD:{path}")])
            .current_dir(repo)
            .output()
            .await
            .map_err(|e| InitiativeError::Persistence(format!("inspect intent projection: {e}")))?;
        if output.status.success() {
            projection =
                Some(serde_json::from_slice::<IntentProjection>(&output.stdout).map_err(|e| {
                    InitiativeError::Invalid(format!(
                        "invalid committed intent projection '{path}': {e}"
                    ))
                })?);
            break;
        }
    }
    let projection = match projection {
        Some(projection) => projection,
        None => IntentProjection {
            goals: read_entities(repo, "goals").await?,
            interactions: read_entities(repo, "interactions").await?,
            journeys: read_entities(repo, "journeys").await?,
        },
    };
    Ok(projection)
}

async fn read_entities<T: serde::de::DeserializeOwned>(
    repo: &Path,
    kind: &str,
) -> Result<Vec<T>, InitiativeError> {
    let output = Command::new("et")
        .args(["list", kind, "--all", "--json"])
        .current_dir(repo)
        .output()
        .await
        .map_err(|e| InitiativeError::Persistence(format!("read et {kind}: {e}")))?;
    if !output.status.success() {
        return Err(InitiativeError::Persistence(format!(
            "et list {kind} failed ({})",
            output.status
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| InitiativeError::Invalid(format!("invalid et {kind} JSON: {e}")))
}

fn parse_reference(from: &str) -> Result<Vec<&str>, InitiativeError> {
    let parts: Vec<_> = from.split(':').collect();
    if parts.len() != 3
        || parts[0] != "et"
        || !matches!(parts[1], "goal" | "journey")
        || parts[2].is_empty()
    {
        return Err(InitiativeError::Invalid(
            "--from must be et:goal:<id> or et:journey:<id>".into(),
        ));
    }
    Ok(parts)
}

/// Convert current intent into candidates. Project assignment and audits await steering.
pub fn derive_from_projection(
    projection: &IntentProjection,
    from: &str,
    projects: Vec<String>,
) -> Result<Initiative, InitiativeError> {
    let parts = parse_reference(from)?;
    let selected_journey = if parts[1] == "journey" {
        Some(projection.journeys.iter().find(|j| j.id == parts[2]).ok_or_else(|| {
            InitiativeError::NotFound(format!("et journey '{}' not found", parts[2]))
        })?)
    } else {
        None
    };
    let goal_id = selected_journey.map_or(parts[2], |j| j.goal.as_str());
    let goal = projection
        .goals
        .iter()
        .find(|g| g.id == goal_id)
        .ok_or_else(|| InitiativeError::NotFound(format!("et goal '{goal_id}' not found")))?;
    let mut interactions: Vec<_> = projection
        .interactions
        .iter()
        .filter(|i| {
            matches!(i.state.as_str(), "create" | "update" | "delete")
                && (i.goals.iter().any(|g| g == goal_id) || i.goal.as_deref() == Some(goal_id))
                && selected_journey.is_none_or(|j| j.steps.contains(&i.id))
        })
        .collect();
    interactions.sort_by_key(|i| (i.priority, &i.id));
    let journeys: Vec<_> = projection
        .journeys
        .iter()
        .filter(|j| {
            j.goal == goal_id && selected_journey.is_none_or(|selected| selected.id == j.id)
        })
        .collect();
    let mut members: Vec<_> = interactions
        .iter()
        .map(|i| InitiativeMember {
            kind: MemberKind::Candidate,
            name: i.id.clone(),
            after: vec![],
            intent_ref: Some(format!("et:interaction:{}", i.id)),
            description: Some(i.description.clone()),
            intent_state: Some(i.state.clone()),
            audit_finding_id: None,
        })
        .collect();
    for journey in &journeys {
        let mut previous: Option<String> = None;
        for step in &journey.steps {
            if let Some(member) = members.iter_mut().find(|m| m.name == *step) {
                if let Some(previous) = &previous
                    && !member.after.contains(previous)
                {
                    member.after.push(previous.clone());
                }
                previous = Some(step.clone());
            }
        }
    }
    let mut intent_refs = vec![format!("et:goal:{goal_id}")];
    intent_refs.extend(interactions.iter().map(|i| format!("et:interaction:{}", i.id)));
    intent_refs.extend(journeys.iter().map(|j| format!("et:journey:{}", j.id)));
    let mut done_evidence = vec![];
    if !goal.success_criteria.trim().is_empty() {
        done_evidence.push(DoneEvidence::Review {
            statement: goal.success_criteria.clone(),
        });
    }
    done_evidence.extend(interactions.iter().map(|i| DoneEvidence::Review {
        statement: i.description.clone(),
    }));
    if done_evidence.is_empty() {
        done_evidence.push(DoneEvidence::Review {
            statement: goal.description.clone(),
        });
    }
    let outcome = if goal.success_criteria.trim().is_empty() {
        goal.description.clone()
    } else {
        format!("{}\nSuccess criteria: {}", goal.description, goal.success_criteria)
    };
    let initiative =
        derived_definition(parts[2], outcome, intent_refs, projects, members, done_evidence);
    initiative.validate()?;
    Ok(initiative)
}

fn derived_definition(
    name: &str,
    outcome: String,
    intent_refs: Vec<String>,
    projects: Vec<String>,
    members: Vec<InitiativeMember>,
    done_evidence: Vec<DoneEvidence>,
) -> Initiative {
    Initiative {
        name: name.into(),
        outcome,
        created_at: None,
        intent_refs,
        projects,
        brief: None,
        members,
        done_evidence,
        budget: InitiativeBudget::default(),
        authorized_by: None,
        agent_provider: None,
        providers: crate::gateway::StageProviders::default(),
        status: InitiativeStatus::Staged,
        owner_decisions: vec![],
        status_notes: vec![],
        open_questions: vec![],
        deferred_questions: vec![],
        derive_source: None,
        provider_pairings: vec![],
        holds: vec![],
        milestones: vec![],
        task_dispatches: vec![],
        reservations: vec![],
        steering: None,
        steering_history: vec![],
        pending_actions: vec![],
        close_proposals: vec![],
        steering_enabled: false,
        intent_revision: String::new(),
        work_admissions: vec![],
        pairing_base_cycles: None,
        integration_results: vec![],
        completion_proposal: None,
        completion_confirmation: None,
        completion_receipts: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection() -> IntentProjection {
        serde_json::from_value(serde_json::json!({"goals":[{"id":"outcome","description":"User succeeds","success_criteria":"User can finish"}],"interactions":[
            {"id":"second","description":"Second","state":"update","priority":2,"goals":["outcome"]},
            {"id":"first","description":"First","state":"create","priority":1,"goals":["outcome"]},
            {"id":"remove","description":"Remove old behavior","state":"delete","priority":3,"goals":["outcome"]},
            {"id":"done","description":"Already there","state":"created","priority":0,"goals":["outcome"]},
            {"id":"unapproved","description":"Not approved","state":"plan","priority":0,"goals":["outcome"]}
        ],"journeys":[{"id":"path","goal":"outcome","steps":["first","done","second"]}]})).unwrap()
    }

    #[test]
    fn goal_derivation_keeps_priority_intent_and_journey_dependencies() {
        let initiative =
            derive_from_projection(&projection(), "et:goal:outcome", vec!["p".into()]).unwrap();
        assert_eq!(
            initiative.members.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["first", "second", "remove"]
        );
        assert_eq!(initiative.members[1].after, ["first"]);
        assert!(initiative.members.iter().all(|m| m.kind == MemberKind::Candidate));
        assert!(initiative.outcome.contains("User can finish"));
        assert_eq!(initiative.done_evidence.len(), 4);
        assert!(initiative.intent_refs.contains(&"et:journey:path".into()));
    }

    #[test]
    fn journey_derivation_stages_only_its_intent_steps() {
        let initiative =
            derive_from_projection(&projection(), "et:journey:path", vec!["p".into()]).unwrap();
        assert_eq!(initiative.members.len(), 2);
        assert_eq!(initiative.name, "path");
        assert!(derive_from_projection(&projection(), "goal:outcome", vec!["p".into()]).is_err());
        assert!(
            derive_from_projection(&projection(), "et:goal:missing", vec!["p".into()]).is_err()
        );
    }

    #[tokio::test]
    async fn committed_projection_is_used_even_without_et_configuration() {
        let repo = tempfile::tempdir().unwrap();
        for args in [
            &["init"][..],
            &["config", "user.name", "Test"],
            &["config", "user.email", "test@example.invalid"],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(repo.path())
                    .output()
                    .await
                    .unwrap()
                    .status
                    .success()
            );
        }
        std::fs::create_dir(repo.path().join(".et")).unwrap();
        std::fs::write(
            repo.path().join(".et/projection.json"),
            serde_json::to_vec(&projection()).unwrap(),
        )
        .unwrap();
        assert!(
            Command::new("git")
                .args(["add", ".et/projection.json"])
                .current_dir(repo.path())
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        assert!(
            Command::new("git")
                .args(["commit", "-m", "Project intent"])
                .current_dir(repo.path())
                .output()
                .await
                .unwrap()
                .status
                .success()
        );
        // An uncommitted edit must not alter the committed source of intent.
        std::fs::write(repo.path().join(".et/projection.json"), "invalid working tree").unwrap();
        assert_eq!(
            derive(repo.path(), "et:goal:outcome", vec!["p".into()])
                .await
                .unwrap()
                .members
                .len(),
            3
        );
    }
}
