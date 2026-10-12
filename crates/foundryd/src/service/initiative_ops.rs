//! Typed initiative control plane over the campaign store and its guard.

use std::path::Path;

use chrono::Utc;
use foundry_sdk::campaign::{CampaignStore, CampaignStoreGuard};
use foundry_sdk::gateway::StageProviders;
use foundry_sdk::initiative::{Finding, Initiative, InitiativeError};
use foundry_sdk::payload::TaskVerdict;
use tonic::Status;

use crate::proto;

pub(super) fn error(error: InitiativeError) -> Status {
    match error {
        InitiativeError::NotFound(message) => Status::not_found(message),
        InitiativeError::AlreadyExists(message) => Status::already_exists(message),
        InitiativeError::Invalid(message) => Status::invalid_argument(message),
        InitiativeError::Precondition(message) => Status::failed_precondition(message),
        InitiativeError::Persistence(message) => Status::internal(message),
    }
}

fn load(path: &Path) -> Result<CampaignStore, Status> {
    CampaignStore::load(path).map_err(|error| Status::internal(error.to_string()))
}

fn lock(path: &Path) -> Result<CampaignStoreGuard, Status> {
    CampaignStore::lock_exclusive(path).map_err(|error| Status::internal(error.to_string()))
}

pub(crate) fn providers_to_proto(providers: StageProviders) -> proto::StageProviders {
    proto::StageProviders {
        formation: providers.formation.map(|provider| provider.to_string()),
        execution: providers.execution.map(|provider| provider.to_string()),
        review: providers.review.map(|provider| provider.to_string()),
    }
}

pub(super) fn to_proto(store: &CampaignStore, initiative: &Initiative) -> proto::Initiative {
    proto::Initiative {
        name: initiative.name.clone(),
        outcome: initiative.outcome.clone(),
        intent_refs: initiative.intent_refs.clone(),
        projects: initiative.projects.clone(),
        brief: initiative.brief.as_ref().map(brief_to_proto),
        members: initiative
            .members
            .iter()
            .map(|member| {
                let campaign = (member.kind == foundry_sdk::initiative::MemberKind::Campaign)
                    .then(|| store.find(&member.name))
                    .flatten();
                let verdict = campaign.and_then(|campaign| {
                    campaign.pending_run_result.as_ref().map(|result| &result.verdict).or_else(
                        || {
                            campaign.objective_history.iter().rev().find_map(|cycle| {
                                cycle.outcome.as_ref().map(|outcome| &outcome.verdict)
                            })
                        },
                    )
                });
                proto::InitiativeMember {
                    kind: member.kind.to_string(),
                    name: member.name.clone(),
                    after: member.after.clone(),
                    intent_ref: member.intent_ref.clone(),
                    description: member.description.clone(),
                    campaign: campaign.map(super::campaign_ops::campaign_to_proto),
                    last_verdict: verdict.map(|verdict| verdict.tag().to_string()),
                    reviewer_gaps: match verdict {
                        Some(TaskVerdict::Remainder { gaps }) => gaps.clone(),
                        Some(TaskVerdict::Defect { diagnosis }) => vec![diagnosis.clone()],
                        _ => vec![],
                    },
                }
            })
            .collect(),
        done_evidence: initiative
            .done_evidence
            .iter()
            .map(|evidence| match evidence {
                foundry_sdk::initiative::DoneEvidence::Gate {
                    command,
                    host,
                    required,
                    artifacts,
                } => proto::InitiativeEvidence {
                    kind: "gate".into(),
                    command: command.clone(),
                    host: host.clone(),
                    required: *required,
                    artifacts: artifacts.clone(),
                    statement: String::new(),
                },
                foundry_sdk::initiative::DoneEvidence::Review { statement } => {
                    proto::InitiativeEvidence {
                        kind: "review".into(),
                        command: String::new(),
                        host: None,
                        required: false,
                        artifacts: vec![],
                        statement: statement.clone(),
                    }
                }
            })
            .collect(),
        budget: Some(proto::InitiativeBudget {
            max_cycles: initiative.budget.max_cycles,
            max_active_members: u64::try_from(initiative.budget.max_active_members)
                .unwrap_or(u64::MAX),
            max_open_questions: u64::try_from(initiative.budget.max_open_questions)
                .unwrap_or(u64::MAX),
        }),
        authorized_by: initiative.authorized_by.clone(),
        providers: Some(providers_to_proto(initiative.providers)),
        status: initiative.status.to_string(),
        owner_decisions: initiative
            .owner_decisions
            .iter()
            .map(|decision| proto::InitiativeDecision {
                decision: decision.decision.clone(),
                authorized_by: decision.authorized_by.clone(),
                decided_at: decision.decided_at.to_rfc3339(),
                members: decision.members.clone(),
            })
            .collect(),
        open_questions: initiative
            .open_questions
            .iter()
            .map(|question| proto::InitiativeQuestion {
                id: question.id.clone(),
                question: question.question.clone(),
                members: question.members.clone(),
                opened_at: question.opened_at.to_rfc3339(),
            })
            .collect(),
        agent_provider: initiative.agent_provider.clone(),
    }
}

fn brief_to_proto(brief: &foundry_sdk::initiative::InitiativeBrief) -> proto::InitiativeBrief {
    proto::InitiativeBrief {
        path: brief.path.clone(),
        hash: brief.hash.clone(),
    }
}

pub(super) fn finding_to_proto(finding: &Finding) -> proto::Finding {
    proto::Finding {
        id: finding.id.clone(),
        initiative: finding.initiative.clone(),
        from: Some(proto::FindingSource {
            kind: finding.from.kind.clone(),
            id: finding.from.id.clone(),
        }),
        to: finding.to.clone(),
        kind: finding.kind.to_string(),
        text: finding.text.clone(),
        refs: finding.refs.clone(),
        consumed_by: finding
            .consumed_by
            .iter()
            .map(|consumed| proto::FindingConsumption {
                member: consumed.member.clone(),
                work_item: consumed.work_item.clone(),
            })
            .collect(),
        recorded_at: finding.recorded_at.to_rfc3339(),
    }
}

pub(super) fn get(path: &Path, name: &str) -> Result<proto::Initiative, Status> {
    let store = load(path)?;
    let initiative = store.initiative_view(name).map_err(error)?;
    Ok(to_proto(&store, &initiative))
}

pub(super) fn list(path: &Path) -> Result<Vec<proto::Initiative>, Status> {
    let store = load(path)?;
    store
        .initiatives
        .iter()
        .map(|initiative| {
            let view = store.initiative_view(&initiative.name).map_err(error)?;
            Ok(to_proto(&store, &view))
        })
        .collect()
}

pub(super) fn add(
    path: &Path,
    projects: &[String],
    json: &str,
) -> Result<proto::Initiative, Status> {
    let initiative: Initiative = serde_json::from_str(json).map_err(|error| {
        Status::invalid_argument(format!("invalid initiative definition: {error}"))
    })?;
    let name = initiative.name.clone();
    let mut guard = lock(path)?;
    guard.store.add_initiative(initiative, projects).map_err(error)?;
    guard.save().map_err(|error| Status::internal(error.to_string()))?;
    let view = guard.store.initiative_view(&name).map_err(error)?;
    Ok(to_proto(&guard.store, &view))
}

pub(super) fn decide(
    path: &Path,
    name: &str,
    decision: &str,
    members: &[String],
) -> Result<proto::Initiative, Status> {
    let mut guard = lock(path)?;
    guard
        .store
        .decide_initiative(name, decision, members, Utc::now())
        .map_err(error)?;
    guard.save().map_err(|error| Status::internal(error.to_string()))?;
    let view = guard.store.initiative_view(name).map_err(error)?;
    Ok(to_proto(&guard.store, &view))
}

pub(super) fn end(
    path: &Path,
    name: &str,
    reason: &str,
    complete: bool,
) -> Result<(proto::Initiative, bool), Status> {
    let mut guard = lock(path)?;
    let transition = if complete {
        guard.store.complete_initiative(name, reason, Utc::now()).map_err(error)?
    } else {
        guard.store.cancel_initiative(name, reason, Utc::now()).map_err(error)?
    };
    let changed = transition == foundry_sdk::campaign::Transition::Applied;
    if changed {
        guard.save().map_err(|error| Status::internal(error.to_string()))?;
    }
    let view = guard.store.initiative_view(name).map_err(error)?;
    Ok((to_proto(&guard.store, &view), changed))
}

pub(super) fn set_brief(
    path: &Path,
    name: &str,
    content: &str,
) -> Result<proto::InitiativeBriefResponse, Status> {
    let mut guard = lock(path)?;
    let home = path
        .parent()
        .ok_or_else(|| Status::internal("campaign store has no home directory"))?;
    let brief = guard.set_initiative_brief(name, home, content).map_err(error)?;
    Ok(proto::InitiativeBriefResponse {
        brief: Some(brief_to_proto(&brief)),
        content: content.to_string(),
    })
}

pub(super) fn get_brief(path: &Path, name: &str) -> Result<proto::InitiativeBriefResponse, Status> {
    let guard = lock(path)?;
    let store = &guard.store;
    let initiative = store
        .find_initiative(name)
        .ok_or_else(|| Status::not_found(format!("initiative '{name}' not found")))?;
    let content = store.show_brief(name).map_err(error)?;
    Ok(proto::InitiativeBriefResponse {
        brief: initiative.brief.as_ref().map(brief_to_proto),
        content,
    })
}

pub(super) fn add_finding(path: &Path, json: &str) -> Result<proto::Finding, Status> {
    let finding: Finding = serde_json::from_str(json).map_err(|error| {
        Status::invalid_argument(format!("invalid finding definition: {error}"))
    })?;
    let mut guard = lock(path)?;
    let finding = guard.store.add_finding(finding).map_err(error)?;
    guard.save().map_err(|error| Status::internal(error.to_string()))?;
    Ok(finding_to_proto(&finding))
}

pub(super) fn list_findings(
    path: &Path,
    initiative: &str,
    member: &str,
) -> Result<Vec<proto::Finding>, Status> {
    let store = load(path)?;
    let findings = store.list_findings(initiative).map_err(error)?;
    Ok(findings
        .iter()
        .filter(|finding| {
            member.is_empty()
                || finding.to.is_empty()
                || finding.to.iter().any(|target| target == member)
        })
        .map(finding_to_proto)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn definition() -> String {
        json!({"name":"test","outcome":"A test outcome","projects":["sample"],
            "authorized_by":"Stacey","members":[],"done_evidence":[{"kind":"review","statement":"Test outcome exists"}]}).to_string()
    }

    #[test]
    fn add_and_decide_return_the_persisted_definition() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("campaigns.json");
        let result = add(&path, &["sample".into()], &definition()).unwrap();
        assert_eq!(result.name, "test");
        let result = decide(&path, "test", "Use the current contract", &[]).unwrap();
        assert_eq!(result.owner_decisions[0].decision, "Use the current contract");
        assert_eq!(get(&path, "test").unwrap().owner_decisions, result.owner_decisions);
    }

    #[test]
    fn validation_and_save_failures_do_not_claim_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("campaigns.json");
        assert_eq!(
            add(&path, &[], &definition()).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
        assert!(!path.exists());
        add(&path, &["sample".into()], &definition()).unwrap();
        let before = std::fs::read(&path).unwrap();
        std::fs::create_dir(path.with_extension("json.tmp")).unwrap();
        assert_eq!(decide(&path, "test", "Answer", &[]).unwrap_err().code(), tonic::Code::Internal);
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn unknown_and_duplicate_records_have_typed_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("campaigns.json");
        assert_eq!(get(&path, "absent").unwrap_err().code(), tonic::Code::NotFound);
        add(&path, &["sample".into()], &definition()).unwrap();
        assert_eq!(
            add(&path, &["sample".into()], &definition()).unwrap_err().code(),
            tonic::Code::AlreadyExists
        );
        assert_eq!(
            decide(&path, "test", " ", &[]).unwrap_err().code(),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn terminal_retries_neither_save_again_nor_duplicate_owner_decisions() {
        for complete in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("campaigns.json");
            add(&path, &["sample".into()], &definition()).unwrap();
            let (first, changed) =
                end(&path, "test", "Owner verified the outcome", complete).unwrap();
            assert!(changed);
            let bytes = std::fs::read(&path).unwrap();
            // A retry must succeed even if an unnecessary save would fail.
            std::fs::create_dir(path.with_extension("json.tmp")).unwrap();
            let (second, changed) = end(&path, "test", "Repeated reason", complete).unwrap();
            assert!(!changed);
            assert_eq!(second.status, first.status);
            assert_eq!(second.owner_decisions, first.owner_decisions);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn brief_reads_wait_for_the_matching_store_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("campaigns.json");
        add(&path, &["sample".into()], &definition()).unwrap();
        set_brief(&path, "test", "Original").unwrap();
        let mut guard = lock(&path).unwrap();
        let brief = guard.store.set_brief("test", dir.path(), "Replacement").unwrap();
        let read_path = path.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            result_tx.send(get_brief(&read_path, "test")).unwrap();
        });
        started_rx.recv().unwrap();
        assert!(matches!(
            result_rx.recv_timeout(std::time::Duration::from_millis(20)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        guard.save().unwrap();
        drop(guard);
        let result = result_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap();
        assert_eq!(result.content, "Replacement");
        assert_eq!(result.brief.unwrap().hash, brief.hash);
        reader.join().unwrap();
    }
}
