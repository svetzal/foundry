use super::*;
use crate::initiative::{Initiative, SteeringDecision};

fn store() -> CampaignStore {
    let mut store = CampaignStore::default();
    let initiative: Initiative = serde_json::from_value(serde_json::json!({
        "name":"test","outcome":"User succeeds","projects":["one","two"],
        "members":[],"authorized_by":"Stacey",
        "providers":{"formation":"claude","execution":"codex","review":"claude"},
        "budget":{"max_cycles":8},
        "done_evidence":[{"kind":"gate","host":"dev","command":"./probe","artifacts":["evidence/result.json"]},
            {"kind":"review","statement":"User succeeds"}]
    })).unwrap();
    store.add_initiative(initiative, &["one".into(), "two".into()]).unwrap();
    store.enable_steering("test").unwrap();
    store
}

fn snapshot(store: &CampaignStore) -> OutcomeSnapshot {
    store
        .outcome_snapshot(
            "test",
            ["one", "two"]
                .iter()
                .map(|project| SteeringRepositoryIdentity {
                    project: (*project).into(),
                    root: format!("/repos/{project}"),
                    registry: project.to_string(),
                    trunk: "aabbcc".into(),
                    head: "aabbcc".into(),
                    status: String::new(),
                    diff: String::new(),
                })
                .collect(),
        )
        .unwrap()
}

fn result(store: &CampaignStore) -> IntegrationGateResult {
    IntegrationGateResult {
        id: "gate1".into(),
        gate_index: 0,
        host: "dev".into(),
        command: "./probe".into(),
        snapshot: snapshot(store),
        passed: true,
        exit_code: Some(0),
        report: "Both trunks worked".into(),
        artifacts: vec!["evidence/result.json".into()],
        recorded_at: Utc::now(),
    }
}

fn reviews() -> Vec<OutcomeReview> {
    vec![OutcomeReview {
        statement: "User succeeds".into(),
        affirmed: true,
        evidence: vec!["evidence/result.json".into()],
    }]
}

#[test]
fn done_needs_current_gates_reviews_and_explicit_owner_confirmation() {
    let mut store = store();
    let snapshot = snapshot(&store);
    assert!(store.propose_completion("test", "done", snapshot.clone(), reviews()).is_err());
    store.record_integration_result("test", result(&store)).unwrap();
    let mut refused = reviews();
    refused[0].affirmed = false;
    assert!(store.propose_completion("test", "done", snapshot.clone(), refused).is_err());
    store.propose_completion("test", "done", snapshot.clone(), reviews()).unwrap();
    assert!(!store.find_initiative("test").unwrap().status.is_terminal());
    assert!(store.find_initiative("test").unwrap().completion_confirmation.is_none());
    assert!(store.begin_steering("test", "again").is_err());
    assert!(store.record_et_close("test", "et:goal:x", Utc::now()).is_err());
    store
        .confirm_completion("test", "I observed the result", &snapshot, Utc::now())
        .unwrap();
    let confirmation = store.find_initiative("test").unwrap().completion_confirmation.clone();
    store.confirm_completion("test", "retry", &snapshot, Utc::now()).unwrap();
    assert_eq!(store.find_initiative("test").unwrap().completion_confirmation, confirmation);
}

#[test]
fn stale_trunk_policy_and_intent_block_completion() {
    let mut store = store();
    store.record_integration_result("test", result(&store)).unwrap();
    let original = snapshot(&store);
    store.propose_completion("test", "done", original.clone(), reviews()).unwrap();
    let mut changed = original.clone();
    changed.repositories[0].head = "newhead".into();
    changed.repositories[0].trunk = "newhead".into();
    assert!(store.validate_completion("test", &changed).is_err());
    assert!(store.invalidate_completion("test", &changed).unwrap());
    assert!(store.propose_completion("test", "done2", changed, reviews()).is_err());
    store.find_initiative_mut("test").unwrap().intent_revision = "new intent".into();
    assert!(store.propose_completion("test", "done2", original.clone(), reviews()).is_err());
    store.find_initiative_mut("test").unwrap().intent_revision.clear();
    store.find_initiative_mut("test").unwrap().budget.max_cycles += 1;
    assert!(store.propose_completion("test", "done2", original, reviews()).is_err());
}

#[test]
fn absent_report_artifact_wrong_host_and_dirty_trunks_cannot_pass() {
    for mutation in 0..4 {
        let mut store = store();
        let mut evidence = result(&store);
        match mutation {
            0 => evidence.report.clear(),
            1 => evidence.artifacts.clear(),
            2 => evidence.host = "elsewhere".into(),
            _ => evidence.snapshot.repositories[0].status = " M source.rs".into(),
        }
        assert!(store.record_integration_result("test", evidence).is_err());
        assert!(
            store.find_initiative("test").unwrap().integration_results.is_empty(),
            "expected `store.find_initiative(\"test\").unwrap().integration_results` to be empty: {:?}",
            store.find_initiative("test").unwrap().integration_results
        );
    }
}

#[test]
fn integration_retries_are_idempotent_and_failure_supersedes_green() {
    let mut store = store();
    let success = result(&store);
    store.record_integration_result("test", success.clone()).unwrap();
    store.record_integration_result("test", success.clone()).unwrap();
    let mut conflict = success.clone();
    conflict.report = "changed".into();
    assert!(store.record_integration_result("test", conflict).is_err());
    let mut failed = success;
    failed.id = "gate2".into();
    failed.passed = false;
    failed.exit_code = Some(1);
    failed.report = "probe failed".into();
    store.record_integration_result("test", failed.clone()).unwrap();
    store.record_integration_result("test", failed).unwrap();
    assert_eq!(store.find_initiative("test").unwrap().integration_results.len(), 2);
    assert_eq!(store.findings.len(), 1);
    assert!(store.propose_completion("test", "done", snapshot(&store), reviews()).is_err());
}

fn definition() -> StageCampaignDefinition {
    serde_json::from_value(
        serde_json::json!({"name":"new","project":"one","mission":"Ship something",
    "done_evidence":[{"kind":"review","statement":"Something ships"}]}),
    )
    .unwrap()
}

#[test]
fn stage_inherits_owner_budget_providers_and_requires_a_later_advance() {
    let mut store = store();
    let snapshot = store.begin_steering("test", "stage1").unwrap();
    let decision = SteeringDecision::Stage {
        campaign: Box::new(definition()),
        member: None,
    };
    store
        .commit_steering(&snapshot, "stage1", decision, vec![], "stage".into(), None)
        .unwrap();
    let campaign = store.find("new").unwrap();
    assert_eq!(campaign.status, CampaignStatus::Staged);
    assert_eq!(campaign.initiative.as_deref(), Some("test"));
    assert_eq!(campaign.authorized_by.as_deref(), Some("Stacey"));
    assert_eq!(campaign.budget.max_cycles, 8);
    assert_eq!(
        campaign.providers.inherit(&store.find_initiative("test").unwrap().providers),
        store.find_initiative("test").unwrap().providers
    );
    assert_eq!(store.shared_cycles_used("test").unwrap(), 0);
    assert!(
        store.find_initiative("test").unwrap().reservations.is_empty(),
        "expected `store.find_initiative(\"test\").unwrap().reservations` to be empty: {:?}",
        store.find_initiative("test").unwrap().reservations
    );
    assert!(
        store
            .commit_steering(
                &snapshot,
                "stage1",
                SteeringDecision::Stage {
                    campaign: Box::new(definition()),
                    member: None
                },
                vec![],
                "replay".into(),
                None
            )
            .is_err()
    );
    assert_eq!(store.campaigns.len(), 1);
    store.acknowledge_steering_action("test", "stage1", None).unwrap();
    store.acknowledge_steering_action("test", "stage1", None).unwrap();
    assert_eq!(store.campaigns.len(), 1);
}

#[test]
fn invalid_stage_never_partially_creates_a_campaign() {
    for mutation in 0..4 {
        let mut store = store();
        let snapshot = store.begin_steering("test", "stage").unwrap();
        let mut campaign = definition();
        match mutation {
            0 => campaign.project = "outside".into(),
            1 => campaign.done_evidence.clear(),
            2 => campaign.writable_repositories = vec!["one".into(), "two".into()],
            _ => campaign.intent_refs = vec!["et:interaction:unknown".into()],
        }
        let before = serde_json::to_value(&store).unwrap();
        assert!(
            store
                .commit_steering(
                    &snapshot,
                    "stage",
                    SteeringDecision::Stage {
                        campaign: Box::new(campaign),
                        member: None
                    },
                    vec![],
                    "invalid".into(),
                    None
                )
                .is_err()
        );
        assert_eq!(serde_json::to_value(&store).unwrap(), before);
    }
}

#[test]
fn completion_intent_and_receipts_survive_restart_without_double_closing() {
    let mut store = store();
    store.record_integration_result("test", result(&store)).unwrap();
    let evidence = snapshot(&store);
    store.propose_completion("test", "done", evidence.clone(), reviews()).unwrap();
    store
        .find_initiative_mut("test")
        .unwrap()
        .close_proposals
        .push(EtCloseProposal {
            intent_ref: "et:interaction:ship".into(),
            evidence: vec!["proof.json".into()],
        });
    store.confirm_completion("test", "Observed", &evidence, Utc::now()).unwrap();
    store.set_completion_intent("test", "original intent").unwrap();
    assert!(store.set_completion_intent("test", "replacement intent").is_err());
    store.record_et_close("test", "et:interaction:ship", Utc::now()).unwrap();
    store.record_completion_error("test", "goal close failed").unwrap();
    let mut restarted: CampaignStore =
        serde_json::from_value(serde_json::to_value(&store).unwrap()).unwrap();
    assert_eq!(
        restarted
            .find_initiative("test")
            .unwrap()
            .completion_confirmation
            .as_ref()
            .unwrap()
            .last_error
            .as_deref(),
        Some("goal close failed")
    );
    restarted.record_et_close("test", "et:interaction:ship", Utc::now()).unwrap();
    let initiative = restarted.find_initiative("test").unwrap();
    assert_eq!(initiative.completion_receipts.len(), 1);
    let confirmation = initiative.completion_confirmation.as_ref().unwrap();
    assert_eq!(confirmation.intent_json.as_deref(), Some("original intent"));
    assert!(confirmation.last_error.is_none());
    assert!(!initiative.status.is_terminal());
}

#[test]
fn stage_rejects_unknown_self_and_indirect_dependencies_atomically() {
    let mut store = store();
    let mut initial = definition();
    initial.name = "first".into();
    store.stage_initiative_campaign("test", &initial, None).unwrap();
    for dependency in ["unknown", "new"] {
        let mut campaign = definition();
        campaign.after = vec![dependency.into()];
        let before = serde_json::to_value(&store).unwrap();
        assert!(store.stage_initiative_campaign("test", &campaign, None).is_err());
        assert_eq!(serde_json::to_value(&store).unwrap(), before);
    }
    let initiative = store.find_initiative_mut("test").unwrap();
    initiative.members.push(crate::initiative::InitiativeMember {
        kind: MemberKind::Candidate,
        name: "new".into(),
        after: vec!["first".into()],
        intent_ref: None,
        description: Some("new work".into()),
        intent_state: None,
        audit_finding_id: None,
    });
    initiative.members[0].after = vec!["new".into()];
    let before = serde_json::to_value(&store).unwrap();
    assert!(store.stage_initiative_campaign("test", &definition(), Some("new")).is_err());
    assert_eq!(serde_json::to_value(&store).unwrap(), before);
}

#[test]
fn a_newly_approved_intent_cannot_hide_behind_old_reviews() {
    let mut store = store();
    let initiative = store.find_initiative_mut("test").unwrap();
    initiative.derive_source = Some(crate::initiative::DeriveSource {
        repo: "/repos/one".into(),
        from: "et:goal:x".into(),
    });
    initiative.intent_refs = vec!["et:goal:x".into(), "et:interaction:new".into()];
    initiative.members.push(crate::initiative::InitiativeMember {
        kind: MemberKind::Candidate,
        name: "new".into(),
        after: vec![],
        intent_ref: Some("et:interaction:new".into()),
        description: Some("A new interaction".into()),
        intent_state: Some("create".into()),
        audit_finding_id: None,
    });
    store.record_integration_result("test", result(&store)).unwrap();
    assert!(store.propose_completion("test", "done", snapshot(&store), reviews()).is_err());
    assert!(store.find_initiative("test").unwrap().completion_proposal.is_none());
    let mut complete = reviews();
    complete.push(OutcomeReview {
        statement: "A new interaction".into(),
        affirmed: true,
        evidence: vec!["new-proof.json".into()],
    });
    store.propose_completion("test", "done", snapshot(&store), complete).unwrap();
    let initiative = store.find_initiative("test").unwrap();
    assert!(
        initiative
            .close_proposals
            .iter()
            .any(|proposal| proposal.intent_ref == "et:interaction:new")
    );
    assert!(
        initiative
            .close_proposals
            .iter()
            .any(|proposal| proposal.intent_ref == "et:goal:x")
    );
}

#[test]
fn owner_reconsideration_starts_a_new_completion_generation_and_retains_history() {
    let mut store = store();
    store.record_integration_result("test", result(&store)).unwrap();
    let first = snapshot(&store);
    store.propose_completion("test", "first", first.clone(), reviews()).unwrap();
    assert_eq!(
        store.initiative_view("test").unwrap().status,
        crate::initiative::InitiativeStatus::Escalated
    );
    assert!(
        store.initiative_view("test").unwrap().open_questions.is_empty(),
        "expected `store.initiative_view(\"test\").unwrap().open_questions` to be empty: {:?}",
        store.initiative_view("test").unwrap().open_questions
    );
    store
        .find_initiative_mut("test")
        .unwrap()
        .close_proposals
        .push(EtCloseProposal {
            intent_ref: "et:goal:x".into(),
            evidence: vec!["proof.json".into()],
        });
    store.confirm_completion("test", "Observed first", &first, Utc::now()).unwrap();
    store.record_et_close("test", "et:goal:x", Utc::now()).unwrap();
    store
        .decide_initiative_with_budget(
            "test",
            "Reconsider with current evidence",
            &[],
            0,
            Utc::now(),
        )
        .unwrap();
    let view = store.initiative_view("test").unwrap();
    assert!(view.completion_proposal.is_none());
    assert!(view.completion_confirmation.is_none());
    assert_eq!(view.completion_receipts.len(), 1);
    let mut second = result(&store);
    second.id = "second_gate".into();
    store.record_integration_result("test", second).unwrap();
    let current = snapshot(&store);
    store.propose_completion("test", "second", current.clone(), reviews()).unwrap();
    store
        .confirm_completion("test", "Observed second", &current, Utc::now())
        .unwrap();
    store.record_et_close("test", "et:goal:x", Utc::now()).unwrap();
    store.record_et_close("test", "et:goal:x", Utc::now()).unwrap();
    let receipts = &store.find_initiative("test").unwrap().completion_receipts;
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0].proposal_id, "first");
    assert_eq!(receipts[1].proposal_id, "second");
    let old: EtCloseReceipt = serde_json::from_value(
        serde_json::json!({"intent_ref":"et:goal:old","completed_at":Utc::now()}),
    )
    .unwrap();
    assert!(
        old.proposal_id.is_empty(),
        "expected `old.proposal_id` to be empty: {:?}",
        old.proposal_id
    );
}

#[test]
fn review_only_completion_still_requires_clean_registered_trunks() {
    let mut store = store();
    store
        .find_initiative_mut("test")
        .unwrap()
        .done_evidence
        .retain(|evidence| matches!(evidence, DoneEvidence::Review { .. }));
    for mutation in 0..3 {
        let mut current = snapshot(&store);
        match mutation {
            0 => current.repositories[0].status = " M file".into(),
            1 => current.repositories[0].diff = "changed file".into(),
            _ => current.repositories[0].head = "other-branch".into(),
        }
        assert!(store.propose_completion("test", "done", current, reviews()).is_err());
    }
    store.propose_completion("test", "done", snapshot(&store), reviews()).unwrap();
}

#[test]
fn current_audit_receipts_do_not_invalidate_gate_evidence_for_unchanged_intent() {
    use crate::initiative::{AuditClassification, AuditEvidence, DeriveSource, InitiativeMember};

    let mut store = store();
    let initiative = store.find_initiative_mut("test").unwrap();
    initiative.derive_source = Some(DeriveSource {
        repo: "/repos/one".into(),
        from: "et:goal:ship".into(),
    });
    initiative.intent_refs = vec!["et:goal:ship".into(), "et:interaction:ship".into()];
    initiative.members.push(InitiativeMember {
        kind: MemberKind::Candidate,
        name: "ship".into(),
        after: vec![],
        intent_ref: Some("et:interaction:ship".into()),
        description: Some("User succeeds".into()),
        intent_state: Some("create".into()),
        audit_finding_id: None,
    });
    let gate_snapshot = snapshot(&store);
    store.record_integration_result("test", result(&store)).unwrap();
    for operation in ["first-audit", "second-audit"] {
        let steering = store.begin_steering("test", operation).unwrap();
        store
            .commit_steering(
                &steering,
                operation,
                SteeringDecision::Done { reviews: reviews() },
                vec![AuditEvidence {
                    intent_ref: "et:interaction:ship".into(),
                    classification: AuditClassification::Realised,
                    project: "one".into(),
                    paths: vec!["src/current.rs".into()],
                    explanation: "Current trunks implement the interaction".into(),
                }],
                "Review current outcome evidence".into(),
                None,
            )
            .unwrap();
        assert_eq!(
            store.find_initiative("test").unwrap().members[0].audit_finding_id.as_deref(),
            Some(format!("fnd_{operation}_audit_0").as_str())
        );
        assert_eq!(snapshot(&store), gate_snapshot);
        store.acknowledge_steering_action("test", operation, None).unwrap();
    }
    assert_eq!(store.findings.len(), 2);
    store.propose_completion("test", "done", snapshot(&store), reviews()).unwrap();
    store.validate_completion("test", &gate_snapshot).unwrap();
    for mutation in 0..6 {
        let mut changed = store.clone();
        let initiative = changed.find_initiative_mut("test").unwrap();
        match mutation {
            0 => initiative.members[0].description = Some("Revised user outcome".into()),
            1 => initiative.members[0].intent_state = Some("update".into()),
            2 => initiative.members[0].after = vec!["ship:contract-landed".into()],
            3 => initiative.providers.review = Some(crate::gateway::AgentProvider::Codex),
            4 => initiative.budget.max_cycles += 1,
            _ => initiative.intent_revision = "revised Tracker intent".into(),
        }
        let changed_snapshot = snapshot(&changed);
        assert_ne!(changed_snapshot, gate_snapshot);
        assert!(changed.validate_completion("test", &changed_snapshot).is_err());
    }
}

#[test]
fn unmet_completion_preconditions_identify_the_exact_waiting_work() {
    use crate::initiative::{MemberReservation, ReservationKind};
    let mut store = store();
    store.record_required_question("test", "Which public contract?", &[]).unwrap();
    let question_id = store.find_initiative("test").unwrap().open_questions[0].id.clone();
    store.find_initiative_mut("test").unwrap().reservations.push(MemberReservation {
        member: "swift".into(),
        operation_id: "swift-probe".into(),
        kind: ReservationKind::Task,
    });
    let error = store.require_settled_outcome("test").unwrap_err().to_string();
    assert!(error.contains(&question_id), "{error}");
    assert!(error.contains("member 'swift' has reservation 'swift-probe'"), "{error}");
    assert!(
        !error.contains("requires settled member work"),
        "a generic error conceals the recovery action"
    );
}
