//! Integration tests for the work-item ledger's *run-shaped* coverage: the
//! per-project maintenance run, the release, and the remediation.
//!
//! The claim under test is correlation. These three kinds nest and fan out —
//! a cycle puts one maintenance item per project on a single trace, and one
//! per-project run can hold a maintenance item, a remediation item and a
//! release item at once on that same trace *and* project. Every test here
//! therefore asserts by item id, never by count alone: "the item its own root
//! opened settled, and the others did not".

use std::path::Path;
use std::sync::{Arc, RwLock};

use foundry_sdk::event::{Event, EventType, mint_trace_id};
use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{WorkItem, WorkItemKind, WorkItemState, WorkItemStore, WorkLane};

use foundry_engine::engine::Engine;

use super::test_helpers;
use super::work_ledger_test::LedgerReadingAgent;

const GATHER: &str = "gth_cycle";

/// A registry of `names`, every project active and release-enabled.
fn registry_of(names: &[&str], path: &str) -> Arc<RwLock<Registry>> {
    Arc::new(RwLock::new(Registry {
        version: 2,
        projects: names
            .iter()
            .map(|name| ProjectEntry {
                name: (*name).to_string(),
                path: path.to_string(),
                stack: Stack::Rust,
                agent: "claude".to_string(),
                repo: String::new(),
                branch: "main".to_string(),
                skip: None,
                notes: None,
                actions: ActionFlags {
                    release: true,
                    ..ActionFlags::default()
                },
                install: None,
                installs_skill: None,
                timeout_secs: None,
                audit_exceptions: Vec::new(),
                update_policy: None,
            })
            .collect(),
    }))
}

/// The two run-ledger blocks, registered exactly as `foundryd` registers them.
fn register_run_ledger(engine: &mut Engine, store_path: &Path, registry: &Arc<RwLock<Registry>>) {
    engine.register(Box::new(foundry_blocks::blocks::RecordRunWorkItem::new(
        store_path.to_path_buf(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::SettleRunWorkItem::new(
        store_path.to_path_buf(),
    )));
}

fn ledger(path: &Path) -> Vec<WorkItem> {
    WorkItemStore::load(path).unwrap().items
}

fn find<'a>(items: &'a [WorkItem], id: &str) -> &'a WorkItem {
    items
        .iter()
        .find(|item| item.id == id)
        .expect("the item must still be in the ledger")
}

fn only_of_kind(items: &[WorkItem], kind: WorkItemKind, project: &str) -> WorkItem {
    let mut matching = items
        .iter()
        .filter(|item| item.kind == kind && item.project == project)
        .cloned();
    let first = matching.next().unwrap_or_else(|| panic!("no {kind:?} item for {project}"));
    assert!(matching.next().is_none(), "expected exactly one {kind:?} item for {project}");
    first
}

/// An event on `trace`, inside the cycle's fan-out.
fn in_cycle(
    event_type: EventType,
    project: &str,
    trace: &str,
    payload: serde_json::Value,
) -> Event {
    Event::new(event_type, project.to_string(), Throttle::Full, payload)
        .with_trace_id(Some(trace.to_string()))
        .with_gather_id(Some(GATHER.to_string()))
}

fn dirty_audit(project: &str, trace: &str) -> Event {
    in_cycle(
        EventType::MainBranchAudited,
        project,
        trace,
        serde_json::json!({"project": project, "cve": "CVE-2026-9", "vulnerable": true, "dirty": true}),
    )
}

fn clean_audit(project: &str, trace: &str) -> Event {
    in_cycle(
        EventType::MainBranchAudited,
        project,
        trace,
        serde_json::json!({"project": project, "cve": "CVE-2026-9", "vulnerable": true, "dirty": false}),
    )
}

// --- fan-out ---------------------------------------------------------------

/// A cycle's scatter opens one maintenance item per active project, all on the
/// cycle's one trace, and each project's own terminal settles only its own.
#[tokio::test]
async fn a_two_project_cycle_records_one_maintenance_item_per_project_on_one_trace() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let registry = registry_of(&["alpha", "beta"], dir.path().to_str().unwrap());

    // A real agent gateway, wired into a real agent block, that records the
    // ledger every time it is asked to run anything.
    let agent = LedgerReadingAgent::new(store_path.clone(), vec!["never reached"]);

    let mut engine = Engine::new();
    register_run_ledger(&mut engine, &store_path, &registry);
    engine.register(Box::new(crate::orchestrator::FanOutMaintenance::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::ExecuteMaintain::new(
        agent.clone(),
        registry.clone(),
    )));

    let trace = mint_trace_id();
    let cycle = Event::new(
        EventType::MaintenanceCycleStarted,
        "system".to_string(),
        Throttle::Full,
        serde_json::json!({"project_count": 2}),
    )
    .with_trace_id(Some(trace.clone()));
    engine.process(cycle).await;

    let items = ledger(&store_path);
    let alpha = only_of_kind(&items, WorkItemKind::Maintenance, "alpha");
    let beta = only_of_kind(&items, WorkItemKind::Maintenance, "beta");
    assert_ne!(alpha.id, beta.id, "each project's run is its own unit of work");
    assert_eq!(alpha.state, WorkItemState::Running);
    assert_eq!(beta.state, WorkItemState::Running);
    assert_eq!(alpha.lane, WorkLane::Maintenance);
    assert_eq!(alpha.trace_id.as_deref(), Some(trace.as_str()));
    assert_eq!(beta.trace_id.as_deref(), Some(trace.as_str()));
    assert!(!alpha.origin.is_empty(), "the maintenance cycle must be named as the submitter");
    assert_eq!(alpha.origin, beta.origin);
    assert!(
        agent.observed.lock().unwrap().is_empty(),
        "both items must be in the ledger before any agent is invoked"
    );

    // beta finishes first: its terminal settles beta's item and nothing else.
    let mut engine = Engine::new();
    register_run_ledger(&mut engine, &store_path, &registry);
    engine
        .process(in_cycle(
            EventType::ProjectRunCompleted,
            "beta",
            &trace,
            serde_json::json!({"success": true}),
        ))
        .await;

    let items = ledger(&store_path);
    assert_eq!(find(&items, &beta.id).state, WorkItemState::Landed);
    assert_eq!(find(&items, &beta.id).reason, "maintenance run completed");
    assert_eq!(
        find(&items, &alpha.id).state,
        WorkItemState::Running,
        "a sibling's terminal must not settle another project's run"
    );

    // alpha then fails, and says so in one line.
    engine
        .process(in_cycle(
            EventType::ProjectRunCompleted,
            "alpha",
            &trace,
            serde_json::json!({"success": false}),
        ))
        .await;
    let items = ledger(&store_path);
    assert_eq!(find(&items, &alpha.id).state, WorkItemState::Failed);
    assert!(
        !find(&items, &alpha.id).reason.is_empty()
            && !find(&items, &alpha.id).reason.contains('\n'),
        "a failed run records a one-line reason"
    );
    assert_eq!(find(&items, &beta.id).state, WorkItemState::Landed, "beta is untouched");
}

// --- nesting ---------------------------------------------------------------

/// A per-project run that remediates holds two items at once, on one trace and
/// one project. Each settles from its own terminal.
#[tokio::test]
async fn a_remediation_nested_in_a_run_settles_without_touching_the_runs_item() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let registry = registry_of(&["alpha"], dir.path().to_str().unwrap());
    let trace = mint_trace_id();

    let mut engine = Engine::new();
    register_run_ledger(&mut engine, &store_path, &registry);

    engine
        .process(in_cycle(EventType::ProjectRunStarted, "alpha", &trace, serde_json::json!({})))
        .await;
    engine.process(dirty_audit("alpha", &trace)).await;

    let items = ledger(&store_path);
    let run = only_of_kind(&items, WorkItemKind::Maintenance, "alpha");
    let remediation = only_of_kind(&items, WorkItemKind::Remediation, "alpha");
    assert_eq!(run.state, WorkItemState::Running);
    assert_eq!(remediation.state, WorkItemState::Running);
    assert_eq!(run.trace_id, remediation.trace_id, "both are on the run's one trace");
    assert_eq!(remediation.lane, WorkLane::Maintenance);

    engine
        .process(in_cycle(
            EventType::RemediationCompleted,
            "alpha",
            &trace,
            serde_json::json!({"cve": "CVE-2026-9", "success": true, "summary": "upgraded openssl"}),
        ))
        .await;
    let items = ledger(&store_path);
    assert_eq!(find(&items, &remediation.id).state, WorkItemState::Landed);
    assert_eq!(find(&items, &remediation.id).reason, "upgraded openssl");
    assert_eq!(
        find(&items, &run.id).state,
        WorkItemState::Running,
        "the run that contains the remediation is still under way"
    );

    engine
        .process(in_cycle(
            EventType::ProjectRunCompleted,
            "alpha",
            &trace,
            serde_json::json!({"success": true}),
        ))
        .await;
    assert_eq!(find(&ledger(&store_path), &run.id).state, WorkItemState::Landed);
}

/// A remediation Foundry stopped for review is failed, and the ledger records
/// the review text verbatim — that is what a person has to read.
#[tokio::test]
async fn a_remediation_that_needs_review_settles_failed_with_that_exact_text() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let registry = registry_of(&["alpha"], dir.path().to_str().unwrap());
    let trace = mint_trace_id();
    let review = "the agent added an advisory suppression instead of upgrading";

    let mut engine = Engine::new();
    register_run_ledger(&mut engine, &store_path, &registry);
    engine.process(dirty_audit("alpha", &trace)).await;
    let remediation = only_of_kind(&ledger(&store_path), WorkItemKind::Remediation, "alpha");

    engine
        .process(in_cycle(
            EventType::RemediationCompleted,
            "alpha",
            &trace,
            serde_json::json!({
                "cve": "CVE-2026-9",
                "success": false,
                "summary": "a summary that must not win",
                "needs_review": review,
            }),
        ))
        .await;

    let settled = find(&ledger(&store_path), &remediation.id).clone();
    assert_eq!(settled.state, WorkItemState::Failed);
    assert_eq!(settled.reason, review);
}

// --- automatic release -----------------------------------------------------

/// A clean main branch inside a run cuts a release; that release is its own
/// unit of work beside the run's, and only it settles from `ReleaseCompleted`.
#[tokio::test]
async fn an_automatic_release_is_its_own_item_beside_the_runs() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let registry = registry_of(&["alpha"], dir.path().to_str().unwrap());
    let trace = mint_trace_id();

    let mut engine = Engine::new();
    register_run_ledger(&mut engine, &store_path, &registry);
    engine
        .process(in_cycle(EventType::ProjectRunStarted, "alpha", &trace, serde_json::json!({})))
        .await;
    engine.process(clean_audit("alpha", &trace)).await;

    let items = ledger(&store_path);
    let run = only_of_kind(&items, WorkItemKind::Maintenance, "alpha");
    let release = only_of_kind(&items, WorkItemKind::Release, "alpha");
    assert_eq!(release.lane, WorkLane::Maintenance);
    assert_eq!(release.state, WorkItemState::Running);
    assert!(
        !items.iter().any(|item| item.kind == WorkItemKind::Remediation),
        "a clean audit remediates nothing"
    );

    engine
        .process(in_cycle(
            EventType::ReleaseCompleted,
            "alpha",
            &trace,
            serde_json::json!({"cve": "CVE-2026-9", "release": "patch", "new_tag": "v2.0.1", "success": true}),
        ))
        .await;

    let items = ledger(&store_path);
    assert_eq!(find(&items, &release.id).state, WorkItemState::Landed);
    assert!(
        find(&items, &release.id).reason.contains("v2.0.1"),
        "the settlement must name the tag, got: {}",
        find(&items, &release.id).reason
    );
    assert_eq!(find(&items, &run.id).state, WorkItemState::Running);
}

// --- `foundry release` -----------------------------------------------------

/// The real manual release chain: the item is `running` in the file by the
/// time the release agent is first asked to do anything, and the downstream
/// observation events leave the settled record alone.
#[tokio::test]
async fn a_manual_release_is_running_before_the_agent_and_untouched_after_it_settles() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "# Release Process\n1. Run gates\n2. Tag")
        .unwrap();
    let store_path = dir.path().join("work-items.json");
    let registry = registry_of(&["alpha"], dir.path().to_str().unwrap());
    let agent = LedgerReadingAgent::new(store_path.clone(), vec!["Release done!\nv1.5.0\nPushed."]);

    let mut engine = Engine::new();
    register_run_ledger(&mut engine, &store_path, &registry);
    engine.register(Box::new(foundry_blocks::blocks::ExecuteRelease::new(
        agent.clone(),
        registry.clone(),
    )));
    engine.register(Box::new(foundry_blocks::blocks::WatchPipeline::new(registry.clone())));
    engine.register(Box::new(foundry_blocks::blocks::InstallLocally::new(registry.clone())));

    let trace = mint_trace_id();
    let requested = Event::new(
        EventType::ReleaseRequested,
        "alpha".to_string(),
        Throttle::Full,
        serde_json::json!({"bump": "minor"}),
    )
    .with_trace_id(Some(trace.clone()));
    let result = engine.process(requested).await;

    let at_first_call = agent.ledger_at_first_invocation();
    assert_eq!(at_first_call.len(), 1, "the release must be recorded before the agent runs");
    assert_eq!(at_first_call[0].kind, WorkItemKind::Release);
    assert_eq!(at_first_call[0].lane, WorkLane::Interactive);
    assert_eq!(at_first_call[0].origin, "foundry release");
    assert_eq!(at_first_call[0].state, WorkItemState::Running);

    for wanted in [
        EventType::ReleaseCompleted,
        EventType::ReleasePipelineCompleted,
        EventType::LocalInstallCompleted,
    ] {
        assert!(
            result.events.iter().any(|event| event.event_type == wanted),
            "the whole release chain must have run; {wanted:?} is missing"
        );
    }

    let settled = find(&ledger(&store_path), &at_first_call[0].id).clone();
    assert_eq!(settled.state, WorkItemState::Landed);
    assert!(
        settled.reason.contains("v1.5.0"),
        "the settlement must name the new tag, got: {}",
        settled.reason
    );

    // The downstream observation events must not touch the settled record.
    let bytes = std::fs::read(&store_path).unwrap();
    for event_type in [
        EventType::ReleasePipelineCompleted,
        EventType::LocalInstallCompleted,
    ] {
        engine
            .process(
                Event::new(
                    event_type,
                    "alpha".to_string(),
                    Throttle::Full,
                    serde_json::json!({"status": "completed", "success": true}),
                )
                .with_trace_id(Some(trace.clone())),
            )
            .await;
    }
    assert_eq!(
        std::fs::read(&store_path).unwrap(),
        bytes,
        "a settled release item must be byte-identical after downstream observation"
    );
}

// --- pipeline remediation --------------------------------------------------

#[tokio::test]
async fn a_failing_pipeline_records_a_remediation_and_a_passing_one_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let registry = registry_of(&["alpha"], dir.path().to_str().unwrap());

    let mut engine = Engine::new();
    register_run_ledger(&mut engine, &store_path, &registry);

    let passing = Event::new(
        EventType::PipelineChecked,
        "alpha".to_string(),
        Throttle::Full,
        serde_json::json!({"passing": true, "conclusion": "success"}),
    )
    .with_trace_id(Some(mint_trace_id()));
    let remediator = foundry_blocks::blocks::RemediatePipeline::new(
        test_helpers::sequenced_agent(vec!["fixed"]),
        registry.clone(),
    );
    assert!(
        !foundry_sdk::task_block::TaskBlock::accepts(&remediator, &passing),
        "the fixture must be one RemediatePipeline rejects"
    );
    engine.process(passing).await;
    assert!(ledger(&store_path).is_empty(), "a pipeline that passes is not work");

    let failing = Event::new(
        EventType::PipelineChecked,
        "alpha".to_string(),
        Throttle::Full,
        serde_json::json!({"passing": false, "conclusion": "failure", "run_name": "CI"}),
    )
    .with_trace_id(Some(mint_trace_id()));
    assert!(
        foundry_sdk::task_block::TaskBlock::accepts(&remediator, &failing),
        "the fixture must be one RemediatePipeline accepts"
    );
    engine.process(failing).await;

    let item = only_of_kind(&ledger(&store_path), WorkItemKind::Remediation, "alpha");
    assert_eq!(item.state, WorkItemState::Running);
    assert_eq!(item.lane, WorkLane::Interactive, "a CLI-rooted pipeline check is interactive");
    assert_eq!(item.origin, "pipeline remediation");
}

// --- dry run ---------------------------------------------------------------

#[tokio::test]
async fn no_new_root_records_anything_at_a_dry_run_throttle() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let registry = registry_of(&["alpha"], dir.path().to_str().unwrap());
    let trace = mint_trace_id();

    let mut engine = Engine::new();
    register_run_ledger(&mut engine, &store_path, &registry);

    let roots = [
        (EventType::ProjectRunStarted, serde_json::json!({})),
        (EventType::ReleaseRequested, serde_json::json!({"bump": "patch"})),
        (
            EventType::MainBranchAudited,
            serde_json::json!({"project": "alpha", "cve": "CVE-2026-9", "vulnerable": true, "dirty": true}),
        ),
        (
            EventType::MainBranchAudited,
            serde_json::json!({"project": "alpha", "cve": "CVE-2026-9", "vulnerable": true, "dirty": false}),
        ),
        (
            EventType::PipelineChecked,
            serde_json::json!({"passing": false, "conclusion": "failure"}),
        ),
    ];
    for (event_type, payload) in roots {
        engine
            .process(
                Event::new(event_type, "alpha".to_string(), Throttle::DryRun, payload)
                    .with_trace_id(Some(trace.clone()))
                    .with_gather_id(Some(GATHER.to_string())),
            )
            .await;
    }

    assert!(
        WorkItemStore::load(&store_path).unwrap().items.is_empty(),
        "a dry run dispatches nothing, so it records nothing"
    );
}

// --- events ----------------------------------------------------------------

/// A run-shaped item's three lifecycle events are engine events like any
/// other: they reach the Watch broadcast and the durable JSONL log.
#[tokio::test]
async fn a_maintenance_run_broadcasts_and_logs_its_three_work_item_events() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let events_dir = dir.path().join("events");
    let registry = registry_of(&["alpha"], dir.path().to_str().unwrap());
    let trace = mint_trace_id();

    let (tx, mut rx) = tokio::sync::broadcast::channel(256);
    let writer = Arc::new(foundry_engine::event_writer::EventWriter::new(events_dir.clone()));
    let mut engine = Engine::new().with_event_broadcaster(tx).with_event_writer(writer);
    register_run_ledger(&mut engine, &store_path, &registry);

    engine
        .process(in_cycle(EventType::ProjectRunStarted, "alpha", &trace, serde_json::json!({})))
        .await;
    engine
        .process(in_cycle(
            EventType::ProjectRunCompleted,
            "alpha",
            &trace,
            serde_json::json!({"success": true}),
        ))
        .await;

    let mut broadcast: Vec<Event> = Vec::new();
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
        ]
    );

    let item_id = ledger_events[0].payload["item_id"].as_str().unwrap().to_string();
    for event in &ledger_events {
        assert_eq!(event.payload["item_id"], item_id.as_str());
        assert_eq!(event.payload["project"], "alpha");
        assert_eq!(event.payload["kind"], "maintenance");
        assert_eq!(event.payload["lane"], "maintenance");
        assert_eq!(event.payload["origin"], "maintenance cycle");
        assert!(event.payload["state"].is_string());
        assert!(!event.payload["reason"].as_str().unwrap().is_empty());
    }
    assert_eq!(ledger_events[2].payload["state"], "landed");
    assert_eq!(ledger_events[2].payload["reason"], "maintenance run completed");

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

/// A task dispatch and a maintenance run can share a project; a task verdict
/// must never settle the maintenance item.
#[tokio::test]
async fn a_task_terminal_never_settles_a_run_shaped_item_sharing_its_trace() {
    let dir = tempfile::tempdir().unwrap();
    let store_path = dir.path().join("work-items.json");
    let registry = registry_of(&["alpha"], dir.path().to_str().unwrap());
    let trace = mint_trace_id();

    let mut engine = Engine::new();
    register_run_ledger(&mut engine, &store_path, &registry);
    engine.register(Box::new(foundry_blocks::blocks::SettleWorkItem::new(store_path.clone())));

    engine
        .process(in_cycle(EventType::ProjectRunStarted, "alpha", &trace, serde_json::json!({})))
        .await;
    let run = only_of_kind(&ledger(&store_path), WorkItemKind::Maintenance, "alpha");

    engine
        .process(in_cycle(
            EventType::TaskRunCompleted,
            "alpha",
            &trace,
            serde_json::json!({
                "project": "alpha",
                "verdict": "complete",
                "summary": "a task that is not this run",
                "landed": true,
                "context": {},
            }),
        ))
        .await;

    assert_eq!(
        find(&ledger(&store_path), &run.id).state,
        WorkItemState::Running,
        "a task verdict settles only task-shaped work"
    );
}
