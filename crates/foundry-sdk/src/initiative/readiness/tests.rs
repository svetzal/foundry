use super::*;
use crate::initiative::{MemberHold, SteeringDecision};

fn store() -> CampaignStore {
    let mut store = CampaignStore::default();
    for name in ["library", "consumer", "independent"] {
        store.add(serde_json::from_value(serde_json::json!({"name":name,"project":"p","mission":"Ship behavior","authorized_by":"owner","status":"active","done_evidence":[{"kind":"review","statement":"works"}]})).unwrap()).unwrap();
    }
    store.add_initiative(serde_json::from_value(serde_json::json!({"name":"release","outcome":"works","projects":["p"],"authorized_by":"owner","members":[{"kind":"campaign","name":"library"},{"kind":"campaign","name":"consumer","after":["library"]},{"kind":"campaign","name":"independent"}],"done_evidence":[{"kind":"review","statement":"works"}]})).unwrap(), &["p".into()]).unwrap();
    store
}

#[test]
fn partial_landing_does_not_release_bare_dependency_but_completion_does() {
    let mut store = store();
    store.find_mut("library").unwrap().cycles_landed = 3;
    let before = serde_json::to_value(&store).unwrap();
    let readiness = store.member_readiness("release", "consumer", &[], &[]).unwrap();
    assert!(!readiness.ready);
    assert_eq!(readiness.dependencies[0].milestone, "completed");
    assert!(!readiness.dependencies[0].satisfied);
    assert!(
        readiness
            .blockers
            .iter()
            .any(|blocker| blocker.kind == ReadinessBlockerKind::DependencyUnmet)
    );
    assert!(store.member_readiness("release", "independent", &[], &[]).unwrap().ready);
    assert_eq!(
        before,
        serde_json::to_value(&store).unwrap(),
        "inspection creates no question, decision or cycle"
    );
    assert!(store.enforce_can_form("consumer", &[]).is_err());
    store.find_mut("library").unwrap().status = CampaignStatus::Completed;
    let readiness = store.member_readiness("release", "consumer", &[], &[]).unwrap();
    assert!(readiness.ready);
    assert_eq!(readiness.dependencies[0].evidence, ["campaign:library:completed"]);
    store.enforce_can_form("consumer", &[]).unwrap();
}

#[test]
fn explicit_landing_and_custom_milestone_release_only_their_exact_predicate() {
    let mut store = store();
    store.find_mut("library").unwrap().cycles_landed = 1;
    assert!(store.dependency_readiness("release", "library:landed").unwrap().satisfied);
    assert!(!store.dependency_readiness("release", "library:completed").unwrap().satisfied);
    store
        .record_milestone(
            "release",
            "library",
            "contract-ready",
            vec!["contract suite v2 passed".into()],
        )
        .unwrap();
    let dependency = store.dependency_readiness("release", "library:contract-ready").unwrap();
    assert!(dependency.satisfied);
    assert_eq!(dependency.evidence, ["contract suite v2 passed"]);
    assert!(!store.dependency_readiness("release", "library:published").unwrap().satisfied);
}

#[test]
fn completed_milestone_cannot_substitute_for_campaign_outcome() {
    let mut store = store();
    store
        .record_milestone(
            "release",
            "library",
            "completed",
            vec!["someone declared completion".into()],
        )
        .unwrap();
    for status in [
        CampaignStatus::Active,
        CampaignStatus::Paused,
        CampaignStatus::Cancelled,
    ] {
        store.find_mut("library").unwrap().status = status;
        assert!(!store.dependency_readiness("release", "library").unwrap().satisfied);
        assert!(!store.dependency_readiness("release", "library:completed").unwrap().satisfied);
    }
}

#[test]
fn owner_milestone_hold_releases_on_exact_evidence_and_indefinite_hold_stays() {
    let mut store = store();
    store.find_mut("library").unwrap().status = CampaignStatus::Completed;
    store.find_initiative_mut("release").unwrap().holds.push(MemberHold {
        member: "consumer".into(),
        until: vec!["library:published".into()],
        reason: "wait for publication".into(),
        owner: true,
    });
    let readiness = store.member_readiness("release", "consumer", &[], &[]).unwrap();
    assert!(!readiness.ready);
    assert!(
        readiness
            .dependencies
            .iter()
            .any(|dependency| dependency.prerequisite == "library:published"
                && !dependency.satisfied)
    );
    store
        .record_milestone("release", "library", "published", vec!["registry receipt v2".into()])
        .unwrap();
    assert!(store.member_readiness("release", "consumer", &[], &[]).unwrap().ready);
    store.hold_member("release", "consumer", "owner review").unwrap();
    assert!(!store.member_readiness("release", "consumer", &[], &[]).unwrap().ready);
}

#[test]
fn malformed_or_cyclic_after_and_hold_dependencies_fail_closed() {
    for prerequisite in [
        "library:",
        ":completed",
        "library: ",
        " library",
        "library:published:extra",
    ] {
        let mut store = store();
        store.find_initiative_mut("release").unwrap().members[1].after = vec![prerequisite.into()];
        assert!(store.find_initiative("release").unwrap().validate().is_err(), "{prerequisite}");
        assert!(store.member_readiness("release", "consumer", &[], &[]).is_err());
    }
    let mut store = store();
    let before = serde_json::to_value(&store).unwrap();
    assert!(
        store
            .commit_owner_action(
                "release",
                "cycle",
                SteeringDecision::Hold {
                    member: "library".into(),
                    until: vec!["consumer:landed".into()],
                    reason: "circular wait".into()
                },
                "hold".into()
            )
            .is_err()
    );
    assert_eq!(before, serde_json::to_value(&store).unwrap());
    store.find_initiative_mut("release").unwrap().holds.push(MemberHold {
        member: "consumer".into(),
        until: vec!["missing:landed".into()],
        reason: "bad stored prerequisite".into(),
        owner: false,
    });
    assert!(store.member_readiness("release", "consumer", &[], &[]).is_err());
}

#[test]
fn direct_admission_obeys_paused_terminal_and_owner_controls() {
    let mut store = store();
    for status in [
        CampaignStatus::Paused,
        CampaignStatus::Completed,
        CampaignStatus::Cancelled,
    ] {
        store.find_mut("independent").unwrap().status = status;
        assert!(!store.member_readiness("release", "independent", &[], &[]).unwrap().ready);
        assert!(store.enforce_can_form("independent", &[]).is_err());
    }
    store.find_mut("independent").unwrap().status = CampaignStatus::Active;
    let observed_ready = store.member_readiness("release", "independent", &[], &[]).unwrap();
    assert!(observed_ready.ready);
    store
        .hold_member("release", "independent", "owner review after inspection")
        .unwrap();
    assert!(
        store.enforce_can_form("independent", &[]).is_err(),
        "an earlier ready projection cannot override a new hold"
    );
    store.release_member("release", "independent").unwrap();
    store
        .record_question("release", "Which public contract?", &["independent".into()])
        .unwrap();
    assert!(
        store.enforce_can_form("independent", &[]).is_err(),
        "an earlier ready projection cannot override a new question"
    );
    let readiness = store.member_readiness("release", "independent", &[], &["p".into()]).unwrap();
    assert!(
        readiness
            .blockers
            .iter()
            .any(|blocker| blocker.kind == ReadinessBlockerKind::OwnerQuestion)
    );
    assert!(
        readiness
            .blockers
            .iter()
            .any(|blocker| blocker.kind == ReadinessBlockerKind::RepositoryBusy)
    );
    let restored: MemberReadiness =
        serde_json::from_value(serde_json::to_value(&readiness).unwrap()).unwrap();
    assert_eq!(readiness, restored);
}

#[test]
fn global_authority_and_owner_questions_block_new_dispatch() {
    use crate::initiative::InitiativeStatus;
    let mut store = store();
    assert!(store.steering_precheck("release", &[], &[]).unwrap().dispatch_open);
    store.find_initiative_mut("release").unwrap().authorized_by = None;
    assert!(!store.steering_precheck("release", &[], &[]).unwrap().dispatch_open);
    store.find_initiative_mut("release").unwrap().authorized_by = Some("owner".into());
    store
        .record_question("release", "Which contract applies to the initiative?", &[])
        .unwrap();
    // Persisted empty scope is the existing predicate for every member,
    // including a task slot created after the question was recorded.
    store.find_initiative_mut("release").unwrap().open_questions[0].members.clear();
    assert!(!store.steering_precheck("release", &[], &[]).unwrap().dispatch_open);
    store.find_initiative_mut("release").unwrap().status = InitiativeStatus::Cancelled;
    assert!(!store.steering_precheck("release", &[], &[]).unwrap().dispatch_open);
}
