use super::tests::{Fixture, ask};
use super::*;
use crate::gateway::ProcessShellGateway;
use crate::gateway::fakes::FakeAgentGateway;
use crate::gateway::{CommandResult, ShellGateway};
use std::path::Path;
use std::pin::Pin;
use std::sync::Mutex;

fn git(root: &Path, args: &[&str]) {
    let result = std::process::Command::new("git").current_dir(root).args(args).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
}

fn repository(fixture: &Fixture) {
    let root = fixture.temp.path();
    std::fs::write(root.join(".gitignore"), "campaigns.*\nledger.*\ninitiatives/\n").unwrap();
    git(root, &["init", "-b", "main"]);
    git(root, &["config", "user.name", "Sandbox"]);
    git(root, &["config", "user.email", "sandbox@example.invalid"]);
    git(root, &["add", "contract.rs", ".gitignore"]);
    git(root, &["commit", "-m", "Sandbox contract"]);
}

struct GateShell {
    result: serde_json::Value,
    calls: Mutex<Vec<(String, Vec<String>)>>,
    change_trunk: bool,
}
impl GateShell {
    fn new(passed: bool, report: &str) -> Arc<Self> {
        Arc::new(Self {
            result: serde_json::json!({"passed":passed,"exit_code":i32::from(!passed),"report":report,"artifacts":["sample/contract.rs"]}),
            calls: Mutex::new(vec![]),
            change_trunk: false,
        })
    }
}
impl ShellGateway for GateShell {
    fn run<'a>(
        &'a self,
        working_dir: &'a Path,
        command: &'a str,
        args: &'a [&'a str],
        env: Option<&'a [(String, String)]>,
        timeout: Option<Duration>,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<CommandResult>> + Send + 'a>> {
        self.calls
            .lock()
            .unwrap()
            .push((command.into(), args.iter().map(ToString::to_string).collect()));
        Box::pin(async move {
            if command == "python3" || command == "ssh" {
                let sync = command == "python3" && args[2].contains("\"mode\":\"sync\"");
                if !sync && self.change_trunk {
                    std::fs::write(working_dir.join("contract.rs"), "changed during gate").unwrap();
                    git(working_dir, &["add", "contract.rs"]);
                    git(working_dir, &["commit", "-m", "Concurrent trunk changed"]);
                }
                let result = if sync {
                    serde_json::json!({"passed":true,"exit_code":0,"report":"synced","artifacts":[]})
                } else {
                    self.result.clone()
                };
                Ok(CommandResult {
                    stdout: result.to_string(),
                    stderr: String::new(),
                    exit_code: 0,
                    success: true,
                })
            } else {
                ProcessShellGateway.run(working_dir, command, args, env, timeout).await
            }
        })
    }
}

fn gate(fixture: &Fixture, host: Option<&str>) {
    let mut store = CampaignStore::load(&fixture.store).unwrap();
    store.find_initiative_mut("shared").unwrap().done_evidence.insert(0, serde_json::from_value(serde_json::json!({"kind":"gate","command":"test -f sample/contract.rs","artifacts":["sample/contract.rs"],"host":host})).unwrap());
    store.save(&fixture.store).unwrap();
}

#[tokio::test]
async fn failed_gate_becomes_a_finding_and_rewrites_premature_done_to_work() {
    let fixture = Fixture::new();
    repository(&fixture);
    gate(&fixture, None);
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"done","reviews":[{"statement":"works","affirmed":true,"evidence":["sample/contract.rs"]}]},"audits":[]}"#,
    );
    let result = fixture
        .steer(agent.clone(), GateShell::new(false, "contract mismatch"))
        .execute(&fixture.trigger_holding_trunks())
        .await
        .unwrap();
    assert!(result.success, "{}", result.summary);
    let store = CampaignStore::load(&fixture.store).unwrap();
    let initiative = store.initiative_view("shared").unwrap();
    assert!(initiative.completion_proposal.is_none());
    assert!(matches!(
        initiative.steering_history[0].decision,
        foundry_sdk::initiative::SteeringDecision::Advance { .. }
    ));
    assert!(
        store
            .list_findings("shared")
            .unwrap()
            .iter()
            .any(|finding| finding.text.contains("contract mismatch"))
    );
    assert!(agent.invocations()[0].prompt.contains("contract mismatch"));
    let fact = result
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeSteered)
        .unwrap();
    assert_eq!(fact.payload["rejected_completion"]["decision"]["decision"], "done");
}

#[tokio::test]
async fn current_gates_and_reviews_propose_done_until_owner_confirmation() {
    let fixture = Fixture::new();
    repository(&fixture);
    gate(&fixture, None);
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"done","reviews":[{"statement":"works","affirmed":true,"evidence":["sample/contract.rs"]}]},"audits":[]}"#,
    );
    let result = fixture
        .steer(agent, GateShell::new(true, "sh: exit 0; preserved capture logs"))
        .execute(&fixture.trigger_holding_trunks())
        .await
        .unwrap();
    assert!(result.success, "{}", result.summary);
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
    let store = CampaignStore::load(&fixture.store).unwrap();
    let initiative = store.initiative_view("shared").unwrap();
    assert!(initiative.completion_proposal.is_some());
    assert!(!initiative.status.is_terminal());
    assert!(
        initiative.completion_receipts.is_empty(),
        "expected `initiative.completion_receipts` to be empty: {:?}",
        initiative.completion_receipts
    );
    assert!(
        initiative.pending_actions.is_empty(),
        "expected `initiative.pending_actions` to be empty: {:?}",
        initiative.pending_actions
    );
    assert!(
        applied.events.is_empty(),
        "expected `applied.events` to be empty: {:?}",
        applied.events
    );
}

#[tokio::test]
async fn a_review_only_pending_proposal_refreshes_trunks_before_reassessment() {
    let fixture = Fixture::new();
    repository(&fixture);
    let remote = tempfile::tempdir().unwrap();
    let origin = remote.path().join("origin.git");
    git(
        remote.path(),
        &[
            "clone",
            "--bare",
            fixture.temp.path().to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    git(fixture.temp.path(), &["remote", "add", "origin", origin.to_str().unwrap()]);
    let done = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"done","reviews":[{"statement":"works","affirmed":true,"evidence":["sample/contract.rs"]}]},"audits":[]}"#,
    );
    let result = fixture
        .steer(done, Arc::new(ProcessShellGateway))
        .execute(&fixture.trigger_holding_trunks())
        .await
        .unwrap();
    assert!(result.success, "{}", result.summary);
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
    assert!(apply.execute(action).await.unwrap().success);
    assert!(
        CampaignStore::load(&fixture.store)
            .unwrap()
            .initiative_view("shared")
            .unwrap()
            .completion_proposal
            .is_some()
    );
    let updated = remote.path().join("upstream");
    git(remote.path(), &["clone", origin.to_str().unwrap(), updated.to_str().unwrap()]);
    git(&updated, &["config", "user.name", "Sandbox"]);
    git(&updated, &["config", "user.email", "sandbox@example.invalid"]);
    std::fs::write(updated.join("contract.rs"), "current origin contract").unwrap();
    git(&updated, &["add", "contract.rs"]);
    git(&updated, &["commit", "-m", "Updated contract"]);
    // Push by path, not remote name: an inherited `remote.origin.pushurl`
    // (e.g. an agent session's push block) must not redirect this fixture.
    git(&updated, &["push", origin.to_str().unwrap(), "main"]);
    assert_eq!(
        std::fs::read_to_string(fixture.temp.path().join("contract.rs")).unwrap(),
        "evidence"
    );
    let result = fixture
        .steer(FakeAgentGateway::success_with(ask()), Arc::new(ProcessShellGateway))
        .execute(&fixture.trigger_holding_trunks())
        .await
        .unwrap();
    assert!(result.success, "{}", result.summary);
    let initiative =
        CampaignStore::load(&fixture.store).unwrap().initiative_view("shared").unwrap();
    assert!(initiative.completion_proposal.is_none());
    assert!(
        initiative.integration_results.is_empty(),
        "expected `initiative.integration_results` to be empty: {:?}",
        initiative.integration_results
    );
    assert_eq!(
        std::fs::read_to_string(fixture.temp.path().join("contract.rs")).unwrap(),
        "current origin contract"
    );
}

#[tokio::test]
async fn stage_records_one_campaign_without_starting_or_spending_a_cycle() {
    let fixture = Fixture::new();
    repository(&fixture);
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"stage","campaign":{"name":"uncovered","project":"sample","mission":"Connect both contracts","done_evidence":[{"kind":"review","statement":"connected"}]}},"audits":[]}"#,
    );
    let result = fixture
        .steer(agent, Arc::new(ProcessShellGateway))
        .execute(&Fixture::trigger(None))
        .await
        .unwrap();
    assert!(result.success, "{}", result.summary);
    let store = CampaignStore::load(&fixture.store).unwrap();
    assert_eq!(store.find("uncovered").unwrap().initiative.as_deref(), Some("shared"));
    assert_eq!(store.shared_cycles_used("shared").unwrap(), 0);
    let action = result
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeActionRequested)
        .unwrap();
    let applied = ApplyInitiativeAction::new(
        fixture.registry.clone(),
        fixture.store.clone(),
        fixture.ledger.clone(),
    )
    .execute(action)
    .await
    .unwrap();
    assert!(applied.success, "{}", applied.summary);
    // Staging starts no work. With members still eligible it asks for one
    // follow-up assessment, which is the only event.
    let kinds: Vec<_> = applied.events.iter().map(|event| &event.event_type).collect();
    assert_eq!(kinds, [&EventType::InitiativeAdvanceRequested], "{:?}", applied.events);
    assert_eq!(applied.events[0].payload["follow_up_of"]["step"], "stage");
}

#[tokio::test]
async fn named_host_uses_ssh_with_remote_registry_and_changed_trunks_fail() {
    let fixture = Fixture::new();
    repository(&fixture);
    gate(&fixture, Some("integration.example.invalid"));
    let shell = GateShell::new(true, "remote capture proof");
    let result = fixture
        .steer(FakeAgentGateway::success_with(ask()), shell.clone())
        .execute(&fixture.trigger_holding_trunks())
        .await
        .unwrap();
    assert!(result.success, "{}", result.summary);
    {
        let calls = shell.calls.lock().unwrap();
        let remote = calls.iter().find(|(command, _)| command == "ssh").unwrap();
        assert!(remote.1.iter().any(|arg| arg == "BatchMode=yes"));
        assert!(remote.1.iter().any(|arg| arg.contains("registry") && arg.contains("--json")));
    }
    let fixture = Fixture::new();
    repository(&fixture);
    gate(&fixture, None);
    let shell = Arc::new(GateShell {
        result: serde_json::json!({"passed":true,"exit_code":0,"report":"command succeeded","artifacts":["sample/contract.rs"]}),
        calls: Mutex::new(vec![]),
        change_trunk: true,
    });
    fixture
        .steer(FakeAgentGateway::success_with(ask()), shell)
        .execute(&fixture.trigger_holding_trunks())
        .await
        .unwrap();
    let initiative =
        CampaignStore::load(&fixture.store).unwrap().initiative_view("shared").unwrap();
    assert!(!initiative.integration_results[0].passed);
    assert!(initiative.integration_results[0].report.contains("Stale"));
}

#[tokio::test]
async fn a_steer_started_beside_member_work_skips_integration_and_records_why() {
    use foundry_sdk::work_item::{WorkItem, WorkItemKind, WorkItemSpec, WorkLane};
    let fixture = Fixture::new();
    repository(&fixture);
    gate(&fixture, None);
    // Pacing started the steer beside a member cycle, so it holds no trunk.
    let trigger = fixture.trigger_reserving(&[]);
    let mut ledger = WorkItemStore::load(&fixture.ledger).unwrap();
    ledger.upsert(WorkItem::dispatched(
        WorkItemSpec {
            project: "sample".into(),
            objective: "campaign".into(),
            kind: WorkItemKind::CampaignCycle,
            lane: WorkLane::Campaign,
            origin: "test".into(),
            trace_id: None,
        },
        chrono::Utc::now(),
    ));
    ledger.save(&fixture.ledger).unwrap();
    // A gate that ran would fail and file a regression finding.
    let shell = GateShell::new(false, "contract mismatch");
    let agent = FakeAgentGateway::success_with(ask());
    let result = fixture.steer(agent.clone(), shell.clone()).execute(&trigger).await.unwrap();
    assert!(result.success, "{}", result.summary);

    let calls = shell.calls.lock().unwrap().clone();
    assert!(
        !calls
            .iter()
            .any(|(command, _)| matches!(command.as_str(), "python3" | "ssh" | "hostname")),
        "integration touched the host or a trunk: {calls:?}"
    );
    assert!(!result.events.iter().any(|event| event.event_type == EventType::FindingRecorded));
    let store = CampaignStore::load(&fixture.store).unwrap();
    let findings = store.list_findings("shared").unwrap();
    assert!(findings.is_empty(), "{findings:?}");
    let initiative = store.initiative_view("shared").unwrap();
    assert!(
        initiative.integration_results.is_empty(),
        "{:?}",
        initiative.integration_results
    );
    let reason = "integration skipped: other work holds initiative trunks: sample";
    assert_eq!(initiative.steering_history[0].integration_skipped.as_deref(), Some(reason));
    assert!(agent.invocations()[0].prompt.contains(reason));
}

#[tokio::test]
async fn premature_done_without_eligible_repairs_records_recovery_and_no_owner_question() {
    let fixture = Fixture::new();
    repository(&fixture);
    gate(&fixture, None);
    let mut store = CampaignStore::load(&fixture.store).unwrap();
    for campaign in &mut store.campaigns {
        campaign.status = foundry_sdk::campaign::CampaignStatus::Completed;
    }
    store
        .record_initiative_recovery(
            "shared",
            "old-worker-wait",
            foundry_sdk::recovery::RecoveryKind::UnmetPrecondition,
            "member work was in flight",
            chrono::Utc::now(),
        )
        .unwrap();
    store.save(&fixture.store).unwrap();
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"done","reviews":[{"statement":"works","affirmed":true,"evidence":["sample/contract.rs"]}]},"audits":[]}"#,
    );
    let mut settled_trigger = fixture.trigger_holding_trunks();
    settled_trigger.payload["trigger_id"] = serde_json::json!("worker-settled");
    let result = fixture
        .steer(agent, GateShell::new(false, "contract probe failed"))
        .execute(&settled_trigger)
        .await
        .unwrap();
    assert!(result.success, "{}", result.summary);
    let action = result
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeActionRequested)
        .unwrap();
    let applied = super::action::ApplyInitiativeAction::new(
        fixture.registry.clone(),
        fixture.store.clone(),
        fixture.ledger.clone(),
    )
    .execute(action)
    .await
    .unwrap();
    assert!(applied.success, "{}", applied.summary);
    assert!(
        applied
            .events
            .iter()
            .any(|event| event.event_type == EventType::InitiativeRecoveryRecorded)
    );
    assert!(
        !applied
            .events
            .iter()
            .any(|event| event.event_type == EventType::InitiativeAdvanceRequested),
        "recovery must not spin itself"
    );
    let store = CampaignStore::load(&fixture.store).unwrap();
    let initiative = store.initiative_view("shared").unwrap();
    assert_eq!(
        initiative.open_questions,
        [] as [foundry_sdk::initiative::InitiativeQuestion; 0]
    );
    assert_eq!(
        initiative.owner_decisions,
        [] as [foundry_sdk::initiative::InitiativeOwnerDecision; 0]
    );
    let recovery = initiative.recovery.as_ref().unwrap();
    assert_ne!(
        recovery.id, "old-worker-wait",
        "fresh failed evidence must supersede the earlier settled-work wait"
    );
    assert!(recovery.is_pending(), "replacement is still a wait, never readiness");
    assert_eq!(recovery.kind, foundry_sdk::recovery::RecoveryKind::UnmetPrecondition);
    assert!(recovery.detail.contains("gate 0"), "{}", recovery.detail);
    assert!(initiative.completion_proposal.is_none());
}

#[tokio::test]
async fn fresh_done_evidence_rechecks_a_previous_completion_precondition_without_owner_policy() {
    let fixture = Fixture::new();
    repository(&fixture);
    gate(&fixture, None);
    let mut store = CampaignStore::load(&fixture.store).unwrap();
    store
        .record_initiative_recovery(
            "shared",
            "old-gate",
            foundry_sdk::recovery::RecoveryKind::UnmetPrecondition,
            "required initiative gate 0 has no current passing evidence",
            chrono::Utc::now(),
        )
        .unwrap();
    store.save(&fixture.store).unwrap();
    let agent = FakeAgentGateway::success_with(
        r#"{"action":{"decision":"done","reviews":[{"statement":"works","affirmed":true,"evidence":["sample/contract.rs"]}]},"audits":[]}"#,
    );
    let result = fixture
        .steer(agent, GateShell::new(true, "current gate capture passes"))
        .execute(&fixture.trigger_holding_trunks())
        .await
        .unwrap();
    assert!(result.success, "{}", result.summary);
    let action = result
        .events
        .iter()
        .find(|event| event.event_type == EventType::InitiativeActionRequested)
        .unwrap();
    let applied = ApplyInitiativeAction::new(
        fixture.registry.clone(),
        fixture.store.clone(),
        fixture.ledger.clone(),
    )
    .execute(action)
    .await
    .unwrap();
    assert!(applied.success, "{}", applied.summary);
    assert!(
        applied
            .events
            .iter()
            .any(|event| event.event_type == EventType::InitiativeRecoveryRechecked)
    );
    let initiative =
        CampaignStore::load(&fixture.store).unwrap().initiative_view("shared").unwrap();
    assert!(initiative.completion_proposal.is_some());
    assert!(!initiative.recovery.unwrap().is_pending());
    assert_eq!(
        initiative.owner_decisions,
        [] as [foundry_sdk::initiative::InitiativeOwnerDecision; 0]
    );
    assert_eq!(
        initiative.open_questions,
        [] as [foundry_sdk::initiative::InitiativeQuestion; 0]
    );
    assert!(
        !initiative.status.is_terminal(),
        "owner completion confirmation is still required"
    );
}
