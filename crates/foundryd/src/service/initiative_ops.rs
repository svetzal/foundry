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
    let mut response = proto::Initiative {
        name: initiative.name.clone(),
        outcome: initiative.outcome.clone(),
        intent_refs: initiative.intent_refs.clone(),
        projects: initiative.projects.clone(),
        brief: initiative.brief.as_ref().map(brief_to_proto),
        members: initiative.members.iter().map(|member| member_to_proto(store, member)).collect(),
        done_evidence: initiative.done_evidence.iter().map(evidence_to_proto).collect(),
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
        deferred_question_count: u64::try_from(initiative.deferred_questions.len())
            .unwrap_or(u64::MAX),
        agent_provider: initiative.agent_provider.clone(),
        ..Default::default()
    };
    set_steering_state(&mut response, initiative);
    set_steering_receipts(&mut response, initiative);
    set_outcome_receipts(&mut response, initiative);
    response
}

fn outcome_snapshot_to_proto(
    snapshot: &foundry_sdk::initiative::OutcomeSnapshot,
) -> proto::InitiativeOutcomeSnapshot {
    proto::InitiativeOutcomeSnapshot {
        repositories: snapshot
            .repositories
            .iter()
            .map(|repo| proto::InitiativeRepositoryIdentity {
                project: repo.project.clone(),
                root: repo.root.clone(),
                registry: repo.registry.clone(),
                trunk: repo.trunk.clone(),
                head: repo.head.clone(),
                status: repo.status.clone(),
                diff: repo.diff.clone(),
            })
            .collect(),
        intent_revision: snapshot.intent_revision.clone(),
        policy_hash: snapshot.policy_hash.clone(),
    }
}

fn set_outcome_receipts(response: &mut proto::Initiative, initiative: &Initiative) {
    response.integration_results = initiative
        .integration_results
        .iter()
        .map(|result| proto::InitiativeIntegrationResult {
            id: result.id.clone(),
            gate_index: u64::try_from(result.gate_index).unwrap_or(u64::MAX),
            host: result.host.clone(),
            command: result.command.clone(),
            snapshot: Some(outcome_snapshot_to_proto(&result.snapshot)),
            passed: result.passed,
            exit_code: result.exit_code,
            report: result.report.clone(),
            artifacts: result.artifacts.clone(),
            recorded_at: result.recorded_at.to_rfc3339(),
        })
        .collect();
    response.completion_proposal = initiative.completion_proposal.as_ref().map(|proposal| {
        proto::InitiativeCompletionProposal {
            id: proposal.id.clone(),
            snapshot: Some(outcome_snapshot_to_proto(&proposal.snapshot)),
            reviews: proposal
                .reviews
                .iter()
                .map(|review| proto::InitiativeOutcomeReview {
                    statement: review.statement.clone(),
                    affirmed: review.affirmed,
                    evidence: review.evidence.clone(),
                })
                .collect(),
            proposed_at: proposal.proposed_at.to_rfc3339(),
        }
    });
    response.completion_confirmation =
        initiative.completion_confirmation.as_ref().map(|confirmation| {
            proto::InitiativeCompletionConfirmation {
                proposal_id: confirmation.proposal_id.clone(),
                reason: confirmation.reason.clone(),
                confirmed_at: confirmation.confirmed_at.to_rfc3339(),
                last_error: confirmation.last_error.clone(),
            }
        });
    response.completion_receipts = initiative
        .completion_receipts
        .iter()
        .map(|receipt| proto::InitiativeEtCloseReceipt {
            intent_ref: receipt.intent_ref.clone(),
            completed_at: receipt.completed_at.to_rfc3339(),
            proposal_id: receipt.proposal_id.clone(),
        })
        .collect();
}
fn set_steering_state(response: &mut proto::Initiative, initiative: &Initiative) {
    response.derive_source = initiative.derive_source.as_ref().map(|source| proto::DeriveSource {
        repo: source.repo.clone(),
        from: source.from.clone(),
    });
    response.provider_pairings =
        initiative.provider_pairings.iter().copied().map(providers_to_proto).collect();
    response.holds = initiative
        .holds
        .iter()
        .map(|hold| proto::MemberHold {
            member: hold.member.clone(),
            until: hold.until.clone(),
            reason: hold.reason.clone(),
            owner: hold.owner,
        })
        .collect();
    response.milestones = initiative
        .milestones
        .iter()
        .map(|milestone| proto::MemberMilestone {
            member: milestone.member.clone(),
            name: milestone.name.clone(),
            evidence: milestone.evidence.clone(),
        })
        .collect();
    response.task_dispatches = initiative
        .task_dispatches
        .iter()
        .map(|task| proto::TaskDispatch {
            member: task.member.clone(),
            work_item: task.work_item.clone(),
            project: task.project.clone(),
        })
        .collect();
    response.reservations = initiative
        .reservations
        .iter()
        .map(|reservation| proto::MemberReservation {
            member: reservation.member.clone(),
            operation_id: reservation.operation_id.clone(),
            kind: match reservation.kind {
                foundry_sdk::initiative::ReservationKind::Formation => "formation",
                foundry_sdk::initiative::ReservationKind::Task => "task",
            }
            .into(),
        })
        .collect();
    response.steering = initiative.steering.as_ref().map(|lease| proto::SteeringLease {
        id: lease.id.clone(),
        fingerprint: lease.fingerprint.clone(),
    });
    response.close_proposals = initiative
        .close_proposals
        .iter()
        .map(|proposal| proto::EtCloseProposal {
            intent_ref: proposal.intent_ref.clone(),
            evidence: proposal.evidence.clone(),
        })
        .collect();
}
fn set_steering_receipts(response: &mut proto::Initiative, initiative: &Initiative) {
    response.steering_enabled = initiative.steering_enabled;
    response.steering_history = initiative
        .steering_history
        .iter()
        .rev()
        .take(1)
        .map(|record| proto::SteeringRecord {
            id: record.id.clone(),
            prompt: record.prompt.clone(),
            decision_json: serde_json::to_string(&record.decision).unwrap_or_default(),
            provider: record.provider.map(|provider| provider.to_string()),
            recorded_at: record.recorded_at.to_rfc3339(),
            follow_up_of_json: record
                .follow_up_of
                .as_ref()
                .and_then(|cause| serde_json::to_string(cause).ok()),
        })
        .collect();
    response.pending_actions = initiative
        .pending_actions
        .iter()
        .map(|action| proto::DurableSteeringAction {
            id: action.id.clone(),
            initiative: action.initiative.clone(),
            decision_json: serde_json::to_string(&action.decision).unwrap_or_default(),
            providers: Some(providers_to_proto(action.providers)),
            fingerprint: action.fingerprint.clone(),
            external_context_json: action
                .external_context
                .as_ref()
                .map(|context| serde_json::to_string(context).unwrap_or_default()),
        })
        .collect();
    response.intent_revision.clone_from(&initiative.intent_revision);
    response.work_admissions = initiative
        .work_admissions
        .iter()
        .map(|admission| proto::InitiativeWorkAdmission {
            action_id: admission.action_id.clone(),
            member: admission.member.clone(),
            providers: Some(providers_to_proto(admission.providers)),
            work_item: admission.work_item.clone(),
        })
        .collect();
    response.pairing_base_cycles = initiative.pairing_base_cycles;
    response.steering_history_count =
        u64::try_from(initiative.steering_history.len()).unwrap_or(u64::MAX);
}
fn member_to_proto(
    store: &CampaignStore,
    member: &foundry_sdk::initiative::InitiativeMember,
) -> proto::InitiativeMember {
    let campaign = (member.kind == foundry_sdk::initiative::MemberKind::Campaign)
        .then(|| store.find(&member.name))
        .flatten();
    let verdict = campaign.and_then(|campaign| {
        campaign.pending_run_result.as_ref().map(|result| &result.verdict).or_else(|| {
            campaign
                .objective_history
                .iter()
                .rev()
                .find_map(|cycle| cycle.outcome.as_ref().map(|outcome| &outcome.verdict))
        })
    });
    proto::InitiativeMember {
        kind: member.kind.to_string(),
        name: member.name.clone(),
        after: member.after.clone(),
        intent_ref: member.intent_ref.clone(),
        description: member.description.clone(),
        intent_state: member.intent_state.clone(),
        audit_finding_id: member.audit_finding_id.clone(),
        project: campaign.map(|campaign| campaign.project.clone()),
        campaign: campaign.map(super::campaign_ops::campaign_to_proto),
        last_verdict: verdict.map(|verdict| verdict.tag().to_string()),
        reviewer_gaps: match verdict {
            Some(TaskVerdict::Remainder { gaps }) => gaps.clone(),
            Some(TaskVerdict::Defect { diagnosis }) => vec![diagnosis.clone()],
            _ => vec![],
        },
    }
}
fn evidence_to_proto(
    evidence: &foundry_sdk::initiative::DoneEvidence,
) -> proto::InitiativeEvidence {
    match evidence {
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
        foundry_sdk::initiative::DoneEvidence::Review { statement } => proto::InitiativeEvidence {
            kind: "review".into(),
            command: String::new(),
            host: None,
            required: false,
            artifacts: vec![],
            statement: statement.clone(),
        },
    }
}

pub(super) fn integration_gates_from_proto(
    gates: Vec<proto::InitiativeEvidence>,
) -> Result<Vec<foundry_sdk::initiative::DoneEvidence>, Status> {
    gates.into_iter().map(|gate| {
        if gate.kind != "gate" || !gate.statement.is_empty() || gate.command.trim().is_empty() {
            return Err(Status::invalid_argument("integration_gates accepts only gate declarations with a command; Tracker supplies outcome reviews"));
        }
        Ok(foundry_sdk::initiative::DoneEvidence::Gate { command:gate.command, host:gate.host, required:gate.required, artifacts:gate.artifacts })
    }).collect()
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
        audit: finding.audit.as_ref().map(|audit| proto::AuditEvidence {
            intent_ref: audit.intent_ref.clone(),
            classification: match audit.classification {
                foundry_sdk::initiative::AuditClassification::Realised => "realised",
                foundry_sdk::initiative::AuditClassification::Partial => "partial",
                foundry_sdk::initiative::AuditClassification::Absent => "absent",
                foundry_sdk::initiative::AuditClassification::Misplaced => "misplaced",
            }
            .into(),
            project: audit.project.clone(),
            paths: audit.paths.clone(),
            explanation: audit.explanation.clone(),
        }),
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
    add_cycles: u64,
) -> Result<proto::Initiative, Status> {
    let mut guard = lock(path)?;
    guard
        .store
        .decide_initiative_with_budget(name, decision, members, add_cycles, Utc::now())
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
        let result = decide(&path, "test", "Use the current contract", &[], 0).unwrap();
        assert_eq!(result.owner_decisions[0].decision, "Use the current contract");
        assert_eq!(get(&path, "test").unwrap().owner_decisions, result.owner_decisions);
    }

    #[test]
    fn wire_history_is_bounded_and_reports_the_complete_receipt_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("campaigns.json");
        add(&path, &["sample".into()], &definition()).unwrap();
        let mut guard = lock(&path).unwrap();
        for number in 0..3 {
            let id = format!("owner_{number}");
            guard
                .store
                .commit_owner_action(
                    "test",
                    &id,
                    foundry_sdk::initiative::SteeringDecision::Ask {
                        question: format!("Question {number}"),
                        members: vec![],
                    },
                    format!("Prompt {number}"),
                )
                .unwrap();
            guard.store.finish_steering_action("test", &id).unwrap();
        }
        guard.save().unwrap();
        drop(guard);
        let response = get(&path, "test").unwrap();
        assert_eq!(response.steering_history_count, 3);
        assert_eq!(response.steering_history.len(), 1);
        assert_eq!(response.steering_history[0].prompt, "Prompt 2");
        assert_eq!(load(&path).unwrap().find_initiative("test").unwrap().steering_history.len(), 3);
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
        assert_eq!(
            decide(&path, "test", "Answer", &[], 0).unwrap_err().code(),
            tonic::Code::Internal
        );
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
            decide(&path, "test", " ", &[], 0).unwrap_err().code(),
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

/// Canonical repository supplying intent. It need not be a writable target.
pub(super) async fn canonical_repository(path: &str) -> Result<std::path::PathBuf, Status> {
    if path.is_empty() {
        return Err(Status::invalid_argument("Tracker source repository is required"));
    }
    let path = tokio::fs::canonicalize(path)
        .await
        .map_err(|error| Status::invalid_argument(format!("Tracker source repository: {error}")))?;
    let output = tokio::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(&path)
        .output()
        .await
        .map_err(|error| Status::internal(format!("inspect Tracker repository: {error}")))?;
    if !output.status.success() {
        return Err(Status::invalid_argument("Tracker source must be a Git repository"));
    }
    let root = std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    tokio::fs::canonicalize(root)
        .await
        .map_err(|error| Status::invalid_argument(format!("Tracker repository root: {error}")))
}

fn steering_work_in_flight(
    name: &str,
    ledger: &foundry_sdk::work_item::WorkItemStore,
    tracked: &[crate::workflow_tracker::ActiveWorkflow],
) -> Option<proto::CampaignWorkInFlight> {
    use foundry_sdk::event::EventType;
    for item in &ledger.items {
        if !item.is_running() && !item.state.is_waiting() {
            continue;
        }
        let Some(root) = item.pending_root.as_ref().or(item.started_root.as_ref()) else {
            continue;
        };
        if root.event_type == EventType::InitiativeAdvanceRequested
            && root.payload_str("initiative") == Some(name)
        {
            return Some(proto::CampaignWorkInFlight {
                kind: "steer".into(),
                work_item_id: item.id.clone(),
                state: item.state.tag().into(),
                event_id: root.id.clone(),
            });
        }
    }
    tracked
        .iter()
        .find(|workflow| {
            workflow.initiative.as_deref() == Some(name)
                && workflow.event_type == EventType::InitiativeAdvanceRequested.to_string()
        })
        .map(|workflow| proto::CampaignWorkInFlight {
            kind: "steer".into(),
            work_item_id: String::new(),
            state: "submitted".into(),
            event_id: workflow.event_id.clone(),
        })
}

/// Keep store validation and tracker insertion in the same critical section.
pub(super) fn advance(
    path: &Path,
    work_items: &Path,
    ctx: &super::RuntimeContext,
    request: proto::AdvanceInitiativeRequest,
) -> Result<proto::AdvanceInitiativeResponse, Status> {
    use foundry_sdk::event::{Event, EventType};
    use foundry_sdk::initiative::InitiativeStatus;
    let mut guard = lock(path)?;
    let initiative = guard
        .store
        .find_initiative(&request.name)
        .ok_or_else(|| Status::not_found(format!("initiative '{}' not found", request.name)))?;
    if matches!(initiative.status, InitiativeStatus::Completed | InitiativeStatus::Cancelled) {
        return Err(Status::failed_precondition("a terminal initiative cannot steer"));
    }
    if initiative.authorized_by.as_deref().is_none_or(|owner| owner.trim().is_empty()) {
        return Err(Status::failed_precondition("authorize the initiative before steering"));
    }
    if let Some(member) = &request.member
        && !initiative.members.iter().any(|candidate| &candidate.name == member)
    {
        return Err(Status::invalid_argument(format!("unknown member '{member}'")));
    }
    let ledger = {
        let _gate = foundry_sdk::work_item::ledger_write_gate()
            .lock()
            .map_err(|_| Status::internal("ledger write gate poisoned"))?;
        super::work_item_ops::load_store(work_items)?
    };
    if let Some(in_flight) =
        steering_work_in_flight(&request.name, &ledger, &ctx.workflow_tracker.list())
    {
        if request.member.is_some() || request.operator_origin.contains(": ") {
            return Err(Status::failed_precondition(
                "a steer is already in flight; wait before submitting a member override or new owner context",
            ));
        }
        let view = guard.store.initiative_view(&request.name).map_err(error)?;
        return Ok(proto::AdvanceInitiativeResponse {
            initiative: Some(to_proto(&guard.store, &view)),
            event_id: String::new(),
            in_flight: Some(in_flight),
        });
    }
    if let Some(member) = &request.member {
        guard.store.release_member(&request.name, member).map_err(error)?;
    }
    guard.store.enable_steering(&request.name).map_err(error)?;
    guard.save().map_err(|error| Status::internal(error.to_string()))?;
    let view = guard.store.initiative_view(&request.name).map_err(error)?;
    let event = Event::new(
        EventType::InitiativeAdvanceRequested,
        "system".into(),
        foundry_sdk::throttle::Throttle::Full,
        Event::serialize_payload(&foundry_sdk::payload::InitiativeAdvanceRequestedPayload {
            initiative: request.name.clone(),
            member: request.member.clone(),
            operator_origin: Some(request.operator_origin.clone()).filter(|text| !text.is_empty()),
            trigger_id: None,
            follow_up_of: None,
        })
        .map_err(|error| Status::internal(error.to_string()))?,
    )
    .with_source(Some(foundry_sdk::work_source::WorkSource::initiative(&request.name, 0)))
    .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
    .with_span_ids(Some(foundry_sdk::event::mint_span_id()), None);
    let event_id = event.id.clone();
    if let Some(member) = request.member {
        announce_owner_control(
            ctx,
            &request.name,
            &event_id,
            request.operator_origin,
            foundry_sdk::payload::InitiativeOwnerControlAction::MemberOverride { member },
        )?;
    }
    super::spawn_workflow(event, ctx);
    Ok(proto::AdvanceInitiativeResponse {
        initiative: Some(to_proto(&guard.store, &view)),
        event_id,
        in_flight: None,
    })
}

/// Owner actions share the persisted steering receipt and child admission path.
pub(super) fn act(
    path: &Path,
    ctx: &super::RuntimeContext,
    request: proto::ActOnInitiativeRequest,
) -> Result<proto::ActOnInitiativeResponse, Status> {
    use foundry_sdk::event::{Event, EventType};
    let action = request
        .action
        .ok_or_else(|| Status::invalid_argument("an owner action is required"))?;
    let mut guard = lock(path)?;
    let initiative = guard
        .store
        .find_initiative(&request.name)
        .ok_or_else(|| Status::not_found("initiative not found"))?;
    if matches!(
        initiative.status,
        foundry_sdk::initiative::InitiativeStatus::Completed
            | foundry_sdk::initiative::InitiativeStatus::Cancelled
    ) || initiative.authorized_by.as_deref().is_none_or(|owner| owner.trim().is_empty())
    {
        return Err(Status::failed_precondition(
            "owner actions require an authorized, nonterminal initiative",
        ));
    }
    if let Some(control) = apply_direct_owner_control(&mut guard.store, &request.name, &action)? {
        guard.save().map_err(|error| Status::internal(error.to_string()))?;
        let view = guard.store.initiative_view(&request.name).map_err(error)?;
        let id = format!("owner_{}", foundry_sdk::event::mint_span_id());
        let event_id =
            announce_owner_control(ctx, &request.name, &id, request.operator_origin, control)?;
        return Ok(proto::ActOnInitiativeResponse {
            initiative: Some(to_proto(&guard.store, &view)),
            event_id,
        });
    }
    let decision = owner_decision(&guard.store, &request.name, action)?;
    let id = format!("owner_{}", foundry_sdk::event::mint_span_id());
    let prompt = format!(
        "Owner action recorded by {}: {}",
        request.operator_origin,
        serde_json::to_string(&decision).map_err(|error| Status::internal(error.to_string()))?
    );
    let action = guard
        .store
        .commit_owner_action(&request.name, &id, decision.clone(), prompt.clone())
        .map_err(error)?;
    guard.save().map_err(|error| Status::internal(error.to_string()))?;
    let view = guard.store.initiative_view(&request.name).map_err(error)?;
    let mut receipt = Event::new(
        EventType::InitiativeSteered,
        "system".into(),
        foundry_sdk::throttle::Throttle::Full,
        Event::serialize_payload(&foundry_sdk::payload::InitiativeSteeredPayload {
            success: true,
            outcome: foundry_sdk::payload::SteeringOutcome::Applied,
            error: None,
            initiative: request.name.clone(),
            steer_id: id.clone(),
            prompt,
            decision: Some(decision),
            provider: None,
            action: Some(action.clone()),
            reason: None,
            eligible_members: vec![],
            follow_up_of: None,
        })
        .map_err(|error| Status::internal(error.to_string()))?,
    )
    .with_source(Some(foundry_sdk::work_source::WorkSource::initiative(&request.name, 0)));
    receipt.id = format!("evt_{id}_steered");
    super::spawn_workflow(receipt, ctx);
    let mut event = Event::new(EventType::InitiativeActionRequested, "system".into(), foundry_sdk::throttle::Throttle::Full,
        serde_json::json!({"action":action,"initiative":request.name,"operator_origin":request.operator_origin}))
        .with_source(Some(foundry_sdk::work_source::WorkSource::initiative(&request.name, 0)))
        .with_trace_id(Some(foundry_sdk::event::mint_trace_id()))
        .with_span_ids(Some(foundry_sdk::event::mint_span_id()), None);
    event.id = format!("evt_{id}_action");
    let event_id = event.id.clone();
    super::spawn_workflow(event, ctx);
    Ok(proto::ActOnInitiativeResponse {
        initiative: Some(to_proto(&guard.store, &view)),
        event_id,
    })
}

fn apply_direct_owner_control(
    store: &mut CampaignStore,
    name: &str,
    action: &proto::act_on_initiative_request::Action,
) -> Result<Option<foundry_sdk::payload::InitiativeOwnerControlAction>, Status> {
    use foundry_sdk::payload::InitiativeOwnerControlAction;
    use proto::act_on_initiative_request::Action;
    match action {
        Action::Hold(hold) if hold.until.is_empty() => {
            store.hold_member(name, &hold.member, &hold.reason).map_err(error)?;
            Ok(Some(InitiativeOwnerControlAction::Hold {
                member: hold.member.clone(),
                reason: hold.reason.clone(),
            }))
        }
        Action::Milestone(milestone) => {
            store
                .record_milestone(
                    name,
                    &milestone.member,
                    &milestone.milestone,
                    milestone.evidence.clone(),
                )
                .map_err(error)?;
            Ok(Some(InitiativeOwnerControlAction::Milestone {
                member: milestone.member.clone(),
                milestone: milestone.milestone.clone(),
                evidence: milestone.evidence.clone(),
            }))
        }
        _ => Ok(None),
    }
}
fn owner_decision(
    store: &CampaignStore,
    name: &str,
    action: proto::act_on_initiative_request::Action,
) -> Result<foundry_sdk::initiative::SteeringDecision, Status> {
    use foundry_sdk::initiative::SteeringDecision;
    use proto::act_on_initiative_request::Action;
    Ok(match action {
        Action::Hold(hold) => SteeringDecision::Hold {
            member: hold.member,
            until: hold.until,
            reason: hold.reason,
        },
        Action::Relay(relay) => {
            let finding = store
                .findings
                .iter()
                .find(|finding| finding.id == relay.finding_id && finding.initiative == name)
                .cloned()
                .ok_or_else(|| Status::not_found("finding does not belong to this initiative"))?;
            SteeringDecision::Relay {
                finding: Box::new(finding),
                to: relay.members,
            }
        }
        Action::Dispatch(dispatch) => SteeringDecision::Dispatch {
            project: dispatch.project,
            objective: dispatch.objective,
            member: dispatch.member,
        },
        Action::Ask(ask) => SteeringDecision::Ask {
            question: ask.question,
            members: ask.members,
        },
        Action::Milestone(_) => unreachable!("direct owner operation handled above"),
    })
}

pub(super) fn recover(
    path: &Path,
    ledger_path: &Path,
) -> Result<Vec<foundry_sdk::event::Event>, Status> {
    use foundry_sdk::event::{Event, EventType};
    let mut guard = lock(path)?;
    let _ledger_gate = foundry_sdk::work_item::ledger_write_gate()
        .lock()
        .map_err(|_| Status::internal("ledger write gate poisoned"))?;
    let mut ledger = super::work_item_ops::load_store(ledger_path)?;
    let live = ledger
        .items
        .iter()
        .filter(|item| item.state.is_waiting() || item.occupies_slot())
        .flat_map(|item| {
            std::iter::once(item.id.clone()).chain(
                item.pending_root.iter().chain(item.started_root.iter()).flat_map(|root| {
                    [
                        Some(root.id.clone()),
                        root.payload_str("steering_action_id").map(str::to_owned),
                    ]
                    .into_iter()
                    .flatten()
                }),
            )
        })
        .collect::<Vec<_>>();
    let names = guard
        .store
        .initiatives
        .iter()
        .map(|initiative| initiative.name.clone())
        .collect::<Vec<_>>();
    let mut replay = Vec::new();
    for name in names {
        let before = guard
            .store
            .find_initiative(&name)
            .ok_or_else(|| Status::not_found("initiative disappeared"))?
            .clone();
        guard.store.recover_steering(&name, &live).map_err(error)?;
        for action in &before.pending_actions {
            let child = ledger.items.iter().find(|item| {
                item.pending_root
                    .iter()
                    .chain(item.started_root.iter())
                    .any(|root| root.payload_str("steering_action_id") == Some(&action.id))
            });
            if let Some(child) = child {
                guard
                    .store
                    .acknowledge_steering_action(&name, &action.id, Some(&child.id))
                    .map_err(error)?;
            } else if !guard.store.pending_action_is_current(&name, &action.id).map_err(error)? {
                guard.store.discard_steering_action(&name, &action.id).map_err(error)?;
            } else if !matches!(
                before.status,
                foundry_sdk::initiative::InitiativeStatus::Completed
                    | foundry_sdk::initiative::InitiativeStatus::Cancelled
            ) {
                let mut event = Event::new(
                    EventType::InitiativeActionRequested,
                    "system".into(),
                    foundry_sdk::throttle::Throttle::Full,
                    Event::serialize_payload(
                        &foundry_sdk::payload::InitiativeActionRequestedPayload {
                            action: action.clone(),
                        },
                    )
                    .map_err(|error| Status::internal(error.to_string()))?,
                )
                .with_source(Some(foundry_sdk::work_source::WorkSource::initiative(&name, 0)))
                .with_trace_id(Some(foundry_sdk::event::mint_trace_id()));
                event.id = format!("evt_{}_action", action.id);
                replay.push(event);
            }
        }
        requeue_abandoned_steer(&before, &live, &mut ledger)?;
    }
    // A fresh queued steer exists before forgetting its abandoned lease.
    ledger.save(ledger_path).map_err(|error| Status::internal(error.to_string()))?;
    guard.save().map_err(|error| Status::internal(error.to_string()))?;
    Ok(replay)
}

fn requeue_abandoned_steer(
    before: &Initiative,
    live: &[String],
    ledger: &mut foundry_sdk::work_item::WorkItemStore,
) -> Result<(), Status> {
    use foundry_sdk::event::{Event, EventType};
    if let Some(lease) = before.steering.as_ref().filter(|lease| !live.contains(&lease.id))
        && before.steering_enabled
        && !matches!(
            before.status,
            foundry_sdk::initiative::InitiativeStatus::Completed
                | foundry_sdk::initiative::InitiativeStatus::Cancelled
        )
    {
        let mut event = Event::new(
            EventType::InitiativeAdvanceRequested,
            "system".into(),
            foundry_sdk::throttle::Throttle::Full,
            Event::serialize_payload(&foundry_sdk::payload::InitiativeAdvanceRequestedPayload {
                initiative: before.name.clone(),
                member: None,
                operator_origin: None,
                trigger_id: Some(lease.id.clone()),
                follow_up_of: None,
            })
            .map_err(|error| Status::internal(error.to_string()))?,
        )
        .with_source(Some(foundry_sdk::work_source::WorkSource::initiative(&before.name, 0)))
        .with_trace_id(Some(foundry_sdk::event::mint_trace_id()));
        event.id = format!("evt_{}_recovered_steer", lease.id);
        if !ledger.items.iter().any(|item| {
            item.pending_root
                .iter()
                .chain(item.started_root.iter())
                .any(|root| root.id == event.id || root.causation_id.as_deref() == Some(&event.id))
        }) {
            ledger.upsert(
                foundry_sdk::work_item::WorkItem::queued(
                    foundry_sdk::work_item::WorkItemSpec {
                        project: "system".into(),
                        objective: format!("Recover steering for {}", before.name),
                        kind: foundry_sdk::work_item::WorkItemKind::Workflow,
                        lane: foundry_sdk::work_item::WorkLane::Campaign,
                        origin: "initiative restart recovery".into(),
                        trace_id: event.trace_id.clone(),
                    },
                    event.clone(),
                    Utc::now(),
                )
                .with_source(event.source),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod steering_recovery_tests {
    use super::*;
    use foundry_sdk::initiative::{Initiative, SteeringDecision};

    fn stored_action(
        dir: &Path,
    ) -> (std::path::PathBuf, foundry_sdk::initiative::DurableSteeringAction) {
        let path = dir.join("campaigns.json");
        let mut store = CampaignStore::default();
        let initiative: Initiative = serde_json::from_value(serde_json::json!({"name":"shared","outcome":"A verified contract",
            "projects":["sample"],"authorized_by":"Stacey","members":[],"done_evidence":[{"kind":"review","statement":"The contract works"}]})).unwrap();
        store.add_initiative(initiative, &["sample".into()]).unwrap();
        let action = store
            .commit_owner_action(
                "shared",
                "owner_test",
                SteeringDecision::Dispatch {
                    project: "sample".into(),
                    objective: "Implement one contract".into(),
                    member: None,
                },
                "Owner requested one task".into(),
            )
            .unwrap();
        store.save(&path).unwrap();
        (path, action)
    }

    #[test]
    fn restart_replays_pending_actions_until_a_durable_child_acknowledges_them() {
        let dir = tempfile::tempdir().unwrap();
        let ledger_path = dir.path().join("work-items.json");
        let (path, action) = stored_action(dir.path());
        let replay = recover(&path, &ledger_path).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].id, "evt_owner_test_action");
        assert_eq!(recover(&path, &ledger_path).unwrap()[0].id, replay[0].id);
        let mut ledger = foundry_sdk::work_item::WorkItemStore::default();
        let root = foundry_sdk::event::Event::new(
            foundry_sdk::event::EventType::ExecutionRequested,
            "sample".into(),
            foundry_sdk::throttle::Throttle::Full,
            serde_json::json!({"initiative":"shared","steering_action_id":action.id,"prompt":"Implement one contract","workflow":"task"}),
        );
        let child = foundry_sdk::work_item::WorkItem::queued(
            foundry_sdk::work_item::WorkItemSpec {
                project: "sample".into(),
                objective: "Implement one contract".into(),
                kind: foundry_sdk::work_item::WorkItemKind::Task,
                lane: foundry_sdk::work_item::WorkLane::Campaign,
                origin: "initiative:shared".into(),
                trace_id: None,
            },
            root,
            Utc::now(),
        );
        let child_id = child.id.clone();
        ledger.upsert(child);
        ledger.save(&ledger_path).unwrap();
        assert!(
            recover(&path, &ledger_path).unwrap().is_empty(),
            "expected `recover(&path, &ledger_path).unwrap()` to be empty: {:?}",
            recover(&path, &ledger_path).unwrap()
        );
        let store = CampaignStore::load(&path).unwrap();
        let initiative = store.find_initiative("shared").unwrap();
        assert!(
            initiative.pending_actions.is_empty(),
            "expected `initiative.pending_actions` to be empty: {:?}",
            initiative.pending_actions
        );
        assert_eq!(initiative.task_dispatches.len(), 1);
        assert_eq!(initiative.task_dispatches[0].work_item, child_id);
        assert!(
            recover(&path, &ledger_path).unwrap().is_empty(),
            "expected `recover(&path, &ledger_path).unwrap()` to be empty: {:?}",
            recover(&path, &ledger_path).unwrap()
        );
        assert_eq!(
            CampaignStore::load(&path)
                .unwrap()
                .find_initiative("shared")
                .unwrap()
                .task_dispatches
                .len(),
            1
        );
    }

    #[test]
    fn failed_recovery_save_preserves_the_action_for_replay() {
        let dir = tempfile::tempdir().unwrap();
        let (path, _) = stored_action(dir.path());
        let bytes = std::fs::read(&path).unwrap();
        std::fs::create_dir(path.with_extension("json.tmp")).unwrap();
        assert_eq!(
            recover(&path, &dir.path().join("work-items.json")).unwrap_err().code(),
            tonic::Code::Internal
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_dir(path.with_extension("json.tmp")).unwrap();
        assert_eq!(recover(&path, &dir.path().join("work-items.json")).unwrap().len(), 1);
    }

    #[test]
    fn abandoned_steering_lease_queues_exactly_one_fresh_packet_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("campaigns.json");
        let ledger_path = dir.path().join("work-items.json");
        let mut store = CampaignStore::default();
        let initiative: Initiative = serde_json::from_value(serde_json::json!({"name":"shared","outcome":"A verified contract",
            "projects":["sample"],"authorized_by":"Stacey","members":[],"done_evidence":[{"kind":"review","statement":"The contract works"}]})).unwrap();
        store.add_initiative(initiative, &["sample".into()]).unwrap();
        store.enable_steering("shared").unwrap();
        store.begin_steering("shared", "interrupted").unwrap();
        store.save(&path).unwrap();
        assert!(
            recover(&path, &ledger_path).unwrap().is_empty(),
            "expected `recover(&path, &ledger_path).unwrap()` to be empty: {:?}",
            recover(&path, &ledger_path).unwrap()
        );
        assert!(
            recover(&path, &ledger_path).unwrap().is_empty(),
            "expected `recover(&path, &ledger_path).unwrap()` to be empty: {:?}",
            recover(&path, &ledger_path).unwrap()
        );
        let ledger = foundry_sdk::work_item::WorkItemStore::load(&ledger_path).unwrap();
        assert_eq!(ledger.items.len(), 1);
        assert_eq!(
            ledger.items[0].pending_root.as_ref().unwrap().id,
            "evt_interrupted_recovered_steer"
        );
        assert!(
            CampaignStore::load(&path)
                .unwrap()
                .find_initiative("shared")
                .unwrap()
                .steering
                .is_none()
        );
    }
}

fn announce_owner_control(
    ctx: &super::RuntimeContext,
    initiative: &str,
    action_id: &str,
    operator_origin: String,
    action: foundry_sdk::payload::InitiativeOwnerControlAction,
) -> Result<String, Status> {
    use foundry_sdk::event::{Event, EventType};
    let mut event = Event::new(
        EventType::InitiativeOwnerActionRecorded,
        "system".into(),
        foundry_sdk::throttle::Throttle::Full,
        Event::serialize_payload(&foundry_sdk::payload::InitiativeOwnerActionRecordedPayload {
            initiative: initiative.into(),
            action_id: action_id.into(),
            operator_origin,
            action,
        })
        .map_err(|error| Status::internal(error.to_string()))?,
    )
    .with_source(Some(foundry_sdk::work_source::WorkSource::initiative(initiative, 0)));
    event.id = format!("evt_{action_id}_owner_control");
    let id = event.id.clone();
    super::spawn_workflow(event, ctx);
    Ok(id)
}
