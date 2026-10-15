#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use foundry_sdk::initiative::{DoneEvidence, IntegrationGateResult, OutcomeReview};
use serde_json::json;
use std::cell::{Cell, RefCell};

struct FakeTracker {
    intent: RefCell<IntentProjection>,
    calls: RefCell<Vec<String>>,
    fail_goal: Cell<bool>,
    lose_receipt: Cell<bool>,
}

impl Tracker for FakeTracker {
    fn read(&self, _repo: &Path) -> Result<IntentProjection, Status> {
        Ok(self.intent.borrow().clone())
    }
    fn close(&self, _repo: &Path, reference: &str, expected: &str) -> Result<(), Status> {
        self.calls.borrow_mut().push(reference.into());
        let (kind, id) = close_reference(reference)?;
        if kind == "goal" && self.fail_goal.get() {
            return Err(Status::unavailable("synthetic Tracker failure"));
        }
        let mut projection = self.intent.borrow_mut();
        if kind == "goal" {
            projection.goals.iter_mut().find(|goal| goal.id == id).unwrap().state = expected.into();
        } else {
            projection
                .interactions
                .iter_mut()
                .find(|interaction| interaction.id == id)
                .unwrap()
                .state = expected.into();
        }
        if self.lose_receipt.replace(false) {
            return Err(Status::unavailable(
                "synthetic process ended after Tracker accepted close",
            ));
        }
        Ok(())
    }
}

struct Fixture {
    home: tempfile::TempDir,
    repo: std::path::PathBuf,
    path: std::path::PathBuf,
    ledger: std::path::PathBuf,
    registry: Registry,
    tracker: FakeTracker,
}

/// Runs a fixture git command isolated from ambient command-line config.
///
/// A Foundry agent session injects `remote.origin.pushurl` through
/// `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_*`, which would redirect this
/// fixture's push to its local `origin` into a disabled remote. Git ignores
/// the numbered keys once the count is removed.
fn git(repo: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn prepared_outcome_store(
    repo: &Path,
    registry: &Registry,
    projection: &IntentProjection,
    journey: bool,
) -> CampaignStore {
    let from = if journey {
        "et:journey:path"
    } else {
        "et:goal:outcome"
    };
    let mut initiative = derive_from_projection(projection, from, vec!["sample".into()]).unwrap();
    initiative.name = "proof".into();
    initiative.authorized_by = Some("Owner".into());
    initiative.derive_source = Some(foundry_sdk::initiative::DeriveSource {
        repo: repo.to_string_lossy().into(),
        from: from.into(),
    });
    let mut store = CampaignStore::default();
    store.add_initiative(initiative, &["sample".into()]).unwrap();
    store.refresh_derived_intent("proof", projection).unwrap();
    store
        .find_initiative_mut("proof")
        .unwrap()
        .done_evidence
        .push(DoneEvidence::Gate {
            command: "true".into(),
            host: None,
            required: true,
            artifacts: vec![],
        });
    let context = foundry_blocks::blocks::capture_initiative_external_context(
        store.find_initiative("proof").unwrap(),
        registry,
    )
    .unwrap();
    let snapshot = store.outcome_snapshot("proof", context.repositories).unwrap();
    let gate_index = store.find_initiative("proof").unwrap().done_evidence.len() - 1;
    store
        .record_integration_result(
            "proof",
            IntegrationGateResult {
                id: "gate_pass".into(),
                gate_index,
                host: "synthetic".into(),
                command: "true".into(),
                snapshot: snapshot.clone(),
                passed: true,
                exit_code: Some(0),
                report: "capture.md".into(),
                artifacts: vec![],
                recorded_at: Utc::now(),
            },
        )
        .unwrap();
    let reviews = store
        .find_initiative("proof")
        .unwrap()
        .done_evidence
        .iter()
        .filter_map(|evidence| match evidence {
            DoneEvidence::Review { statement } => Some(OutcomeReview {
                statement: statement.clone(),
                affirmed: true,
                evidence: vec!["read-only synthetic outcome evidence".into()],
            }),
            DoneEvidence::Gate { .. } => None,
        })
        .collect();
    store.propose_completion("proof", "done_proof", snapshot, reviews).unwrap();
    store
}

impl Fixture {
    fn new(extra: Vec<serde_json::Value>, journey: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--quiet", "--initial-branch=main"]);
        git(&repo, &["config", "user.name", "Test"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        let mut interactions = vec![
            json!({"id":"contract", "description":"The contract works", "state":"create", "goals":["outcome"]}),
        ];
        interactions.extend(extra);
        let projection: IntentProjection = serde_json::from_value(json!({"goals":[{"id":"outcome", "description":"A usable outcome", "success_criteria":"The owner can use it", "state":"create"}], "interactions":interactions, "journeys":[{"id":"path", "goal":"outcome", "steps":["contract"]}]})).unwrap();
        std::fs::create_dir(repo.join(".et")).unwrap();
        std::fs::write(repo.join(".et/projection.json"), serde_json::to_vec(&projection).unwrap())
            .unwrap();
        git(&repo, &["add", ".et/projection.json"]);
        git(&repo, &["commit", "--quiet", "-m", "Seed synthetic intent"]);
        let origin = home.path().join("origin.git");
        git(
            &repo,
            &[
                "init",
                "--quiet",
                "--bare",
                "--initial-branch=main",
                origin.to_str().unwrap(),
            ],
        );
        git(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&repo, &["push", "--quiet", "origin", "main"]);
        let registry: Registry = serde_json::from_value(json!({"version":2,"projects":[{"name":"sample", "path":repo, "stack":"rust", "agent":"codex", "repo":"local/sample", "branch":"main"}]})).unwrap();
        let store = prepared_outcome_store(&repo, &registry, &projection, journey);
        let path = home.path().join("campaigns.json");
        let ledger = home.path().join("work-items.json");
        store.save(&path).unwrap();
        Self {
            home,
            repo,
            path,
            ledger,
            registry,
            tracker: FakeTracker {
                intent: RefCell::new(projection),
                calls: RefCell::new(vec![]),
                fail_goal: Cell::new(false),
                lose_receipt: Cell::new(false),
            },
        }
    }
    fn complete(&self) -> Result<(crate::proto::Initiative, bool), Status> {
        complete_with(
            &self.path,
            &self.ledger,
            &self.registry,
            "proof",
            "Owner verified outcome",
            &self.tracker,
        )
    }
    fn view(&self) -> Initiative {
        CampaignStore::load(&self.path).unwrap().initiative_view("proof").unwrap()
    }
}

#[test]
fn failed_goal_close_retains_confirmation_and_interaction_receipt_then_retry_finishes() {
    let fixture = Fixture::new(vec![], false);
    fixture.tracker.fail_goal.set(true);
    assert_eq!(fixture.complete().unwrap_err().code(), tonic::Code::Unavailable);
    let held = fixture.view();
    assert_ne!(held.status, InitiativeStatus::Completed);
    assert_eq!(held.completion_receipts.len(), 1);
    assert!(held.completion_confirmation.unwrap().last_error.is_some());
    fixture.tracker.fail_goal.set(false);
    let (completed, changed) = fixture.complete().unwrap();
    assert!(changed);
    assert_eq!(completed.status, "completed");
    assert_eq!(completed.completion_receipts.len(), 2);
    assert_eq!(
        fixture
            .tracker
            .calls
            .borrow()
            .iter()
            .filter(|reference| reference.as_str() == "et:interaction:contract")
            .count(),
        1
    );
    let bytes = std::fs::read(&fixture.path).unwrap();
    assert!(!fixture.complete().unwrap().1);
    assert_eq!(std::fs::read(&fixture.path).unwrap(), bytes);
}

#[test]
fn restart_resumes_only_owner_confirmed_completion_and_recovers_accepted_close_without_reclosing() {
    let fixture = Fixture::new(vec![], false);
    assert!(
        recover_with(&fixture.path, &fixture.ledger, &fixture.registry, &fixture.tracker)
            .unwrap()
            .is_empty(),
        "a fresh store has no completion to recover"
    );
    assert!(fixture.tracker.calls.borrow().is_empty());
    fixture.tracker.lose_receipt.set(true);
    fixture.complete().unwrap_err();
    assert!(
        fixture.view().completion_receipts.is_empty(),
        "expected `fixture.view().completion_receipts` to be empty: {:?}",
        fixture.view().completion_receipts
    );
    assert_eq!(fixture.tracker.intent.borrow().interactions[0].state, "created");
    let recovered =
        recover_with(&fixture.path, &fixture.ledger, &fixture.registry, &fixture.tracker).unwrap();
    assert!(
        recovered
            .iter()
            .any(|event| event.event_type == foundry_sdk::event::EventType::InitiativeCompleted)
    );
    assert_eq!(fixture.view().status, InitiativeStatus::Completed);
    assert_eq!(
        fixture
            .tracker
            .calls
            .borrow()
            .iter()
            .filter(|reference| reference.as_str() == "et:interaction:contract")
            .count(),
        1
    );
}

#[test]
fn withdrawn_or_changed_intent_cannot_be_closed_under_old_evidence() {
    for withdrawal in [true, false] {
        let fixture = Fixture::new(vec![], false);
        if withdrawal {
            fixture.tracker.intent.borrow_mut().interactions[0].state = "discarded".into();
        } else {
            fixture.tracker.intent.borrow_mut().interactions[0].description =
                "A changed contract".into();
        }
        assert_eq!(fixture.complete().unwrap_err().code(), tonic::Code::FailedPrecondition);
        assert!(fixture.tracker.calls.borrow().is_empty());
        assert_ne!(fixture.view().status, InitiativeStatus::Completed);
    }
}

#[test]
fn current_goal_children_must_settle_approved_intent_outside_a_selected_journey() {
    let fixture = Fixture::new(
        vec![
            json!({"id":"other", "description":"Another required interaction", "state":"create", "goals":["outcome"]}),
        ],
        true,
    );
    assert_eq!(fixture.complete().unwrap_err().code(), tonic::Code::FailedPrecondition);
    assert_eq!(fixture.tracker.intent.borrow().goals[0].state, "create");
    assert!(
        !fixture
            .tracker
            .calls
            .borrow()
            .iter()
            .any(|reference| reference == "et:goal:outcome")
    );
    assert_ne!(fixture.view().status, InitiativeStatus::Completed);
}

#[test]
fn unapproved_and_discarded_children_do_not_block_goal_reality() {
    let fixture = Fixture::new(
        vec![
            json!({"id":"later", "description":"Future work", "state":"plan", "goals":["outcome"]}),
            json!({"id":"withdrawn", "description":"Withdrawn work", "state":"discarded", "goals":["outcome"]}),
        ],
        true,
    );
    assert_eq!(fixture.complete().unwrap().0.status, "completed");
    assert_eq!(fixture.tracker.calls.borrow().len(), 2);
}

#[test]
fn a_new_trunk_invalidates_done_evidence_before_confirmation_or_tracker_writes() {
    let fixture = Fixture::new(vec![], false);
    std::fs::write(fixture.repo.join("new.txt"), "A changed trunk").unwrap();
    git(&fixture.repo, &["add", "new.txt"]);
    git(&fixture.repo, &["commit", "--quiet", "-m", "Change trunk"]);
    assert_eq!(fixture.complete().unwrap_err().code(), tonic::Code::FailedPrecondition);
    assert!(fixture.view().completion_confirmation.is_none());
    assert!(fixture.tracker.calls.borrow().is_empty());
}

#[test]
fn origin_advancing_after_the_gate_cannot_be_hidden_by_an_unchanged_local_trunk() {
    let fixture = Fixture::new(vec![], false);
    let peer = fixture.home.path().join("peer");
    git(
        fixture.home.path(),
        &[
            "clone",
            "--quiet",
            fixture.home.path().join("origin.git").to_str().unwrap(),
            peer.to_str().unwrap(),
        ],
    );
    git(&peer, &["config", "user.name", "Other author"]);
    git(&peer, &["config", "user.email", "other@example.invalid"]);
    std::fs::write(peer.join("changed.txt"), "Remote changed").unwrap();
    git(&peer, &["add", "changed.txt"]);
    git(&peer, &["commit", "--quiet", "-m", "Advance remote trunk"]);
    git(&peer, &["push", "--quiet", "origin", "main"]);
    assert_eq!(fixture.complete().unwrap_err().code(), tonic::Code::FailedPrecondition);
    assert!(fixture.view().completion_confirmation.is_none());
    assert!(fixture.tracker.calls.borrow().is_empty());
}

#[test]
fn unavailable_origin_prevents_completion_with_saved_gate_evidence() {
    let fixture = Fixture::new(vec![], false);
    git(&fixture.repo, &["remote", "remove", "origin"]);
    assert_eq!(fixture.complete().unwrap_err().code(), tonic::Code::Unavailable);
    assert!(fixture.view().completion_confirmation.is_none());
    assert!(fixture.tracker.calls.borrow().is_empty());
}

#[test]
fn a_failed_required_gate_after_review_cannot_be_bypassed_by_the_owner() {
    let fixture = Fixture::new(vec![], false);
    let mut store = CampaignStore::load(&fixture.path).unwrap();
    let mut failed = store.find_initiative("proof").unwrap().integration_results[0].clone();
    failed.id = "gate_fail".into();
    failed.passed = false;
    failed.exit_code = Some(1);
    store.record_integration_result("proof", failed).unwrap();
    store.save(&fixture.path).unwrap();
    assert_eq!(fixture.complete().unwrap_err().code(), tonic::Code::FailedPrecondition);
    assert!(fixture.tracker.calls.borrow().is_empty());
    assert!(fixture.view().completion_confirmation.is_none());
}

#[test]
fn queued_and_held_initiative_tasks_block_owner_completion() {
    use foundry_sdk::work_item::{
        WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkItemStore, WorkLane,
    };
    for state in [
        WorkItemState::Queued,
        WorkItemState::Held,
        WorkItemState::Running,
    ] {
        let fixture = Fixture::new(vec![], false);
        let mut item = WorkItem::submitted(
            WorkItemSpec {
                project: "sample".into(),
                objective: "Owned task".into(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Campaign,
                origin: "synthetic".into(),
                trace_id: None,
            },
            Utc::now(),
        )
        .with_source(Some(foundry_sdk::work_source::WorkSource::initiative("proof", 0)));
        item.state = state;
        let mut ledger = WorkItemStore::default();
        ledger.items.push(item);
        ledger.save(&fixture.ledger).unwrap();
        assert_eq!(fixture.complete().unwrap_err().code(), tonic::Code::FailedPrecondition);
        assert!(fixture.tracker.calls.borrow().is_empty());
        assert!(fixture.view().completion_confirmation.is_none());
    }
}

#[test]
fn ad_hoc_completion_requires_review_and_gates_but_never_invokes_tracker() {
    let fixture = Fixture::new(vec![], false);
    let mut store = CampaignStore::load(&fixture.path).unwrap();
    let initiative = store.find_initiative_mut("proof").unwrap();
    initiative.derive_source = None;
    initiative.intent_refs.clear();
    initiative.members.clear();
    initiative.completion_proposal = None;
    store.save(&fixture.path).unwrap();
    assert_eq!(fixture.complete().unwrap_err().code(), tonic::Code::FailedPrecondition);
    let context = foundry_blocks::blocks::capture_initiative_external_context(
        store.find_initiative("proof").unwrap(),
        &fixture.registry,
    )
    .unwrap();
    let snapshot = store.outcome_snapshot("proof", context.repositories).unwrap();
    let mut result = store.find_initiative("proof").unwrap().integration_results[0].clone();
    result.id = "ad_hoc_gate".into();
    result.snapshot = snapshot.clone();
    store.record_integration_result("proof", result).unwrap();
    let reviews = store
        .find_initiative("proof")
        .unwrap()
        .done_evidence
        .iter()
        .filter_map(|evidence| match evidence {
            DoneEvidence::Review { statement } => Some(OutcomeReview {
                statement: statement.clone(),
                affirmed: true,
                evidence: vec!["Synthetic review".into()],
            }),
            DoneEvidence::Gate { .. } => None,
        })
        .collect();
    store.propose_completion("proof", "ad_hoc_done", snapshot, reviews).unwrap();
    store.save(&fixture.path).unwrap();
    assert_eq!(fixture.complete().unwrap().0.status, "completed");
    assert!(fixture.tracker.calls.borrow().is_empty());
}
