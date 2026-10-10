//! Supersession through the real engine, finalizer, daemon log and Watch.
use super::*;

fn hash(checkout: &Path, reference: &str) -> String {
    let output = Command::new("git")
        .current_dir(checkout)
        .args(["rev-parse", reference])
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

struct LandingAgent(Mutex<usize>);

impl AgentGateway for LandingAgent {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<AgentResponse>> + Send + 'a>> {
        let mut calls = self.0.lock().unwrap();
        *calls += 1;
        let reply = if *calls == 1 {
            std::fs::write(request.working_dir.join("new-task.txt"), "fresh deliverable").unwrap();
            "Implemented"
        } else {
            r#"```json
{"verdict":"complete"}
```"#
        };
        Box::pin(async move { Ok(AgentResponse::success(reply)) })
    }
}

#[tokio::test]
async fn actual_landing_supersedes_ancestry_and_distinct_cherry_pick_on_registered_trunk() {
    for proof in ["ancestry", "cherry"] {
        for source in ["local", "remote", "bundle"] {
            assert_supersession(proof, source).await;
        }
    }
}

fn supersession_fixture(
    dir: &Path,
    proof: &str,
    source: &str,
) -> (PathBuf, WorkItem, Vec<WorkItem>, String, String) {
    let checkout = task_project(dir);
    let mut preserved = preserved_parent(&checkout);
    preserved.resumes = Some("wi_prior_parent".into());
    let preserved_hash = hash(&checkout, "preserved-work");
    if proof == "ancestry" {
        assert!(git_ok(Some(&checkout), &["merge", "--ff-only", "preserved-work"]));
    } else {
        assert!(git_ok(Some(&checkout), &["cherry-pick", "--no-commit", "preserved-work"]));
        assert!(git_ok(Some(&checkout), &["commit", "-m", "equivalent patch, distinct commit"]));
        assert_ne!(hash(&checkout, "HEAD"), preserved_hash);
    }
    assert!(git_ok(Some(&checkout), &["branch", "-m", "integration-trunk"]));
    assert!(git_ok(Some(&checkout), &["push", "-u", "origin", "integration-trunk"]));
    let before_landing = hash(&checkout, "integration-trunk");
    preserved.disposition.as_mut().unwrap().preservation_ref =
        Some(preservation_source(&checkout, dir, source));
    let mut untouched = Vec::new();
    let mut other_project = preserved.clone();
    other_project.id = "wi_other_project".into();
    other_project.project = "another-project".into();
    untouched.push(other_project);
    for state in [
        WorkItemState::Submitted,
        WorkItemState::Queued,
        WorkItemState::Running,
        WorkItemState::Landed,
        WorkItemState::Cancelled,
        WorkItemState::Failed,
        WorkItemState::NeedsDecision,
    ] {
        let mut item = preserved.clone();
        item.id = format!("wi_{}", state.tag());
        // A queued sibling is the scheduler's to start and re-reason; a held
        // one waits exactly as seeded, which is what "untouched" needs here.
        item.state = if state == WorkItemState::Queued {
            WorkItemState::Held
        } else {
            state
        };
        item.trace_id = Some(mint_trace_id());
        untouched.push(item);
    }
    for (id, reference) in [("wi_missing", Some("absent-ref")), ("wi_no_evidence", None)] {
        let mut item = preserved.clone();
        item.id = id.into();
        item.disposition.as_mut().unwrap().preservation_ref = reference.map(str::to_string);
        untouched.push(item);
    }
    // An edited squash is similar work, but git cherry cannot prove it equivalent.
    assert!(git_ok(
        Some(&checkout),
        &["checkout", "-b", "edited-squash", "integration-trunk~1"]
    ));
    std::fs::write(checkout.join("preserved.txt"), "preserved change with edits").unwrap();
    assert!(git_ok(Some(&checkout), &["add", "preserved.txt"]));
    assert!(git_ok(Some(&checkout), &["commit", "-m", "edited squash"]));
    let mut edited = preserved.clone();
    edited.id = "wi_edited_squash".into();
    edited.disposition.as_mut().unwrap().preservation_ref = Some("edited-squash".into());
    untouched.push(edited);
    // Independent unmatched patches must remain obligations too.
    assert!(git_ok(Some(&checkout), &["checkout", "-b", "unmatched", "integration-trunk"]));
    std::fs::write(checkout.join("unmatched.txt"), "still owed").unwrap();
    assert!(git_ok(Some(&checkout), &["add", "unmatched.txt"]));
    assert!(git_ok(Some(&checkout), &["commit", "-m", "unmatched work"]));
    let mut unmatched = preserved.clone();
    unmatched.id = "wi_unmatched".into();
    unmatched.disposition.as_mut().unwrap().preservation_ref = Some("unmatched".into());
    untouched.push(unmatched);
    assert!(git_ok(Some(&checkout), &["checkout", "integration-trunk"]));

    (checkout, preserved, untouched, before_landing, preserved_hash)
}

fn prior_history(dir: &Path, item: &WorkItem) -> (Event, PathBuf, Vec<u8>) {
    let previous = Event::new(
        EventType::WorkItemSettled,
        item.project.clone(),
        Throttle::Full,
        Event::serialize_payload(&foundry_sdk::payload::WorkItemEventPayload::from_item(item))
            .unwrap(),
    )
    .with_trace_id(item.trace_id.clone());
    let events = dir.join("events");
    foundry_engine::event_writer::EventWriter::new(events.clone())
        .write(&previous)
        .unwrap();
    let path = events.join(format!("{}.jsonl", previous.occurred_at.format("%Y-%m")));
    let bytes = std::fs::read(&path).unwrap();
    (previous, path, bytes)
}

/// Register `test-project` on `integration-trunk` and `another-project` on
/// its own repository, park the running sibling on the latter so it does not
/// hold the fresh task back under the per-repository pacing rule, and save
/// `items` as the ledger.
fn two_repositories(
    checkout: &Path,
    items: &mut [WorkItem],
    ledger: &Path,
) -> Arc<RwLock<Registry>> {
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    {
        let mut registry = registry.write().unwrap();
        registry.projects[0].branch = "integration-trunk".into();
        let mut other = registry.projects[0].clone();
        other.name = "another-project".into();
        other.repo = "another/repository".into();
        registry.projects.push(other);
    }
    for item in items.iter_mut() {
        if item.state == WorkItemState::Running {
            item.project = "another-project".into();
        }
    }
    WorkItemStore {
        version: 1,
        items: items.to_vec(),
    }
    .save(ledger)
    .unwrap();
    registry
}

async fn assert_supersession(proof: &str, source: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (checkout, preserved, untouched, before_landing, preserved_hash) =
        supersession_fixture(dir.path(), proof, source);
    let ledger = dir.path().join("work-items.json");
    let mut items = untouched.clone();
    items.push(preserved.clone());
    let (previous, history_path, history) = prior_history(dir.path(), &preserved);
    let registry = two_repositories(&checkout, &mut items, &ledger);
    let untouched: Vec<WorkItem> =
        items.iter().filter(|item| item.id != preserved.id).cloned().collect();
    let agent = Arc::new(LandingAgent(Mutex::new(0)));
    let engine = continuation_engine(agent.clone(), registry.clone(), &ledger);
    let (mut client, server, mut events) =
        resume_service(dir.path(), registry.clone(), engine).await;
    let watch = client
        .watch(crate::proto::WatchRequest {
            project: preserved.project.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    let trace = mint_trace_id();
    client
        .emit(crate::proto::EmitRequest {
            event_type: "execution_requested".into(),
            project: preserved.project.clone(),
            throttle: 0,
            payload_json: serde_json::json!({"project":"test-project", "workflow":"task", "prompt":"Add fresh deliverable"})
                .to_string(),
            trace_id: trace.clone(),
            span_id: String::new(),
            parent_span_id: String::new(),
            source: None,
        })
        .await
        .unwrap();
    let mut observed = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .unwrap_or_else(|error| panic!("{error}: observed {observed:#?}"))
            .unwrap();
        let done = event.event_type == EventType::WorkItemSettled
            && event.payload["item_id"] == preserved.id;
        observed.push(event);
        if done {
            break;
        }
    }
    assert_watch_matches(watch, &observed).await;
    let terminal = observed.iter().find(|e| e.event_type == EventType::TaskRunCompleted).unwrap();
    assert_eq!(terminal.payload["landed"], true);
    let commit = hash(&checkout, "integration-trunk");
    assert_ne!(commit, before_landing);
    assert_ne!(commit, preserved_hash);
    assert_eq!(terminal.payload["preservation_ref"], commit);
    assert!(!Path::new(terminal.payload["task_worktree"].as_str().unwrap()).exists());
    let store = WorkItemStore::load(&ledger).unwrap();
    for original in &untouched {
        assert_eq!(store.find(&original.id), Some(original), "{}", original.id);
    }
    let landed = store.find(&preserved.id).unwrap();
    let mut expected = preserved.clone();
    expected.settle_landed(&format!("superseded by {commit}"), landed.settled_at.unwrap());
    expected.disposition.as_mut().unwrap().landed_commit = Some(commit.clone());
    let cleanup = &landed.disposition.as_ref().unwrap().branch_cleanup;
    assert!(
        cleanup.iter().all(|ref_result| !ref_result.deleted),
        "shared evidence must be retained"
    );
    expected.disposition.as_mut().unwrap().branch_cleanup = cleanup.clone();
    assert_eq!(landed, &expected);
    let task = store.items.iter().find(|item| item.trace_id.as_ref() == Some(&trace)).unwrap();
    assert_eq!(task.state, WorkItemState::Landed);
    assert_eq!(task.disposition.as_ref().unwrap().landed_commit.as_ref(), Some(&commit));
    assert_settled_reads(&mut client, dir.path(), &observed, &previous, landed, task, &commit)
        .await;
    assert!(std::fs::read(history_path).unwrap().starts_with(&history));
    let bytes = std::fs::read(&ledger).unwrap();
    let repeat = foundry_blocks::blocks::SettleWorkItem::with_registry(ledger.clone(), registry);
    assert!(repeat.execute(terminal).await.unwrap().events.is_empty());
    assert_eq!(std::fs::read(&ledger).unwrap(), bytes);
    assert_eq!(*agent.0.lock().unwrap(), 2);
    server.abort();
}

async fn assert_settled_reads(
    client: &mut crate::proto::foundry_client::FoundryClient<tonic::transport::Channel>,
    dir: &Path,
    observed: &[Event],
    previous: &Event,
    landed: &WorkItem,
    task: &WorkItem,
    commit: &String,
) {
    let mut history = vec![previous.clone()];
    history.extend_from_slice(observed);
    for item in [landed, task] {
        let event = observed
            .iter()
            .find(|event| {
                event.event_type == EventType::WorkItemSettled
                    && event.payload["item_id"] == item.id
            })
            .unwrap();
        assert_eq!(event.project, item.project);
        assert_eq!(event.trace_id, item.trace_id);
        assert_eq!(
            event.parse_payload::<foundry_sdk::payload::WorkItemEventPayload>().unwrap(),
            foundry_sdk::payload::WorkItemEventPayload::from_item(item)
        );
        assert_lifecycle_log_matches(&dir.join("events"), &item.id, &history);
        let read = client
            .get_work_item(crate::proto::GetWorkItemRequest {
                id: item.id.clone(),
            })
            .await
            .unwrap()
            .into_inner()
            .item
            .unwrap();
        assert_eq!(read.id, item.id);
        assert_eq!(read.reason, item.reason);
        assert_eq!(read.landed_commit.as_ref(), Some(commit));
    }
}

fn terminal(item: &WorkItem, landed: bool, commit: &str) -> Event {
    Event::new(
        EventType::TaskRunCompleted,
        item.project.clone(),
        Throttle::Full,
        serde_json::json!({"project": item.project, "success":true, "landed":landed,
            "summary":"ordinary task result", "preservation_ref":commit, "verdict":"complete"}),
    )
    .with_trace_id(item.trace_id.clone())
}

fn unresolved_evidence(checkout: &Path, dir: &Path, fault: &str, preserved: &mut WorkItem) {
    if fault == "ambiguous-bundle" {
        let bundle = dir.join("ambiguous.bundle");
        assert!(git_ok(
            Some(checkout),
            &[
                "bundle",
                "create",
                bundle.to_str().unwrap(),
                "main",
                "preserved-work"
            ]
        ));
        preserved.disposition.as_mut().unwrap().preservation_ref =
            Some(format!("bundle:{}", bundle.display()));
    } else if fault == "missing-bundle" {
        preserved.disposition.as_mut().unwrap().preservation_ref =
            Some(format!("bundle:{}", dir.join("missing.bundle").display()));
    } else if fault == "divergent-remote" {
        assert!(git_ok(Some(checkout), &["checkout", "-b", "remote-diverged"]));
        std::fs::write(checkout.join("remote-only.txt"), "not on trunk").unwrap();
        assert!(git_ok(Some(checkout), &["add", "remote-only.txt"]));
        assert!(git_ok(Some(checkout), &["commit", "-m", "remote-only preserved work"]));
        assert!(git_ok(
            Some(checkout),
            &[
                "push",
                "origin",
                "remote-diverged:refs/heads/preserved-work"
            ]
        ));
        assert!(git_ok(Some(checkout), &["checkout", "main"]));
    } else if fault == "checkout-head" {
        preserved.disposition.as_mut().unwrap().preservation_ref = Some("HEAD".into());
    }
}

#[tokio::test]
async fn supersession_git_failures_no_landing_and_failed_save_are_not_proof() {
    for fault in [
        "no-landing",
        "not-a-repository",
        "missing-trunk",
        "save-failure",
        "ambiguous-bundle",
        "missing-bundle",
        "checkout-head",
        "divergent-remote",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let checkout = task_project(dir.path());
        let mut preserved = preserved_parent(&checkout);
        assert!(git_ok(Some(&checkout), &["merge", "--ff-only", "preserved-work"]));
        let commit = hash(&checkout, "main");
        unresolved_evidence(&checkout, dir.path(), fault, &mut preserved);
        let mut running = preserved.clone();
        running.id = "wi_actual_task".into();
        running.state = WorkItemState::Running;
        running.trace_id = Some(mint_trace_id());
        running.disposition = None;
        let ledger = dir.path().join("work-items.json");
        WorkItemStore {
            version: 1,
            items: vec![preserved.clone(), running.clone()],
        }
        .save(&ledger)
        .unwrap();
        let bytes = std::fs::read(&ledger).unwrap();
        let registry =
            test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
        if fault == "not-a-repository" {
            registry.write().unwrap().projects[0].path = dir.path().to_str().unwrap().into();
        }
        if fault == "missing-trunk" {
            registry.write().unwrap().projects[0].branch = "absent-trunk".into();
        }
        if fault == "save-failure" {
            std::fs::create_dir(ledger.with_extension("json.tmp")).unwrap();
        }
        let events_dir = dir.path().join("events");
        let (tx, mut watch) = tokio::sync::broadcast::channel(64);
        let mut engine = Engine::new().with_event_broadcaster(tx).with_event_writer(Arc::new(
            foundry_engine::event_writer::EventWriter::new(events_dir.clone()),
        ));
        engine.register(Box::new(foundry_blocks::blocks::SettleWorkItem::with_registry(
            ledger.clone(),
            registry,
        )));
        let outcome = engine.process(terminal(&running, fault != "no-landing", &commit)).await;
        let after = WorkItemStore::load(&ledger).unwrap();
        assert_eq!(after.find(&preserved.id), Some(&preserved), "{fault}");
        let settlements: Vec<_> = outcome
            .events
            .iter()
            .filter(|event| event.event_type == EventType::WorkItemSettled)
            .collect();
        if fault == "save-failure" {
            assert_eq!(std::fs::read(&ledger).unwrap(), bytes);
            assert!(settlements.is_empty());
        } else {
            assert_eq!(settlements.len(), 1);
            assert_eq!(settlements[0].payload["item_id"], running.id);
        }
        if matches!(fault, "not-a-repository" | "missing-trunk") {
            assert!(
                outcome
                    .block_executions
                    .iter()
                    .any(|execution| execution.summary.contains("supersession unresolved"))
            );
        }
        assert!(
            foundry_sdk::work_item_events::read_work_item_events(&events_dir, &preserved.id)
                .unwrap()
                .is_empty()
        );
        while let Ok(event) = watch.try_recv() {
            if event.event_type == EventType::WorkItemSettled {
                assert_eq!(event.payload["item_id"], running.id);
            }
        }
    }
}

struct HeldLandingAgent {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl AgentGateway for HeldLandingAgent {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<AgentResponse>> + Send + 'a>> {
        Box::pin(async move {
            if request.access == foundry_sdk::gateway::AgentAccess::Full {
                self.started.notify_one();
                self.release.notified().await;
                std::fs::write(request.working_dir.join("new-task.txt"), "fresh deliverable")?;
                Ok(AgentResponse::success("Implemented"))
            } else {
                Ok(AgentResponse::success("```json\n{\"verdict\":\"complete\"}\n```"))
            }
        })
    }
}

#[tokio::test]
async fn automatic_supersession_respects_concurrent_owner_cancellation_and_unrelated_close() {
    let dir = tempfile::tempdir().unwrap();
    let (checkout, preserved, untouched, _, _) =
        supersession_fixture(dir.path(), "ancestry", "local");
    let unrelated = untouched
        .iter()
        .find(|item| item.state == WorkItemState::Failed)
        .unwrap()
        .clone();
    let ledger = dir.path().join("work-items.json");
    WorkItemStore {
        version: 1,
        items: vec![preserved.clone(), unrelated.clone()],
    }
    .save(&ledger)
    .unwrap();
    let (previous, log_path, history) = prior_history(dir.path(), &preserved);
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    registry.write().unwrap().projects[0].branch = "integration-trunk".into();
    let agent = Arc::new(HeldLandingAgent {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let engine = continuation_engine(agent.clone(), registry.clone(), &ledger);
    let (mut client, server, mut events) =
        resume_service(dir.path(), registry.clone(), engine).await;
    let watch = client
        .watch(crate::proto::WatchRequest {
            project: preserved.project.clone(),
        })
        .await
        .unwrap()
        .into_inner();
    let trace = mint_trace_id();
    client.emit(crate::proto::EmitRequest {
        event_type: "execution_requested".into(), project: preserved.project.clone(), throttle: 0,
        payload_json: serde_json::json!({"project":preserved.project, "workflow":"task", "prompt":"fresh task"}).to_string(),
        trace_id: trace.clone(), span_id: String::new(), parent_span_id: String::new(), source: None,
    }).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), agent.started.notified())
        .await
        .unwrap();
    for item in [&preserved, &unrelated] {
        client
            .close_work_item(crate::proto::CloseWorkItemRequest {
                id: item.id.clone(),
                reason: "owner stopped".into(),
                operator_origin: "owner host".into(),
            })
            .await
            .unwrap();
    }
    let owner_state = WorkItemStore::load(&ledger).unwrap();
    let task = owner_state
        .items
        .iter()
        .find(|item| item.trace_id.as_ref() == Some(&trace))
        .unwrap();
    agent.release.notify_one();
    let mut observed = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv()).await.unwrap().unwrap();
        let done =
            event.event_type == EventType::WorkItemSettled && event.payload["item_id"] == task.id;
        observed.push(event);
        if done {
            break;
        }
    }
    assert_watch_matches(watch, &observed).await;
    let after = WorkItemStore::load(&ledger).unwrap();
    for item in [&preserved, &unrelated] {
        assert_eq!(after.find(&item.id), owner_state.find(&item.id));
        assert_eq!(after.find(&item.id).unwrap().state, WorkItemState::Cancelled);
    }
    assert_eq!(after.find(&task.id).unwrap().state, WorkItemState::Landed);
    let terminal = observed
        .iter()
        .find(|event| event.event_type == EventType::TaskRunCompleted)
        .unwrap();
    assert_eq!(terminal.payload["landed"], true);
    let recorded = foundry_sdk::work_item_events::read_work_item_events(
        &dir.path().join("events"),
        &preserved.id,
    )
    .unwrap();
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].event_id, previous.id);
    assert_eq!(recorded[1].payload.state, WorkItemState::Cancelled);
    assert_eq!(recorded[1].trace_id, preserved.trace_id);
    assert!(std::fs::read(log_path).unwrap().starts_with(&history));
    let repeat = foundry_blocks::blocks::SettleWorkItem::with_registry(ledger, registry);
    assert!(repeat.execute(terminal).await.unwrap().events.is_empty());
    server.abort();
}

#[tokio::test]
#[allow(
    clippy::too_many_lines,
    reason = "One ordered integration scenario keeps preservation snapshots and exact boundary assertions together"
)]
async fn scheduled_reconciliation_proves_registered_trunk_and_preserves_history() {
    use foundry_blocks::blocks::{ObserveEvents, ReconcileWork};
    use foundry_sdk::payload::WorkReconcileCompletedPayload;
    for proof in ["ancestry", "cherry"] {
        for source in ["local", "remote", "bundle"] {
            // Git reports canonical worktree paths, including /private/var on macOS.
            let temp_root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
            let dir = tempfile::tempdir_in(temp_root).unwrap();
            let (checkout, preserved, mut untouched, trunk, _) =
                supersession_fixture(dir.path(), proof, source);
            // A running item's identity is obtained from durable trace evidence, not its name.
            let active_root = dir.path().join("worktrees/test-project/active-work");
            std::fs::create_dir_all(active_root.parent().unwrap()).unwrap();
            assert!(git_ok(
                Some(&checkout),
                &[
                    "worktree",
                    "add",
                    "-b",
                    "foundry-task/active",
                    active_root.to_str().unwrap(),
                    "integration-trunk"
                ]
            ));
            let running = untouched.iter_mut().find(|i| i.state == WorkItemState::Running).unwrap();
            running.disposition = None;
            let active_event = Event::new(EventType::ExecutionRequested, "test-project".into(), Throttle::Full,
                serde_json::json!({"task_worktree": active_root, "task_branch": "foundry-task/active"}))
                .with_trace_id(running.trace_id.clone());
            let active_id = running.id.clone();
            let orphan = dir.path().join("worktrees/test-project/orphan-directory");
            std::fs::create_dir_all(&orphan).unwrap();
            std::fs::write(orphan.join("keep.txt"), "valuable orphan content").unwrap();
            let informational = dir.path().join("personal worktree");
            assert!(git_ok(
                Some(&checkout),
                &[
                    "worktree",
                    "add",
                    "--detach",
                    informational.to_str().unwrap(),
                    "integration-trunk"
                ]
            ));
            assert!(git_ok(
                Some(&checkout),
                &["branch", "foundry-task/local-only", "integration-trunk"]
            ));
            assert!(git_ok(
                Some(&checkout),
                &[
                    "push",
                    "origin",
                    "integration-trunk:refs/heads/foundry-task/remote-only"
                ]
            ));
            assert!(git_ok(
                Some(&checkout),
                &[
                    "update-ref",
                    "refs/remotes/origin/foundry-task/stale",
                    &trunk
                ]
            ));
            std::fs::write(checkout.join("dirty.txt"), "keep dirty checkout").unwrap();
            assert!(git_ok(Some(&checkout), &["branch", "foundry-task/unlanded", "unmatched"]));
            assert!(git_ok(
                Some(&checkout),
                &[
                    "push",
                    "origin",
                    "integration-trunk:refs/heads/ambiguous-head"
                ]
            ));
            assert!(git_ok(Some(&checkout), &["branch", "ambiguous-head", "unmatched"]));
            let mut ambiguous = preserved.clone();
            ambiguous.id = "wi_ambiguous_exact".into();
            ambiguous.disposition.as_mut().unwrap().preservation_ref =
                Some("ambiguous-head".into());
            untouched.push(ambiguous);
            let bundle_snapshot = preserved
                .disposition
                .as_ref()
                .unwrap()
                .preservation_ref
                .as_ref()
                .and_then(|reference| reference.strip_prefix("bundle:"))
                .map(|path| (PathBuf::from(path), std::fs::read(path).unwrap()));
            let ledger = dir.path().join("work-items.json");
            let mut items = untouched.clone();
            items.push(preserved.clone());
            WorkItemStore { version: 1, items }.save(&ledger).unwrap();
            let (previous, history_path, history) = prior_history(dir.path(), &preserved);
            foundry_engine::event_writer::EventWriter::new(dir.path().join("events"))
                .write(&active_event)
                .unwrap();
            let registry =
                test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
            registry.write().unwrap().projects[0].branch = "integration-trunk".into();
            let mut engine = Engine::new();
            engine.register(Box::new(ReconcileWork::new(
                registry.clone(),
                ledger.clone(),
                dir.path().join("worktrees"),
                dir.path().join("events"),
                dir.path().join("reconcile"),
            )));
            engine.register(Box::new(
                ObserveEvents::new(dir.path().join("intake"), dir.path().join("watermark"))
                    .with_disk(
                        Vec::new(),
                        foundry_sdk::disk::DiskThreshold {
                            min_free_bytes: 0,
                            min_free_percent: 0,
                        },
                    ),
            ));
            let content_snapshots = [&checkout, &active_root, &informational, &orphan]
                .map(|path| (path.clone(), content_snapshot(path)));
            let index_snapshot = std::fs::read(checkout.join(".git/index")).unwrap();
            let local_before = git_text(&checkout, &["for-each-ref", "refs/heads"]);
            let remote_before = git_text(&checkout, &["ls-remote", "--heads", "origin"]);
            let (mut client, server, mut events) =
                resume_service(dir.path(), registry, engine).await;
            let watch = client
                .watch(crate::proto::WatchRequest {
                    project: String::new(),
                })
                .await
                .unwrap()
                .into_inner();
            let seed = foundry_sdk::sentinel::SentinelStore::default_seed();
            let scheduled = seed.find_sentinel("work-reconciler").unwrap();
            assert_eq!(
                scheduled.schedule,
                foundry_sdk::sentinel::Schedule::Cron("30 */3 * * *".into())
            );
            let trace = mint_trace_id();
            client
                .emit(crate::proto::EmitRequest {
                    event_type: scheduled.emit.event_type.to_string(),
                    project: scheduled.emit.project.clone(),
                    throttle: 0,
                    payload_json: scheduled.emit.payload.to_string(),
                    trace_id: trace.clone(),
                    span_id: String::new(),
                    parent_span_id: String::new(),
                    source: None,
                })
                .await
                .unwrap();
            let mut observed = Vec::new();
            loop {
                let event = tokio::time::timeout(std::time::Duration::from_secs(20), events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                let done = event.event_type == EventType::OpsObserved;
                observed.push(event);
                if done {
                    break;
                }
            }
            assert_watch_matches(watch, &observed).await;
            let completion = observed
                .iter()
                .find(|e| e.event_type == EventType::WorkReconcileCompleted)
                .unwrap();
            assert_eq!(completion.trace_id.as_ref(), Some(&trace));
            let report: WorkReconcileCompletedPayload = completion.parse_payload().unwrap();
            assert!(report.success, "{:?}", report.errors);
            assert_eq!(report.settled_ids, vec![preserved.id.clone()]);
            let has = |category: &str, identity: &str| {
                report.findings.iter().any(|f| f.category == category && f.identity == identity)
            };
            assert!(has("orphan_worktree", orphan.to_str().unwrap()));
            assert!(has("orphan_branch", "refs/heads/foundry-task/local-only"));
            assert!(has("orphan_branch", "refs/remotes/origin/foundry-task/remote-only"));
            assert!(has("informational", informational.to_str().unwrap()));
            assert!(has("dirty_checkout", checkout.to_str().unwrap()));
            assert!(has("unresolved", "wi_missing"));
            assert!(has("broken_item", "wi_no_evidence"));
            assert!(has("broken_item", "wi_other_project"));
            assert_eq!(report.broken_items, 2);
            assert!(has("unresolved", "wi_edited_squash"));
            assert!(has("unresolved", "wi_ambiguous_exact"));
            assert!(
                report
                    .findings
                    .iter()
                    .any(|f| f.identity == "wi_ambiguous_exact"
                        && f.detail.contains("heads disagree"))
            );
            assert!(
                report.findings.iter().any(|f| f.identity == "refs/heads/foundry-task/unlanded"
                    && f.detail.contains("unmatched patches"))
            );
            assert!(has("orphan_branch", "refs/remotes/origin/foundry-task/stale"));
            assert_eq!(hash(&checkout, "refs/remotes/origin/foundry-task/stale"), trunk);
            assert!(!report.findings.iter().any(|f| f.identity == active_root.to_str().unwrap()
                || f.identity == active_id));
            assert!(
                report
                    .findings
                    .iter()
                    .filter(|f| f.category == "orphan_branch"
                        && f.identity != "refs/heads/foundry-task/unlanded")
                    .all(|f| {
                        f.detail.contains("trunk=integration-trunk; ancestor of registered trunk")
                    })
            );
            assert_eq!(report.orphan_worktrees, 1);
            assert_eq!(report.orphan_branches, 4);
            let digest = std::fs::read_to_string(report.digest_path.as_ref().unwrap()).unwrap();
            assert_eq!(digest, report.markdown);
            assert!(digest.contains(&preserved.id));
            assert!(digest.contains("foundry-task/remote-only"));
            let ops = observed.last().unwrap();
            assert_eq!(ops.payload["anomaly_present"], true);
            assert_eq!(ops.payload["new_event_count"], 1);
            assert_eq!(ops.payload["events"][0]["summary"], digest);
            let store = WorkItemStore::load(&ledger).unwrap();
            for item in &untouched {
                assert_eq!(store.find(&item.id), Some(item));
            }
            let landed = store.find(&preserved.id).unwrap();
            let mut expected = preserved.clone();
            expected.settle_landed(&format!("superseded by {trunk}"), landed.settled_at.unwrap());
            expected.disposition.as_mut().unwrap().landed_commit = Some(trunk.clone());
            let cleanup = &landed.disposition.as_ref().unwrap().branch_cleanup;
            assert!(
                cleanup.iter().all(|ref_result| !ref_result.deleted),
                "shared evidence must be retained"
            );
            expected.disposition.as_mut().unwrap().branch_cleanup = cleanup.clone();
            assert_eq!(landed, &expected);
            let evidence = foundry_sdk::work_item_events::read_work_item_events(
                &dir.path().join("events"),
                &preserved.id,
            )
            .unwrap();
            assert_eq!(evidence.len(), 2);
            assert_eq!(evidence[0].event_id, previous.id);
            assert_eq!(evidence[1].trace_id, preserved.trace_id);
            assert_eq!(evidence[1].payload.reason, format!("superseded by {trunk}"));
            assert!(std::fs::read(&history_path).unwrap().starts_with(&history));
            let fetched = client
                .get_work_item(crate::proto::GetWorkItemRequest {
                    id: preserved.id.clone(),
                })
                .await
                .unwrap()
                .into_inner()
                .item
                .unwrap();
            assert_eq!(fetched.state, "landed");
            assert_eq!(fetched.reason, format!("superseded by {trunk}"));
            let repeat = client
                .reconcile_work(crate::proto::ReconcileWorkRequest {})
                .await
                .unwrap()
                .into_inner();
            let repeat: WorkReconcileCompletedPayload =
                serde_json::from_str(&repeat.completion_json).unwrap();
            assert!(repeat.settled_ids.is_empty());
            assert_eq!(
                foundry_sdk::work_item_events::read_work_item_events(
                    &dir.path().join("events"),
                    &preserved.id
                )
                .unwrap()
                .len(),
                2
            );
            assert_eq!(git_text(&checkout, &["for-each-ref", "refs/heads"]), local_before);
            assert_eq!(git_text(&checkout, &["ls-remote", "--heads", "origin"]), remote_before);
            assert_eq!(
                std::fs::read_to_string(orphan.join("keep.txt")).unwrap(),
                "valuable orphan content"
            );
            assert_eq!(
                std::fs::read_to_string(checkout.join("dirty.txt")).unwrap(),
                "keep dirty checkout"
            );
            for (path, snapshot) in content_snapshots {
                assert_eq!(content_snapshot(&path), snapshot, "{}", path.display());
            }
            assert_eq!(std::fs::read(checkout.join(".git/index")).unwrap(), index_snapshot);
            if let Some((path, bytes)) = bundle_snapshot {
                assert_eq!(std::fs::read(path).unwrap(), bytes);
            }
            assert!(active_root.exists());
            assert!(informational.exists());
            server.abort();
        }
    }
}

fn git_text(checkout: &Path, args: &[&str]) -> String {
    let result = Command::new("git").current_dir(checkout).args(args).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    String::from_utf8(result.stdout).unwrap()
}

fn reconciliation_engine(dir: &Path, registry: Arc<RwLock<Registry>>, output: PathBuf) -> Engine {
    let mut engine = Engine::new();
    engine.register(Box::new(foundry_blocks::blocks::ReconcileWork::new(
        registry,
        dir.join("work-items.json"),
        dir.join("worktrees"),
        dir.join("events"),
        output,
    )));
    engine.register(Box::new(
        foundry_blocks::blocks::ObserveEvents::new(dir.join("intake"), dir.join("watermark"))
            .with_disk(
                Vec::new(),
                foundry_sdk::disk::DiskThreshold {
                    min_free_bytes: 0,
                    min_free_percent: 0,
                },
            ),
    ));
    engine
}

#[tokio::test]
async fn reconcile_service_surfaces_fetch_git_ledger_and_digest_failures() {
    use foundry_sdk::payload::WorkReconcileCompletedPayload;
    for failure in ["fetch", "git", "ledger", "digest"] {
        let dir = tempfile::tempdir().unwrap();
        let (checkout, preserved, _, _, _) = supersession_fixture(dir.path(), "ancestry", "local");
        let ledger = dir.path().join("work-items.json");
        WorkItemStore {
            version: 1,
            items: vec![preserved.clone()],
        }
        .save(&ledger)
        .unwrap();
        let bytes = std::fs::read(&ledger).unwrap();
        let registry =
            test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
        registry.write().unwrap().projects[0].branch = "integration-trunk".into();
        let output = dir.path().join("reconcile");
        match failure {
            "fetch" => assert!(git_ok(
                Some(&checkout),
                &[
                    "remote",
                    "set-url",
                    "origin",
                    dir.path().join("absent.git").to_str().unwrap()
                ]
            )),
            "git" => {
                registry.write().unwrap().projects[0].path =
                    dir.path().join("missing checkout").display().to_string();
            }
            "ledger" => std::fs::create_dir(ledger.with_extension("json.tmp")).unwrap(),
            "digest" => std::fs::write(&output, "block directory creation").unwrap(),
            _ => unreachable!(),
        }
        let engine = reconciliation_engine(dir.path(), registry.clone(), output);
        let (mut client, server, mut events) = resume_service(dir.path(), registry, engine).await;
        let error = client.reconcile_work(crate::proto::ReconcileWorkRequest {}).await.unwrap_err();
        assert_eq!(error.code(), tonic::Code::Internal);
        let mut observed = Vec::new();
        while let Ok(event) = events.try_recv() {
            observed.push(event);
        }
        let report: WorkReconcileCompletedPayload = observed
            .iter()
            .find(|e| e.event_type == EventType::WorkReconcileCompleted)
            .unwrap()
            .parse_payload()
            .unwrap();
        assert!(!report.success);
        assert!(!report.errors.is_empty());
        assert!(observed.iter().any(
            |e| e.event_type == EventType::OpsObserved && e.payload["anomaly_present"] == true
        ));
        if failure == "digest" {
            assert_eq!(report.settled_ids, vec![preserved.id.clone()]);
            assert_eq!(
                WorkItemStore::load(&ledger).unwrap().find(&preserved.id).unwrap().state,
                WorkItemState::Landed
            );
            assert!(report.digest_path.is_none());
            assert!(report.markdown.contains("digest write"));
        } else {
            assert!(report.settled_ids.is_empty());
            assert_eq!(std::fs::read(&ledger).unwrap(), bytes);
            assert!(!observed.iter().any(|e| e.event_type == EventType::WorkItemSettled));
            assert!(report.digest_path.is_some());
        }
        server.abort();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn reconcile_reloads_under_owner_gate_and_keeps_concurrent_cancellation_and_unrelated_writes()
{
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let (checkout, preserved, _, _, _) = supersession_fixture(dir.path(), "ancestry", "local");
    let ledger = dir.path().join("work-items.json");
    WorkItemStore {
        version: 1,
        items: vec![preserved.clone()],
    }
    .save(&ledger)
    .unwrap();
    let entered = dir.path().join("fetch-entered");
    let release = dir.path().join("release-fetch");
    let script = dir.path().join("upload-pack");
    std::fs::write(&script, format!("#!/bin/sh\ntouch '{}'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\nexec git-upload-pack \"$@\"\n", entered.display(), release.display())).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(git_ok(
        Some(&checkout),
        &[
            "config",
            "remote.origin.uploadpack",
            script.to_str().unwrap()
        ]
    ));
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    registry.write().unwrap().projects[0].branch = "integration-trunk".into();
    let engine = reconciliation_engine(dir.path(), registry.clone(), dir.path().join("reconcile"));
    let (mut client, server, mut events) = resume_service(dir.path(), registry, engine).await;
    let mut runner = client.clone();
    let invocation =
        tokio::spawn(
            async move { runner.reconcile_work(crate::proto::ReconcileWorkRequest {}).await },
        );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !entered.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let cancelled = client
        .close_work_item(crate::proto::CloseWorkItemRequest {
            id: preserved.id.clone(),
            reason: "owner stopped work".into(),
            operator_origin: "test owner".into(),
        })
        .await
        .unwrap()
        .into_inner()
        .item
        .unwrap();
    assert_eq!(cancelled.state, "cancelled");
    let mut unrelated = preserved.clone();
    unrelated.id = "wi_unrelated_concurrent".into();
    unrelated.project = "other-project".into();
    {
        let _guard = foundry_sdk::work_item::ledger_write_gate().lock().unwrap();
        let mut store = WorkItemStore::load(&ledger).unwrap();
        store.upsert(unrelated.clone());
        store.save(&ledger).unwrap();
    }
    std::fs::write(&release, "continue").unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(15), invocation)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .into_inner();
    let report: foundry_sdk::payload::WorkReconcileCompletedPayload =
        serde_json::from_str(&response.completion_json).unwrap();
    assert!(report.settled_ids.is_empty());
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.identity == preserved.id && f.detail.contains("ledger changed"))
    );
    let store = WorkItemStore::load(&ledger).unwrap();
    assert_eq!(store.find(&preserved.id).unwrap().state, WorkItemState::Cancelled);
    assert_eq!(store.find(&unrelated.id), Some(&unrelated));
    let mut observed = Vec::new();
    while let Ok(event) = events.try_recv() {
        observed.push(event);
    }
    assert!(
        observed.iter().any(|e| e.event_type == EventType::WorkItemCancelled
            && e.payload["item_id"] == preserved.id)
    );
    assert!(!observed.iter().any(|e| e.event_type == EventType::WorkItemSettled));
    server.abort();
}

/// File bytes only: repository object and tracking-ref updates are checked separately.
fn content_snapshot(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, path: &Path, files: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                if entry.file_name() != ".git" {
                    visit(root, &path, files);
                }
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = std::collections::BTreeMap::new();
    visit(root, root, &mut files);
    files
}
