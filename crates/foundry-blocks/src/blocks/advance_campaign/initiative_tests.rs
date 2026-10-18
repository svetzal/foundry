fn initiative_store(dir: &std::path::Path, max_cycles: u64, max_active: usize) -> (std::path::PathBuf, std::sync::Arc<std::sync::RwLock<foundry_sdk::registry::Registry>>) {
    let (path, registry) = staged_active_campaign(dir);
    let mut store = CampaignStore::load(&path).unwrap();
    store.find_mut("c").unwrap().cycles_completed = 0;
    let mut second = store.find("c").unwrap().clone();
    second.name = "second".into();
    store.add(second).unwrap();
    let initiative = serde_json::from_value(serde_json::json!({
        "name":"both", "outcome":"Both behaviors work", "projects":["p"],
        "members":[{"kind":"campaign","name":"c"},{"kind":"campaign","name":"second"}],
        "done_evidence":[{"kind":"review","statement":"Both work together"}],
        "authorized_by":"owner", "budget":{"max_cycles":max_cycles,"max_active_members":max_active,"max_open_questions":3},
        "providers":{"formation":"claude","execution":"codex","review":"claude"}
    })).unwrap();
    store.add_initiative(initiative, &["p".into()]).unwrap();
    store.save(&path).unwrap();
    (path, registry)
}

#[tokio::test]
async fn initiative_findings_and_brief_reach_formation_and_execution() {
    let dir = tempfile::tempdir().unwrap();
    let (path, registry) = initiative_store(dir.path(), 5, 2);
    let mut store = CampaignStore::load(&path).unwrap();
    let brief = store.set_brief("both", dir.path(), "Binding acceptance policy").unwrap();
    let finding = store.add_finding(foundry_sdk::initiative::Finding::new("both", foundry_sdk::initiative::FindingSource {kind:"owner".into(),id:"owner".into()}, vec!["c".into()], foundry_sdk::initiative::FindingKind::ContractChanged, "The sibling now uses a different port", vec!["contract.rs".into()])).unwrap();
    store.save(&path).unwrap();
    let agent = FakeAgentGateway::success_with(r#"{"decision":"advance","objective":"Fix the port","reason":"contract changed"}"#);
    let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, path.clone());
    let result = block.execute(&manual_advance_trigger()).await.unwrap();
    assert!(result.success, "{}", result.summary);
    let calls = agent.invocations();
    assert_eq!(calls[0].provider, Some(crate::gateway::AgentProvider::Claude));
    assert!(calls[0].prompt.contains("Findings from the initiative"));
    assert!(calls[0].prompt.contains(&finding.id));
    assert!(calls[0].prompt.contains(&brief.path));
    assert!(calls[0].prompt.contains(&brief.hash));
    let execution = result.events.iter().find(|e|e.event_type==EventType::ExecutionRequested).unwrap();
    assert_eq!(execution.payload["finding_ids"], serde_json::json!([finding.id]));
    assert_eq!(execution.payload["initiative_brief"]["hash"], brief.hash);
    assert_eq!(execution.payload["providers"]["review"], "claude");
    assert!(execution.source.as_ref().unwrap().matches(foundry_sdk::work_source::WorkSourceKind::Initiative, "both"));
    let store = CampaignStore::load(&path).unwrap();
    let consumed_by = &store.find_finding(&finding.id).unwrap().consumed_by;
    assert!(consumed_by.is_empty(), "expected the finding to stay unconsumed: {consumed_by:?}");
}

#[tokio::test]
async fn initiative_formation_receives_contract_guidance_without_a_brief_or_findings() {
    let dir = tempfile::tempdir().unwrap();
    let (path, registry) = initiative_store(dir.path(), 5, 2);
    let agent = FakeAgentGateway::success_with(r#"{"decision":"advance","objective":"Verify shared contract","reason":"missing verification"}"#);
    let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, path);
    let result = block.execute(&manual_advance_trigger()).await.unwrap();
    assert!(result.success, "{}", result.summary);
    assert!(agent.invocations()[0].prompt.contains(crate::blocks::agent_helpers::INITIATIVE_CONTRACT_GUIDANCE));
    let execution = result.events.iter().find(|event| event.event_type == EventType::ExecutionRequested).unwrap();
    assert_eq!(execution.payload["initiative"], "both");
    assert!(execution.payload["initiative_brief"].is_null());
    assert_eq!(execution.payload["initiative_findings"], serde_json::json!([]));
}

#[tokio::test]
async fn initiative_consumes_only_seen_findings_when_the_cycle_lands() {
    let dir = tempfile::tempdir().unwrap();
    let (path, registry) = initiative_store(dir.path(), 5, 2);
    let mut store = CampaignStore::load(&path).unwrap();
    let finding = store.add_finding(foundry_sdk::initiative::Finding::new("both", foundry_sdk::initiative::FindingSource {kind:"owner".into(),id:"owner".into()}, vec![], foundry_sdk::initiative::FindingKind::Evidence, "Already delivered contract", vec![])).unwrap();
    store.save(&path).unwrap();
    let agent = FakeAgentGateway::success_with(r#"{"decision":"done","reason":"delivered"}"#);
    let block = advance_block(agent, FakeShellGateway::success(), registry, path.clone());
    let mut result = run_result_with(TaskVerdict::Remainder {gaps:vec!["Document the port".into()]});
    result.context.finding_ids = vec![finding.id.clone()];
    result.context.initiative = Some("both".into());
    result.context.campaign_cycle = Some(1);
    block.execute(&automatic_advance_trigger(result.clone())).await.unwrap();
    let store = CampaignStore::load(&path).unwrap();
    let consumed_by = &store.find_finding(&finding.id).unwrap().consumed_by;
    assert!(consumed_by.is_empty(), "expected the finding to stay unconsumed: {consumed_by:?}");
    let mut guard = CampaignStore::lock_exclusive(&path).unwrap();
    let campaign = guard.store.find_mut("c").unwrap();
    campaign.status = CampaignStatus::Active;
    campaign.last_run_event_id = None;
    campaign.pending_run_result = None;
    guard.save().unwrap();
    drop(guard);
    result.landed = true;
    let outcome = block.execute(&automatic_advance_trigger(result)).await.unwrap();
    let store = CampaignStore::load(&path).unwrap();
    assert_eq!(store.find_finding(&finding.id).unwrap().consumed_by.len(), 1);
    assert_eq!(store.formation_context("second", 4096).unwrap().unwrap().findings.len(), 2);
    assert!(outcome.events.iter().any(|e|e.event_type==EventType::FindingConsumed));
    assert!(outcome.events.iter().any(|e|e.event_type==EventType::FindingRecorded));
}

#[tokio::test]
async fn simultaneous_member_advances_never_exceed_the_initiative_budget_or_active_cap() {
    for (budget, active_cap) in [(1,2),(5,1)] {
        let dir = tempfile::tempdir().unwrap();
        let (path, registry) = initiative_store(dir.path(), budget, active_cap);
        let agent = FakeAgentGateway::success_with(r#"{"decision":"advance","objective":"Build one behavior","reason":"gap"}"#);
        let block = advance_block(agent, FakeShellGateway::success(), registry, path.clone());
        let first = manual_advance_trigger();
        let second = Event::new(EventType::CampaignAdvanceRequested, "p".into(), Throttle::Full, serde_json::json!({"campaign":"second"}));
        let (first,second) = tokio::join!(block.execute(&first), block.execute(&second));
        let results = [first.unwrap(),second.unwrap()];
        assert_eq!(results.iter().flat_map(|r|&r.events).filter(|e|e.event_type==EventType::ExecutionRequested).count(), 1);
        let store = CampaignStore::load(&path).unwrap();
        assert_eq!(store.find("c").unwrap().cycles_completed+store.find("second").unwrap().cycles_completed,1);
    }
}

struct InitiativeDecisionDuringFormation {
    status_only: bool,
    path: std::path::PathBuf,
    asked: std::sync::atomic::AtomicUsize,
    prompts: std::sync::Mutex<Vec<String>>,
}

impl crate::gateway::AgentGateway for InitiativeDecisionDuringFormation {
    fn invoke<'a>(&'a self, request:&'a crate::gateway::AgentRequest) -> std::pin::Pin<Box<dyn std::future::Future<Output=anyhow::Result<AgentResponse>>+Send+'a>> {
        Box::pin(async move {
            self.prompts.lock().unwrap().push(request.prompt.clone());
            if self.asked.fetch_add(1,std::sync::atomic::Ordering::SeqCst)==0 {
                let mut guard=CampaignStore::lock_exclusive(&self.path).unwrap();
                if self.status_only {
                    guard.store.record_initiative_status("both", "Verification is running", chrono::Utc::now()).unwrap();
                } else {
                    guard.store.decide_initiative("both","Use port 5001",&[],chrono::Utc::now()).unwrap();
                }
                guard.save().unwrap();
            }
            Ok(AgentResponse::success(r#"{"decision":"advance","objective":"Use the agreed port","reason":"owner policy"}"#))
        })
    }
}

#[tokio::test]
async fn initiative_decision_arriving_during_formation_is_kept_and_reformed() {
    let dir=tempfile::tempdir().unwrap();
    let (path,registry)=initiative_store(dir.path(),5,2);
    let agent=std::sync::Arc::new(InitiativeDecisionDuringFormation {status_only:false,path:path.clone(),asked:std::sync::atomic::AtomicUsize::new(0),prompts:std::sync::Mutex::new(Vec::new())});
    let block=advance_block(agent.clone(),FakeShellGateway::success(),registry,path.clone());
    let result=block.execute(&manual_advance_trigger()).await.unwrap();
    assert!(result.success,"{}",result.summary);
    assert_eq!(agent.asked.load(std::sync::atomic::Ordering::SeqCst),2);
    assert!(agent.prompts.lock().unwrap()[1].contains("Use port 5001"));
    let store=CampaignStore::load(&path).unwrap();
    assert_eq!(store.find("c").unwrap().owner_decisions.len(),1);
    assert_eq!(store.find("second").unwrap().owner_decisions.len(),1);
    assert_eq!(store.find("c").unwrap().cycles_completed,1);
}

#[tokio::test]
async fn automatic_diagnosis_names_the_exact_member_cycle_before_trace_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let (path, registry) = initiative_store(dir.path(), 5, 2);
    let mut ledger = WorkItemStore::default();
    for (id, member, minute) in [("wi_exact", "c", 0), ("wi_unrelated", "second", 1)] {
        let mut item = foundry_sdk::work_item::WorkItem::dispatched(foundry_sdk::work_item::WorkItemSpec {
            project:"p".into(),objective:"one cycle".into(),kind:foundry_sdk::work_item::WorkItemKind::CampaignCycle,
            lane:foundry_sdk::work_item::WorkLane::Campaign,origin:"initiative".into(),trace_id:Some("shared-trace".into()),
        },chrono::Utc::now()+chrono::Duration::minutes(minute));
        item.id = id.into();
        item.started_root = Some(Event::new(EventType::ExecutionRequested,"p".into(),Throttle::Full,serde_json::json!({"campaign":member,"campaign_cycle":1})));
        item.source = Some(WorkSource::initiative("both",1));
        ledger.upsert(item);
    }
    ledger.save(&ledger_path_beside(&path)).unwrap();
    let mut result = run_result_with(TaskVerdict::Defect {diagnosis:"The acceptance assertion bypasses the parser".into()});
    result.context.campaign_cycle = Some(1);
    let agent = FakeAgentGateway::success_with(r#"{"decision":"escalate","reason":"Need a parser decision"}"#);
    let block = advance_block(agent,FakeShellGateway::success(),registry,path.clone());
    block.execute(&automatic_advance_trigger(result).with_trace_id(Some("shared-trace".into()))).await.unwrap();
    let store=CampaignStore::load(&path).unwrap();
    let findings=store.list_findings("both").unwrap();
    assert_eq!(findings.len(),1);
    assert_eq!(findings[0].from.id,"wi_exact");
    assert_eq!(findings[0].from.kind,"campaign_cycle");
    assert_eq!(findings[0].kind,foundry_sdk::initiative::FindingKind::Regression);
}

#[tokio::test]
async fn initiative_status_arriving_during_formation_is_retained_without_reformation() {
    let dir = tempfile::tempdir().unwrap();
    let (path, registry) = initiative_store(dir.path(), 5, 2);
    let agent = std::sync::Arc::new(InitiativeDecisionDuringFormation {
        status_only: true, path: path.clone(),
        asked: std::sync::atomic::AtomicUsize::new(0), prompts: std::sync::Mutex::new(Vec::new()),
    });
    let block = advance_block(agent.clone(), FakeShellGateway::success(), registry, path.clone());
    let result = block.execute(&manual_advance_trigger()).await.unwrap();
    assert!(result.success, "{}", result.summary);
    assert_eq!(agent.asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    let store = CampaignStore::load(&path).unwrap();
    assert_eq!(store.find_initiative("both").unwrap().status_notes.len(), 1);
    assert!(store.find_initiative("both").unwrap().owner_decisions.is_empty());
    assert!(store.find("c").unwrap().owner_decisions.is_empty());
    assert_eq!(store.find("c").unwrap().cycles_completed, 1);
}
