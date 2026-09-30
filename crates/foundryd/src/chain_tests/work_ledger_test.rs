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
use foundry_sdk::task_block::TaskBlock as _;
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{WorkItem, WorkItemKind, WorkItemState, WorkItemStore, WorkLane};

use super::test_helpers;
use foundry_engine::engine::Engine;

/// An agent gateway that reads the work-item ledger from disk every time it is
/// invoked, and replays a fixed script of responses.
pub(super) struct LedgerReadingAgent {
    store_path: PathBuf,
    responses: Vec<String>,
    /// The ledger as it stood at each invocation, in invocation order.
    pub(super) observed: Mutex<Vec<Vec<WorkItem>>>,
}

impl LedgerReadingAgent {
    pub(super) fn new(store_path: PathBuf, responses: Vec<&str>) -> Arc<Self> {
        Arc::new(Self {
            store_path,
            responses: responses.into_iter().map(str::to_string).collect(),
            observed: Mutex::new(Vec::new()),
        })
    }

    /// The ledger as it stood when the agent was first invoked.
    pub(super) fn ledger_at_first_invocation(&self) -> Vec<WorkItem> {
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

/// Agent-only fake: execution must begin with the actual preserved Git tree.
struct ContinuationAgent {
    parent: WorkItem,
    ledger: PathBuf,
    calls: Mutex<usize>,
    verdict: &'static str,
}

impl AgentGateway for ContinuationAgent {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<AgentResponse>> + Send + 'a>> {
        let mut calls = self.calls.lock().unwrap();
        let first = *calls == 0;
        *calls += 1;
        if first {
            assert_eq!(
                std::fs::read_to_string(request.working_dir.join("preserved.txt")).unwrap(),
                "preserved change"
            );
            let store = WorkItemStore::load(&self.ledger).unwrap();
            assert_eq!(store.find(&self.parent.id), Some(&self.parent));
            let child = store
                .items
                .iter()
                .find(|item| item.resumes.as_deref() == Some(&self.parent.id))
                .unwrap();
            assert_ne!(child.id, self.parent.id);
            assert_eq!(child.objective, self.parent.objective);
            assert_eq!(child.state, WorkItemState::Running);
        }
        let reply = if first {
            "Implemented".to_string()
        } else {
            format!("```json\n{}\n```", self.verdict)
        };
        Box::pin(async move {
            if reply.contains("agent failure") {
                Ok(AgentResponse::failure("agent failure"))
            } else {
                Ok(AgentResponse::success(reply))
            }
        })
    }
}

async fn resume_service(
    dir: &Path,
    registry: Arc<RwLock<Registry>>,
    engine: Engine,
) -> (
    crate::proto::foundry_client::FoundryClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
    tokio::sync::broadcast::Receiver<Event>,
) {
    let (client, server, events, _) = resume_service_tracked(dir, registry, engine).await;
    (client, server, events)
}

async fn resume_service_tracked(
    dir: &Path,
    registry: Arc<RwLock<Registry>>,
    engine: Engine,
) -> (
    crate::proto::foundry_client::FoundryClient<tonic::transport::Channel>,
    tokio::task::JoinHandle<()>,
    tokio::sync::broadcast::Receiver<Event>,
    Arc<crate::workflow_tracker::WorkflowTracker>,
) {
    use crate::service::{FoundryService, RuntimeContext, StoreConfig};
    let (event_tx, events) = tokio::sync::broadcast::channel(256);
    let trace_writer = Arc::new(foundry_blocks::trace_writer::TraceWriter::new(
        dir.join("traces").to_str().unwrap(),
    ));
    let ctx = RuntimeContext {
        engine: Arc::new(engine.with_event_broadcaster(event_tx.clone()).with_event_writer(
            Arc::new(foundry_engine::event_writer::EventWriter::new(dir.join("events"))),
        )),
        trace_store: Arc::new(crate::trace_store::TraceStore::with_trace_writer(
            std::time::Duration::from_secs(60),
            trace_writer.clone(),
        )),
        workflow_tracker: Arc::new(crate::workflow_tracker::WorkflowTracker::new()),
        trace_writer,
        event_tx,
        registry,
    };
    let tracker = ctx.workflow_tracker.clone();
    let service = FoundryService::new(
        ctx,
        StoreConfig {
            work_items_path: dir.join("work-items.json"),
            events_dir: dir.join("events"),
            campaigns_path: dir.join("campaigns.json"),
            registry_path: dir.join("registry.json"),
            sentinels: Arc::new(RwLock::new(foundry_sdk::sentinel::SentinelStore::default_seed())),
            sentinels_path: dir.join("sentinels.json"),
            scheduler_reload: Arc::new(tokio::sync::Notify::new()),
        },
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(crate::proto::foundry_server::FoundryServer::new(service))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let client = crate::proto::foundry_client::FoundryClient::connect(addr).await.unwrap();
    (client, server, events, tracker)
}

fn preserved_parent(checkout: &Path) -> WorkItem {
    assert!(git_ok(Some(checkout), &["checkout", "-b", "preserved-work"]));
    std::fs::write(checkout.join("preserved.txt"), "preserved change").unwrap();
    assert!(git_ok(Some(checkout), &["add", "preserved.txt"]));
    assert!(git_ok(Some(checkout), &["commit", "-m", "preserved work"]));
    assert!(git_ok(Some(checkout), &["checkout", "main"]));
    let mut parent = WorkItem::dispatched(
        foundry_sdk::work_item::WorkItemSpec {
            project: "test-project".to_string(),
            objective: "Finish preserved changes".to_string(),
            kind: WorkItemKind::Task,
            lane: WorkLane::Interactive,
            origin: "original submitter".to_string(),
            trace_id: Some(mint_trace_id()),
        },
        chrono::Utc::now(),
    );
    parent.state = WorkItemState::Preserved;
    parent.reason = "prior remainder".to_string();
    parent.settled_at = Some(chrono::Utc::now());
    parent.disposition = Some(foundry_sdk::work_item::WorkDisposition {
        verdict: Some("remainder".to_string()),
        preservation_ref: Some("preserved-work".to_string()),
        landed_commit: None,
        worktree: Some("prior-worktree".to_string()),
        worktree_removed: Some(true),
    });
    parent
}

#[tokio::test]
async fn resume_generated_client_runs_preserved_tree_and_settles_exact_parent_on_landing() {
    for source in ["local", "remote", "bundle"] {
        assert_resume_landing(source).await;
    }
}

async fn assert_resume_landing(source: &str) {
    let dir = tempfile::tempdir().unwrap();
    let checkout = task_project(dir.path());
    let mut parent = preserved_parent(&checkout);
    let base = preservation_source(&checkout, dir.path(), source);
    parent.disposition.as_mut().unwrap().preservation_ref = Some(base.clone());
    let mut sibling = parent.clone();
    sibling.id = "wi_unrelated_sibling".to_string();
    let ledger = dir.path().join("work-items.json");
    WorkItemStore {
        version: 1,
        items: vec![parent.clone(), sibling.clone()],
    }
    .save(&ledger)
    .unwrap();
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let agent = Arc::new(ContinuationAgent {
        parent: parent.clone(),
        ledger: ledger.clone(),
        calls: Mutex::new(0),
        verdict: r#"{"verdict":"complete"}"#,
    });
    let engine = continuation_engine(agent.clone(), registry.clone(), &ledger);
    let (mut client, server, mut events) = resume_service(dir.path(), registry, engine).await;
    let watch = client
        .watch(crate::proto::WatchRequest {
            project: parent.project.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    let child = client
        .resume_work_item(crate::proto::ResumeWorkItemRequest {
            id: parent.id.clone(),
            operator_origin: "owner-host (finish it)".to_string(),
        })
        .await
        .unwrap()
        .into_inner()
        .item
        .unwrap();
    assert_eq!(child.resumes.as_deref(), Some(parent.id.as_str()));
    assert_eq!(child.objective, parent.objective);
    assert_eq!(child.origin, parent.origin);
    assert_eq!(child.operator_action.as_ref().unwrap().origin, "owner-host (finish it)");
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut observed = Vec::new();
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv()).await.unwrap().unwrap();
        let done =
            event.event_type == EventType::WorkItemSettled && event.payload["item_id"] == parent.id;
        observed.push(event);
        if done {
            break;
        }
    }
    assert_watch_matches(watch, &observed).await;
    let store = WorkItemStore::load(&ledger).unwrap();
    let landed = store.find(&parent.id).unwrap();
    let child_item = store.find(&child.id).unwrap();
    assert_eq!(landed.state, WorkItemState::Landed);
    assert_eq!(child_item.state, WorkItemState::Landed);
    assert_eq!(store.find(&sibling.id), Some(&sibling));
    assert_eq!(landed.origin, parent.origin);
    assert_eq!(
        landed.disposition.as_ref().unwrap().preservation_ref,
        parent.disposition.as_ref().unwrap().preservation_ref
    );
    let commit = landed.disposition.as_ref().unwrap().landed_commit.as_ref().unwrap();
    assert_eq!(child_item.disposition.as_ref().unwrap().landed_commit.as_ref(), Some(commit));
    let actual = Command::new("git")
        .current_dir(&checkout)
        .args(["rev-parse", "main"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(actual.stdout).unwrap().trim(), commit);
    let dispatch = observed.iter().find(|e| e.event_type == EventType::ExecutionRequested).unwrap();
    assert_eq!(dispatch.payload["base_ref"], base);
    assert_eq!(dispatch.payload["prompt"], parent.objective);
    assert_resume_admission(&store, &parent, &child, &observed);
    for id in [&parent.id, &child.id] {
        let read = client
            .get_work_item(crate::proto::GetWorkItemRequest { id: id.clone() })
            .await
            .unwrap()
            .into_inner()
            .item
            .unwrap();
        assert_eq!(read.landed_commit.as_ref(), Some(commit));
        assert_lifecycle_log_matches(&dir.path().join("events"), id, &observed);
    }
    assert_terminal_redelivery_is_inert(&ledger, &observed).await;
    assert_eq!(*agent.calls.lock().unwrap(), 2);
    server.abort();
}

#[tokio::test]
async fn resume_generated_client_refuses_every_other_state_and_failed_admission_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = task_project(dir.path());
    let parent = preserved_parent(&checkout);
    let ledger = dir.path().join("work-items.json");
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let mut items = vec![parent.clone()];
    for state in [
        WorkItemState::Submitted,
        WorkItemState::Queued,
        WorkItemState::Running,
        WorkItemState::Landed,
        WorkItemState::NeedsDecision,
        WorkItemState::Failed,
        WorkItemState::Cancelled,
    ] {
        let mut other = parent.clone();
        other.id = format!("wi_{}", state.tag());
        other.state = state;
        items.push(other);
    }
    let mut missing = parent.clone();
    missing.id = "wi_missing_evidence".to_string();
    missing.disposition = None;
    items.push(missing);
    let mut unusable = parent.clone();
    unusable.id = "wi_unusable".to_string();
    unusable.disposition.as_mut().unwrap().preservation_ref = Some("no-such-branch".to_string());
    items.push(unusable);
    let mut unknown_project = parent.clone();
    unknown_project.id = "wi_unregistered".to_string();
    unknown_project.project = "absent".to_string();
    items.push(unknown_project);
    WorkItemStore {
        version: 1,
        items: items.clone(),
    }
    .save(&ledger)
    .unwrap();
    let (mut client, server, mut events) =
        resume_service(dir.path(), registry, Engine::new()).await;
    let before = std::fs::read(&ledger).unwrap();
    for other in items.iter().skip(1) {
        let error = client
            .resume_work_item(crate::proto::ResumeWorkItemRequest {
                id: other.id.clone(),
                operator_origin: "host desk".to_string(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition, "{}", other.id);
        assert_eq!(std::fs::read(&ledger).unwrap(), before);
    }
    for (id, origin, code) in [
        ("absent", "host desk", tonic::Code::NotFound),
        (" ", "host desk", tonic::Code::InvalidArgument),
        (parent.id.as_str(), " ", tonic::Code::InvalidArgument),
    ] {
        assert_eq!(
            client
                .resume_work_item(crate::proto::ResumeWorkItemRequest {
                    id: id.to_string(),
                    operator_origin: origin.to_string()
                })
                .await
                .unwrap_err()
                .code(),
            code
        );
    }
    std::fs::create_dir(ledger.with_extension("json.tmp")).unwrap();
    assert_eq!(
        client
            .resume_work_item(crate::proto::ResumeWorkItemRequest {
                id: parent.id,
                operator_origin: "host desk".to_string()
            })
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Internal
    );
    assert_eq!(std::fs::read(&ledger).unwrap(), before);
    assert!(events.try_recv().is_err());
    assert!(!dir.path().join("events").exists());
    server.abort();
}

fn continuation_engine(
    agent: Arc<dyn AgentGateway>,
    registry: Arc<RwLock<Registry>>,
    ledger: &Path,
) -> Engine {
    let mut engine = Engine::new();
    engine.register(Box::new(foundry_blocks::blocks::CheckCharter::new(registry.clone())));
    test_helpers::register_gate_scaffold(
        &mut engine,
        Arc::new(foundry_blocks::gateway::ProcessShellGateway),
        registry.clone(),
    );
    engine.register(Box::new(foundry_blocks::blocks::DirectPrompt));
    register_ledger_blocks(&mut engine, ledger, &registry);
    engine.register(Box::new(foundry_blocks::blocks::ExecutePlan::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::ReviewTask::new(agent, registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::FinalizeTask::new(registry)));
    engine
}

#[tokio::test]
async fn resume_nonlanding_results_and_pre_agent_failure_keep_parent_open() {
    for (verdict, expected, mode) in [
        (r#"{"verdict":"complete"}"#, WorkItemState::Landed, "no-deliverable"),
        (r#"{"verdict":"complete"}"#, WorkItemState::Preserved, "dirty-trunk"),
        (
            r#"{"verdict":"defect","diagnosis":"faulty approach"}"#,
            WorkItemState::Preserved,
            "normal",
        ),
        (
            r#"{"verdict":"blocked_on_decision","finding":"policy","options":["ask owner"]}"#,
            WorkItemState::NeedsDecision,
            "normal",
        ),
        ("agent failure", WorkItemState::Failed, "normal"),
        (r#"{"verdict":"complete"}"#, WorkItemState::Failed, "no-charter"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let checkout = task_project(dir.path());
        let parent = preserved_parent(&checkout);
        if mode == "no-deliverable" {
            assert!(git_ok(Some(&checkout), &["merge", "--ff-only", "preserved-work"]));
        } else if mode == "dirty-trunk" {
            std::fs::write(checkout.join("dirty.txt"), "owner work").unwrap();
        } else if mode == "no-charter" {
            std::fs::remove_file(checkout.join("CHARTER.md")).unwrap();
        }
        let ledger = dir.path().join("work-items.json");
        WorkItemStore {
            version: 1,
            items: vec![parent.clone()],
        }
        .save(&ledger)
        .unwrap();
        let registry =
            test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
        let agent = Arc::new(ContinuationAgent {
            parent: parent.clone(),
            ledger: ledger.clone(),
            calls: Mutex::new(0),
            verdict,
        });
        let engine = continuation_engine(agent.clone(), registry.clone(), &ledger);
        let (mut client, server, mut events) = resume_service(dir.path(), registry, engine).await;
        let child = client
            .resume_work_item(crate::proto::ResumeWorkItemRequest {
                id: parent.id.clone(),
                operator_origin: "host desk".to_string(),
            })
            .await
            .unwrap()
            .into_inner()
            .item
            .unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let event = tokio::time::timeout_at(deadline, events.recv()).await.unwrap().unwrap();
            if event.event_type == EventType::WorkItemSettled
                && event.payload["item_id"] == child.id
            {
                assert_eq!(
                    event.payload["state"],
                    expected.tag(),
                    "mode {mode}, verdict {verdict}"
                );
                break;
            }
        }
        let store = WorkItemStore::load(&ledger).unwrap();
        assert_eq!(store.find(&parent.id), Some(&parent));
        assert_eq!(store.find(&child.id).unwrap().state, expected);
        assert_eq!(store.find(&child.id).unwrap().resumes.as_ref(), Some(&parent.id));
        assert!(
            foundry_sdk::work_item_events::read_work_item_events(
                &dir.path().join("events"),
                &parent.id
            )
            .unwrap()
            .is_empty()
        );
        let logged = foundry_sdk::work_item_events::read_work_item_events(
            &dir.path().join("events"),
            &child.id,
        )
        .unwrap();
        assert_eq!(logged.last().unwrap().payload.state, expected);
        assert_eq!(logged.last().unwrap().payload.resumes.as_ref(), Some(&parent.id));
        if mode == "no-charter" {
            assert_eq!(*agent.calls.lock().unwrap(), 0);
        }
        server.abort();
    }
}

struct HeldContinuationAgent {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
    held: std::sync::atomic::AtomicBool,
    fail_save: Option<PathBuf>,
}

impl AgentGateway for HeldContinuationAgent {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            if request.access == foundry_sdk::gateway::AgentAccess::Full {
                if !self.held.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    assert_eq!(
                        std::fs::read_to_string(request.working_dir.join("preserved.txt"))?,
                        "preserved change"
                    );
                    self.started.notify_one();
                    self.release.notified().await;
                }
                Ok(AgentResponse::success("Implemented"))
            } else {
                if let Some(path) = &self.fail_save {
                    std::fs::create_dir(path)?;
                }
                Ok(AgentResponse::success(
                    r#"```json
{"verdict":"complete"}
```"#,
                ))
            }
        })
    }
}

async fn next_settled(events: &mut tokio::sync::broadcast::Receiver<Event>, id: &str) -> Event {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv()).await.unwrap().unwrap();
        if event.event_type == EventType::WorkItemSettled && event.payload["item_id"] == id {
            return event;
        }
    }
}

#[tokio::test]
async fn resume_owner_cancel_before_landing_and_unrelated_settlement_preserve_exact_records_and_history()
 {
    let dir = tempfile::tempdir().unwrap();
    let checkout = task_project(dir.path());
    let parent = preserved_parent(&checkout);
    let ledger = dir.path().join("work-items.json");
    WorkItemStore {
        version: 1,
        items: vec![parent.clone()],
    }
    .save(&ledger)
    .unwrap();
    let writer = foundry_engine::event_writer::EventWriter::new(dir.path().join("events"));
    let previous = Event::new(
        EventType::WorkItemSettled,
        parent.project.clone(),
        Throttle::Full,
        Event::serialize_payload(&foundry_sdk::payload::WorkItemEventPayload::from_item(&parent))
            .unwrap(),
    )
    .with_trace_id(parent.trace_id.clone());
    writer.write(&previous).unwrap();
    let log_path = dir
        .path()
        .join("events")
        .join(format!("{}.jsonl", chrono::Utc::now().format("%Y-%m")));
    let history = std::fs::read(&log_path).unwrap();
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let agent = Arc::new(HeldContinuationAgent {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        held: std::sync::atomic::AtomicBool::new(false),
        fail_save: None,
    });
    let engine = continuation_engine(agent.clone(), registry.clone(), &ledger);
    let (mut client, server, mut events) = resume_service(dir.path(), registry, engine).await;
    let child = client
        .resume_work_item(crate::proto::ResumeWorkItemRequest {
            id: parent.id.clone(),
            operator_origin: "host desk".to_string(),
        })
        .await
        .unwrap()
        .into_inner()
        .item
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), agent.started.notified())
        .await
        .unwrap();
    let unrelated_trace = mint_trace_id();
    client.emit(crate::proto::EmitRequest { event_type: "execution_requested".to_string(), project: parent.project.clone(), throttle: 0,
        payload_json: serde_json::json!({"project": parent.project, "workflow": "task", "prompt": "Unrelated objective"}).to_string(),
        trace_id: unrelated_trace.clone(), span_id: String::new(), parent_span_id: String::new() }).await.unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let unrelated = loop {
        let event = tokio::time::timeout_at(deadline, events.recv()).await.unwrap().unwrap();
        if event.event_type == EventType::WorkItemSettled
            && event.trace_id.as_ref() == Some(&unrelated_trace)
        {
            break event.payload["item_id"].as_str().unwrap().to_string();
        }
    };
    let sibling = WorkItemStore::load(&ledger).unwrap().find(&unrelated).unwrap().clone();
    assert_ne!(sibling.id, child.id);
    assert_ne!(sibling.id, parent.id);
    assert_eq!(sibling.objective, "Unrelated objective");
    assert_eq!(sibling.state, WorkItemState::Landed);
    client
        .close_work_item(crate::proto::CloseWorkItemRequest {
            id: parent.id.clone(),
            reason: "owner stopped".to_string(),
            operator_origin: "host desk".to_string(),
        })
        .await
        .unwrap();
    let cancelled = WorkItemStore::load(&ledger).unwrap().find(&parent.id).unwrap().clone();
    agent.release.notify_one();
    let event = next_settled(&mut events, &child.id).await;
    assert_eq!(event.payload["state"], "landed");
    assert!(event.payload["disposition"]["landed_commit"].as_str().is_some());
    let store = WorkItemStore::load(&ledger).unwrap();
    assert_eq!(store.find(&parent.id), Some(&cancelled));
    assert_eq!(store.find(&unrelated), Some(&sibling));
    assert_eq!(store.find(&child.id).unwrap().resumes.as_ref(), Some(&parent.id));
    let log = std::fs::read(&log_path).unwrap();
    assert!(log.starts_with(&history));
    let recorded = foundry_sdk::work_item_events::read_work_item_events(
        &dir.path().join("events"),
        &parent.id,
    )
    .unwrap();
    assert_eq!(recorded[0].event_id, previous.id);
    assert_eq!(recorded[1].payload.state, WorkItemState::Cancelled);
    assert_eq!(recorded.len(), 2);
    server.abort();
}

fn preservation_source(checkout: &Path, dir: &Path, source: &str) -> String {
    if source == "bundle" {
        let bundle = dir.join("preserved.bundle");
        assert!(git_ok(
            Some(checkout),
            &[
                "bundle",
                "create",
                bundle.to_str().unwrap(),
                "preserved-work"
            ]
        ));
        format!("bundle:{}", bundle.display())
    } else {
        if source == "remote" {
            assert!(git_ok(Some(checkout), &["push", "origin", "preserved-work"]));
            assert!(git_ok(Some(checkout), &["branch", "-D", "preserved-work"]));
        }
        "preserved-work".to_string()
    }
}

async fn assert_watch_matches(
    mut watch: tonic::Streaming<crate::proto::WatchResponse>,
    observed: &[Event],
) {
    for expected in observed {
        let event = tokio::time::timeout(std::time::Duration::from_secs(30), watch.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(event.event_id, expected.id);
        assert_eq!(event.trace_id, expected.trace_id.clone().unwrap_or_default());
        assert_eq!(event.project, expected.project);
        assert_eq!(event.event_type, expected.event_type.as_str());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&event.payload_json).unwrap(),
            expected.payload
        );
    }
}

#[tokio::test]
async fn resume_settlement_save_failure_emits_no_false_child_or_parent_settlement() {
    let dir = tempfile::tempdir().unwrap();
    let checkout = task_project(dir.path());
    let parent = preserved_parent(&checkout);
    let ledger = dir.path().join("work-items.json");
    WorkItemStore {
        version: 1,
        items: vec![parent.clone()],
    }
    .save(&ledger)
    .unwrap();
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let agent = Arc::new(HeldContinuationAgent {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        held: std::sync::atomic::AtomicBool::new(false),
        fail_save: Some(ledger.with_extension("json.tmp")),
    });
    let engine = continuation_engine(agent.clone(), registry.clone(), &ledger);
    let (mut client, server, mut events) = resume_service(dir.path(), registry, engine).await;
    let child = client
        .resume_work_item(crate::proto::ResumeWorkItemRequest {
            id: parent.id.clone(),
            operator_origin: "host desk".to_string(),
        })
        .await
        .unwrap()
        .into_inner()
        .item
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), agent.started.notified())
        .await
        .unwrap();
    let admitted_bytes = std::fs::read(&ledger).unwrap();
    agent.release.notify_one();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let terminal = loop {
        let event = tokio::time::timeout_at(deadline, events.recv()).await.unwrap().unwrap();
        assert_ne!(event.event_type, EventType::WorkItemSettled);
        if event.event_type == EventType::TaskRunCompleted {
            break event;
        }
    };
    assert_eq!(terminal.payload["landed"], true);
    loop {
        let status = client
            .status(crate::proto::StatusRequest {
                workflow_id: String::new(),
            })
            .await
            .unwrap()
            .into_inner();
        if status.workflows.is_empty() {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let store = WorkItemStore::load(&ledger).unwrap();
    assert_eq!(std::fs::read(&ledger).unwrap(), admitted_bytes);
    assert_eq!(store.find(&parent.id), Some(&parent));
    assert_eq!(store.find(&child.id).unwrap().state, WorkItemState::Running);
    assert!(
        foundry_sdk::work_item_events::read_work_item_events(
            &dir.path().join("events"),
            &parent.id
        )
        .unwrap()
        .is_empty()
    );
    let child_events =
        foundry_sdk::work_item_events::read_work_item_events(&dir.path().join("events"), &child.id)
            .unwrap();
    assert_eq!(child_events[0].payload.item_id, child.id);
    assert_eq!(child_events[1].payload.state, WorkItemState::Running);
    assert_eq!(child_events.len(), 2);
    while let Ok(event) = events.try_recv() {
        assert_ne!(event.event_type, EventType::WorkItemSettled);
    }
    server.abort();
}

async fn assert_terminal_redelivery_is_inert(ledger: &Path, observed: &[Event]) {
    let terminal = observed
        .iter()
        .find(|e| e.event_type == EventType::TaskRunCompleted)
        .unwrap()
        .clone();
    let before = std::fs::read(ledger).unwrap();
    let settle = foundry_blocks::blocks::SettleWorkItem::new(ledger.to_path_buf());
    assert!(settle.execute(&terminal).await.unwrap().events.is_empty());
    assert_eq!(std::fs::read(ledger).unwrap(), before);
}

fn assert_lifecycle_log_matches(events_dir: &Path, id: &str, observed: &[Event]) {
    let logged = foundry_sdk::work_item_events::read_work_item_events(events_dir, id).unwrap();
    let expected: Vec<_> = observed
        .iter()
        .filter(|event| {
            foundry_sdk::work_item_events::is_work_item_event(&event.event_type)
                && event.payload["item_id"] == id
        })
        .collect();
    assert_eq!(logged.len(), expected.len());
    for (record, event) in logged.iter().zip(expected) {
        assert_eq!(record.event_id, event.id);
        assert_eq!(record.trace_id, event.trace_id);
        assert_eq!(
            record.payload,
            event.parse_payload::<foundry_sdk::payload::WorkItemEventPayload>().unwrap()
        );
    }
}

/// Break a real persistence destination after an earlier lifecycle append succeeds.
struct BreakResumeLog {
    events: PathBuf,
    ledger: Option<PathBuf>,
}

impl foundry_sdk::task_block::TaskBlock for BreakResumeLog {
    fn name(&self) -> &'static str {
        "BreakResumeLog"
    }
    fn kind(&self) -> foundry_sdk::task_block::BlockKind {
        foundry_sdk::task_block::BlockKind::Observer
    }
    fn sinks_on(&self) -> &[EventType] {
        if self.ledger.is_some() {
            &[EventType::WorkItemStarted]
        } else {
            &[EventType::WorkItemSubmitted]
        }
    }
    fn execute(
        &self,
        _: &Event,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = anyhow::Result<foundry_sdk::task_block::TaskBlockResult>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(async move {
            if let Some(ledger) = &self.ledger {
                std::fs::create_dir(ledger.with_extension("json.tmp"))?;
            } else {
                let month = chrono::Utc::now().format("%Y-%m.jsonl").to_string();
                std::fs::rename(self.events.join(&month), self.events.join("retained.jsonl"))?;
                std::fs::create_dir(self.events.join(month))?;
            }
            Ok(foundry_sdk::task_block::TaskBlockResult::success(
                "destination now fails",
                vec![],
            ))
        })
    }
}

#[tokio::test]
async fn resume_generated_client_rejects_first_and_partial_lifecycle_persistence_failure() {
    for failure in ["first", "partial", "final-ledger-save"] {
        let partial = failure == "partial";
        let final_save = failure == "final-ledger-save";
        let dir = tempfile::tempdir().unwrap();
        let checkout = task_project(dir.path());
        let parent = preserved_parent(&checkout);
        let mut sibling = parent.clone();
        sibling.id = "wi_unrelated_persistence_sibling".to_string();
        let ledger = dir.path().join("work-items.json");
        WorkItemStore {
            version: 1,
            items: vec![parent.clone(), sibling.clone()],
        }
        .save(&ledger)
        .unwrap();
        let events_dir = dir.path().join("events");
        std::fs::create_dir(&events_dir).unwrap();
        let history = events_dir.join("2000-01.jsonl");
        let mut prior_event = Event::new(
            EventType::WorkItemSettled,
            parent.project.clone(),
            Throttle::Full,
            Event::serialize_payload(&foundry_sdk::payload::WorkItemEventPayload::from_item(
                &parent,
            ))
            .unwrap(),
        )
        .with_trace_id(parent.trace_id.clone());
        prior_event.occurred_at = "2000-01-01T00:00:00Z".parse().unwrap();
        foundry_engine::event_writer::EventWriter::new(&events_dir)
            .write(&prior_event)
            .unwrap();
        let prior = std::fs::read(&history).unwrap();
        if failure == "first" {
            std::fs::create_dir(
                events_dir.join(chrono::Utc::now().format("%Y-%m.jsonl").to_string()),
            )
            .unwrap();
        }
        let registry =
            test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
        let agent = Arc::new(ContinuationAgent {
            parent: parent.clone(),
            ledger: ledger.clone(),
            calls: Mutex::new(0),
            verdict: r#"{"verdict":"complete"}"#,
        });
        let mut engine = continuation_engine(agent.clone(), registry.clone(), &ledger);
        if partial || final_save {
            engine.register(Box::new(BreakResumeLog {
                events: events_dir.clone(),
                ledger: final_save.then(|| ledger.clone()),
            }));
        }
        let (mut client, server, mut events, tracker) =
            resume_service_tracked(dir.path(), registry, engine).await;
        let watch = client
            .watch(crate::proto::WatchRequest {
                project: parent.project.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        let error = client
            .resume_work_item(crate::proto::ResumeWorkItemRequest {
                id: parent.id.clone(),
                operator_origin: "owner-host (persistence regression)".to_string(),
            })
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::Internal);
        // spawn_workflow tracks before spawning and publishes its root before
        // removing tracking. Together, an empty tracker and no dispatch in
        // the broadcaster prove no execution was admitted after this RPC.
        assert!(tracker.list().is_empty());
        assert_eq!(*agent.calls.lock().unwrap(), 0);
        let store = WorkItemStore::load(&ledger).unwrap();
        let child = assert_rejected_resume_record(&store, &parent, &sibling);
        assert_eq!(std::fs::read(history).unwrap(), prior);
        // The receiver sees all synchronous admission output before the RPC returns.
        let mut observed = Vec::new();
        while let Ok(event) = events.try_recv() {
            observed.push(event);
        }
        assert!(observed.iter().all(|event| event.event_type != EventType::ExecutionRequested));
        let lifecycle: Vec<_> = observed
            .iter()
            .filter(|e| {
                matches!(e.event_type, EventType::WorkItemSubmitted | EventType::WorkItemStarted)
            })
            .collect();
        assert_rejected_resume_history(child, &parent, &events_dir, failure, &lifecycle, &observed);
        assert_watch_through_barrier(&mut client, &parent.project, watch, &lifecycle).await;
        server.abort();
    }
}

fn assert_resume_admission(
    store: &WorkItemStore,
    parent: &WorkItem,
    child: &crate::proto::WorkItem,
    observed: &[Event],
) {
    let dispatch = observed
        .iter()
        .find(|event| event.event_type == EventType::ExecutionRequested)
        .unwrap();
    assert_eq!(dispatch.payload["admitted_work_item_id"], child.id);
    assert_eq!(dispatch.trace_id, child.trace_id);
    let admission: Vec<_> = observed
        .iter()
        .filter(|event| {
            event.payload["item_id"] == child.id
                && matches!(
                    event.event_type,
                    EventType::WorkItemSubmitted | EventType::WorkItemStarted
                )
        })
        .collect();
    assert_eq!(
        admission.iter().map(|event| event.event_type.clone()).collect::<Vec<_>>(),
        vec![EventType::WorkItemSubmitted, EventType::WorkItemStarted]
    );
    for event in admission {
        assert_eq!(event.trace_id, child.trace_id);
        assert_eq!(event.payload["resumes"], parent.id);
        assert_eq!(event.payload["objective"], parent.objective);
        assert_eq!(event.payload["origin"], parent.origin);
        assert_eq!(event.payload["operator_action"]["command"], "resume");
        assert_eq!(event.payload["operator_action"]["origin"], "owner-host (finish it)");
        assert_eq!(event.payload["operator_action"]["previous_reason"], parent.reason);
    }
    assert_eq!(
        store
            .items
            .iter()
            .filter(|item| item.resumes.as_deref() == Some(&parent.id))
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec![child.id.as_str()]
    );
}

fn assert_rejected_resume_record<'a>(
    store: &'a WorkItemStore,
    parent: &WorkItem,
    sibling: &WorkItem,
) -> &'a WorkItem {
    assert_eq!(store.find(&parent.id), Some(parent));
    assert_eq!(store.find(&sibling.id), Some(sibling));
    let children: Vec<_> = store
        .items
        .iter()
        .filter(|i| i.resumes.as_deref() == Some(parent.id.as_str()))
        .collect();
    assert_eq!(children.len(), 1);
    let child = children[0];
    assert_eq!(
        store.items.iter().map(|item| item.id.as_str()).collect::<Vec<_>>(),
        vec![parent.id.as_str(), sibling.id.as_str(), child.id.as_str()]
    );
    assert_eq!(child.state, WorkItemState::Failed);
    assert_eq!(child.reason, "resume admission incomplete; execution not dispatched");
    assert!(child.started_at.is_none());
    assert!(child.settled_at.is_some());
    assert_eq!(child.objective, parent.objective);
    assert_eq!(child.origin, parent.origin);
    assert_eq!(child.project, parent.project);
    assert_eq!(child.kind, WorkItemKind::Task);
    assert_eq!(child.lane, WorkLane::Interactive);
    assert!(child.disposition.is_none());
    assert_ne!(child.trace_id, parent.trace_id);
    assert_eq!(child.trace_id.as_ref().unwrap().len(), 32);
    let operator = child.operator_action.as_ref().unwrap();
    assert_eq!(operator.command, "resume");
    assert_eq!(operator.previous_state, parent.state);
    assert_eq!(operator.previous_reason, parent.reason);
    assert_eq!(operator.previous_settled_at, parent.settled_at);
    assert_eq!(
        child.operator_action.as_ref().unwrap().origin,
        "owner-host (persistence regression)"
    );
    child
}

async fn assert_watch_through_barrier(
    client: &mut crate::proto::foundry_client::FoundryClient<tonic::transport::Channel>,
    project: &str,
    mut watch: tonic::Streaming<crate::proto::WatchResponse>,
    lifecycle: &[&Event],
) {
    // Emit a harmless root through the production RPC, whose Watch
    // publication establishes a bounded stream barrier.
    client
        .emit(crate::proto::EmitRequest {
            event_type: "resume_test_barrier".to_string(),
            project: project.to_string(),
            payload_json: "{}".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut watched = Vec::new();
    loop {
        let event = tokio::time::timeout_at(deadline, watch.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if event.event_type == "resume_test_barrier" {
            break;
        }
        if event.event_type.starts_with("work_item_") {
            watched.push(event);
        }
    }
    assert_eq!(watched.len(), lifecycle.len());
    for (actual, expected) in watched.iter().zip(lifecycle.iter()) {
        assert_eq!(actual.event_id, expected.id);
        assert_eq!(actual.trace_id, expected.trace_id.clone().unwrap());
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&actual.payload_json).unwrap(),
            expected.payload
        );
    }
}

fn assert_rejected_resume_history(
    child: &WorkItem,
    parent: &WorkItem,
    events_dir: &Path,
    failure: &str,
    lifecycle: &[&Event],
    observed: &[Event],
) {
    if failure == "partial" {
        assert_eq!(lifecycle.len(), 1);
        let submitted = lifecycle[0];
        assert_eq!(submitted.event_type, EventType::WorkItemSubmitted);
        assert_eq!(submitted.payload["item_id"], child.id);
        assert_eq!(submitted.payload["resumes"], parent.id);
        assert_eq!(submitted.trace_id, child.trace_id);
        let bytes = std::fs::read_to_string(events_dir.join("retained.jsonl")).unwrap();
        let durable: Event = serde_json::from_str(bytes.lines().next().unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(durable).unwrap(),
            serde_json::to_value(submitted).unwrap()
        );
    } else if failure == "final-ledger-save" {
        assert_eq!(
            lifecycle.iter().map(|event| event.event_type.clone()).collect::<Vec<_>>(),
            vec![EventType::WorkItemSubmitted, EventType::WorkItemStarted]
        );
        assert_lifecycle_log_matches(events_dir, &child.id, observed);
        for event in lifecycle {
            assert_eq!(event.payload["item_id"], child.id);
            assert_eq!(event.payload["resumes"], parent.id);
            assert_eq!(event.trace_id, child.trace_id);
        }
    } else {
        assert!(lifecycle.is_empty());
    }
}
