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
    store.commit_steering(
        &snapshot,
        id,
        decision,
        vec![],
        "Observed current evidence".into(),
        Some(AgentProvider::Codex),
    )
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
    assert!(store.find_initiative("shared").unwrap().reservations.is_empty());
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

fn projection(state: &str) -> IntentProjection {
    IntentProjection {
        goals: vec![Goal {
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
    assert!(store.refresh_derived_intent("goal", &projection("created")).unwrap().is_empty());
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
        assert!(initiative.close_proposals.is_empty());
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
    store.decide_initiative("shared", "Proceed", &[], Utc::now()).unwrap();
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
