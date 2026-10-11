//! `foundry task --continues`: a follow-up that lands settles the open items
//! it continues, through the real admission, scheduler and task chain.
use super::*;

use foundry_sdk::work_item::WorkDisposition;
use foundry_sdk::work_source::WorkSource;

/// Implements on the first call (writing a file when `deliver` is set) and
/// returns `verdict` from the review.
struct FollowUpAgent {
    calls: Mutex<usize>,
    deliver: bool,
    verdict: &'static str,
}

impl AgentGateway for FollowUpAgent {
    fn invoke<'a>(
        &'a self,
        request: &'a AgentRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<AgentResponse>> + Send + 'a>> {
        let mut calls = self.calls.lock().unwrap();
        *calls += 1;
        let reply = if *calls == 1 {
            if self.deliver {
                std::fs::write(request.working_dir.join("follow-up.txt"), "review findings fixed")
                    .unwrap();
            }
            "Implemented".to_string()
        } else {
            format!("```json\n{}\n```", self.verdict)
        };
        Box::pin(async move { Ok(AgentResponse::success(reply)) })
    }
}

/// An item of `test-project` that settled `state` an hour ago.
fn continued(id: &str, state: WorkItemState) -> WorkItem {
    let earlier = chrono::Utc::now() - chrono::Duration::hours(1);
    let mut item = WorkItem::dispatched(
        foundry_sdk::work_item::WorkItemSpec {
            project: "test-project".into(),
            objective: format!("objective of {id}"),
            kind: WorkItemKind::Task,
            lane: WorkLane::Interactive,
            origin: "original submitter".into(),
            trace_id: Some(mint_trace_id()),
        },
        earlier,
    )
    .with_source(Some(WorkSource::operator("workbench")));
    item.id = id.into();
    item.state = state;
    item.reason = format!("{} by its run", state.tag());
    item.settled_at = Some(earlier);
    item.disposition = (state != WorkItemState::NeedsDecision).then(|| WorkDisposition {
        task_branch: None,
        branch_cleanup: Vec::new(),
        verdict: Some("remainder".into()),
        landed_commit: None,
        preservation_ref: Some(format!("foundry-task/{id}")),
        worktree: None,
        worktree_removed: Some(true),
    });
    item
}

async fn run_follow_up(
    dir: &Path,
    agent: FollowUpAgent,
) -> (PathBuf, WorkItemStore, [WorkItem; 3], Option<String>) {
    let checkout = task_project(dir);
    let store_path = dir.join("work-items.json");
    let mut owner_closed = continued("wi_owner_closed", WorkItemState::Preserved);
    let evidence = owner_closed.disposition.clone();
    owner_closed.settle_cancelled("owner closed", evidence, chrono::Utc::now());
    let items = [
        continued("wi_preserved", WorkItemState::Preserved),
        continued("wi_needs_decision", WorkItemState::NeedsDecision),
        owner_closed,
    ];
    WorkItemStore {
        version: 1,
        items: items.to_vec(),
    }
    .save(&store_path)
    .unwrap();
    let registry = test_helpers::registry_with_project("test-project", checkout.to_str().unwrap());
    let engine = Arc::new(task_engine(Arc::new(agent), registry.clone(), &store_path));
    let root = task_dispatch(
        "test-project",
        "Fix the review findings on the earlier branches.",
        &serde_json::json!({"continues": ["wi_preserved", "wi_needs_decision", "wi_owner_closed"]}),
    );
    let trace = root.trace_id.clone();
    admit_and_run(&engine, &registry, &store_path, root).await;
    let store = WorkItemStore::load(&store_path).unwrap();
    (checkout, store, items, trace)
}

#[tokio::test]
async fn a_landed_follow_up_settles_the_open_items_it_continues() {
    let dir = tempfile::tempdir().unwrap();
    let agent = FollowUpAgent {
        calls: Mutex::new(0),
        deliver: true,
        verdict: r#"{"verdict":"complete"}"#,
    };
    let (checkout, store, [preserved, needs_decision, owner_closed], trace) =
        run_follow_up(dir.path(), agent).await;

    let follow_up = store.items.iter().find(|item| item.trace_id == trace).unwrap();
    assert_eq!(follow_up.state, WorkItemState::Landed, "{}", follow_up.reason);
    assert_eq!(
        follow_up.continues,
        vec!["wi_preserved", "wi_needs_decision", "wi_owner_closed"],
        "the lineage is recorded on the follow-up"
    );
    let commit = follow_up.disposition.as_ref().unwrap().landed_commit.clone().unwrap();
    let mut trunk = std::process::Command::new("git");
    trunk.current_dir(&checkout).args(["rev-parse", "main"]);
    assert_eq!(String::from_utf8(trunk.output().unwrap().stdout).unwrap().trim(), commit);
    let reason = format!("superseded by {commit} via {}", follow_up.id);
    let logged = super::campaign_carry_test::logged_events(&dir.path().join("events"));
    for before in [&preserved, &needs_decision] {
        let after = store.find(&before.id).unwrap();
        assert_eq!(after.state, WorkItemState::Landed, "{}", before.id);
        assert_eq!(after.reason, reason);
        assert_eq!(after.disposition.as_ref().unwrap().landed_commit.as_ref(), Some(&commit));
        assert_eq!(after.operator_action, None);
        let settled = logged
            .iter()
            .find(|event| {
                event.event_type == EventType::WorkItemSettled
                    && event.payload["item_id"] == before.id
            })
            .unwrap_or_else(|| panic!("no work_item_settled for {}", before.id));
        assert_eq!(settled.trace_id, before.trace_id, "on the continued item's own trace");
        assert_eq!(settled.payload["reason"], reason.as_str());
    }
    assert_eq!(
        store
            .find(&preserved.id)
            .unwrap()
            .disposition
            .as_ref()
            .unwrap()
            .preservation_ref,
        preserved.disposition.as_ref().unwrap().preservation_ref,
        "preservation evidence is kept"
    );
    assert_eq!(store.find(&owner_closed.id), Some(&owner_closed), "the owner's close stands");
}

#[tokio::test]
async fn a_follow_up_that_does_not_land_leaves_the_continued_items_open() {
    let dir = tempfile::tempdir().unwrap();
    let agent = FollowUpAgent {
        calls: Mutex::new(0),
        deliver: false,
        verdict: r#"{"verdict":"remainder","gaps":["still open"]}"#,
    };
    let (_, store, continued, trace) = run_follow_up(dir.path(), agent).await;

    let follow_up = store.items.iter().find(|item| item.trace_id == trace).unwrap();
    assert_eq!(follow_up.state, WorkItemState::Preserved, "{}", follow_up.reason);
    for before in &continued {
        assert_eq!(store.find(&before.id), Some(before), "{}", before.id);
    }
}
