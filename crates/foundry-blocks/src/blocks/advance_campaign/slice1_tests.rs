#[tokio::test]
async fn enabled_initiative_accounts_settlement_without_independent_formation() {
    let dir = tempfile::tempdir().unwrap();
    let (path, registry) = initiative_store(dir.path(), 5, 2);
    let mut store = CampaignStore::load(&path).unwrap();
    store.enable_steering("both").unwrap();
    store.find_mut("c").unwrap().cycles_completed = 1;
    store.save(&path).unwrap();
    let agent = FakeAgentGateway::success_with(
        r#"{"decision":"advance","objective":"Next gap","reason":"remaining"}"#,
    );
    let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, path.clone());
    let mut run = run_result_with(TaskVerdict::Remainder {
        gaps: vec!["Document the API".into()],
    });
    run.landed = true;
    run.context.initiative = Some("both".into());
    run.context.campaign_cycle = Some(1);
    let trigger = automatic_advance_trigger(run);
    for _ in 0..2 {
        let result = block.execute(&trigger).await.unwrap();
        assert!(result.success, "{}", result.summary);
        assert!(
            result
                .events
                .iter()
                .all(|event| event.event_type != EventType::ExecutionRequested)
        );
    }
    assert!(agent.invocations().is_empty());
    let store = CampaignStore::load(&path).unwrap();
    let campaign = store.find("c").unwrap();
    assert_eq!(campaign.cycles_completed, 1);
    assert_eq!(campaign.cycles_landed, 1);
    assert!(campaign.pending_run_result.is_some());
    assert_eq!(store.list_findings("both").unwrap().len(), 1);
    let milestone = store
        .find_initiative("both")
        .unwrap()
        .milestones
        .iter()
        .find(|proof| proof.member == "c" && proof.name == "landed")
        .unwrap();
    assert!(milestone.evidence.contains(&"cycle:1".to_string()));
    assert!(milestone.evidence.iter().any(|proof| proof.starts_with("event:")));
    let manual = block.execute(&manual_advance_trigger()).await.unwrap();
    assert!(
        manual
            .events
            .iter()
            .any(|event| event.event_type == EventType::ExecutionRequested)
    );
    assert_eq!(agent.invocations().len(), 1);
}

#[tokio::test]
async fn campaign_completion_records_a_named_milestone_with_terminal_event_proof() {
    let dir = tempfile::tempdir().unwrap();
    let (path, registry) = initiative_store(dir.path(), 5, 2);
    let block = advance_block(
        FakeAgentGateway::success_with(r#"{"decision":"done","reason":"Mission verified"}"#),
        FakeShellGateway::success(),
        registry,
        path.clone(),
    );
    let result = block.execute(&manual_advance_trigger()).await.unwrap();
    assert!(result.success, "{}", result.summary);
    let completed = result
        .events
        .iter()
        .find(|event| event.event_type == EventType::CampaignCompleted)
        .unwrap();
    let store = CampaignStore::load(&path).unwrap();
    let proof = store
        .find_initiative("both")
        .unwrap()
        .milestones
        .iter()
        .find(|proof| proof.member == "c" && proof.name == "completed")
        .unwrap();
    assert_eq!(proof.evidence, vec![format!("event:{}", completed.id)]);
}

#[tokio::test]
async fn admitted_pairing_reaches_formation_and_task_below_explicit_member_stages() {
    let dir = tempfile::tempdir().unwrap();
    let (path, registry) = initiative_store(dir.path(), 5, 2);
    let mut store = CampaignStore::load(&path).unwrap();
    store.find_mut("c").unwrap().providers.review = Some(AgentProvider::Claude);
    let original = store.find("c").unwrap().providers;
    store.save(&path).unwrap();
    let agent = FakeAgentGateway::success_with(
        r#"{"decision":"advance","objective":"Next gap","reason":"remaining"}"#,
    );
    let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, path.clone());
    let mut trigger = manual_advance_trigger();
    trigger.payload["providers"] =
        serde_json::json!({"formation":"codex","execution":"claude","review":"codex"});
    let result = block.execute(&trigger).await.unwrap();
    assert!(result.success, "{}", result.summary);
    assert_eq!(agent.invocations()[0].provider, Some(AgentProvider::Codex));
    let task = result
        .events
        .iter()
        .find(|event| event.event_type == EventType::ExecutionRequested)
        .unwrap();
    assert_eq!(
        task.payload["providers"],
        serde_json::json!({"formation":"codex","execution":"claude","review":"claude"})
    );
    assert_eq!(CampaignStore::load(&path).unwrap().find("c").unwrap().providers, original);
}

#[test]
fn preserved_continuation_keeps_its_admitted_pairing() {
    let campaign = campaign_for_accumulation_test();
    let original = foundry_sdk::gateway::StageProviders {
        formation: Some(AgentProvider::Claude),
        execution: Some(AgentProvider::Codex),
        review: Some(AgentProvider::Claude),
    };
    let next = foundry_sdk::gateway::StageProviders {
        formation: Some(AgentProvider::Codex),
        execution: Some(AgentProvider::Claude),
        review: Some(AgentProvider::Codex),
    };
    let mut run = run_result_with(TaskVerdict::Defect {
        diagnosis: "Fix the regression".into(),
    });
    run.context.providers = original;
    let mut request = CampaignAdvanceRequestedPayload {
        campaign: "c".into(),
        run_result: Some(run),
        run_event_id: None,
        operator_origin: None,
        providers: next,
    };
    assert_eq!(super::cycle_providers(&campaign, &request, None), original);
    request.run_result.as_mut().unwrap().preservation_ref = None;
    assert_eq!(super::cycle_providers(&campaign, &request, None), next);
}

#[tokio::test]
async fn settled_standalone_task_keeps_member_capacity_until_cleanup_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let (path, registry) = initiative_store(dir.path(), 5, 1);
    let mut store = CampaignStore::load(&path).unwrap();
    let initiative = store.find_initiative_mut("both").unwrap();
    initiative.members.push(
        serde_json::from_value(serde_json::json!({"kind":"task","name":"contract-check"})).unwrap(),
    );
    initiative.task_dispatches.push(foundry_sdk::initiative::TaskDispatch {
        member: "contract-check".into(),
        work_item: "wi_contract".into(),
        project: "p".into(),
    });
    store.save(&path).unwrap();
    let mut ledger = WorkItemStore::default();
    let mut item = foundry_sdk::work_item::WorkItem::dispatched(
        foundry_sdk::work_item::WorkItemSpec {
            project: "p".into(),
            objective: "Verify contract".into(),
            kind: foundry_sdk::work_item::WorkItemKind::Task,
            lane: foundry_sdk::work_item::WorkLane::Campaign,
            origin: "initiative".into(),
            trace_id: None,
        },
        chrono::Utc::now(),
    );
    item.id = "wi_contract".into();
    item.started_root = Some(Event::new(
        EventType::ExecutionRequested,
        "p".into(),
        Throttle::Full,
        serde_json::json!({"workflow":"task"}),
    ));
    item.settle_landed("task landed; cleanup pending", chrono::Utc::now());
    ledger.upsert(item);
    ledger.save(&ledger_path_beside(&path)).unwrap();
    let agent = FakeAgentGateway::success_with(
        r#"{"decision":"advance","objective":"Next gap","reason":"remaining"}"#,
    );
    let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, path.clone());
    let blocked = block.execute(&manual_advance_trigger()).await.unwrap();
    assert!(blocked.success);
    assert!(agent.invocations().is_empty());
    ledger.find_mut("wi_contract").unwrap().execution_completed = true;
    ledger.save(&ledger_path_beside(&path)).unwrap();
    let next = block.execute(&manual_advance_trigger()).await.unwrap();
    assert!(
        next.events
            .iter()
            .any(|event| event.event_type == EventType::ExecutionRequested)
    );
    assert_eq!(agent.invocations().len(), 1);
}
