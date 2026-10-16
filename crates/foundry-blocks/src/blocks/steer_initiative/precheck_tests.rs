//! The deterministic precheck, `wait`, decisions already in force, and how
//! guard rejections and genuine failures are reported.

use super::tests::Fixture;
use super::*;
use crate::gateway::fakes::{FakeAgentGateway, FakeShellGateway};
use foundry_sdk::campaign::CampaignStatus;
use foundry_sdk::initiative::{
    Finding, FindingKind, FindingSource, SteeringDecision, SteeringRecord,
};
use foundry_sdk::payload::SteeringSettlement;
use foundry_sdk::throttle::Throttle;

/// A steer requested by a settled fact rather than by the owner.
fn automatic() -> Event {
    Event::new(
        EventType::InitiativeAdvanceRequested,
        "system".into(),
        Throttle::Full,
        serde_json::json!({"initiative":"shared","trigger_id":"evt_member_settled"}),
    )
}

fn seed(fixture: &Fixture, edit: impl FnOnce(&mut CampaignStore)) {
    let mut store = CampaignStore::load(&fixture.store).unwrap();
    edit(&mut store);
    store.save(&fixture.store).unwrap();
}

fn running(store: &mut CampaignStore, member: &str) {
    let campaign = store.find_mut(member).unwrap();
    campaign.status = CampaignStatus::Active;
    campaign.cycles_completed += 1;
    campaign.record_dispatched_objective("Deliver the next slice".into());
}

/// The initiative was assessed an hour ago, so only newer facts are unjudged.
fn steered_an_hour_ago(store: &mut CampaignStore) {
    store
        .find_initiative_mut("shared")
        .unwrap()
        .steering_history
        .push(SteeringRecord {
            id: "earlier".into(),
            prompt: "Earlier assessment".into(),
            decision: SteeringDecision::Wait {
                reason: "Both members run".into(),
            },
            provider: Some(AgentProvider::Claude),
            recorded_at: chrono::Utc::now() - chrono::Duration::hours(1),
            follow_up_of: None,
        });
}

fn fact(result: &TaskBlockResult) -> InitiativeSteeredPayload {
    result
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeSteered)
        .unwrap()
        .parse_payload()
        .unwrap()
}

fn requests_action(result: &TaskBlockResult) -> bool {
    result
        .events
        .iter()
        .any(|event| event.event_type == EventType::InitiativeActionRequested)
}

async fn steer_with(fixture: &Fixture, response: &str, trigger: &Event) -> TaskBlockResult {
    fixture
        .steer(FakeAgentGateway::success_with(response), FakeShellGateway::success())
        .execute(trigger)
        .await
        .unwrap()
}

#[tokio::test]
async fn no_agent_is_asked_when_no_steering_action_could_apply() {
    let fixture = Fixture::new();
    seed(&fixture, |store| {
        running(store, "one");
        running(store, "two");
        steered_an_hour_ago(store);
    });
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"wait","reason":"unused"},"audits":[]}"#,
    );

    let result = fixture
        .steer(agent.clone(), FakeShellGateway::success())
        .execute(&automatic())
        .await
        .unwrap();

    assert!(agent.invocations().is_empty(), "no agent session for a pass with nothing to do");
    assert!(result.success);
    let fact = fact(&result);
    assert_eq!(fact.outcome, SteeringOutcome::Skipped);
    let reason = fact.reason.clone().unwrap();
    assert!(reason.contains("one (has work in flight)"), "{reason}");
    assert!(reason.contains("2 of 2 active member slots"), "{reason}");
    assert!(fact.error.is_none());
    assert!(!requests_action(&result));
    assert_eq!(
        fact.settlement(),
        Some(SteeringSettlement::Landed(format!("steering skipped: {reason}")))
    );
    let initiative =
        CampaignStore::load(&fixture.store).unwrap().initiative_view("shared").unwrap();
    assert_eq!(initiative.steering_history.len(), 1, "a skip records no decision");
    assert!(initiative.steering.is_none());
}

#[tokio::test]
async fn an_owner_request_is_assessed_even_when_nothing_could_apply() {
    let fixture = Fixture::new();
    seed(&fixture, |store| {
        running(store, "one");
        running(store, "two");
        steered_an_hour_ago(store);
    });
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"wait","reason":"Both members run"},"audits":[]}"#,
    );

    fixture
        .steer(agent.clone(), FakeShellGateway::success())
        .execute(&Fixture::trigger(None))
        .await
        .unwrap();

    assert_eq!(agent.invocations().len(), 1);
}

#[tokio::test]
async fn the_agent_sees_the_eligible_members_and_a_wait_beside_them_is_recorded() {
    let fixture = Fixture::new();
    seed(&fixture, |store| {
        running(store, "one");
        steered_an_hour_ago(store);
    });
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"wait","reason":"Two needs the contract from one first"},"audits":[]}"#,
    );

    let result = fixture
        .steer(agent.clone(), FakeShellGateway::success())
        .execute(&automatic())
        .await
        .unwrap();

    let prompt = &agent.invocations()[0].prompt;
    assert!(prompt.contains(r#""eligibility":{"#), "{prompt}");
    assert!(prompt.contains(r#""eligible":["two"]"#), "{prompt}");
    assert!(prompt.contains(r#"wait: {"decision":"wait""#));
    assert!(prompt.contains("Use ask only for a question the owner must answer"));
    let fact = fact(&result);
    assert_eq!(fact.outcome, SteeringOutcome::Waited);
    assert_eq!(fact.eligible_members, ["two"]);
    assert_eq!(fact.reason.as_deref(), Some("Two needs the contract from one first"));
}

#[tokio::test]
async fn wait_is_recorded_lands_and_opens_no_question() {
    let fixture = Fixture::new();
    let status = CampaignStore::load(&fixture.store).unwrap().initiative_view("shared").unwrap();

    let result = steer_with(
        &fixture,
        r#"{"action":{"decision":"wait","reason":"Workers are still running"},"audits":[]}"#,
        &Fixture::trigger(None),
    )
    .await;

    assert!(result.success);
    assert!(!requests_action(&result), "a wait requests no action");
    let fact = fact(&result);
    assert_eq!(fact.outcome, SteeringOutcome::Waited);
    assert_eq!(
        fact.decision,
        Some(SteeringDecision::Wait {
            reason: "Workers are still running".into()
        })
    );
    assert_eq!(
        fact.settlement(),
        Some(SteeringSettlement::Landed("steering waited: Workers are still running".into()))
    );
    let initiative =
        CampaignStore::load(&fixture.store).unwrap().initiative_view("shared").unwrap();
    assert!(initiative.open_questions.is_empty(), "{:?}", initiative.open_questions);
    assert_eq!(initiative.status, status.status);
    assert_eq!(initiative.steering_history.len(), 1);
    assert!(initiative.pending_actions.is_empty(), "{:?}", initiative.pending_actions);
    assert!(initiative.steering.is_none());
}

#[tokio::test]
async fn a_wait_without_a_reason_is_rejected() {
    let fixture = Fixture::new();

    let result = steer_with(
        &fixture,
        r#"{"action":{"decision":"wait","reason":"  "},"audits":[]}"#,
        &Fixture::trigger(None),
    )
    .await;

    let fact = fact(&result);
    assert_eq!(fact.outcome, SteeringOutcome::Rejected);
    assert_eq!(fact.error.as_deref(), Some("wait requires a reason"));
}

#[tokio::test]
async fn a_hold_on_an_owner_held_member_changes_nothing_and_lands() {
    let fixture = Fixture::new();
    seed(&fixture, |store| {
        store.hold_member("shared", "two", "Owner reviews the API").unwrap();
    });

    let result = steer_with(
        &fixture,
        r#"{"action":{"decision":"hold","member":"two","until":["one:landed"],"reason":"Wait for one"},"audits":[]}"#,
        &Fixture::trigger(None),
    )
    .await;

    assert!(result.success);
    assert!(!requests_action(&result));
    let fact = fact(&result);
    assert_eq!(fact.outcome, SteeringOutcome::NoOp);
    assert_eq!(
        fact.reason.as_deref(),
        Some("member 'two' already has an owner hold, which steering keeps: Owner reviews the API")
    );
    assert!(matches!(fact.settlement(), Some(SteeringSettlement::Landed(_))));
    let initiative =
        CampaignStore::load(&fixture.store).unwrap().initiative_view("shared").unwrap();
    assert_eq!(initiative.holds.len(), 1);
    assert!(initiative.holds[0].owner);
    assert_eq!(initiative.holds[0].until, Vec::<String>::new());
}

#[tokio::test]
async fn a_relay_to_members_the_finding_already_reaches_changes_nothing() {
    let fixture = Fixture::new();
    let finding = Finding::new(
        "shared",
        FindingSource {
            kind: "owner".into(),
            id: "Stacey".into(),
        },
        vec!["one".into(), "two".into()],
        FindingKind::ContractChanged,
        "The producer listens on port 5001",
        vec![],
    );
    seed(&fixture, |store| {
        store.add_finding(finding.clone()).unwrap();
    });
    let response = serde_json::json!({
        "action": {"decision": "relay", "finding": finding, "to": ["one", "two"]},
        "audits": []
    });

    let result = steer_with(&fixture, &response.to_string(), &Fixture::trigger(None)).await;

    let fact = fact(&result);
    assert_eq!(fact.outcome, SteeringOutcome::NoOp, "{:?}", fact.error);
    assert_eq!(fact.reason, Some(format!("finding '{}' already reaches one, two", finding.id)));
    assert!(!requests_action(&result));
    let store = CampaignStore::load(&fixture.store).unwrap();
    assert_eq!(store.find_finding(&finding.id).unwrap().to, ["one", "two"]);
}

#[tokio::test]
async fn each_guard_rejection_settles_cancelled_with_its_reason() {
    type Setup = fn(&mut CampaignStore);
    let cases: [(&str, Setup, &str); 3] = [
        (
            r#"{"action":{"decision":"advance","member":"two"},"audits":[]}"#,
            |store| store.hold_member("shared", "two", "Owner reviews the API").unwrap(),
            "member 'two' is held: Owner reviews the API",
        ),
        (
            r#"{"action":{"decision":"advance","member":"two"},"audits":[]}"#,
            |store| {
                store.find_initiative_mut("shared").unwrap().budget.max_active_members = 1;
                running(store, "one");
            },
            "initiative 'shared' has reached its active member limit",
        ),
        (
            r#"{"action":{"decision":"dispatch","project":"elsewhere","objective":"Probe","member":null},"audits":[]}"#,
            |_| {},
            "action project is outside initiative scope",
        ),
    ];
    for (response, setup, refusal) in cases {
        let fixture = Fixture::new();
        seed(&fixture, setup);

        let result = steer_with(&fixture, response, &Fixture::trigger(None)).await;

        assert!(result.success, "{refusal}: a rejection is not a failure");
        assert!(!requests_action(&result), "{refusal}");
        let fact = fact(&result);
        assert_eq!(fact.outcome, SteeringOutcome::Rejected, "{refusal}");
        assert_eq!(fact.error.as_deref(), Some(refusal));
        assert_eq!(
            fact.settlement(),
            Some(SteeringSettlement::Cancelled(format!("steering rejected: {refusal}")))
        );
        let initiative =
            CampaignStore::load(&fixture.store).unwrap().initiative_view("shared").unwrap();
        assert!(initiative.pending_actions.is_empty(), "{refusal}");
        assert!(initiative.steering.is_none(), "{refusal}");
    }
}

#[tokio::test]
async fn a_genuine_agent_failure_or_unusable_output_still_fails() {
    for agent in [
        FakeAgentGateway::failure("provider crashed"),
        FakeAgentGateway::success_with("I could not decide."),
    ] {
        let fixture = Fixture::new();

        let result = fixture
            .steer(agent, FakeShellGateway::success())
            .execute(&Fixture::trigger(None))
            .await
            .unwrap();

        assert!(!result.success);
        let fact = fact(&result);
        assert_eq!(fact.outcome, SteeringOutcome::Failed);
        assert_eq!(fact.settlement(), None, "the ordinary failed settlement applies");
    }
}
