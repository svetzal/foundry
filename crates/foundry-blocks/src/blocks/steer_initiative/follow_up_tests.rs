//! The one follow-up pass after a step that changes state but dispatches nothing.

use super::tests::Fixture;
use super::*;
use crate::gateway::fakes::{FakeAgentGateway, FakeShellGateway};
use foundry_sdk::initiative::{Finding, FindingKind, FindingSource, SteeringFollowUp};
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{WorkItem, WorkItemKind, WorkItemSpec, WorkLane};

/// A finding that reaches only member `one`, recorded in the fixture's store.
fn targeted_finding(fixture: &Fixture) -> Finding {
    let mut store = CampaignStore::load(&fixture.store).unwrap();
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
    store.save(&fixture.store).unwrap();
    finding
}

fn relay_to_two(finding: &Finding) -> String {
    serde_json::json!({
        "action": {"decision": "relay", "finding": finding, "to": ["two"]},
        "audits": []
    })
    .to_string()
}

fn trigger(follow_up_of: Option<&SteeringFollowUp>) -> Event {
    Event::new(
        EventType::InitiativeAdvanceRequested,
        "system".into(),
        Throttle::Full,
        serde_json::json!({"initiative":"shared","follow_up_of":follow_up_of}),
    )
}

/// Steer once with `response`, then apply the action it requested, if any.
async fn steer_and_apply(
    fixture: &Fixture,
    response: &str,
    trigger: &Event,
) -> (TaskBlockResult, Option<TaskBlockResult>) {
    let steered = fixture
        .steer(FakeAgentGateway::success_with(response), FakeShellGateway::success())
        .execute(trigger)
        .await
        .unwrap();
    let action = steered
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeActionRequested);
    let applied = match action {
        Some(action) => Some(
            ApplyInitiativeAction::new(
                fixture.registry.clone(),
                fixture.store.clone(),
                fixture.ledger.clone(),
            )
            .execute(action)
            .await
            .unwrap(),
        ),
        None => None,
    };
    (steered, applied)
}

fn follow_ups(result: &TaskBlockResult) -> Vec<InitiativeAdvanceRequestedPayload> {
    result
        .events
        .iter()
        .filter(|event| event.event_type == EventType::InitiativeAdvanceRequested)
        .filter_map(|event| event.parse_payload::<InitiativeAdvanceRequestedPayload>().ok())
        .filter(|request| request.follow_up_of.is_some())
        .collect()
}

#[tokio::test]
async fn a_relay_that_adds_a_recipient_requests_one_follow_up_while_a_member_is_eligible() {
    let fixture = Fixture::new();
    let finding = targeted_finding(&fixture);
    let trigger = trigger(None);

    let (_, applied) = steer_and_apply(&fixture, &relay_to_two(&finding), &trigger).await;

    let applied = applied.expect("a relay that adds a recipient is an action");
    assert!(applied.success, "{}", applied.summary);
    let requests: Vec<_> = applied
        .events
        .iter()
        .filter(|event| event.event_type == EventType::InitiativeAdvanceRequested)
        .collect();
    assert_eq!(requests.len(), 1, "exactly one follow-up");
    assert_eq!(requests[0].id, format!("evt_{}_follow_up", trigger.id));
    assert_eq!(requests[0].project, "system");
    let request = requests[0].parse_payload::<InitiativeAdvanceRequestedPayload>().unwrap();
    assert_eq!(
        request.follow_up_of,
        Some(SteeringFollowUp {
            steer_id: trigger.id.clone(),
            step: "relay".into()
        })
    );
    assert_eq!(request.trigger_id.as_deref(), Some(trigger.id.as_str()));
    assert_eq!(request.member, None);
}

#[tokio::test]
async fn no_follow_up_is_requested_when_no_member_is_eligible() {
    let fixture = Fixture::new();
    let finding = targeted_finding(&fixture);
    let mut store = CampaignStore::load(&fixture.store).unwrap();
    store.hold_member("shared", "one", "Owner reviews").unwrap();
    store.hold_member("shared", "two", "Owner reviews").unwrap();
    store.save(&fixture.store).unwrap();

    let (_, applied) = steer_and_apply(&fixture, &relay_to_two(&finding), &trigger(None)).await;

    assert!(follow_ups(&applied.unwrap()).is_empty());
}

#[tokio::test]
async fn a_follow_up_pass_records_its_cause_and_requests_no_further_pass() {
    let fixture = Fixture::new();
    let finding = targeted_finding(&fixture);
    let cause = SteeringFollowUp {
        steer_id: "evt_first_steer".into(),
        step: "hold".into(),
    };
    let trigger = trigger(Some(&cause));

    let (steered, applied) = steer_and_apply(&fixture, &relay_to_two(&finding), &trigger).await;

    let fact = steered
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeSteered)
        .unwrap()
        .parse_payload::<InitiativeSteeredPayload>()
        .unwrap();
    assert_eq!(fact.follow_up_of.as_ref(), Some(&cause));
    let store = CampaignStore::load(&fixture.store).unwrap();
    let record = &store.initiative_view("shared").unwrap().steering_history[0];
    assert_eq!(record.follow_up_of.as_ref(), Some(&cause));
    let applied = applied.unwrap();
    assert!(applied.success, "{}", applied.summary);
    assert!(follow_ups(&applied).is_empty(), "a follow-up never requests another");
}

#[tokio::test]
async fn wait_rejected_and_failed_passes_request_no_follow_up() {
    let responses = [
        r#"{"action":{"decision":"wait","reason":"Workers run"},"audits":[]}"#.to_string(),
        r#"{"action":{"decision":"advance","member":"missing"},"audits":[]}"#.to_string(),
        "no decision".to_string(),
    ];
    for response in responses {
        let fixture = Fixture::new();
        let (steered, applied) = steer_and_apply(&fixture, &response, &trigger(None)).await;
        assert!(applied.is_none(), "{response}: no action is requested");
        assert!(follow_ups(&steered).is_empty(), "{response}");
    }
}

/// A relay to the member the finding already reaches: a no-op.
fn relay_to_one(finding: &Finding) -> String {
    serde_json::json!({
        "action": {"decision": "relay", "finding": finding, "to": ["one"]},
        "audits": []
    })
    .to_string()
}

fn outcome(result: &TaskBlockResult) -> SteeringOutcome {
    result
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeSteered)
        .unwrap()
        .parse_payload::<InitiativeSteeredPayload>()
        .unwrap()
        .outcome
}

#[tokio::test]
async fn a_no_op_pass_requests_one_follow_up_while_a_member_is_eligible() {
    let fixture = Fixture::new();
    let finding = targeted_finding(&fixture);
    let trigger = trigger(None);

    let (steered, applied) = steer_and_apply(&fixture, &relay_to_one(&finding), &trigger).await;

    assert_eq!(outcome(&steered), SteeringOutcome::NoOp);
    assert!(applied.is_none(), "a decision already in force needs no action");
    let requests: Vec<_> = steered
        .events
        .iter()
        .filter(|event| event.event_type == EventType::InitiativeAdvanceRequested)
        .collect();
    assert_eq!(requests.len(), 1, "exactly one follow-up");
    assert_eq!(requests[0].id, format!("evt_{}_follow_up", trigger.id));
    assert_eq!(requests[0].project, "system");
    let request = requests[0].parse_payload::<InitiativeAdvanceRequestedPayload>().unwrap();
    assert_eq!(
        request.follow_up_of,
        Some(SteeringFollowUp {
            steer_id: trigger.id.clone(),
            step: "relay".into()
        })
    );
    assert_eq!(request.trigger_id.as_deref(), Some(trigger.id.as_str()));
}

#[tokio::test]
async fn a_no_op_follow_up_requests_no_further_pass() {
    let fixture = Fixture::new();
    let finding = targeted_finding(&fixture);
    let cause = SteeringFollowUp {
        steer_id: "evt_first_steer".into(),
        step: "relay".into(),
    };

    let (steered, _) =
        steer_and_apply(&fixture, &relay_to_one(&finding), &trigger(Some(&cause))).await;

    assert_eq!(outcome(&steered), SteeringOutcome::NoOp);
    assert!(follow_ups(&steered).is_empty(), "a follow-up never requests another");
}

#[tokio::test]
async fn a_no_op_with_no_member_eligible_requests_no_follow_up() {
    let fixture = Fixture::new();
    let finding = targeted_finding(&fixture);
    let mut store = CampaignStore::load(&fixture.store).unwrap();
    store.hold_member("shared", "one", "Owner reviews").unwrap();
    store.hold_member("shared", "two", "Owner reviews").unwrap();
    store.save(&fixture.store).unwrap();

    let (steered, _) = steer_and_apply(&fixture, &relay_to_one(&finding), &trigger(None)).await;

    assert_eq!(outcome(&steered), SteeringOutcome::NoOp);
    assert!(follow_ups(&steered).is_empty());
}

#[tokio::test]
async fn skipped_and_stale_passes_request_no_follow_up() {
    // Skipped: both members run and nothing new arrived since the last decision.
    let fixture = Fixture::new();
    let mut store = CampaignStore::load(&fixture.store).unwrap();
    for member in ["one", "two"] {
        let campaign = store.find_mut(member).unwrap();
        campaign.status = foundry_sdk::campaign::CampaignStatus::Active;
        campaign.cycles_completed = 1;
        campaign.record_dispatched_objective("Deliver".into());
    }
    store.find_initiative_mut("shared").unwrap().steering_history.push(
        foundry_sdk::initiative::SteeringRecord {
            id: "earlier".into(),
            prompt: String::new(),
            decision: foundry_sdk::initiative::SteeringDecision::Wait {
                reason: "Workers run".into(),
            },
            provider: Some(AgentProvider::Claude),
            recorded_at: chrono::Utc::now() - chrono::Duration::hours(1),
            follow_up_of: None,
            integration_skipped: None,
        },
    );
    store.save(&fixture.store).unwrap();
    let automatic = Event::new(
        EventType::InitiativeAdvanceRequested,
        "system".into(),
        Throttle::Full,
        serde_json::json!({"initiative":"shared","trigger_id":"evt_settled"}),
    );
    let (steered, applied) = steer_and_apply(&fixture, "unused", &automatic).await;
    assert!(applied.is_none());
    assert!(follow_ups(&steered).is_empty());

    // Stale: the trunk changes while the agent runs.
    let fixture = Fixture::new();
    let finding = targeted_finding(&fixture);
    let shell = FakeShellGateway::sequence(
        (0..12)
            .map(|index| crate::gateway::CommandResult {
                stdout: if index == 6 {
                    "changed".into()
                } else {
                    String::new()
                },
                stderr: String::new(),
                success: true,
                exit_code: 0,
            })
            .collect(),
    );
    let steered = fixture
        .steer(FakeAgentGateway::success_with(relay_to_two(&finding)), shell)
        .execute(&trigger(None))
        .await
        .unwrap();
    assert_eq!(steered.events[0].payload["outcome"], "stale");
    assert!(follow_ups(&steered).is_empty());
}

#[tokio::test]
async fn an_assessment_already_queued_suppresses_the_follow_up() {
    let fixture = Fixture::new();
    let finding = targeted_finding(&fixture);
    let queued = WorkItem::queued(
        WorkItemSpec {
            project: "system".into(),
            objective: "Assess the initiative".into(),
            kind: WorkItemKind::Workflow,
            lane: WorkLane::Campaign,
            origin: "initiative".into(),
            trace_id: None,
        },
        Event::new(
            EventType::InitiativeAdvanceRequested,
            "system".into(),
            Throttle::Full,
            serde_json::json!({"initiative":"shared","trigger_id":"evt_other_fact"}),
        ),
        chrono::Utc::now(),
    );
    WorkItemStore {
        items: vec![queued],
        ..WorkItemStore::default()
    }
    .save(&fixture.ledger)
    .unwrap();

    let (_, applied) = steer_and_apply(&fixture, &relay_to_two(&finding), &trigger(None)).await;

    let applied = applied.unwrap();
    assert!(applied.success, "{}", applied.summary);
    assert!(follow_ups(&applied).is_empty(), "the queued assessment already follows");
}

#[tokio::test]
async fn an_owner_relay_relies_on_its_own_fact_and_requests_no_follow_up() {
    let fixture = Fixture::new();
    let finding = targeted_finding(&fixture);
    let mut store = CampaignStore::load(&fixture.store).unwrap();
    let action = store
        .commit_owner_action(
            "shared",
            "owner-relay",
            foundry_sdk::initiative::SteeringDecision::Relay {
                finding: Box::new(finding),
                to: vec!["two".into()],
            },
            "Owner relay".into(),
        )
        .unwrap();
    store.save(&fixture.store).unwrap();
    let event = Event::new(
        EventType::InitiativeActionRequested,
        "system".into(),
        Throttle::Full,
        serde_json::json!({"action":action}),
    );

    let applied = ApplyInitiativeAction::new(
        fixture.registry.clone(),
        fixture.store.clone(),
        fixture.ledger.clone(),
    )
    .execute(&event)
    .await
    .unwrap();

    assert!(applied.success, "{}", applied.summary);
    assert!(follow_ups(&applied).is_empty());
}
