use super::*;
use crate::gateway::fakes::{FakeAgentGateway, FakeShellGateway};
use crate::gateway::{AgentRequest, AgentResponse, CommandResult};
use foundry_sdk::initiative::{Finding, FindingKind, FindingSource};
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{WorkDisposition, WorkItem, WorkItemKind, WorkItemSpec, WorkLane};

pub(super) struct Fixture {
    pub(super) temp: tempfile::TempDir,
    pub(super) store: PathBuf,
    pub(super) ledger: PathBuf,
    pub(super) registry: Arc<RwLock<Registry>>,
}
impl Fixture {
    pub(super) fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let store_path = temp.path().join("campaigns.json");
        let ledger = temp.path().join("ledger.json");
        std::fs::write(temp.path().join("contract.rs"), "evidence").unwrap();
        let mut store = CampaignStore::default();
        for name in ["one", "two"] {
            let campaign = serde_json::from_value(serde_json::json!({"name":name,"project":"sample","mission":"Ship behavior","authorized_by":"Stacey","done_evidence":[{"kind":"review","statement":"works"}]})).unwrap();
            store.add(campaign).unwrap();
        }
        let initiative = serde_json::from_value(serde_json::json!({"name":"shared","outcome":"Both work","projects":["sample"],"agent_provider":"claude","authorized_by":"Stacey","members":[{"kind":"campaign","name":"one"},{"kind":"campaign","name":"two"}],"done_evidence":[{"kind":"review","statement":"works"}]})).unwrap();
        store.add_initiative(initiative, &["sample".into()]).unwrap();
        store.enable_steering("shared").unwrap();
        store.save(&store_path).unwrap();
        let registry = serde_json::from_value(serde_json::json!({"version":2,"projects":[{"name":"sample","path":temp.path(),"branch":"main","stack":"rust","agent":"rust-craftsperson","repo":"svetzal/sample"}]})).unwrap();
        Self {
            temp,
            store: store_path,
            ledger,
            registry: Arc::new(RwLock::new(registry)),
        }
    }
    pub(super) fn trigger(member: Option<&str>) -> Event {
        Event::new(
            EventType::InitiativeAdvanceRequested,
            "system".into(),
            Throttle::Full,
            serde_json::json!({"initiative":"shared","member":member}),
        )
    }
    /// A trigger whose running ledger item reserves `reserved`, as admission
    /// records a started steer. One assessment runs at a time, so an earlier
    /// steer of this fixture has finished.
    pub(super) fn trigger_reserving(&self, reserved: &[&str]) -> Event {
        let trigger = Self::trigger(None);
        let mut item = WorkItem::dispatched(
            WorkItemSpec {
                project: "system".into(),
                objective: "steer".into(),
                kind: WorkItemKind::Workflow,
                lane: WorkLane::Campaign,
                origin: EventType::InitiativeAdvanceRequested.to_string(),
                trace_id: None,
            },
            chrono::Utc::now(),
        );
        item.started_root = Some(trigger.clone());
        item.reserved_projects = reserved.iter().map(ToString::to_string).collect();
        let mut ledger = WorkItemStore::load(&self.ledger).unwrap();
        for earlier in &mut ledger.items {
            if earlier.occupies_slot() && earlier.kind == WorkItemKind::Workflow {
                earlier.execution_completed = true;
                earlier.settle_landed("steered", chrono::Utc::now());
            }
        }
        ledger.upsert(item);
        ledger.save(&self.ledger).unwrap();
        trigger
    }
    /// A trigger admitted while the ledger was quiet: it holds every trunk.
    pub(super) fn trigger_holding_trunks(&self) -> Event {
        self.trigger_reserving(&["sample"])
    }
    pub(super) fn steer(
        &self,
        agent: Arc<dyn AgentGateway>,
        shell: Arc<dyn ShellGateway>,
    ) -> SteerInitiative {
        SteerInitiative::new(
            agent,
            shell,
            self.registry.clone(),
            self.store.clone(),
            self.ledger.clone(),
        )
    }
}
pub(super) fn ask() -> &'static str {
    r#"{"action":{"decision":"ask","question":"Choose delivery priority","members":["one","two"]},"audits":[]}"#
}

#[tokio::test]
async fn steering_is_read_only_and_records_exact_prompt_once_without_idle_loop() {
    let fixture = Fixture::new();
    let agent = FakeAgentGateway::success_with(ask());
    let block = fixture.steer(agent.clone(), FakeShellGateway::success());
    let trigger = Fixture::trigger(None);
    let result = block.execute(&trigger).await.unwrap();
    assert!(result.success);
    let invocation = &agent.invocations()[0];
    assert_eq!(invocation.access, AgentAccess::ReadOnly);
    assert_eq!(invocation.tier, ModelTier::Balanced);
    assert_eq!(invocation.effort, ReasoningEffort::Medium);
    assert_eq!(invocation.provider, Some(AgentProvider::Claude));
    assert!(!invocation.prompt.contains("campaigns.json"));
    let store = CampaignStore::load(&fixture.store).unwrap();
    let initiative = store.initiative_view("shared").unwrap();
    assert_eq!(initiative.steering_history[0].prompt, invocation.prompt);
    assert!(initiative.steering.is_none());
    let action = result
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeActionRequested)
        .unwrap();
    let apply = ApplyInitiativeAction::new(
        fixture.registry.clone(),
        fixture.store.clone(),
        fixture.ledger.clone(),
    );
    let unexpected = apply.execute(action).await.unwrap().events;
    assert!(
        unexpected.is_empty(),
        "expected `apply.execute(action).await.unwrap().events` to be empty: {unexpected:?}"
    );
    let unexpected = apply.execute(action).await.unwrap().events;
    assert!(
        unexpected.is_empty(),
        "expected `apply.execute(action).await.unwrap().events` to be empty: {unexpected:?}"
    );
    assert!(!block.execute(&trigger).await.unwrap().success);
    assert_eq!(agent.invocations().len(), 1);
    assert!(
        CampaignStore::load(&fixture.store)
            .unwrap()
            .initiative_view("shared")
            .unwrap()
            .pending_actions
            .is_empty(),
        "no steering action is left pending"
    );
}

#[tokio::test]
async fn an_agent_cannot_replace_the_owners_requested_member() {
    let fixture = Fixture::new();
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"advance","member":"two"},"audits":[]}"#,
    );
    let result = fixture
        .steer(agent, FakeShellGateway::success())
        .execute(&Fixture::trigger(Some("one")))
        .await
        .unwrap();
    // A refused choice is a guard rejection, not a failure that needs a person.
    assert!(result.success);
    let fact = result.events[0].parse_payload::<InitiativeSteeredPayload>().unwrap();
    assert_eq!(fact.outcome, SteeringOutcome::Rejected);
    assert_eq!(
        fact.error.as_deref(),
        Some("owner requested member 'one'; steering cannot choose another work member")
    );
    assert!(
        result
            .events
            .iter()
            .all(|event| event.event_type != EventType::InitiativeActionRequested)
    );
    let initiative =
        CampaignStore::load(&fixture.store).unwrap().initiative_view("shared").unwrap();
    assert!(
        initiative.pending_actions.is_empty(),
        "expected `initiative.pending_actions` to be empty: {:?}",
        initiative.pending_actions
    );
    assert!(initiative.steering.is_none());
}

struct EditOwner {
    store: PathBuf,
}
impl AgentGateway for EditOwner {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<AgentResponse>> + Send + 'a>,
    > {
        Box::pin(async move {
            assert_eq!(request.timeout, Duration::ZERO);
            let mut guard = lock_store(&self.store).await?;
            guard.store.decide_initiative(
                "shared",
                "Owner changed priorities",
                &[],
                chrono::Utc::now(),
            )?;
            guard.save()?;
            Ok(AgentResponse::success(ask()))
        })
    }
}
#[tokio::test]
async fn owner_change_during_agent_discards_output_and_requests_one_fresh_assessment() {
    let fixture = Fixture::new();
    let block = fixture.steer(
        Arc::new(EditOwner {
            store: fixture.store.clone(),
        }),
        FakeShellGateway::success(),
    );
    let result = block.execute(&Fixture::trigger(None)).await.unwrap();
    assert!(result.success);
    let fact = result
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeSteered)
        .unwrap()
        .parse_payload::<InitiativeSteeredPayload>()
        .unwrap();
    assert_eq!(fact.outcome, SteeringOutcome::Stale);
    assert_eq!(
        result
            .events
            .iter()
            .filter(|event| event.event_type == EventType::InitiativeAdvanceRequested)
            .count(),
        1
    );
    assert!(
        CampaignStore::load(&fixture.store)
            .unwrap()
            .initiative_view("shared")
            .unwrap()
            .steering_history
            .is_empty(),
        "no steering decision is recorded"
    );
}

#[tokio::test]
async fn changed_trunk_discards_the_single_provider_response() {
    let fixture = Fixture::new();
    let shell = FakeShellGateway::sequence(
        (0..12)
            .map(|index| CommandResult {
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
    let agent = FakeAgentGateway::success_with(ask());
    let result = fixture
        .steer(agent.clone(), shell)
        .execute(&Fixture::trigger(None))
        .await
        .unwrap();
    assert_eq!(agent.invocations().len(), 1);
    assert_eq!(result.events[0].payload["outcome"], "stale");
    assert!(
        result
            .events
            .iter()
            .all(|event| event.event_type != EventType::InitiativeActionRequested)
    );
}

#[test]
fn task_landing_consumes_only_the_shown_findings_and_records_verified_milestones() {
    let fixture = Fixture::new();
    let mut store = CampaignStore::load(&fixture.store).unwrap();
    let initiative = store.find_initiative_mut("shared").unwrap();
    initiative
        .members
        .push(serde_json::from_value(serde_json::json!({"kind":"task","name":"task"})).unwrap());
    let finding = store
        .add_finding(Finding::new(
            "shared",
            FindingSource {
                kind: "owner".into(),
                id: "decision".into(),
            },
            vec!["task".into()],
            FindingKind::Evidence,
            "Shown contract",
            vec![],
        ))
        .unwrap();
    let unseen = store
        .add_finding(Finding::new(
            "shared",
            FindingSource {
                kind: "owner".into(),
                id: "decision2".into(),
            },
            vec!["task".into()],
            FindingKind::Evidence,
            "Unseen contract",
            vec![],
        ))
        .unwrap();
    let root = Event::new(
        EventType::ExecutionRequested,
        "sample".into(),
        Throttle::Full,
        serde_json::json!({"initiative":"shared","finding_ids":[finding.id]}),
    );
    let mut item = WorkItem::queued(
        WorkItemSpec {
            project: "sample".into(),
            objective: "Work".into(),
            kind: WorkItemKind::Task,
            lane: WorkLane::Campaign,
            origin: "initiative".into(),
            trace_id: None,
        },
        root,
        chrono::Utc::now(),
    );
    item.started_root = item.pending_root.clone();
    item.start(chrono::Utc::now());
    item.execution_completed = true;
    item.disposition = Some(WorkDisposition {
        landed_commit: Some("abc123".into()),
        verdict: Some("complete".into()),
        task_branch: None,
        branch_cleanup: vec![],
        preservation_ref: None,
        worktree: None,
        worktree_removed: None,
    });
    store.find_initiative_mut("shared").unwrap().task_dispatches.push(
        foundry_sdk::initiative::TaskDispatch {
            member: "task".into(),
            work_item: item.id.clone(),
            project: "sample".into(),
        },
    );
    let ledger = WorkItemStore {
        items: vec![item],
        ..Default::default()
    };
    admission::record_task_evidence(&mut store, "shared", &ledger).unwrap();
    admission::record_task_evidence(&mut store, "shared", &ledger).unwrap();
    assert_eq!(store.find_finding(&finding.id).unwrap().consumed_by.len(), 1);
    assert!(
        store.find_finding(&unseen.id).unwrap().consumed_by.is_empty(),
        "expected `store.find_finding(&unseen.id).unwrap().consumed_by` to be empty: {:?}",
        store.find_finding(&unseen.id).unwrap().consumed_by
    );
    assert_eq!(store.initiative_view("shared").unwrap().milestones.len(), 2);
    assert!(fixture.temp.path().exists());
}

fn git_repository(fixture: &Fixture) {
    let repo = fixture.temp.path().join("registered");
    std::fs::create_dir(&repo).unwrap();
    std::fs::write(repo.join("contract.rs"), "contract v1").unwrap();
    for args in [
        vec!["init", "-b", "main"],
        vec!["add", "contract.rs"],
        vec![
            "-c",
            "user.name=Foundry Test",
            "-c",
            "user.email=foundry-test@example.invalid",
            "commit",
            "-m",
            "fixture",
        ],
    ] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    fixture.registry.write().unwrap().projects[0].path = repo.to_string_lossy().into_owned();
}

#[tokio::test]
async fn committed_action_rechecks_external_evidence_and_releases_stale_reservation() {
    let fixture = Fixture::new();
    git_repository(&fixture);
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"advance","member":"one"},"audits":[]}"#,
    );
    let result = fixture
        .steer(agent, Arc::new(crate::gateway::ProcessShellGateway))
        .execute(&Fixture::trigger(None))
        .await
        .unwrap();
    assert!(result.success);
    let action = result
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeActionRequested)
        .unwrap();
    let store = CampaignStore::load(&fixture.store).unwrap();
    assert!(
        store.initiative_view("shared").unwrap().pending_actions[0]
            .external_context
            .is_some()
    );
    std::fs::write(fixture.temp.path().join("registered/contract.rs"), "changed after assessment")
        .unwrap();
    let apply = ApplyInitiativeAction::new(
        fixture.registry.clone(),
        fixture.store.clone(),
        fixture.ledger.clone(),
    );
    let rejected = apply.execute(action).await.unwrap();
    assert_eq!(rejected.events[0].payload["outcome"], "stale");
    assert!(!rejected.events.iter().any(|event| matches!(
        event.event_type,
        EventType::CampaignAdvanceRequested | EventType::ExecutionRequested
    )));
    let initiative =
        CampaignStore::load(&fixture.store).unwrap().initiative_view("shared").unwrap();
    assert!(
        initiative.pending_actions.is_empty(),
        "expected `initiative.pending_actions` to be empty: {:?}",
        initiative.pending_actions
    );
    assert!(
        initiative.reservations.is_empty(),
        "expected `initiative.reservations` to be empty: {:?}",
        initiative.reservations
    );
}

#[tokio::test]
async fn cleanup_owner_that_arrives_after_steer_refuses_child_and_releases_reservation() {
    let fixture = Fixture::new();
    git_repository(&fixture);
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"advance","member":"one"},"audits":[]}"#,
    );
    let result = fixture
        .steer(agent, Arc::new(crate::gateway::ProcessShellGateway))
        .execute(&Fixture::trigger(None))
        .await
        .unwrap();
    assert!(result.success);
    let action = result
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeActionRequested)
        .unwrap();
    let mut owner = WorkItem::dispatched(
        WorkItemSpec {
            project: "sample".into(),
            objective: "cleanup".into(),
            kind: WorkItemKind::Task,
            lane: WorkLane::Interactive,
            origin: "owner".into(),
            trace_id: None,
        },
        chrono::Utc::now(),
    );
    owner.started_root = Some(Event::new(
        EventType::ExecutionRequested,
        "sample".into(),
        Throttle::Full,
        serde_json::json!({"project":"sample"}),
    ));
    owner.settle_landed("done but cleanup pending", chrono::Utc::now());
    WorkItemStore {
        items: vec![owner],
        ..Default::default()
    }
    .save(&fixture.ledger)
    .unwrap();
    let apply = ApplyInitiativeAction::new(
        fixture.registry.clone(),
        fixture.store.clone(),
        fixture.ledger.clone(),
    );
    let rejected = apply.execute(action).await.unwrap();
    assert!(rejected.success, "a busy repository is a guard rejection");
    assert_eq!(rejected.events[0].payload["outcome"], "rejected");
    assert_eq!(
        rejected.events[0].payload["error"],
        "action repository still has an execution or cleanup owner"
    );
    assert!(
        !rejected
            .events
            .iter()
            .any(|event| event.event_type == EventType::InitiativeAdvanceRequested)
    );
    let mut store = CampaignStore::load(&fixture.store).unwrap();
    assert!(
        store.initiative_view("shared").unwrap().pending_actions.is_empty(),
        "expected `store.initiative_view(\"shared\").unwrap().pending_actions` to be empty: {:?}",
        store.initiative_view("shared").unwrap().pending_actions
    );
    assert!(
        store.initiative_view("shared").unwrap().reservations.is_empty(),
        "expected `store.initiative_view(\"shared\").unwrap().reservations` to be empty: {:?}",
        store.initiative_view("shared").unwrap().reservations
    );
    assert!(store.begin_steering("shared", "next-event").is_ok());
}

#[tokio::test]
async fn accumulated_broadcast_findings_do_not_strand_a_new_task_action() {
    let fixture = Fixture::new();
    git_repository(&fixture);
    let mut store = CampaignStore::load(&fixture.store).unwrap();
    for index in 0..12 {
        store
            .add_finding(Finding::new(
                "shared",
                FindingSource {
                    kind: "owner".into(),
                    id: format!("decision-{index}"),
                },
                vec![],
                FindingKind::Evidence,
                &format!("{index}:{}", "evidence ".repeat(240)),
                vec![],
            ))
            .unwrap();
    }
    let action = store
        .commit_owner_action(
            "shared",
            "task-dispatch",
            foundry_sdk::initiative::SteeringDecision::Dispatch {
                project: "sample".into(),
                objective: "Verify concrete behavior".into(),
                member: None,
            },
            "Owner requested task".into(),
        )
        .unwrap();
    store.save(&fixture.store).unwrap();
    let event = Event::new(
        EventType::InitiativeActionRequested,
        "system".into(),
        Throttle::Full,
        serde_json::json!({"action":action}),
    );
    let apply = ApplyInitiativeAction::new(
        fixture.registry.clone(),
        fixture.store.clone(),
        fixture.ledger.clone(),
    );
    let result = apply.execute(&event).await.unwrap();
    assert!(result.success, "{}", result.summary);
    let child = &result.events[0];
    assert_eq!(child.event_type, EventType::ExecutionRequested);
    assert_eq!(
        foundry_sdk::workflow::WorkflowType::from_payload(&child.payload),
        foundry_sdk::workflow::WorkflowType::Task
    );
    let charter = super::super::CheckCharter::new(fixture.registry.clone());
    assert!(!charter.accepts(child), "task must wait for durable admission");
    let mut started = child.clone();
    started.payload[super::super::work_ledger::ADMITTED_WORK_ITEM_KEY] =
        serde_json::json!("wi_started_task");
    assert!(charter.accepts(&started), "admitted task must enter execution chain");
    let checked = charter.execute(&started).await.unwrap();
    assert_eq!(checked.events[0].event_type, EventType::CharterCheckCompleted);
    assert_eq!(
        foundry_sdk::workflow::WorkflowType::from_payload(&checked.events[0].payload),
        foundry_sdk::workflow::WorkflowType::Task
    );

    assert_eq!(child.source.as_ref().unwrap().reference, "shared");
    let findings = child.payload["initiative_findings"].as_array().unwrap();
    assert!(!findings.is_empty(), "expected `findings` to be non-empty");
    assert!(findings.len() < 12);
    assert!(serde_json::to_vec(findings).unwrap().len() <= 8192);
    let ids = child.payload["finding_ids"].as_array().unwrap();
    assert_eq!(ids, &findings.iter().map(|finding| finding["id"].clone()).collect::<Vec<_>>());
    assert!(child.payload.get("campaign").is_none());
    assert!(child.payload.get("campaign_limits").is_some());
    assert_eq!(
        child.payload["steering_external_context"]["repositories"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let replay = apply.execute(&event).await.unwrap();
    assert_eq!(replay.events[0].id, child.id);
    assert_eq!(
        CampaignStore::load(&fixture.store)
            .unwrap()
            .initiative_view("shared")
            .unwrap()
            .pending_actions
            .len(),
        1
    );
}

#[tokio::test]
async fn campaign_advance_action_retains_its_strategic_route() {
    let fixture = Fixture::new();
    git_repository(&fixture);
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"advance","member":"one"},"audits":[]}"#,
    );
    let result = fixture
        .steer(agent, Arc::new(crate::gateway::ProcessShellGateway))
        .execute(&Fixture::trigger(None))
        .await
        .unwrap();
    let action = result
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeActionRequested)
        .unwrap();
    let apply = ApplyInitiativeAction::new(
        fixture.registry.clone(),
        fixture.store.clone(),
        fixture.ledger.clone(),
    );
    let applied = apply.execute(action).await.unwrap();
    assert!(applied.success, "{}", applied.summary);
    let child = &applied.events[0];
    assert_eq!(child.event_type, EventType::CampaignAdvanceRequested);
    assert_eq!(child.payload["campaign"], "one");
    assert!(child.payload.get("workflow").is_none());
    assert!(child.payload.get("workflow_type").is_none());
}

#[tokio::test]
async fn full_question_budget_advances_unblocked_work_or_waits_once() {
    for block_all in [false, true] {
        let fixture = Fixture::new();
        let mut store = CampaignStore::load(&fixture.store).unwrap();
        store.find_initiative_mut("shared").unwrap().budget.max_open_questions = 1;
        let members = if block_all {
            vec![]
        } else {
            vec!["one".into()]
        };
        store.record_question("shared", "First policy?", &members).unwrap();
        store.save(&fixture.store).unwrap();
        let agent = FakeAgentGateway::success_with(
            r#"{"action":{"decision":"ask","question":"Fourth policy?","members":["two"]},"audits":[]}"#,
        );
        let result = fixture
            .steer(agent, FakeShellGateway::success())
            .execute(&Fixture::trigger(None))
            .await
            .unwrap();
        assert!(result.success, "{}", result.summary);
        let action = result
            .events
            .iter()
            .find(|event| event.event_type == EventType::InitiativeActionRequested)
            .unwrap()
            .parse_payload::<InitiativeActionRequestedPayload>()
            .unwrap()
            .action;
        if block_all {
            assert!(matches!(
                action.decision,
                foundry_sdk::initiative::SteeringDecision::Escalate { .. }
            ));
            let apply = ApplyInitiativeAction::new(
                fixture.registry.clone(),
                fixture.store.clone(),
                fixture.ledger.clone(),
            );
            let event = result
                .events
                .iter()
                .find(|event| event.event_type == EventType::InitiativeActionRequested)
                .unwrap();
            assert!(
                apply.execute(event).await.unwrap().events.is_empty(),
                "waiting on owner must not trigger an idle loop"
            );
        } else {
            assert!(
                matches!(action.decision, foundry_sdk::initiative::SteeringDecision::Advance { ref member, .. } if member == "two")
            );
        }
        let view = CampaignStore::load(&fixture.store).unwrap().initiative_view("shared").unwrap();
        assert_eq!(view.open_questions.len(), 1);
        assert_eq!(view.open_questions[0].question, "First policy?");
    }
}

#[tokio::test]
async fn full_question_budget_can_advance_an_audited_candidate() {
    let fixture = Fixture::new();
    let mut store = CampaignStore::load(&fixture.store).unwrap();
    let initiative = store.find_initiative_mut("shared").unwrap();
    initiative.budget.max_open_questions = 1;
    initiative.intent_refs.push("et:interaction:next".into());
    initiative.members.push(
        serde_json::from_value(serde_json::json!({
            "kind":"candidate","name":"candidate","description":"Finish approved API behavior",
            "intent_ref":"et:interaction:next","intent_state":"create"
        }))
        .unwrap(),
    );
    store
        .record_question("shared", "Existing policy?", &["one".into(), "two".into()])
        .unwrap();
    store.save(&fixture.store).unwrap();
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"ask","question":"Another policy?","members":["one"]},"audits":[{"intent_ref":"et:interaction:next","classification":"partial","project":"sample","paths":["contract.rs"],"explanation":"Approved behavior is incomplete"}]}"#,
    );
    let result = fixture
        .steer(agent, FakeShellGateway::success())
        .execute(&Fixture::trigger(None))
        .await
        .unwrap();
    assert!(result.success, "{}", result.summary);
    let action = result
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeActionRequested)
        .unwrap()
        .parse_payload::<InitiativeActionRequestedPayload>()
        .unwrap()
        .action;
    assert!(
        matches!(action.decision, foundry_sdk::initiative::SteeringDecision::Advance { ref member, ref project } if member == "candidate" && project.as_deref() == Some("sample"))
    );
    let store = CampaignStore::load(&fixture.store).unwrap();
    assert_eq!(store.find("candidate").unwrap().project, "sample");
    assert_eq!(store.initiative_view("shared").unwrap().open_questions.len(), 1);
}

#[tokio::test]
async fn new_escalation_at_question_capacity_still_advances_eligible_work() {
    let fixture = Fixture::new();
    let mut store = CampaignStore::load(&fixture.store).unwrap();
    store.find_initiative_mut("shared").unwrap().budget.max_open_questions = 1;
    store.record_question("shared", "Existing policy?", &["one".into()]).unwrap();
    store.save(&fixture.store).unwrap();
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"escalate","reason":"Another policy needed"},"audits":[]}"#,
    );
    let result = fixture
        .steer(agent, FakeShellGateway::success())
        .execute(&Fixture::trigger(None))
        .await
        .unwrap();
    assert!(result.success, "{}", result.summary);
    let action = result
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeActionRequested)
        .unwrap()
        .parse_payload::<InitiativeActionRequestedPayload>()
        .unwrap()
        .action;
    assert!(
        matches!(action.decision, foundry_sdk::initiative::SteeringDecision::Advance { ref member, .. } if member == "two")
    );
    assert_eq!(
        CampaignStore::load(&fixture.store)
            .unwrap()
            .initiative_view("shared")
            .unwrap()
            .open_questions
            .len(),
        1
    );
}
