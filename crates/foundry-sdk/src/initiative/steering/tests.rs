use super::*;
use crate::initiative::derive::{Goal, IntentProjection, Interaction};

fn store() -> CampaignStore {
    let mut store = CampaignStore::default();
    for name in ["one", "two"] {
        let campaign = serde_json::from_value(serde_json::json!({"name":name,"project":"sample","mission":"Ship the behavior","authorized_by":"Stacey","done_evidence":[{"kind":"review","statement":"It works"}]})).unwrap();
        store.add(campaign).unwrap();
    }
    let initiative = serde_json::from_value(serde_json::json!({"name":"shared","outcome":"Both work","projects":["sample"],"authorized_by":"Stacey","members":[{"kind":"campaign","name":"one"},{"kind":"campaign","name":"two"}],"done_evidence":[{"kind":"review","statement":"Both work"}]})).unwrap();
    store.add_initiative(initiative, &["sample".into()]).unwrap();
    store.enable_steering("shared").unwrap();
    store
}

fn commit(
    store: &mut CampaignStore,
    id: &str,
    decision: SteeringDecision,
) -> Result<DurableSteeringAction> {
    let snapshot = store.begin_steering("shared", id)?;
    store
        .commit_steering(
            &snapshot,
            id,
            decision,
            vec![],
            "Observed current evidence".into(),
            Some(AgentProvider::Codex),
        )
        .map(|committed| committed.into_action().expect("the decision needs an action"))
}

#[test]
fn stale_owner_context_is_rejected_without_a_partial_action() {
    let mut store = store();
    let snapshot = store.begin_steering("shared", "steer-1").unwrap();
    assert!(store.begin_steering("shared", "steer-2").is_err());
    store
        .decide_initiative("shared", "Keep the mission focused", &[], Utc::now())
        .unwrap();
    let before = serde_json::to_string(&store).unwrap();
    assert!(
        store
            .commit_steering(
                &snapshot,
                "steer-1",
                SteeringDecision::Advance {
                    member: "one".into(),
                    project: None
                },
                vec![],
                "stale prompt".into(),
                None
            )
            .is_err()
    );
    assert_eq!(serde_json::to_string(&store).unwrap(), before);
    store.recover_steering("shared", &["steer-1".into()]).unwrap();
    assert!(store.begin_steering("shared", "steer-2").is_err());
    store.recover_steering("shared", &[]).unwrap();
    assert!(store.begin_steering("shared", "steer-2").is_ok());
}

#[test]
fn pending_action_persists_original_external_evidence_without_changing_owner_fingerprint() {
    let mut store = store();
    let action = commit(
        &mut store,
        "external-proof",
        SteeringDecision::Advance {
            member: "one".into(),
            project: None,
        },
    )
    .unwrap();
    let context = SteeringExternalContext {
        repositories: vec![SteeringRepositoryIdentity {
            project: "sample".into(),
            root: "/registered/sample".into(),
            registry: "registered project definition".into(),
            trunk: "checked-trunk".into(),
            head: "checked-head".into(),
            status: String::new(),
            diff: String::new(),
        }],
        derive_source: None,
        intent: None,
        brief: None,
        brief_content: None,
    };
    let bound = store.bind_external_context("shared", &action.id, context.clone()).unwrap();
    assert_eq!(bound.fingerprint, action.fingerprint);
    assert!(store.pending_action_is_current("shared", &action.id).unwrap());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("campaigns.json");
    store.save(&path).unwrap();
    let mut restored = CampaignStore::load(&path).unwrap();
    assert_eq!(
        restored.bind_external_context("shared", &action.id, context.clone()).unwrap(),
        bound
    );
    let before = serde_json::to_string(&restored).unwrap();
    let mut replacement = context;
    replacement.repositories[0].trunk = "changed-trunk".into();
    assert!(restored.bind_external_context("shared", &action.id, replacement).is_err());
    assert_eq!(serde_json::to_string(&restored).unwrap(), before);
    let mut old_record = serde_json::to_value(&action).unwrap();
    old_record.as_object_mut().unwrap().remove("external_context");
    assert!(
        serde_json::from_value::<DurableSteeringAction>(old_record)
            .unwrap()
            .external_context
            .is_none()
    );
}

#[test]
fn named_milestones_and_owner_holds_are_binding() {
    let mut store = store();
    store.find_mut("one").unwrap().cycles_landed = 1;
    commit(
        &mut store,
        "hold",
        SteeringDecision::Hold {
            member: "two".into(),
            until: vec!["one:published_contract".into()],
            reason: "Wait for the contract".into(),
        },
    )
    .unwrap();
    store.finish_steering_action("shared", "hold").unwrap();
    assert!(store.enforce_can_form("two", &[]).is_err());
    store
        .record_milestone("shared", "one", "published_contract", vec!["src/contract.rs".into()])
        .unwrap();
    assert!(store.enforce_can_form("two", &[]).is_ok());
    store.hold_member("shared", "two", "Owner is reviewing the API").unwrap();
    assert!(store.enforce_can_form("two", &[]).is_err());
    assert!(
        commit(
            &mut store,
            "advance-held",
            SteeringDecision::Advance {
                member: "two".into(),
                project: None
            }
        )
        .is_err()
    );
    store.finish_steering("shared", "advance-held").unwrap();
    store.release_member("shared", "two").unwrap();
    assert!(store.enforce_can_form("two", &[]).is_ok());
}

#[test]
fn task_reservations_share_the_active_limit_and_total_budget_without_copying_status() {
    let mut store = store();
    store.find_initiative_mut("shared").unwrap().budget.max_active_members = 1;
    store.find_initiative_mut("shared").unwrap().budget.max_cycles = 1;
    let action = commit(
        &mut store,
        "task-1",
        SteeringDecision::Dispatch {
            project: "sample".into(),
            objective: "Document the contract".into(),
            member: None,
        },
    )
    .unwrap();
    assert!(store.enforce_can_form("one", &[]).is_err());
    assert!(store.enforce_steering_action("shared", &action.id, &[]).is_ok());
    assert_eq!(store.shared_cycles_used("shared").unwrap(), 1);
    assert!(store.acknowledge_steering_action("shared", &action.id, None).is_err());
    store
        .acknowledge_steering_action("shared", &action.id, Some("wi_task-1"))
        .unwrap();
    store
        .acknowledge_steering_action("shared", &action.id, Some("wi_task-1"))
        .unwrap();
    assert_eq!(store.find_initiative("shared").unwrap().task_dispatches.len(), 1);
    assert_eq!(store.shared_cycles_used("shared").unwrap(), 1);
    assert!(store.enforce_can_advance("one", &[]).is_err());
    store.release_member_reservation("shared", "wi_task-1").unwrap();
    store
        .decide_initiative_with_budget("shared", "Continue", &[], 2, Utc::now())
        .unwrap();
    assert!(store.enforce_can_advance("one", &[]).is_ok());
    assert!(store.enforce_can_form("one", &["task_task-1".into()]).is_err());
}

#[test]
fn committed_action_is_invalidated_by_a_later_owner_hold() {
    let mut store = store();
    let action = commit(
        &mut store,
        "step",
        SteeringDecision::Advance {
            member: "one".into(),
            project: None,
        },
    )
    .unwrap();
    assert!(store.enforce_steering_action("shared", &action.id, &[]).is_ok());
    store.hold_member("shared", "one", "Wait for owner review").unwrap();
    assert!(store.enforce_steering_action("shared", &action.id, &[]).is_err());
    store.discard_steering_action("shared", &action.id).unwrap();
    assert!(
        store.find_initiative("shared").unwrap().reservations.is_empty(),
        "expected `store.find_initiative(\"shared\").unwrap().reservations` to be empty: {:?}",
        store.find_initiative("shared").unwrap().reservations
    );
}

#[test]
fn member_budget_is_granted_only_within_the_shared_total_and_atomic_owner_extensions() {
    let mut store = store();
    let member = store.find_mut("one").unwrap();
    member.budget.max_cycles = 1;
    member.cycles_completed = 1;
    member.status = CampaignStatus::Escalated;
    member.open_escalation = Some(crate::campaign::CampaignEscalation {
        reason: "campaign cycle budget exhausted (1)".into(),
        escalated_at: Utc::now(),
        cycles_completed: 1,
        cycles_landed: 0,
        question: None,
        choices: vec![],
        from_event_log: false,
    });
    store.refresh_questions("shared").unwrap();
    store.grant_member_budget("one", &[]).unwrap();
    assert_eq!(store.find("one").unwrap().status, CampaignStatus::Active);
    assert_eq!(store.find("one").unwrap().budget.max_cycles, 20);
    let before = serde_json::to_string(&store).unwrap();
    assert!(store.decide_initiative_with_budget("shared", "", &[], 2, Utc::now()).is_err());
    assert!(
        store
            .decide_initiative_with_budget("shared", "Continue", &[], u64::MAX, Utc::now())
            .is_err()
    );
    assert_eq!(serde_json::to_string(&store).unwrap(), before);
}

/// A shared budget of two cycles, both spent by member `one`, which escalated
/// on budget.
fn spent_store() -> CampaignStore {
    let mut store = store();
    store.find_initiative_mut("shared").unwrap().budget.max_cycles = 2;
    let member = store.find_mut("one").unwrap();
    member.budget.max_cycles = 2;
    member.cycles_completed = 2;
    member.status = CampaignStatus::Escalated;
    member.open_escalation = Some(crate::campaign::CampaignEscalation {
        reason: "campaign cycle budget exhausted (2)".into(),
        escalated_at: Utc::now(),
        cycles_completed: 2,
        cycles_landed: 2,
        question: None,
        choices: vec![],
        from_event_log: false,
    });
    store.refresh_questions("shared").unwrap();
    store
}

/// A decision without cycles on a spent shared budget would leave the
/// escalated member escalated and reopen the same question, so it is refused
/// and records nothing, as `decide` on a spent standalone campaign is.
#[test]
fn decision_on_a_spent_shared_budget_requires_more_cycles() {
    let mut store = spent_store();
    let before = serde_json::to_string(&store).unwrap();

    assert_eq!(
        store.decide_initiative_with_budget("shared", "Finish the migration", &[], 0, Utc::now()),
        Err(InitiativeError::Precondition(
            "initiative 'shared' exhausted its cycle budget; pass add_cycles > 0 to authorize more work"
                .into()
        ))
    );
    assert_eq!(serde_json::to_string(&store).unwrap(), before, "a refusal changes nothing");

    store
        .decide_initiative_answering_with_budget(
            "shared",
            "Finish the migration",
            &[],
            &store
                .initiative_view("shared")
                .unwrap()
                .open_questions
                .iter()
                .map(|question| question.id.clone())
                .collect::<Vec<_>>(),
            3,
            Utc::now(),
        )
        .unwrap();
    assert_eq!(store.find("one").unwrap().status, CampaignStatus::Active);
    assert_eq!(store.find_initiative("shared").unwrap().owner_decisions.len(), 1);
}

/// The decision's own refusals keep precedence over the spent budget, so the
/// owner hears about the mistake they can fix in the request first.
#[test]
fn decision_refusals_precede_the_spent_budget_refusal() {
    let mut store = spent_store();
    let before = serde_json::to_string(&store).unwrap();

    assert!(matches!(
        store.decide_initiative_with_budget("missing", "Continue", &[], 0, Utc::now()),
        Err(InitiativeError::NotFound(_))
    ));
    assert_eq!(
        store.decide_initiative_with_budget("shared", " ", &[], 0, Utc::now()),
        Err(InitiativeError::Invalid("decision must be non-empty".into()))
    );
    assert_eq!(
        store.decide_initiative_with_budget("shared", "Continue", &["ghost".into()], 0, Utc::now()),
        Err(InitiativeError::Invalid(
            "decision or finding names a campaign outside the initiative".into()
        ))
    );
    assert_eq!(serde_json::to_string(&store).unwrap(), before, "a refusal changes nothing");
}

/// Completion records its reason as a decision, but it spends nothing, so a
/// spent budget does not stand in its way.
#[test]
fn a_spent_shared_budget_does_not_block_completion() {
    let mut store = spent_store();
    let _ = store
        .complete_initiative("shared", "Owner accepts the outcome", Utc::now())
        .unwrap();
    assert_eq!(
        store.find_initiative("shared").unwrap().status,
        crate::initiative::InitiativeStatus::Completed
    );
}

fn projection(state: &str) -> IntentProjection {
    IntentProjection {
        goals: vec![Goal {
            state: "create".into(),
            id: "goal".into(),
            description: "Both behaviors work".into(),
            success_criteria: "Both verified".into(),
        }],
        interactions: vec![Interaction {
            id: "intent".into(),
            description: "The actor can greet the team".into(),
            state: state.into(),
            priority: 1,
            goals: vec!["goal".into()],
            goal: None,
        }],
        journeys: vec![],
    }
}

#[test]
fn refreshed_intent_preserves_links_and_history_and_revokes_only_approved_work() {
    let mut store = CampaignStore::default();
    let mut initiative = super::super::derive::derive_from_projection(
        &projection("create"),
        "et:goal:goal",
        vec!["sample".into()],
    )
    .unwrap();
    initiative.authorized_by = Some("Stacey".into());
    initiative.derive_source = Some(DeriveSource {
        repo: "/tmp/intent".into(),
        from: "et:goal:goal".into(),
    });
    store.add_initiative(initiative, &["sample".into()]).unwrap();
    store
        .record_audits(
            "goal",
            "audit",
            vec![AuditEvidence {
                intent_ref: "et:interaction:intent".into(),
                classification: AuditClassification::Partial,
                project: "sample".into(),
                paths: vec!["src/main.rs".into()],
                explanation: "Audited the greeting".into(),
            }],
        )
        .unwrap();
    store.activate_candidate_campaign("goal", "intent", "sample").unwrap();
    store.find_mut("intent").unwrap().cycles_completed = 1;
    assert!(
        store.refresh_derived_intent("goal", &projection("created")).unwrap().is_empty(),
        "expected `store.refresh_derived_intent(\"goal\", &projection(\"created\")).unwrap()` to be empty: {:?}",
        store.refresh_derived_intent("goal", &projection("created")).unwrap()
    );
    assert_eq!(store.find("intent").unwrap().cycles_completed, 1);
    assert_ne!(store.find("intent").unwrap().status, CampaignStatus::Cancelled);
    assert_eq!(
        store.refresh_derived_intent("goal", &projection("discarded")).unwrap(),
        ["intent"]
    );
    assert_eq!(store.find("intent").unwrap().status, CampaignStatus::Cancelled);
    assert!(store.enforce_can_advance("intent", &[]).is_err());
    assert_eq!(store.find_initiative("goal").unwrap().members.len(), 1);
}

#[test]
fn historical_realised_audits_do_not_restore_revoked_close_proposals() {
    for disappeared in [false, true] {
        let mut store = CampaignStore::default();
        let mut initiative = super::super::derive::derive_from_projection(
            &projection("create"),
            "et:goal:goal",
            vec!["sample".into()],
        )
        .unwrap();
        initiative.authorized_by = Some("Stacey".into());
        initiative.derive_source = Some(DeriveSource {
            repo: "/tmp/intent".into(),
            from: "et:goal:goal".into(),
        });
        store.add_initiative(initiative, &["sample".into()]).unwrap();
        let audit = |path: &str| AuditEvidence {
            intent_ref: "et:interaction:intent".into(),
            classification: AuditClassification::Realised,
            project: "sample".into(),
            paths: vec![path.into()],
            explanation: "The implementation exists".into(),
        };
        store.record_audits("goal", "before", vec![audit("src/original.rs")]).unwrap();
        assert_eq!(store.find_initiative("goal").unwrap().close_proposals.len(), 1);
        let mut revised = projection("discarded");
        if disappeared {
            revised.interactions.clear();
        }
        assert_eq!(store.refresh_derived_intent("goal", &revised).unwrap(), ["intent"]);
        let milestones = store.find_initiative("goal").unwrap().milestones.clone();
        store
            .record_audits("goal", "historical", vec![audit("src/current.rs")])
            .unwrap();
        let initiative = store.find_initiative("goal").unwrap();
        assert!(
            initiative.close_proposals.is_empty(),
            "expected `initiative.close_proposals` to be empty: {:?}",
            initiative.close_proposals
        );
        assert_eq!(initiative.milestones, milestones);
        assert!(initiative.members[0].audit_finding_id.is_none());
        assert!(store.find_finding("fnd_historical_audit_0").is_some());
    }
}

#[test]
fn realised_audit_proposes_close_and_proves_journey_prerequisite_without_dispatch() {
    let mut store = CampaignStore::default();
    let mut projection = projection("create");
    projection.interactions.push(Interaction {
        id: "next".into(),
        description: "A second greeting works".into(),
        state: "create".into(),
        priority: 2,
        goals: vec!["goal".into()],
        goal: None,
    });
    projection.journeys.push(super::super::derive::Journey {
        id: "journey".into(),
        goal: "goal".into(),
        steps: vec!["intent".into(), "next".into()],
    });
    let mut initiative = super::super::derive::derive_from_projection(
        &projection,
        "et:goal:goal",
        vec!["sample".into()],
    )
    .unwrap();
    initiative.authorized_by = Some("Stacey".into());
    initiative.derive_source = Some(DeriveSource {
        repo: "/tmp/intent".into(),
        from: "et:goal:goal".into(),
    });
    store.add_initiative(initiative, &["sample".into()]).unwrap();
    store.enable_steering("goal").unwrap();
    let snapshot = store.begin_steering("goal", "initial").unwrap();
    let decision = SteeringDecision::Advance {
        member: "next".into(),
        project: Some("sample".into()),
    };
    assert!(
        store
            .commit_steering(&snapshot, "initial", decision.clone(), vec![], "Audit".into(), None)
            .is_err()
    );
    let audit = |intent: &str, classification| AuditEvidence {
        intent_ref: format!("et:interaction:{intent}"),
        classification,
        project: "sample".into(),
        paths: vec!["src/main.rs".into()],
        explanation: "Inspected the implementation".into(),
    };
    let action = store
        .commit_steering(
            &snapshot,
            "initial",
            decision,
            vec![
                audit("intent", AuditClassification::Realised),
                audit("next", AuditClassification::Absent),
            ],
            "Audit".into(),
            Some(AgentProvider::Claude),
        )
        .unwrap()
        .into_action()
        .unwrap();
    assert!(store.find("intent").is_none());
    assert!(store.find("next").is_some());
    assert_eq!(store.find_initiative("goal").unwrap().close_proposals.len(), 1);
    assert!(store.enforce_steering_action("goal", &action.id, &[]).is_ok());
}

#[test]
fn configured_pairings_alternate_on_work_actions_and_preserve_legacy_inheritance() {
    let mut store = store();
    let pair_a = StageProviders {
        formation: None,
        execution: Some(AgentProvider::Codex),
        review: Some(AgentProvider::Claude),
    };
    let pair_b = StageProviders {
        formation: None,
        execution: Some(AgentProvider::Claude),
        review: Some(AgentProvider::Codex),
    };
    store.find_initiative_mut("shared").unwrap().provider_pairings = vec![pair_a, pair_b];
    let first = commit(
        &mut store,
        "first",
        SteeringDecision::Advance {
            member: "one".into(),
            project: None,
        },
    )
    .unwrap();
    assert_eq!(first.providers, pair_a);
    store.finish_steering_action("shared", "first").unwrap();
    store.find_mut("one").unwrap().cycles_completed = 1;
    commit(
        &mut store,
        "question",
        SteeringDecision::Ask {
            question: "Proceed?".into(),
            members: vec![],
        },
    )
    .unwrap();
    store.finish_steering_action("shared", "question").unwrap();
    store
        .decide_initiative_answering(
            "shared",
            "Proceed",
            &[],
            &store
                .initiative_view("shared")
                .unwrap()
                .open_questions
                .iter()
                .map(|question| question.id.clone())
                .collect::<Vec<_>>(),
            Utc::now(),
        )
        .unwrap();
    let second = commit(
        &mut store,
        "second",
        SteeringDecision::Advance {
            member: "two".into(),
            project: None,
        },
    )
    .unwrap();
    assert_eq!(second.providers, pair_b);
}

#[test]
fn concurrent_steering_claims_share_the_existing_store_guard() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("campaigns.json");
    store().save(&path).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let workers: Vec<_> = ["first", "second"]
        .into_iter()
        .map(|id| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let mut guard = CampaignStore::lock_exclusive(&path).unwrap();
                let acquired = guard.store.begin_steering("shared", id).is_ok();
                if acquired {
                    guard.save().unwrap();
                }
                acquired
            })
        })
        .collect();
    barrier.wait();
    assert_eq!(
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(|acquired| *acquired)
            .count(),
        1
    );
    let persisted = CampaignStore::load(&path).unwrap();
    assert!(persisted.find_initiative("shared").unwrap().steering.is_some());
}

#[test]
fn refresh_updates_derived_mission_and_revokes_removed_scope_without_erasing_evidence() {
    let mut store = CampaignStore::default();
    let mut initial = super::super::derive::derive_from_projection(
        &projection("create"),
        "et:goal:goal",
        vec!["sample".into()],
    )
    .unwrap();
    initial.authorized_by = Some("Stacey".into());
    initial.derive_source = Some(DeriveSource {
        repo: "/tmp/intent".into(),
        from: "et:goal:goal".into(),
    });
    store.add_initiative(initial, &["sample".into()]).unwrap();
    store
        .record_audits(
            "goal",
            "audit",
            vec![AuditEvidence {
                intent_ref: "et:interaction:intent".into(),
                classification: AuditClassification::Partial,
                project: "sample".into(),
                paths: vec!["src/main.rs".into()],
                explanation: "Audited the greeting".into(),
            }],
        )
        .unwrap();
    store.activate_candidate_campaign("goal", "intent", "sample").unwrap();
    store.find_mut("intent").unwrap().cycles_completed = 1;
    store.find_mut("intent").unwrap().context_paths = vec!["README.md".into()];
    let mut revised = projection("update");
    revised.interactions[0].description = "The actor can greet an excited team".into();
    store.refresh_derived_intent("goal", &revised).unwrap();
    let member = store.find("intent").unwrap();
    assert_eq!(member.mission, "The actor can greet an excited team");
    assert_eq!(member.cycles_completed, 1);
    assert_eq!(member.context_paths, ["README.md"]);
    assert_eq!(
        serde_json::to_value(&member.done_evidence).unwrap()[0]["statement"],
        member.mission
    );
    revised.interactions[0].goals = vec!["other".into()];
    assert_eq!(store.refresh_derived_intent("goal", &revised).unwrap(), ["intent"]);
    assert_eq!(store.find("intent").unwrap().status, CampaignStatus::Cancelled);
    assert_eq!(store.find("intent").unwrap().cycles_completed, 1);
}

#[test]
fn relay_preserves_recorded_evidence_and_existing_consumption() {
    let mut store = store();
    let finding = Finding::new(
        "shared",
        FindingSource {
            kind: "owner".into(),
            id: "Stacey".into(),
        },
        vec!["one".into()],
        FindingKind::Evidence,
        "The existing contract is stable",
        vec!["src/contract.rs".into()],
    );
    store.add_finding(finding.clone()).unwrap();
    store
        .consume_findings("shared", "one", "wi_landed", std::slice::from_ref(&finding.id))
        .unwrap();
    commit(
        &mut store,
        "relay",
        SteeringDecision::Relay {
            finding: Box::new(finding.clone()),
            to: vec!["two".into()],
        },
    )
    .unwrap();
    let updated = store.find_finding(&finding.id).unwrap();
    assert_eq!(updated.to, ["one", "two"]);
    assert_eq!(updated.consumed_by.len(), 1);
    assert_eq!(updated.text, finding.text);
    store.finish_steering_action("shared", "relay").unwrap();
    let mut replacement = finding;
    replacement.text = "An invented replacement".into();
    let before = serde_json::to_string(&store).unwrap();
    let snapshot = store.begin_steering("shared", "replacement").unwrap();
    assert!(
        store
            .commit_steering(
                &snapshot,
                "replacement",
                SteeringDecision::Relay {
                    finding: Box::new(replacement),
                    to: vec!["two".into()]
                },
                vec![],
                "Prompt".into(),
                None
            )
            .is_err()
    );
    store.finish_steering("shared", "replacement").unwrap();
    assert_eq!(serde_json::to_string(&store).unwrap(), before);
}

#[test]
fn owner_operations_preserve_passive_groups_and_cannot_bypass_candidate_audits() {
    let mut store = store();
    store.find_initiative_mut("shared").unwrap().steering_enabled = false;
    store
        .commit_owner_action(
            "shared",
            "owner-question",
            SteeringDecision::Ask {
                question: "Which contract is authoritative?".into(),
                members: vec![],
            },
            "Owner request".into(),
        )
        .unwrap();
    assert!(!store.find_initiative("shared").unwrap().steering_enabled);
    store.finish_steering_action("shared", "owner-question").unwrap();

    let mut derived = super::super::derive::derive_from_projection(
        &projection("create"),
        "et:goal:goal",
        vec!["sample".into()],
    )
    .unwrap();
    derived.authorized_by = Some("Stacey".into());
    derived.derive_source = Some(DeriveSource {
        repo: "/tmp/intent".into(),
        from: "et:goal:goal".into(),
    });
    store.add_initiative(derived, &["sample".into()]).unwrap();
    assert!(store.activate_candidate_campaign("goal", "intent", "sample").is_err());
    let before = serde_json::to_string(&store).unwrap();
    assert!(
        store
            .commit_owner_action(
                "goal",
                "bypass",
                SteeringDecision::Dispatch {
                    project: "sample".into(),
                    objective: "Implement the greeting".into(),
                    member: Some("intent".into())
                },
                "Owner dispatch".into()
            )
            .is_err()
    );
    assert_eq!(serde_json::to_string(&store).unwrap(), before);
}

#[test]
fn admitted_formations_alternate_before_either_cycle_dispatches() {
    let mut store = store();
    let first_pair = StageProviders {
        formation: Some(AgentProvider::Claude),
        execution: Some(AgentProvider::Codex),
        review: Some(AgentProvider::Claude),
    };
    let second_pair = StageProviders {
        formation: Some(AgentProvider::Codex),
        execution: Some(AgentProvider::Claude),
        review: Some(AgentProvider::Codex),
    };
    store.find_initiative_mut("shared").unwrap().provider_pairings = vec![first_pair, second_pair];
    let abandoned = commit(
        &mut store,
        "abandoned",
        SteeringDecision::Advance {
            member: "one".into(),
            project: None,
        },
    )
    .unwrap();
    store.discard_steering_action("shared", &abandoned.id).unwrap();
    let first = commit(
        &mut store,
        "first-admitted",
        SteeringDecision::Advance {
            member: "one".into(),
            project: None,
        },
    )
    .unwrap();
    assert_eq!(first.providers, first_pair);
    store
        .acknowledge_steering_action("shared", &first.id, Some("wi_form_one"))
        .unwrap();
    assert_eq!(store.find("one").unwrap().cycles_completed, 0);
    let second = commit(
        &mut store,
        "second-admitted",
        SteeringDecision::Advance {
            member: "two".into(),
            project: None,
        },
    )
    .unwrap();
    assert_eq!(second.providers, second_pair);
    store
        .acknowledge_steering_action("shared", &second.id, Some("wi_form_two"))
        .unwrap();
    assert_eq!(store.find("two").unwrap().cycles_completed, 0);
    assert_eq!(store.find_initiative("shared").unwrap().work_admissions.len(), 2);
    assert_eq!(store.find_initiative("shared").unwrap().reservations.len(), 2);
    assert!(
        commit(
            &mut store,
            "third-admission",
            SteeringDecision::Dispatch {
                project: "sample".into(),
                objective: "A third independent task".into(),
                member: None,
            },
        )
        .is_err()
    );
    let explicit = StageProviders {
        formation: None,
        execution: Some(AgentProvider::Codex),
        review: None,
    };
    let selected = explicit.inherit(&second.providers);
    assert_eq!(selected.formation, Some(AgentProvider::Codex));
    assert_eq!(selected.execution, Some(AgentProvider::Codex));
    assert_eq!(selected.review, Some(AgentProvider::Codex));
}

#[test]
fn member_override_preserves_unmet_milestone_and_steering_holds() {
    let mut store = store();
    let initiative = store.find_initiative_mut("shared").unwrap();
    initiative.holds = vec![
        MemberHold {
            member: "two".into(),
            until: vec![],
            reason: "Owner review".into(),
            owner: true,
        },
        MemberHold {
            member: "two".into(),
            until: vec!["one:contract".into()],
            reason: "Need contract".into(),
            owner: true,
        },
        MemberHold {
            member: "two".into(),
            until: vec!["one:tests".into()],
            reason: "Need tests".into(),
            owner: false,
        },
    ];
    store.release_member("shared", "two").unwrap();
    let holds = &store.find_initiative("shared").unwrap().holds;
    assert_eq!(holds.len(), 2);
    assert!(holds.iter().all(|hold| !hold.until.is_empty()));
    assert!(store.enforce_can_form("two", &[]).is_err());
}

use crate::initiative::{
    DoneEvidence, IntegrationGateResult, OutcomeReview, SteeringRepositoryIdentity,
};

fn derived_gate_store() -> (
    CampaignStore,
    IntentProjection,
    Vec<DoneEvidence>,
    Vec<SteeringRepositoryIdentity>,
) {
    let original = projection("create");
    let mut initiative = super::super::derive::derive_from_projection(
        &original,
        "et:goal:goal",
        vec!["sample".into()],
    )
    .unwrap();
    initiative.authorized_by = Some("Stacey".into());
    initiative.derive_source = Some(DeriveSource {
        repo: "/tmp/intent".into(),
        from: "et:goal:goal".into(),
    });
    let gates: Vec<DoneEvidence> = serde_json::from_value(serde_json::json!([
        {"kind":"gate","command":"./first-probe","host":"development","artifacts":["first.json"]},
        {"kind":"gate","command":"./second-probe","host":"development","artifacts":["second.json"]}
    ]))
    .unwrap();
    let mut evidence = gates.clone();
    evidence.extend(initiative.done_evidence);
    initiative.done_evidence = evidence;
    let mut store = CampaignStore::default();
    store.add_initiative(initiative, &["sample".into()]).unwrap();
    store.refresh_derived_intent("goal", &original).unwrap();
    let identities = vec![SteeringRepositoryIdentity {
        project: "sample".into(),
        root: "/tmp/sample".into(),
        registry: "sample".into(),
        trunk: "aabbcc".into(),
        head: "aabbcc".into(),
        status: String::new(),
        diff: String::new(),
    }];
    (store, original, gates, identities)
}

fn outcome_reviews(store: &CampaignStore) -> Vec<OutcomeReview> {
    store
        .find_initiative("goal")
        .unwrap()
        .done_evidence
        .iter()
        .filter_map(|evidence| {
            if let DoneEvidence::Review { statement } = evidence {
                Some(OutcomeReview {
                    statement: statement.clone(),
                    affirmed: true,
                    evidence: vec!["review-proof.json".into()],
                })
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
}

#[test]
fn refreshed_tracker_intent_preserves_owner_gate_indexes_and_invalidates_old_outcome_evidence() {
    let (mut store, original, gates, identities) = derived_gate_store();
    let before = store.outcome_snapshot("goal", identities.clone()).unwrap();
    for (index, gate) in gates.iter().enumerate() {
        let DoneEvidence::Gate {
            command, artifacts, ..
        } = gate
        else {
            panic!("expected gate")
        };
        store
            .record_integration_result(
                "goal",
                IntegrationGateResult {
                    id: format!("gate-{index}"),
                    gate_index: index,
                    host: "development".into(),
                    command: command.clone(),
                    snapshot: before.clone(),
                    passed: true,
                    exit_code: Some(0),
                    report: "Captured integration evidence".into(),
                    artifacts: artifacts.clone(),
                    recorded_at: Utc::now(),
                },
            )
            .unwrap();
    }
    store
        .propose_completion("goal", "done", before.clone(), outcome_reviews(&store))
        .unwrap();
    store.refresh_derived_intent("goal", &original).unwrap();
    assert_eq!(store.outcome_snapshot("goal", identities.clone()).unwrap(), before);
    store.validate_completion("goal", &before).unwrap();
    let mut changed = original;
    changed.interactions[0].description = "The actor can greet the team by name".into();
    changed.interactions.push(Interaction {
        id: "follow-up".into(),
        description: "The actor can receive a reply".into(),
        state: "create".into(),
        priority: 2,
        goals: vec!["goal".into()],
        goal: None,
    });
    store.refresh_derived_intent("goal", &changed).unwrap();
    let current = store.find_initiative("goal").unwrap();
    assert_eq!(
        serde_json::to_value(&current.done_evidence[..2]).unwrap(),
        serde_json::to_value(&gates).unwrap()
    );
    assert!(current.done_evidence.iter().any(|evidence|matches!(evidence,DoneEvidence::Review {statement} if statement=="The actor can greet the team by name")));
    assert!(current.done_evidence.iter().any(|evidence|matches!(evidence,DoneEvidence::Review {statement} if statement=="The actor can receive a reply")));
    assert!(!current.done_evidence.iter().any(|evidence|matches!(evidence,DoneEvidence::Review {statement} if statement=="The actor can greet the team")));
    let fresh = store.outcome_snapshot("goal", identities).unwrap();
    assert!(store.validate_completion("goal", &fresh).is_err());
    assert!(store.invalidate_completion("goal", &fresh).unwrap());
    assert!(
        store
            .propose_completion("goal", "new-done", fresh, outcome_reviews(&store))
            .is_err()
    );
    assert_eq!(
        store
            .find_initiative("goal")
            .unwrap()
            .integration_results
            .iter()
            .map(|result| result.gate_index)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
}

fn commit_any(store: &mut CampaignStore, id: &str, decision: SteeringDecision) -> SteeringCommit {
    let snapshot = store.begin_steering("shared", id).unwrap();
    store
        .commit_steering(
            &snapshot,
            id,
            decision,
            vec![],
            "Prompt".into(),
            Some(AgentProvider::Codex),
        )
        .unwrap()
}

#[test]
fn wait_is_recorded_with_its_reason_and_changes_nothing_else() {
    let mut store = store();
    let status = store.find_initiative("shared").unwrap().status;

    let committed = commit_any(
        &mut store,
        "wait",
        SteeringDecision::Wait {
            reason: "  Both members are running  ".into(),
        },
    );

    assert_eq!(
        committed,
        SteeringCommit::Wait {
            reason: "Both members are running".into()
        }
    );
    let initiative = store.find_initiative("shared").unwrap();
    assert_eq!(initiative.steering_history.len(), 1);
    assert_eq!(initiative.steering_history[0].id, "wait");
    assert!(initiative.pending_actions.is_empty(), "{:?}", initiative.pending_actions);
    assert!(initiative.reservations.is_empty(), "{:?}", initiative.reservations);
    assert!(initiative.open_questions.is_empty(), "{:?}", initiative.open_questions);
    assert!(initiative.steering.is_none());
    assert_eq!(initiative.status, status);
    assert_eq!(store.shared_cycles_used("shared").unwrap(), 0);
    assert!(store.begin_steering("shared", "next").is_ok(), "the lease was released");
}

#[test]
fn wait_without_a_reason_is_refused_and_records_nothing() {
    let mut store = store();
    let snapshot = store.begin_steering("shared", "blank").unwrap();
    let before = serde_json::to_string(&store).unwrap();
    let refused = store
        .commit_steering(
            &snapshot,
            "blank",
            SteeringDecision::Wait { reason: " ".into() },
            vec![],
            "Prompt".into(),
            None,
        )
        .unwrap_err();
    assert_eq!(refused, InitiativeError::Invalid("wait requires a reason".into()));
    assert_eq!(serde_json::to_string(&store).unwrap(), before);
}

#[test]
fn a_steering_hold_on_an_owner_held_member_keeps_the_owner_hold() {
    let mut store = store();
    store.hold_member("shared", "two", "Owner reviews the API").unwrap();

    let committed = commit_any(
        &mut store,
        "hold",
        SteeringDecision::Hold {
            member: "two".into(),
            until: vec!["one:landed".into()],
            reason: "Wait for the contract".into(),
        },
    );

    assert_eq!(
        committed,
        SteeringCommit::NoOp {
            reason: "member 'two' already has an owner hold, which steering keeps: Owner reviews the API"
                .into()
        }
    );
    let initiative = store.find_initiative("shared").unwrap();
    assert_eq!(
        initiative.holds,
        [MemberHold {
            member: "two".into(),
            until: vec![],
            reason: "Owner reviews the API".into(),
            owner: true,
        }]
    );
    assert!(initiative.pending_actions.is_empty(), "{:?}", initiative.pending_actions);
    assert_eq!(initiative.steering_history.len(), 1);
}

#[test]
fn a_relay_to_members_a_finding_already_reaches_is_a_no_op() {
    let mut store = store();
    let finding = store
        .add_finding(Finding::new(
            "shared",
            FindingSource {
                kind: "owner".into(),
                id: "Stacey".into(),
            },
            vec!["one".into(), "two".into()],
            FindingKind::ContractChanged,
            "The producer listens on port 5001",
            vec![],
        ))
        .unwrap();

    let committed = commit_any(
        &mut store,
        "relay",
        SteeringDecision::Relay {
            finding: Box::new(finding.clone()),
            to: vec!["two".into()],
        },
    );

    assert_eq!(
        committed,
        SteeringCommit::NoOp {
            reason: format!("finding '{}' already reaches one, two", finding.id)
        }
    );
    assert_eq!(store.find_finding(&finding.id).unwrap(), &finding);
    assert_eq!(
        store.find_initiative("shared").unwrap().pending_actions,
        Vec::<DurableSteeringAction>::new()
    );
}

#[test]
fn a_relay_that_adds_a_recipient_is_still_an_action() {
    let mut store = store();
    let finding = store
        .add_finding(Finding::new(
            "shared",
            FindingSource {
                kind: "owner".into(),
                id: "Stacey".into(),
            },
            vec!["one".into()],
            FindingKind::ContractChanged,
            "The producer listens on port 5001",
            vec![],
        ))
        .unwrap();

    let committed = commit_any(
        &mut store,
        "relay",
        SteeringDecision::Relay {
            finding: Box::new(finding.clone()),
            to: vec!["two".into()],
        },
    );

    assert!(matches!(committed, SteeringCommit::Action(_)), "{committed:?}");
    assert_eq!(store.find_finding(&finding.id).unwrap().to, ["one", "two"]);
}

#[test]
fn a_cancelled_work_item_releases_only_the_reservation_it_held() {
    let mut store = store();
    store.find_initiative_mut("shared").unwrap().reservations = vec![
        MemberReservation {
            member: "one".into(),
            operation_id: "wi_withdrawn".into(),
            kind: ReservationKind::Formation,
        },
        MemberReservation {
            member: "two".into(),
            operation_id: "wi_running".into(),
            kind: ReservationKind::Formation,
        },
    ];

    assert!(store.release_reservations_held_by(&["wi_withdrawn".into()]));
    let reservations = &store.find_initiative("shared").unwrap().reservations;
    assert_eq!(reservations.len(), 1);
    assert_eq!(reservations[0].operation_id, "wi_running");
    assert!(
        !store.release_reservations_held_by(&["wi_withdrawn".into()]),
        "nothing left to release"
    );
    store.enforce_member_eligibility("shared", "one", &[]).unwrap();
}

#[test]
fn a_status_note_does_not_invalidate_an_inflight_assessment_or_grant_authority() {
    let mut store = store();
    let snapshot = store.begin_steering("shared", "steer-note").unwrap();
    store
        .record_initiative_status("shared", "The verification worker is running", Utc::now())
        .unwrap();
    store.validate_steering(&snapshot).unwrap();
    assert!(store.find_initiative("shared").unwrap().owner_decisions.is_empty());
    store
        .commit_steering(
            &snapshot,
            "steer-note",
            SteeringDecision::Wait {
                reason: "Await verification".into(),
            },
            vec![],
            "Assessment retained".into(),
            None,
        )
        .unwrap();
    assert_eq!(store.find_initiative("shared").unwrap().status_notes.len(), 1);
}

#[test]
fn recovering_campaign_blocks_only_its_member_and_decisions_cannot_clear_it() {
    use crate::recovery::RecoveryKind;
    let mut store = store();
    store.find_mut("one").unwrap().status = CampaignStatus::Active;
    store
        .record_campaign_recovery(
            "one",
            "missing-macos",
            RecoveryKind::CapabilityUnavailable,
            "macOS runner missing",
            Utc::now(),
        )
        .unwrap();
    assert!(store.enforce_member_eligibility("shared", "one", &[]).is_err());
    store.enforce_member_eligibility("shared", "two", &[]).unwrap();
    store
        .find_mut("one")
        .unwrap()
        .record_owner_decision("Keep current scope".into(), Utc::now(), 0)
        .unwrap();
    assert!(store.find("one").unwrap().recovery.as_ref().unwrap().is_pending());
    assert!(store.enforce_member_eligibility("shared", "one", &[]).is_err());
}
