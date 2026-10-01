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
        item.state = state;
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

async fn assert_supersession(proof: &str, source: &str) {
    let dir = tempfile::tempdir().unwrap();
    let (checkout, preserved, untouched, before_landing, preserved_hash) =
        supersession_fixture(dir.path(), proof, source);
    let ledger = dir.path().join("work-items.json");
    let mut items = untouched.clone();
    items.push(preserved.clone());
    WorkItemStore { version: 1, items }.save(&ledger).unwrap();
    let (previous, history_path, history) = prior_history(dir.path(), &preserved);
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    {
        let mut registry = registry.write().unwrap();
        registry.projects[0].branch = "integration-trunk".into();
        let mut other = registry.projects[0].clone();
        other.name = "another-project".into();
        registry.projects.push(other);
    }
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
        trace_id: trace.clone(), span_id: String::new(), parent_span_id: String::new(),
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
