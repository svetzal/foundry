//! Integration tests for the work-item ledger against the real task chain.
//!
//! The claim under test is a timing claim: the item is in the ledger file, in
//! state `running`, *before* the coding agent is invoked. A count taken after
//! the chain has finished cannot show that, so the agent gateway itself reads
//! the ledger on its first invocation and the test asserts on what it saw.
//!
//! The second claim is completeness: a dispatch that stops before the agent
//! starts still enters *and leaves* the ledger. Those tests assert the agent
//! recorded zero invocations, so "it was settled" cannot be confused with
//! "it ran and failed".

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::{Arc, Mutex, RwLock};

use foundry_sdk::event::{Event, EventType, mint_trace_id};
use foundry_sdk::gateway::{AgentGateway, AgentRequest, AgentResponse};
use foundry_sdk::registry::Registry;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{WorkItem, WorkItemKind, WorkItemState, WorkItemStore, WorkLane};

use super::test_helpers;
use foundry_engine::engine::Engine;

/// An agent gateway that reads the work-item ledger from disk every time it is
/// invoked, and replays a fixed script of responses.
struct LedgerReadingAgent {
    store_path: PathBuf,
    responses: Vec<String>,
    /// The ledger as it stood at each invocation, in invocation order.
    observed: Mutex<Vec<Vec<WorkItem>>>,
}

impl LedgerReadingAgent {
    fn new(store_path: PathBuf, responses: Vec<&str>) -> Arc<Self> {
        Arc::new(Self {
            store_path,
            responses: responses.into_iter().map(str::to_string).collect(),
            observed: Mutex::new(Vec::new()),
        })
    }

    /// The ledger as it stood when the agent was first invoked.
    fn ledger_at_first_invocation(&self) -> Vec<WorkItem> {
        self.observed.lock().unwrap().first().cloned().unwrap_or_default()
    }
}

impl AgentGateway for LedgerReadingAgent {
    fn invoke<'a>(
        &'a self,
        _request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<AgentResponse>> + Send + 'a>> {
        let mut observed = self.observed.lock().unwrap();
        let store = WorkItemStore::load(&self.store_path).unwrap_or_default();
        let index = observed.len();
        observed.push(store.items);
        drop(observed);
        let reply = self.responses.get(index).cloned().unwrap_or_default();
        Box::pin(async move { Ok(AgentResponse::success(reply)) })
    }
}

fn git_ok(cwd: Option<&Path>, args: &[&str]) -> bool {
    let mut command = Command::new("git");
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .args(args);
    for index in 0..8 {
        command.env_remove(format!("GIT_CONFIG_KEY_{index}"));
        command.env_remove(format!("GIT_CONFIG_VALUE_{index}"));
    }
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    command.status().unwrap().success()
}

/// A project checkout with a pushable origin and a trivially passing gate —
/// the same fixture `task_workflow_happy_path` uses.
fn task_project(dir: &Path) -> PathBuf {
    let remote = dir.join("remote.git");
    let remote_url = format!("file://{}", remote.display());
    let checkout = dir.join("checkout");
    assert!(git_ok(None, &["init", "--bare", remote.to_str().unwrap()]));
    assert!(git_ok(None, &["init", "-b", "main", checkout.to_str().unwrap()]));
    assert!(git_ok(Some(&checkout), &["config", "user.email", "foundry-test@example.com"]));
    assert!(git_ok(Some(&checkout), &["config", "user.name", "Foundry Test"]));
    std::fs::write(checkout.join("CHARTER.md"), "a".repeat(100)).unwrap();
    std::fs::write(
        checkout.join(".hone-gates.json"),
        r#"{"gates":[{"name":"fmt","command":"true","required":true}]}"#,
    )
    .unwrap();
    assert!(git_ok(Some(&checkout), &["add", "CHARTER.md", ".hone-gates.json"]));
    assert!(git_ok(Some(&checkout), &["commit", "-m", "initial"]));
    assert!(git_ok(Some(&checkout), &["remote", "add", "origin", &remote_url]));
    let _ = Command::new("git")
        .current_dir(&checkout)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .args(["config", "--unset-all", "remote.origin.pushurl"])
        .status();
    assert!(git_ok(Some(&checkout), &["remote", "set-url", "--push", "origin", &remote_url]));
    assert!(git_ok(Some(&checkout), &["push", "-u", "origin", "main"]));
    checkout
}

/// The real task chain, with the three ledger blocks registered exactly as
/// `foundryd` registers them.
fn task_engine(
    agent: Arc<dyn AgentGateway>,
    registry: Arc<RwLock<Registry>>,
    store_path: &Path,
) -> Engine {
    let shell = test_helpers::passing_shell();
    let mut engine = Engine::new();
    engine.register(Box::new(foundry_blocks::blocks::CheckCharter::new(registry.clone())));
    test_helpers::register_gate_scaffold(&mut engine, shell, registry.clone());
    engine.register(Box::new(foundry_blocks::blocks::DirectPrompt));
    register_ledger_blocks(&mut engine, store_path, &registry);
    engine.register(Box::new(foundry_blocks::blocks::ExecutePlan::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::ReviewTask::new(agent, registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::FinalizeTask::new(registry)));
    engine
}

/// The ledger blocks that bracket the task chain.
fn register_ledger_blocks(
    engine: &mut Engine,
    store_path: &Path,
    registry: &Arc<RwLock<Registry>>,
) {
    engine.register(Box::new(foundry_blocks::blocks::RecordWorkItem::new(
        store_path.to_path_buf(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::SettleFailedDispatch::new(
        store_path.to_path_buf(),
    )));
    engine
        .register(Box::new(foundry_blocks::blocks::SettleWorkItem::new(store_path.to_path_buf())));
}

/// A `foundry task`-shaped dispatch, optionally carrying campaign context.
fn task_dispatch(project: &str, prompt: &str, extra: &serde_json::Value) -> Event {
    let mut payload = serde_json::json!({
        "project": project,
        "workflow": "task",
        "prompt": prompt,
    });
    if let (Some(target), Some(extra)) = (payload.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    Event::new(EventType::ExecutionRequested, project.to_string(), Throttle::Full, payload)
        .with_trace_id(Some(mint_trace_id()))
}

/// Run one task dispatch through the chain and return the agent probe, the
/// ledger afterwards, and every event the chain produced.
async fn run_dispatch(
    dir: &Path,
    prompt: &str,
    extra: &serde_json::Value,
) -> (Arc<LedgerReadingAgent>, WorkItemStore, Vec<Event>) {
    let checkout = task_project(dir);
    let store_path = dir.join("work-items.json");
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let agent = LedgerReadingAgent::new(
        store_path.clone(),
        vec![
            "Done, implemented the task",
            "```json\n{\"verdict\":\"complete\"}\n```",
        ],
    );
    let engine = task_engine(agent.clone(), registry, &store_path);
    let result = engine.process(task_dispatch("test-project", prompt, extra)).await;
    let store = WorkItemStore::load(&store_path).unwrap();
    (agent, store, result.events)
}

#[tokio::test]
async fn a_task_dispatch_is_running_in_the_ledger_before_the_agent_is_invoked() {
    let dir = tempfile::tempdir().unwrap();
    let (agent, store, events) =
        run_dispatch(dir.path(), "Add a --quiet flag to the CLI.", &serde_json::json!({})).await;

    let at_first_call = agent.ledger_at_first_invocation();
    assert_eq!(
        at_first_call.len(),
        1,
        "the ledger must already hold the item when the agent is first invoked"
    );
    assert_eq!(at_first_call[0].state, WorkItemState::Running);
    assert_eq!(at_first_call[0].kind, WorkItemKind::Task);
    assert_eq!(at_first_call[0].lane, WorkLane::Interactive);
    assert_eq!(at_first_call[0].objective, "Add a --quiet flag to the CLI.");
    assert!(at_first_call[0].started_at.is_some());

    // …and after `task_run_completed`, the same item is settled.
    assert!(
        events.iter().any(|e| e.event_type == EventType::TaskRunCompleted),
        "the chain must have reached its terminal task result"
    );
    assert_eq!(store.items.len(), 1, "no second item was created");
    assert_eq!(store.items[0].id, at_first_call[0].id);
    assert_eq!(store.items[0].state, WorkItemState::Landed);
    assert!(store.items[0].settled_at.is_some());
    assert_eq!(store.items[0].disposition.clone().unwrap().verdict.as_deref(), Some("complete"));
}

#[tokio::test]
async fn a_campaign_cycle_dispatch_records_its_campaign_lane_and_origin() {
    let dir = tempfile::tempdir().unwrap();
    let (agent, store, _) = run_dispatch(
        dir.path(),
        "Close the next gap in the CLI surface.",
        &serde_json::json!({"campaign": "tidy-cli", "campaign_cycle": 4}),
    )
    .await;

    let at_first_call = agent.ledger_at_first_invocation();
    assert_eq!(at_first_call.len(), 1);
    assert_eq!(at_first_call[0].state, WorkItemState::Running);
    assert_eq!(at_first_call[0].kind, WorkItemKind::CampaignCycle);
    assert_eq!(at_first_call[0].lane, WorkLane::Campaign);
    assert_eq!(at_first_call[0].origin, "campaign tidy-cli cycle 4");

    assert_eq!(store.items[0].state, WorkItemState::Landed);
    assert_eq!(store.items[0].kind, WorkItemKind::CampaignCycle);
}

#[tokio::test]
async fn a_majors_lane_dispatch_records_the_major_upgrade_kind_and_maintenance_lane() {
    let dir = tempfile::tempdir().unwrap();
    // The objective the nightly majors lane writes, verbatim.
    let objective = "Upgrade serde from 1.0.0 to 2.0.0 in test-project: adapt call sites, \
                     keep all gates green. The cargo dependency is declared in the repository \
                     root. Change only what this upgrade needs.";
    let (agent, store, _) = run_dispatch(dir.path(), objective, &serde_json::json!({})).await;

    let at_first_call = agent.ledger_at_first_invocation();
    assert_eq!(at_first_call.len(), 1);
    assert_eq!(at_first_call[0].state, WorkItemState::Running);
    assert_eq!(at_first_call[0].kind, WorkItemKind::MajorUpgrade);
    assert_eq!(at_first_call[0].lane, WorkLane::Maintenance);
    assert_eq!(at_first_call[0].origin, "nightly majors lane");

    assert_eq!(store.items[0].kind, WorkItemKind::MajorUpgrade);
    assert_eq!(store.items[0].state, WorkItemState::Landed);
}

/// An unreachable origin is the canonical pre-agent fault: preflight passes,
/// then `prepare_task_workspace` cannot fetch a base to branch from, so the
/// chain emits `task_run_completed` with a `runner_error` and the agent never
/// runs. The item must not be left running.
#[tokio::test]
async fn a_dispatch_that_fails_before_the_agent_runs_settles_failed_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = task_project(dir.path());
    let absent = format!("file://{}", dir.path().join("absent.git").display());
    assert!(git_ok(Some(&checkout), &["remote", "set-url", "origin", &absent]));

    let store_path = dir.path().join("work-items.json");
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let agent = LedgerReadingAgent::new(store_path.clone(), vec!["never reached"]);
    let engine = task_engine(agent.clone(), registry, &store_path);

    let result = engine
        .process(task_dispatch(
            "test-project",
            "Add a --quiet flag to the CLI.",
            &serde_json::json!({}),
        ))
        .await;

    let completed = result
        .events
        .iter()
        .find(|e| e.event_type == EventType::TaskRunCompleted)
        .expect("a pre-agent fault still reports a terminal task result");
    assert_eq!(completed.payload["verdict"], "runner_error");
    assert!(
        agent.observed.lock().unwrap().is_empty(),
        "the agent must not have run for a workspace that could not be prepared"
    );

    let store = WorkItemStore::load(&store_path).unwrap();
    assert_eq!(store.items.len(), 1);
    assert_eq!(store.items[0].state, WorkItemState::Failed);
    assert!(
        store.items[0].reason.contains("fetch") || store.items[0].reason.contains("origin"),
        "the fault's reason must be recorded, got: {}",
        store.items[0].reason
    );
}

/// The ledger's events are engine events like any other: they reach the Watch
/// broadcast and the durable JSONL log.
#[tokio::test]
async fn one_dispatch_broadcasts_and_logs_its_three_work_item_events() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = task_project(dir.path());
    let store_path = dir.path().join("work-items.json");
    let events_dir = dir.path().join("events");
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let agent = LedgerReadingAgent::new(
        store_path.clone(),
        vec![
            "Done, implemented the task",
            "```json\n{\"verdict\":\"complete\"}\n```",
        ],
    );

    let (tx, mut rx) = tokio::sync::broadcast::channel(256);
    let writer = Arc::new(foundry_engine::event_writer::EventWriter::new(events_dir.clone()));
    let engine = task_engine(agent, registry, &store_path)
        .with_event_broadcaster(tx)
        .with_event_writer(writer);

    engine
        .process(task_dispatch(
            "test-project",
            "Add a --quiet flag to the CLI.",
            &serde_json::json!({}),
        ))
        .await;

    let mut broadcast = Vec::new();
    while let Ok(event) = rx.try_recv() {
        broadcast.push(event);
    }
    let ledger_events: Vec<&Event> = broadcast
        .iter()
        .filter(|e| {
            matches!(
                e.event_type,
                EventType::WorkItemSubmitted
                    | EventType::WorkItemStarted
                    | EventType::WorkItemSettled
            )
        })
        .collect();
    let types: Vec<String> = ledger_events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        vec![
            "work_item_submitted",
            "work_item_started",
            "work_item_settled"
        ],
        "in {:?}",
        broadcast.iter().map(|e| e.event_type.as_str()).collect::<Vec<String>>()
    );

    let item_id = ledger_events[0].payload["item_id"].as_str().unwrap().to_string();
    for event in &ledger_events {
        assert_eq!(event.payload["item_id"], item_id.as_str());
        assert_eq!(event.payload["project"], "test-project");
        assert_eq!(event.payload["kind"], "task");
        assert_eq!(event.payload["lane"], "interactive");
        assert_eq!(event.payload["origin"], "foundry task");
        assert!(event.payload["state"].is_string());
        assert!(!event.payload["reason"].as_str().unwrap().is_empty());
    }

    let logged: String = std::fs::read_dir(&events_dir)
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    for wanted in [
        "work_item_submitted",
        "work_item_started",
        "work_item_settled",
    ] {
        assert!(logged.contains(wanted), "{wanted} missing from the JSONL event log");
    }
    assert!(logged.contains(&item_id), "the item id must reach the durable log");
}

// --- dispatches that stop before the agent starts --------------------------

/// Stands in for a task-workflow preflight that ran gates and failed one.
///
/// `RunPreflightGates` skips preflight for the task workflow and emits
/// `PreflightCompleted { all_passed: true, skipped: true }`, so the only way to
/// drive a *failing* task preflight through the real chain is to emit the event
/// the gate runner would emit. Everything downstream of it — `DirectPrompt`
/// rejecting a failed preflight, `SettleFailedDispatch` closing the item — is
/// the production block, unchanged.
struct FailingTaskPreflight;

impl foundry_sdk::task_block::TaskBlock for FailingTaskPreflight {
    fn name(&self) -> &'static str {
        "Failing Task Preflight"
    }

    fn kind(&self) -> foundry_sdk::task_block::BlockKind {
        foundry_sdk::task_block::BlockKind::Observer
    }

    fn sinks_on(&self) -> &[EventType] {
        &[EventType::CharterCheckCompleted]
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let project = trigger.project.clone();
        let prompt = trigger.payload["prompt"].clone();
        let throttle = trigger.throttle;
        Box::pin(async move {
            let payload = serde_json::json!({
                "project": project,
                "workflow": "task",
                "all_passed": false,
                "required_passed": false,
                "results": [{
                    "name": "clippy",
                    "command": "cargo clippy",
                    "passed": false,
                    "required": true,
                    "output": "",
                    "exit_code": 101,
                }],
                "prompt": prompt,
            });
            Ok(foundry_sdk::task_block::TaskBlockResult::success(
                format!("{project}: preflight failed"),
                vec![Event::new(
                    EventType::PreflightCompleted,
                    project,
                    throttle,
                    payload,
                )],
            ))
        })
    }
}

#[tokio::test]
async fn a_charter_failed_dispatch_settles_failed_without_ever_invoking_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = task_project(dir.path());
    // The one thing that makes the charter check fail: no intent documentation.
    std::fs::remove_file(checkout.join("CHARTER.md")).unwrap();

    let store_path = dir.path().join("work-items.json");
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let agent = LedgerReadingAgent::new(store_path.clone(), vec!["never reached"]);
    let engine = task_engine(agent.clone(), registry, &store_path);

    let result = engine
        .process(task_dispatch(
            "test-project",
            "Add a --quiet flag to the CLI.",
            &serde_json::json!({}),
        ))
        .await;

    assert!(
        result.events.iter().any(|e| {
            e.event_type == EventType::CharterCheckCompleted && e.payload["success"] == false
        }),
        "the charter check must have failed"
    );
    assert!(
        !result.events.iter().any(|e| e.event_type == EventType::PreflightCompleted),
        "the chain must stop before preflight"
    );
    assert_eq!(
        agent.observed.lock().unwrap().len(),
        0,
        "the agent must never be invoked for a project with no charter"
    );

    let store = WorkItemStore::load(&store_path).unwrap();
    assert_eq!(store.items.len(), 1, "exactly one item, entered and left");
    assert_eq!(store.items[0].state, WorkItemState::Failed);
    assert!(store.items[0].settled_at.is_some());
    assert!(
        store.items[0].reason.contains("charter"),
        "the reason must carry the charter guidance, got: {}",
        store.items[0].reason
    );
    assert_eq!(store.items[0].kind, WorkItemKind::Task);
    assert_eq!(store.items[0].lane, WorkLane::Interactive);
}

#[tokio::test]
async fn a_preflight_failed_dispatch_settles_failed_naming_the_gate_without_invoking_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = task_project(dir.path());
    let store_path = dir.path().join("work-items.json");
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let agent = LedgerReadingAgent::new(store_path.clone(), vec!["never reached"]);

    let mut engine = Engine::new();
    engine.register(Box::new(foundry_blocks::blocks::CheckCharter::new(registry.clone())));
    engine.register(Box::new(FailingTaskPreflight));
    engine.register(Box::new(foundry_blocks::blocks::DirectPrompt));
    register_ledger_blocks(&mut engine, &store_path, &registry);
    engine.register(Box::new(foundry_blocks::blocks::ExecutePlan::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::ReviewTask::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::FinalizeTask::new(registry)));

    let result = engine
        .process(task_dispatch(
            "test-project",
            "Add a --quiet flag to the CLI.",
            &serde_json::json!({}),
        ))
        .await;

    assert!(
        !result.events.iter().any(|e| e.event_type == EventType::PlanCompleted),
        "a failed preflight must not forward the prompt to execution"
    );
    assert_eq!(
        agent.observed.lock().unwrap().len(),
        0,
        "the agent must never be invoked when preflight fails"
    );

    let store = WorkItemStore::load(&store_path).unwrap();
    assert_eq!(store.items.len(), 1, "exactly one item, entered and left");
    assert_eq!(store.items[0].state, WorkItemState::Failed);
    assert_eq!(store.items[0].reason, "preflight gates failed: clippy");
}

/// The ledger's events are engine events like any other even when the dispatch
/// never reaches an agent: the whole record still reaches the broadcast.
#[tokio::test]
async fn a_charter_failed_dispatch_broadcasts_its_three_work_item_events() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = task_project(dir.path());
    std::fs::remove_file(checkout.join("CHARTER.md")).unwrap();
    let store_path = dir.path().join("work-items.json");
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let agent = LedgerReadingAgent::new(store_path.clone(), vec!["never reached"]);

    let (tx, mut rx) = tokio::sync::broadcast::channel(256);
    let engine = task_engine(agent, registry, &store_path).with_event_broadcaster(tx);

    engine
        .process(task_dispatch(
            "test-project",
            "Add a --quiet flag to the CLI.",
            &serde_json::json!({}),
        ))
        .await;

    let mut broadcast = Vec::new();
    while let Ok(event) = rx.try_recv() {
        broadcast.push(event);
    }
    let ledger_events: Vec<&Event> = broadcast
        .iter()
        .filter(|e| {
            matches!(
                e.event_type,
                EventType::WorkItemSubmitted
                    | EventType::WorkItemStarted
                    | EventType::WorkItemSettled
            )
        })
        .collect();
    let types: Vec<String> = ledger_events.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(
        types,
        vec![
            "work_item_submitted",
            "work_item_started",
            "work_item_settled"
        ],
        "in {:?}",
        broadcast.iter().map(|e| e.event_type.as_str()).collect::<Vec<String>>()
    );

    let item_id = ledger_events[0].payload["item_id"].as_str().unwrap().to_string();
    for event in &ledger_events {
        assert_eq!(event.payload["item_id"], item_id.as_str());
        assert_eq!(event.payload["project"], "test-project");
        assert_eq!(event.payload["kind"], "task");
        assert_eq!(event.payload["lane"], "interactive");
        assert_eq!(event.payload["origin"], "foundry task");
        assert!(!event.payload["reason"].as_str().unwrap().is_empty());
    }
    let settled = ledger_events[2];
    assert_eq!(settled.payload["state"], "failed");
    assert!(
        settled.payload["reason"].as_str().unwrap().contains("charter"),
        "the settlement reason must name why the dispatch stopped"
    );
}
