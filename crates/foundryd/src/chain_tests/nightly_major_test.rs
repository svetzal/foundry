//! Nightly planning → service admission → real task execution and settlement.
use super::*;
use foundry_blocks::blocks::PlanMajorUpgrades;
use foundry_blocks::dependency_updates::majors::{Caps, objective};
use foundry_blocks::trace_writer::TraceWriter;
use foundry_sdk::payload::{
    ChainContext, ChangeKind, ClassificationPhase, DependencyBrief, DependencyClassification,
    DependencyUpdatesClassifiedPayload, Ecosystem, MajorUpgradeStatus, MajorUpgradesPlannedPayload,
    PlannedUpdate, UpdateClass,
};
use foundry_sdk::registry::UpdatePolicy;
use foundry_sdk::trace::ProcessResult;

fn major(package: &str) -> PlannedUpdate {
    PlannedUpdate {
        ecosystem: Ecosystem::Npm,
        manifest: ".".into(),
        package: package.into(),
        from: "1.0.0".into(),
        to: "2.0.0".into(),
        class: UpdateClass::Major,
        change: ChangeKind::Manifest,
        security: None,
        beyond_policy: false,
        beyond_hold: false,
    }
}

struct Nightly {
    dir: tempfile::TempDir,
    checkout: PathBuf,
    history: Event,
    parent: WorkItem,
    unrelated: Vec<WorkItem>,
    ctx: crate::service::RuntimeContext,
    events: tokio::sync::broadcast::Receiver<Event>,
    agent: Arc<NightlyAgent>,
    planner: PlanMajorUpgrades,
}

struct NightlyAgent {
    expected_commit: String,
    heads: Mutex<Vec<String>>,
    verdict: &'static str,
}

impl AgentGateway for NightlyAgent {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            if request.access == foundry_sdk::gateway::AgentAccess::Full {
                let head = Command::new("git")
                    .current_dir(&request.working_dir)
                    .args(["rev-parse", "HEAD"])
                    .output()?;
                assert!(head.status.success());
                let head = String::from_utf8(head.stdout)?.trim().to_string();
                let mut heads = self.heads.lock().unwrap();
                if heads.is_empty() {
                    assert_eq!(
                        head, self.expected_commit,
                        "execution must start on the preserved commit"
                    );
                    assert_eq!(
                        std::fs::read_to_string(request.working_dir.join("preserved.txt"))?,
                        "preserved change"
                    );
                    assert!(request.prompt.contains("Keep the original owner's evidence."));
                }
                heads.push(head);
                Ok(AgentResponse::success("Implemented"))
            } else {
                Ok(AgentResponse::success(format!("```json\n{}\n```", self.verdict)))
            }
        })
    }
}

fn near_matches(parent: &WorkItem) -> Vec<WorkItem> {
    let mut unrelated = Vec::new();
    for (id, project, package, target, state) in [
        ("other-project", "test-project-extra", "x", "2.0.0", WorkItemState::Preserved),
        ("other-package", "test-project", "xx", "2.0.0", WorkItemState::Preserved),
        ("other-target", "test-project", "x", "3.0.0", WorkItemState::Preserved),
        ("cancelled", "test-project", "x", "2.0.0", WorkItemState::Cancelled),
        ("landed", "test-project", "x", "2.0.0", WorkItemState::Landed),
        ("decision", "test-project", "x", "2.0.0", WorkItemState::NeedsDecision),
        ("failed", "test-project", "x", "2.0.0", WorkItemState::Failed),
        ("running", "test-project", "x", "2.0.0", WorkItemState::Running),
        ("queued", "test-project", "x", "2.0.0", WorkItemState::Queued),
        ("submitted", "test-project", "x", "2.0.0", WorkItemState::Submitted),
    ] {
        let mut item = parent.clone();
        item.id = id.into();
        item.project = project.into();
        let mut update = major(package);
        update.to = target.into();
        item.objective = objective(project, &update);
        item.state = state;
        item.trace_id = Some(mint_trace_id());
        unrelated.push(item);
    }
    unrelated
}

impl Nightly {
    fn new(verdict: &'static str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let checkout = task_project(dir.path());
        let mut parent = preserved_parent(&checkout);
        assert!(git_ok(Some(&checkout), &["push", "origin", "preserved-work"]));
        parent.objective =
            objective("test-project", &major("x")) + " Keep the original owner's evidence.";
        let unrelated = near_matches(&parent);
        let ledger = dir.path().join("work-items.json");
        let mut items = unrelated.clone();
        items.push(parent.clone());
        WorkItemStore { version: 1, items }.save(&ledger).unwrap();
        let expected = Command::new("git")
            .current_dir(&checkout)
            .args(["rev-parse", "preserved-work"])
            .output()
            .unwrap();
        let agent = Arc::new(NightlyAgent {
            expected_commit: String::from_utf8(expected.stdout).unwrap().trim().into(),
            heads: Mutex::new(Vec::new()),
            verdict,
        });
        let registry =
            test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
        let trace_writer = Arc::new(TraceWriter::new(dir.path().join("traces").to_str().unwrap()));
        let writer =
            Arc::new(foundry_engine::event_writer::EventWriter::new(dir.path().join("events")));
        // Actual prior task history would suppress x without the planner change.
        let history = Event::new(
            EventType::WorkItemSettled,
            parent.project.clone(),
            Throttle::Full,
            Event::serialize_payload(&foundry_sdk::payload::WorkItemEventPayload::from_item(
                &parent,
            ))
            .unwrap(),
        )
        .with_trace_id(parent.trace_id.clone());
        writer.write(&history).unwrap();
        for (event_type, payload) in [
            (
                EventType::TaskRunStarted,
                serde_json::json!({"project": parent.project, "objective": parent.objective}),
            ),
            (
                EventType::TaskRunCompleted,
                serde_json::json!({"project": parent.project, "success": false, "landed": false, "summary": "prior remainder", "verdict": "remainder", "gaps": [], "preservation_ref": "preserved-work"}),
            ),
        ] {
            writer
                .write(
                    &Event::new(event_type, parent.project.clone(), Throttle::Full, payload)
                        .with_trace_id(parent.trace_id.clone()),
                )
                .unwrap();
        }
        let (tx, events) = tokio::sync::broadcast::channel(256);
        let engine = continuation_engine(agent.clone(), registry.clone(), &ledger)
            .with_event_writer(writer)
            .with_event_broadcaster(tx.clone());
        let ctx = crate::service::RuntimeContext {
            engine: Arc::new(engine),
            trace_store: Arc::new(crate::trace_store::TraceStore::with_trace_writer(
                std::time::Duration::from_secs(60),
                trace_writer.clone(),
            )),
            workflow_tracker: Arc::new(crate::workflow_tracker::WorkflowTracker::new()),
            trace_writer: trace_writer.clone(),
            event_tx: tx,
            registry: registry.clone(),
        };
        let planner = PlanMajorUpgrades::with_config(
            trace_writer,
            registry,
            Arc::new(foundry_blocks::gateway::ProcessShellGateway),
            dir.path().join("events"),
            Caps {
                per_project: 2,
                per_night: 2,
            },
        )
        .with_work_items_path(ledger);
        Self {
            dir,
            checkout,
            history,
            parent,
            unrelated,
            ctx,
            events,
            agent,
            planner,
        }
    }

    async fn client(
        &self,
    ) -> (
        crate::proto::foundry_client::FoundryClient<tonic::transport::Channel>,
        tokio::task::JoinHandle<()>,
    ) {
        let service = crate::service::FoundryService::new(
            self.ctx.clone(),
            crate::service::StoreConfig {
                work_items_path: self.dir.path().join("work-items.json"),
                events_dir: self.dir.path().join("events"),
                campaigns_path: self.dir.path().join("campaigns.json"),
                registry_path: self.dir.path().join("registry.json"),
                sentinels: Arc::new(RwLock::new(
                    foundry_sdk::sentinel::SentinelStore::default_seed(),
                )),
                sentinels_path: self.dir.path().join("sentinels.json"),
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
        (
            crate::proto::foundry_client::FoundryClient::connect(addr).await.unwrap(),
            server,
        )
    }

    async fn watching_client(
        &self,
    ) -> (
        crate::proto::foundry_client::FoundryClient<tonic::transport::Channel>,
        tokio::task::JoinHandle<()>,
        tonic::Streaming<crate::proto::WatchResponse>,
    ) {
        let (mut client, server) = self.client().await;
        let watch = client
            .watch(crate::proto::WatchRequest {
                project: self.parent.project.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        (client, server, watch)
    }

    async fn plan(
        &self,
        throttle: Throttle,
        policy: UpdatePolicy,
        succeeded: bool,
    ) -> ProcessResult {
        let classified = DependencyUpdatesClassifiedPayload {
            project: self.parent.project.clone(),
            phase: ClassificationPhase::After,
            workflow: Some("maintain".into()),
            success: None,
            classification: DependencyClassification::default(),
            brief: DependencyBrief {
                policy,
                policy_set: true,
                apply: vec![],
                held_by_policy: vec![],
                held_by_hold: vec![],
                majors: vec![major("z"), major("y"), major("x")],
            },
            chain: ChainContext::default(),
        };
        self.ctx.trace_writer.write("maintenance", &ProcessResult {
            events: vec![Event::new(EventType::DependencyUpdatesClassified, self.parent.project.clone(), throttle, Event::serialize_payload(&classified).unwrap()),
                Event::new(EventType::ProjectMaintenanceCompleted, self.parent.project.clone(), throttle, serde_json::json!({"project": self.parent.project, "success": succeeded, "summary": "", "workflow": "maintain"}))],
            block_executions: vec![], total_duration_ms: 1,
        }).unwrap();
        let request = Event::new(
            EventType::MaintenanceSummaryRequested,
            "system".into(),
            throttle,
            serde_json::json!({"project_trace_ids": {"test-project": "maintenance"}}),
        );
        let result = self.planner.execute(&request).await.unwrap();
        ProcessResult {
            events: result.events,
            block_executions: vec![],
            total_duration_ms: 1,
        }
    }
}

#[tokio::test]
async fn nightly_real_plan_resumes_exact_parent_and_runs_fresh_task_sequentially() {
    for (verdict, expected) in [
        (r#"{"verdict":"complete"}"#, WorkItemState::Landed),
        (r#"{"verdict":"remainder","gaps":["unfinished"]}"#, WorkItemState::Landed),
        (
            r#"{"verdict":"defect","diagnosis":"faulty approach"}"#,
            WorkItemState::Preserved,
        ),
        (
            r#"{"verdict":"blocked_on_decision","finding":"policy","options":["ask owner"]}"#,
            WorkItemState::NeedsDecision,
        ),
    ] {
        let mut f = Nightly::new(verdict);
        let (_client, server, watch) = f.watching_client().await;
        let summary = f.plan(Throttle::Full, UpdatePolicy::Major, true).await;
        let plan: MajorUpgradesPlannedPayload = summary.events[0].parse_payload().unwrap();
        assert_eq!(
            plan.upgrades.iter().map(|m| (m.package.as_str(), m.status)).collect::<Vec<_>>(),
            vec![
                ("x", MajorUpgradeStatus::Dispatch),
                ("y", MajorUpgradeStatus::Dispatch),
                ("z", MajorUpgradeStatus::Overflow)
            ]
        );
        let dispatches = crate::service::eventing_ops::planned_major_dispatches(&summary);
        assert_eq!(dispatches[0].payload["nightly_resume_item_id"], f.parent.id);
        assert!(dispatches[1].payload.get("nightly_resume_item_id").is_none());
        crate::service::eventing_ops::run_major_upgrades(
            dispatches,
            f.ctx.clone(),
            &f.dir.path().join("work-items.json"),
        )
        .await;
        let mut observed = Vec::new();
        while let Ok(event) = f.events.try_recv() {
            observed.push(event);
        }
        assert_watch_matches(watch, &observed).await;
        server.abort();
        let store = WorkItemStore::load(&f.dir.path().join("work-items.json")).unwrap();
        let child =
            store.items.iter().find(|i| i.resumes.as_deref() == Some(&f.parent.id)).unwrap();
        assert_eq!(child.state, expected, "verdict {verdict}");
        assert_eq!(child.objective, f.parent.objective);
        assert_eq!(
            (child.kind, child.lane, child.origin.as_str()),
            (WorkItemKind::MajorUpgrade, WorkLane::Maintenance, "nightly majors lane")
        );
        assert!(child.operator_action.is_none());
        assert_parent_outcome(&f.parent, child, &store);
        for unrelated in &f.unrelated {
            assert_eq!(store.find(&unrelated.id), Some(unrelated));
        }
        let roots: Vec<_> = observed
            .iter()
            .filter(|e| e.event_type == EventType::ExecutionRequested)
            .collect();
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].payload["admitted_work_item_id"], child.id);
        assert_eq!(roots[0].payload["base_ref"], "preserved-work");
        assert_eq!(roots[0].payload["prompt"], f.parent.objective);
        assert_eq!(roots[0].trace_id, child.trace_id);
        assert!(roots[1].payload.get("base_ref").is_none());
        let fresh = store.items.iter().find(|i| i.trace_id == roots[1].trace_id).unwrap();
        assert_eq!(fresh.objective, objective("test-project", &major("y")));
        assert!(fresh.resumes.is_none());
        let first_terminal = observed
            .iter()
            .position(|e| {
                e.event_type == EventType::TaskRunCompleted && e.trace_id == child.trace_id
            })
            .unwrap();
        let second_root = observed.iter().position(|e| e.id == roots[1].id).unwrap();
        assert!(first_terminal < second_root, "nightly execution must remain sequential");
        for event in observed.iter().filter(|e| e.payload["item_id"] == child.id) {
            assert_eq!(event.payload["resumes"], f.parent.id);
            assert_eq!(event.payload["objective"], f.parent.objective);
            assert_eq!(event.payload["kind"], "major_upgrade");
            assert_eq!(event.payload["lane"], "maintenance");
            assert_eq!(event.payload["origin"], "nightly majors lane");
            assert_eq!(event.trace_id, child.trace_id);
        }
        assert_lifecycle_log_matches(&f.dir.path().join("events"), &child.id, &observed);
        let mut all = vec![f.history.clone()];
        all.extend(observed.clone());
        assert_lifecycle_log_matches(&f.dir.path().join("events"), &f.parent.id, &all);
        assert_eq!(f.agent.heads.lock().unwrap().len(), 2);
        assert!(f.checkout.join("CHARTER.md").exists());
    }
}

fn assert_parent_outcome(parent: &WorkItem, child: &WorkItem, store: &WorkItemStore) {
    let current = store.find(&parent.id).unwrap();
    if child.state != WorkItemState::Landed {
        assert_eq!(current, parent);
        return;
    }
    assert_eq!(current.state, WorkItemState::Landed);
    assert_eq!(current.origin, parent.origin);
    assert_eq!(current.objective, parent.objective);
    assert_eq!(current.trace_id, parent.trace_id);
    let mut expected = parent.disposition.clone().unwrap();
    expected.landed_commit = child.disposition.as_ref().unwrap().landed_commit.clone();
    assert!(expected.landed_commit.is_some());
    assert_eq!(current.disposition, Some(expected));
}

#[tokio::test]
async fn nightly_resume_retains_eligibility_inflight_and_non_dispatch_modes() {
    let f = Nightly::new(r#"{"verdict":"complete"}"#);
    let before = std::fs::read(f.dir.path().join("work-items.json")).unwrap();
    for (throttle, policy, succeeded, expected) in [
        (Throttle::DryRun, UpdatePolicy::Major, true, MajorUpgradeStatus::Dispatch),
        (Throttle::Full, UpdatePolicy::Minor, true, MajorUpgradeStatus::Proposed),
        (Throttle::Full, UpdatePolicy::Patch, true, MajorUpgradeStatus::Proposed),
        (Throttle::Full, UpdatePolicy::Major, false, MajorUpgradeStatus::Deferred),
    ] {
        let summary = f.plan(throttle, policy, succeeded).await;
        let plan: MajorUpgradesPlannedPayload = summary.events[0].parse_payload().unwrap();
        assert_eq!(plan.upgrades[0].status, expected);
        assert!(crate::service::eventing_ops::planned_major_dispatches(&summary).is_empty());
    }
    let summary = f.plan(Throttle::Full, UpdatePolicy::Major, true).await;
    let mut request = Event::new(
        EventType::MaintenanceSummaryRequested,
        "system".into(),
        Throttle::Full,
        serde_json::json!({"project_trace_ids": {"test-project": "maintenance"}, "interrupted": {
            "started_at": chrono::Utc::now(), "last_event_at": chrono::Utc::now(), "unfinished": ["test-project"]}}),
    );
    let interrupted = f.planner.execute(&request).await.unwrap();
    assert!(
        crate::service::eventing_ops::planned_major_dispatches(&ProcessResult {
            events: interrupted.events,
            block_executions: vec![],
            total_duration_ms: 0
        })
        .is_empty()
    );
    let trace = f.ctx.trace_writer.read("maintenance").unwrap();
    request = trace.events[0].clone();
    request.payload["phase"] = "review".into();
    let reviewed = f.planner.execute(&request).await.unwrap();
    assert!(
        crate::service::eventing_ops::planned_major_dispatches(&ProcessResult {
            events: reviewed.events,
            block_executions: vec![],
            total_duration_ms: 0
        })
        .is_empty()
    );
    let writer = foundry_engine::event_writer::EventWriter::new(f.dir.path().join("events"));
    writer
        .write(
            &Event::new(
                EventType::TaskRunStarted,
                f.parent.project.clone(),
                Throttle::Full,
                serde_json::json!({"project": f.parent.project, "objective": f.parent.objective}),
            )
            .with_trace_id(Some(mint_trace_id())),
        )
        .unwrap();
    let inflight = f.plan(Throttle::Full, UpdatePolicy::Major, true).await;
    let plan: MajorUpgradesPlannedPayload = inflight.events[0].parse_payload().unwrap();
    assert_eq!(plan.upgrades[0].status, MajorUpgradeStatus::Deduped);
    assert!(plan.upgrades[0].reason.as_deref().unwrap().contains("in flight"));
    assert_eq!(plan.upgrades[1].status, MajorUpgradeStatus::Dispatch);
    assert_eq!(
        plan.upgrades[2].status,
        MajorUpgradeStatus::Dispatch,
        "in-flight suppression still consumes no cap"
    );
    assert_eq!(std::fs::read(f.dir.path().join("work-items.json")).unwrap(), before);
    assert!(f.agent.heads.lock().unwrap().is_empty());
    assert_eq!(
        summary.events[0].payload["major_upgrade_resumes"][objective("test-project", &major("x"))],
        f.parent.id
    );
}

#[tokio::test]
async fn nightly_selected_resume_failure_never_falls_back_to_fresh_work() {
    for failure in [
        "cancelled",
        "missing-ref",
        "identity-changed",
        "first",
        "partial",
        "final-ledger-save",
        "initial-ledger-save",
    ] {
        let mut f = Nightly::new(r#"{"verdict":"complete"}"#);
        let summary = f.plan(Throttle::Full, UpdatePolicy::Major, true).await;
        let event = crate::service::eventing_ops::planned_major_dispatches(&summary).remove(0);
        let ledger = f.dir.path().join("work-items.json");
        let log = f
            .dir
            .path()
            .join("events")
            .join(chrono::Utc::now().format("%Y-%m.jsonl").to_string());
        let history = std::fs::read(&log).unwrap();
        if matches!(failure, "cancelled" | "missing-ref" | "identity-changed") {
            let mut store = WorkItemStore::load(&ledger).unwrap();
            let parent = store.items.iter_mut().find(|i| i.id == f.parent.id).unwrap();
            if failure == "cancelled" {
                parent.state = WorkItemState::Cancelled;
            } else if failure == "identity-changed" {
                parent.project = "other-project".into();
            } else {
                parent.disposition.as_mut().unwrap().preservation_ref = Some("missing-ref".into());
            }
            store.save(&ledger).unwrap();
        } else if failure == "first" {
            std::fs::rename(&log, log.with_extension("retained")).unwrap();
            std::fs::create_dir(&log).unwrap();
        } else if failure == "initial-ledger-save" {
            std::fs::create_dir(ledger.with_extension("json.tmp")).unwrap();
        } else {
            let mut engine = continuation_engine(f.agent.clone(), f.ctx.registry.clone(), &ledger);
            engine.register(Box::new(BreakResumeLog {
                events: f.dir.path().join("events"),
                ledger: (failure == "final-ledger-save").then(|| ledger.clone()),
            }));
            f.ctx.engine = Arc::new(
                engine
                    .with_event_writer(Arc::new(foundry_engine::event_writer::EventWriter::new(
                        f.dir.path().join("events"),
                    )))
                    .with_event_broadcaster(f.ctx.event_tx.clone()),
            );
        }
        let (mut client, server, watch) = f.watching_client().await;
        let before = std::fs::read(&ledger).unwrap();
        let error = crate::service::eventing_ops::prepare_major_dispatch(event, &f.ctx, &ledger)
            .await
            .unwrap_err();
        assert_eq!(
            error.code(),
            if matches!(failure, "cancelled" | "missing-ref" | "identity-changed") {
                tonic::Code::FailedPrecondition
            } else {
                tonic::Code::Internal
            }
        );
        let store = WorkItemStore::load(&ledger).unwrap();
        for unrelated in &f.unrelated {
            assert_eq!(store.find(&unrelated.id), Some(unrelated));
        }
        let mut observed = Vec::new();
        while let Ok(event) = f.events.try_recv() {
            observed.push(event);
        }
        assert!(observed.iter().all(|e| e.event_type != EventType::ExecutionRequested));
        assert!(f.agent.heads.lock().unwrap().is_empty());
        assert!(f.ctx.workflow_tracker.list().is_empty());
        if matches!(
            failure,
            "cancelled" | "missing-ref" | "identity-changed" | "initial-ledger-save"
        ) {
            assert_eq!(std::fs::read(&ledger).unwrap(), before);
            assert_eq!(std::fs::read(&log).unwrap(), history);
        } else {
            assert_eq!(store.find(&f.parent.id), Some(&f.parent));
            let child =
                store.items.iter().find(|i| i.resumes.as_deref() == Some(&f.parent.id)).unwrap();
            assert_eq!(child.state, WorkItemState::Failed);
            assert!(child.started_at.is_none());
            assert!(child.disposition.is_none());
            assert_eq!(child.objective, f.parent.objective);
            assert_eq!(child.kind, WorkItemKind::MajorUpgrade);
            assert_retained_admission(&f, failure, &log, &history, child, &observed);
        }
        let lifecycle: Vec<_> = observed
            .iter()
            .filter(|e| foundry_sdk::work_item_events::is_work_item_event(&e.event_type))
            .collect();
        assert_watch_through_barrier(&mut client, &f.parent.project, watch, &lifecycle).await;
        server.abort();
    }
}

fn assert_retained_admission(
    f: &Nightly,
    failure: &str,
    log: &Path,
    history: &[u8],
    child: &WorkItem,
    observed: &[Event],
) {
    let retained = match failure {
        "first" => std::fs::read(log.with_extension("retained")).unwrap(),
        "partial" => std::fs::read(f.dir.path().join("events/retained.jsonl")).unwrap(),
        _ => std::fs::read(log).unwrap(),
    };
    assert!(retained.starts_with(history));
    let appended: Vec<Event> = std::str::from_utf8(&retained[history.len()..])
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        appended
            .iter()
            .filter(|e| foundry_sdk::work_item_events::is_work_item_event(&e.event_type))
            .map(|e| &e.id)
            .collect::<Vec<_>>(),
        observed
            .iter()
            .filter(|e| foundry_sdk::work_item_events::is_work_item_event(&e.event_type))
            .map(|e| &e.id)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        appended
            .iter()
            .filter(|e| foundry_sdk::work_item_events::is_work_item_event(&e.event_type))
            .count(),
        match failure {
            "first" => 0,
            "partial" => 1,
            _ => 2,
        }
    );
    for event in appended
        .iter()
        .filter(|e| foundry_sdk::work_item_events::is_work_item_event(&e.event_type))
    {
        assert_eq!(event.payload["item_id"], child.id);
        assert_eq!(event.payload["resumes"], f.parent.id);
        assert_eq!(event.payload["origin"], "nightly majors lane");
    }
}

#[tokio::test]
async fn nightly_unmatched_preserved_history_still_suppresses_and_ledger_fault_disables_dispatch() {
    let f = Nightly::new(r#"{"verdict":"complete"}"#);
    let ledger = f.dir.path().join("work-items.json");
    WorkItemStore {
        version: 1,
        items: f.unrelated.clone(),
    }
    .save(&ledger)
    .unwrap();
    let summary = f.plan(Throttle::Full, UpdatePolicy::Major, true).await;
    let plan: MajorUpgradesPlannedPayload = summary.events[0].parse_payload().unwrap();
    assert_eq!(plan.upgrades[0].status, MajorUpgradeStatus::Deduped);
    assert!(plan.upgrades[0].reason.as_deref().unwrap().contains("preserved remainder"));
    assert!(
        crate::service::eventing_ops::planned_major_dispatches(&summary)
            .iter()
            .all(|e| e.payload["prompt"] != objective("test-project", &major("x")))
    );
    std::fs::write(&ledger, "malformed").unwrap();
    let summary = f.plan(Throttle::Full, UpdatePolicy::Major, true).await;
    let plan: MajorUpgradesPlannedPayload = summary.events[0].parse_payload().unwrap();
    assert!(!plan.dispatch_enabled);
    assert!(plan.history_warning.as_deref().unwrap().contains("ledger unreadable"));
    assert!(crate::service::eventing_ops::planned_major_dispatches(&summary).is_empty());
}

#[tokio::test]
async fn nightly_child_landing_respects_owner_cancellation_and_retains_parent_evidence() {
    let mut f = Nightly::new(r#"{"verdict":"complete"}"#);
    let summary = f.plan(Throttle::Full, UpdatePolicy::Major, true).await;
    let event = crate::service::eventing_ops::planned_major_dispatches(&summary).remove(0);
    let ledger = f.dir.path().join("work-items.json");
    let root = crate::service::eventing_ops::prepare_major_dispatch(event, &f.ctx, &ledger)
        .await
        .unwrap();
    let child_id = root.payload["admitted_work_item_id"].as_str().unwrap().to_string();
    let (mut client, server) = f.client().await;
    client
        .close_work_item(crate::proto::CloseWorkItemRequest {
            id: f.parent.id.clone(),
            reason: "owner stopped".into(),
            operator_origin: "owner-host".into(),
        })
        .await
        .unwrap();
    let cancelled = WorkItemStore::load(&ledger).unwrap().find(&f.parent.id).unwrap().clone();
    assert_eq!(cancelled.state, WorkItemState::Cancelled);
    assert_eq!(cancelled.disposition, f.parent.disposition);
    crate::service::eventing_ops::run_major_upgrades(vec![root], f.ctx.clone(), &ledger).await;
    let store = WorkItemStore::load(&ledger).unwrap();
    assert_eq!(store.find(&f.parent.id), Some(&cancelled));
    assert_eq!(store.find(&child_id).unwrap().state, WorkItemState::Landed);
    for unrelated in &f.unrelated {
        assert_eq!(store.find(&unrelated.id), Some(unrelated));
    }
    let parent_events = foundry_sdk::work_item_events::read_work_item_events(
        &f.dir.path().join("events"),
        &f.parent.id,
    )
    .unwrap();
    assert_eq!(
        parent_events.iter().map(|e| e.event_type.clone()).collect::<Vec<_>>(),
        vec![EventType::WorkItemSettled, EventType::WorkItemCancelled]
    );
    let mut observed = Vec::new();
    while let Ok(event) = f.events.try_recv() {
        observed.push(event);
    }
    assert_lifecycle_log_matches(&f.dir.path().join("events"), &child_id, &observed);
    server.abort();
}
