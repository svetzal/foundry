use super::*;
use crate::campaign::{Campaign, CampaignStatus};

fn campaign(name: &str) -> Campaign {
    serde_json::from_value(serde_json::json!({
        "name":name,"project":"sample-rs","mission":"Ship the behavior",
        "done_evidence":[{"kind":"review","statement":"The behavior works"}],
        "authorized_by":"Stacey"
    }))
    .unwrap()
}

fn initiative() -> Initiative {
    serde_json::from_value(serde_json::json!({
        "name":"sample","outcome":"Both behaviors work","projects":["sample-rs"],
        "members":[{"kind":"campaign","name":"one"},{"kind":"campaign","name":"two"}],
        "done_evidence":[{"kind":"review","statement":"Both work together"}],
        "authorized_by":"Stacey"
    }))
    .unwrap()
}

fn store() -> CampaignStore {
    let mut store = CampaignStore::default();
    store.add(campaign("one")).unwrap();
    store.add(campaign("two")).unwrap();
    store.add_initiative(initiative(), &["sample-rs".into()]).unwrap();
    store
}

#[test]
fn completion_stops_exhausted_members_and_preserves_terminal_members() {
    for terminal_status in [CampaignStatus::Completed, CampaignStatus::Cancelled] {
        let mut store = store();
        let at = chrono::Utc::now();
        let member = store.find_mut("one").unwrap();
        member.status = CampaignStatus::Escalated;
        member.cycles_completed = member.budget.max_cycles;
        member.open_escalation = Some(crate::campaign::CampaignEscalation {
            reason: "Cycle budget exhausted".into(),
            escalated_at: at,
            cycles_completed: member.cycles_completed,
            cycles_landed: 0,
            question: Some("Extend the budget?".into()),
            choices: vec![],
            from_event_log: false,
        });
        store.find_mut("two").unwrap().status = terminal_status;
        store.refresh_questions("sample").unwrap();
        assert!(!store.initiative_view("sample").unwrap().open_questions.is_empty());

        assert_eq!(
            store.complete_initiative("sample", "Overall outcome verified", at).unwrap(),
            Transition::Applied
        );
        let member = store.find("one").unwrap();
        assert_eq!(member.status, CampaignStatus::Cancelled);
        assert!(member.open_escalation.is_none());
        assert!(member.pending_run_result.is_none());
        assert_eq!(
            member.owner_decisions.last().unwrap().decision,
            "Cancelled externally: initiative sample completed: Overall outcome verified"
        );
        assert_eq!(store.find("two").unwrap().status, terminal_status);
        let view = store.initiative_view("sample").unwrap();
        assert_eq!(view.status, InitiativeStatus::Completed);
        assert!(view.open_questions.is_empty());
        let before_retry = serde_json::to_string(&store).unwrap();
        assert_eq!(
            store.complete_initiative("sample", "Retry", at).unwrap(),
            Transition::AlreadySettled
        );
        assert_eq!(serde_json::to_string(&store).unwrap(), before_retry);
    }
}

#[test]
fn validates_projects_members_and_single_ownership_without_partial_changes() {
    let mut store = store();
    let before = serde_json::to_string(&store).unwrap();
    let mut invalid = initiative();
    invalid.name = "another".into();
    assert!(store.add_initiative(invalid, &["sample-rs".into()]).is_err());
    assert_eq!(serde_json::to_string(&store).unwrap(), before);
    let mut missing = initiative();
    missing.name = "missing".into();
    missing.members[0].name = "unknown".into();
    assert!(store.add_initiative(missing, &["sample-rs".into()]).is_err());
    let mut outside = initiative();
    outside.name = "outside".into();
    outside.projects = vec!["other".into()];
    assert!(store.add_initiative(outside, &["sample-rs".into()]).is_err());
}

#[test]
fn decision_fans_out_to_active_members_and_closes_only_applicable_questions() {
    let mut store = store();
    store.find_mut("one").unwrap().status = CampaignStatus::Active;
    store
        .record_question("sample", "Which behavior first?", &["one".into(), "two".into()])
        .unwrap();
    store
        .decide_initiative("sample", "One first", &["one".into()], chrono::Utc::now())
        .unwrap();
    assert_eq!(store.find("one").unwrap().owner_decisions.len(), 1);
    assert!(store.find("two").unwrap().owner_decisions.is_empty());
    assert_eq!(store.find_initiative("sample").unwrap().open_questions[0].members, ["two"]);
    store.decide_initiative("sample", "Proceed", &[], chrono::Utc::now()).unwrap();
    assert!(store.find_initiative("sample").unwrap().open_questions.is_empty());
    assert_eq!(store.find("two").unwrap().owner_decisions.len(), 1);
}

#[test]
fn findings_are_newest_first_bounded_and_consumed_only_for_seen_member() {
    let mut store = store();
    for text in ["first", "second"] {
        store
            .add_finding(Finding::new(
                "sample",
                FindingSource {
                    kind: "owner".into(),
                    id: "Stacey".into(),
                },
                vec![],
                FindingKind::Evidence,
                text,
                vec![],
            ))
            .unwrap();
    }
    let context = store.formation_context("one", 4096).unwrap().unwrap();
    assert_eq!(context.findings[0].text, "second");
    let ids = context.finding_ids();
    assert_eq!(store.consume_findings("sample", "one", "wi_1", &ids).unwrap().len(), 2);
    assert!(store.formation_context("one", 4096).unwrap().unwrap().findings.is_empty());
    assert_eq!(store.formation_context("two", 4096).unwrap().unwrap().findings.len(), 2);
    assert!(store.formation_context("two", 1).unwrap().unwrap().findings.is_empty());
}

#[test]
fn brief_is_owned_hashed_and_checked_on_read() {
    let home = tempfile::tempdir().unwrap();
    let mut store = store();
    let brief = store.set_brief("sample", home.path(), "Binding brief").unwrap();
    assert_eq!(brief.path, home.path().join("initiatives/sample/BRIEF.md").to_string_lossy());
    assert_eq!(store.show_brief("sample").unwrap(), "Binding brief");
    std::fs::write(&brief.path, "tampered").unwrap();
    assert!(store.show_brief("sample").is_err());
}

#[test]
fn question_deduplication_and_budget_are_enforced() {
    let mut store = store();
    store.find_initiative_mut("sample").unwrap().budget.max_open_questions = 1;
    store.record_question("sample", " Decide this ", &["one".into()]).unwrap();
    store.record_question("sample", "Decide this", &["two".into()]).unwrap();
    assert_eq!(store.find_initiative("sample").unwrap().open_questions.len(), 1);
    assert!(store.record_question("sample", "Another", &[]).is_err());
    assert!(store.enforce_can_advance("one", &[]).is_err());
    store.decide_initiative("sample", "Proceed", &[], chrono::Utc::now()).unwrap();
    assert!(store.enforce_can_advance("one", &["one".into(), "two".into()]).is_ok());
    store.find_initiative_mut("sample").unwrap().budget.max_active_members = 1;
    assert!(store.enforce_can_advance("one", &["two".into()]).is_err());
    store.find_mut("two").unwrap().cycles_completed = 20;
    assert!(store.enforce_can_advance("one", &[]).is_err());
}

#[test]
fn concurrent_decision_and_campaign_write_preserve_both_and_reads_never_tear() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("campaigns.json");
    store().save(&path).unwrap();
    let writer_path = path.clone();
    let writer = std::thread::spawn(move || {
        for _ in 0..25 {
            let mut guard = CampaignStore::lock_exclusive(&writer_path).unwrap();
            guard.store.find_mut("one").unwrap().cycles_completed += 1;
            guard.save().unwrap();
        }
    });
    for _ in 0..25 {
        let mut guard = CampaignStore::lock_exclusive(&path).unwrap();
        guard
            .store
            .decide_initiative("sample", "Continue", &[], chrono::Utc::now())
            .unwrap();
        guard.save().unwrap();
        assert_eq!(CampaignStore::load(&path).unwrap().initiatives.len(), 1);
    }
    writer.join().unwrap();
    let final_store = CampaignStore::load(&path).unwrap();
    assert_eq!(final_store.find("one").unwrap().cycles_completed, 25);
    assert_eq!(final_store.find("one").unwrap().owner_decisions.len(), 25);
}

#[test]
fn failed_brief_record_save_restores_previous_content_and_hash() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("campaigns.json");
    store().save(&path).unwrap();
    let mut guard = CampaignStore::lock_exclusive(&path).unwrap();
    let first = guard.set_initiative_brief("sample", dir.path(), "Original").unwrap();
    // The replacement cannot rename over a directory, after acquiring the guard.
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(guard.set_initiative_brief("sample", dir.path(), "Replacement").is_err());
    assert_eq!(guard.store.find_initiative("sample").unwrap().brief.as_ref(), Some(&first));
    assert_eq!(guard.store.show_brief("sample").unwrap(), "Original");
}

#[test]
fn simultaneous_member_reservations_share_the_cycle_and_active_budget() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("campaigns.json");
    let mut initial = store();
    initial.find_initiative_mut("sample").unwrap().budget.max_active_members = 1;
    initial.save(&path).unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let threads: Vec<_> = ["one", "two"]
        .into_iter()
        .map(|member| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                let mut guard = CampaignStore::lock_exclusive(&path).unwrap();
                if guard.store.enforce_can_advance(member, &[]).is_err() {
                    return false;
                }
                let campaign = guard.store.find_mut(member).unwrap();
                campaign.cycles_completed += 1;
                campaign.record_dispatched_objective("Reserved cycle".into());
                guard.save().unwrap();
                true
            })
        })
        .collect();
    assert_eq!(
        threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .filter(|reserved| *reserved)
            .count(),
        1
    );
    let store = CampaignStore::load(&path).unwrap();
    assert_eq!(store.campaigns.iter().map(|c| c.cycles_completed).sum::<u64>(), 1);
}

#[test]
fn initiative_view_coalesces_live_escalations_without_mutating_the_store() {
    let mut store = store();
    for name in ["one", "two"] {
        let campaign = store.find_mut(name).unwrap();
        campaign.status = CampaignStatus::Escalated;
        campaign.open_escalation = Some(crate::campaign::CampaignEscalation {
            reason: "Choose the order".into(),
            escalated_at: chrono::Utc::now(),
            cycles_completed: 0,
            cycles_landed: 0,
            question: Some("Which first?".into()),
            choices: vec![],
            from_event_log: false,
        });
    }
    let before = serde_json::to_string(&store).unwrap();
    let view = store.initiative_view("sample").unwrap();
    assert_eq!(view.status, InitiativeStatus::Escalated);
    assert_eq!(view.open_questions.len(), 1);
    assert_eq!(view.open_questions[0].members, ["one", "two"]);
    assert_eq!(
        view.open_questions[0].id,
        store.initiative_view("sample").unwrap().open_questions[0].id
    );
    assert_eq!(serde_json::to_string(&store).unwrap(), before);
}

#[test]
fn repeated_member_selectors_record_the_decision_once() {
    let mut store = store();
    let decision = store
        .decide_initiative("sample", "Proceed", &["one".into(), "one".into()], chrono::Utc::now())
        .unwrap();
    assert_eq!(decision.members, ["one"]);
    assert_eq!(store.find("one").unwrap().owner_decisions.len(), 1);
}

#[test]
fn cancellation_retains_its_reason_on_members_and_clears_pending_continuations() {
    let mut store = store();
    assert_eq!(
        store
            .cancel_initiative("sample", "Owner stopped the outcome", chrono::Utc::now())
            .unwrap(),
        Transition::Applied
    );
    assert_eq!(store.find("one").unwrap().status, CampaignStatus::Cancelled);
    assert_eq!(
        store.find("one").unwrap().owner_decisions[0].decision,
        "Cancelled externally: Owner stopped the outcome"
    );
    assert!(store.find("one").unwrap().pending_run_result.is_none());
}

#[test]
fn adding_an_initiative_cannot_install_an_external_brief_reference() {
    let mut store = CampaignStore::default();
    store.add(campaign("one")).unwrap();
    store.add(campaign("two")).unwrap();
    let mut definition = initiative();
    definition.brief = Some(InitiativeBrief {
        path: "/external/BRIEF.md".into(),
        hash: "forged".into(),
    });
    let before = serde_json::to_string(&store).unwrap();
    assert!(matches!(
        store.add_initiative(definition, &["sample-rs".into()]),
        Err(InitiativeError::Invalid(_))
    ));
    assert_eq!(serde_json::to_string(&store).unwrap(), before);
}

#[test]
fn final_cycle_can_form_to_assess_evidence_but_cannot_dispatch_more() {
    let mut store = store();
    store.find_mut("one").unwrap().cycles_completed = 20;
    assert!(store.enforce_can_form("one", &[]).is_ok());
    assert!(store.enforce_can_advance("one", &[]).is_err());
}

#[test]
fn member_questions_stay_visible_above_the_asking_budget_and_clear_with_member_status() {
    let mut store = store();
    store.find_initiative_mut("sample").unwrap().budget.max_open_questions = 1;
    for name in ["one", "two"] {
        let campaign = store.find_mut(name).unwrap();
        campaign.status = CampaignStatus::Escalated;
        campaign.open_escalation = Some(crate::campaign::CampaignEscalation {
            reason: format!("Question for {name}"),
            escalated_at: chrono::Utc::now(),
            cycles_completed: 0,
            cycles_landed: 0,
            question: None,
            choices: vec![],
            from_event_log: false,
        });
    }
    store.refresh_questions("sample").unwrap();
    assert_eq!(store.find_initiative("sample").unwrap().open_questions.len(), 2);
    store.find_mut("one").unwrap().status = CampaignStatus::Completed;
    store.find_mut("two").unwrap().status = CampaignStatus::Active;
    store.refresh_questions("sample").unwrap();
    assert!(store.find_initiative("sample").unwrap().open_questions.is_empty());
    assert_eq!(store.find_initiative("sample").unwrap().status, InitiativeStatus::Active);
}

#[test]
fn manual_questions_survive_member_refresh_and_progress_sets_active_status() {
    let mut store = store();
    store.find_mut("one").unwrap().status = CampaignStatus::Active;
    assert_eq!(store.initiative_view("sample").unwrap().status, InitiativeStatus::Active);
    store
        .record_question("sample", "An owner policy question", &["one".into()])
        .unwrap();
    store.refresh_questions("sample").unwrap();
    assert_eq!(store.find_initiative("sample").unwrap().open_questions.len(), 1);
}
