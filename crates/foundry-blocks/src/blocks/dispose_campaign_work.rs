use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{CampaignCancelledPayload, WorkItemEventPayload};
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::throttle::Throttle;
use foundry_sdk::work_item::{WorkDisposition, WorkItem};

use crate::gateway::ShellGateway;
use crate::workspace;

/// Disposes of the worktree a cancelled campaign left behind, and closes the
/// cycle's work-item ledger entry.
///
/// Observer — sinks on `CampaignCancelled`, and only when the operator
/// passed `--now`. A graceful cancellation lets the in-flight cycle finish,
/// so `FinalizeTask` has already committed and preserved (or landed) its
/// work and there is nothing orphaned to dispose of, and its `TaskRunCompleted`
/// settles the ledger item as usual.
///
/// An immediate cancellation aborts the workflow mid-agent, so
/// `FinalizeTask` never runs and the worktree is left with uncommitted
/// changes. This block resolves those worktrees from the campaign's
/// workspace-id convention (see the crate-internal `workspace` module) and
/// either preserves the work or throws it away, per `discard_work`.
///
/// It then settles the aborted cycle's ledger item `cancelled`, because that
/// abort also means no `TaskRunCompleted` will ever arrive to settle it — the
/// item would otherwise sit `running` until a restart closed it with the wrong
/// reason. Settling here rather than in the `CancelCampaign` RPC is what lets
/// the item's disposition report what disposal *actually did*: the worktree, and
/// whether it is gone, are observed after the work above, and the preservation
/// ref is the one `preserve` returned.
///
/// The one event it emits is that recording — `work_item_cancelled`, on the
/// aborted cycle's own trace. Disposal failures and ledger faults are logged
/// and reported in the block summary rather than failing the workflow: the
/// campaign is already cancelled, and nothing is served by making the
/// cancellation itself look unsuccessful.
pub struct DisposeCampaignWork {
    registry: Arc<RwLock<Registry>>,
    shell: Arc<dyn ShellGateway>,
    /// The work-item ledger this cancellation settles the cycle's item in.
    work_items_path: PathBuf,
}

impl DisposeCampaignWork {
    /// Dispose of cancelled campaign work, settling items in the ledger at
    /// `work_items_path`.
    #[must_use]
    pub fn new(registry: Arc<RwLock<Registry>>, work_items_path: PathBuf) -> Self {
        Self {
            registry,
            shell: Arc::new(crate::gateway::ProcessShellGateway),
            work_items_path,
        }
    }

    /// [`Self::new`] with an injected shell gateway.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn with_gateways(
        registry: Arc<RwLock<Registry>>,
        shell: Arc<dyn ShellGateway>,
        work_items_path: PathBuf,
    ) -> Self {
        Self {
            registry,
            shell,
            work_items_path,
        }
    }
}

/// Worktrees under `worktrees_root` that belong to `campaign`.
///
/// Reads the directory rather than `git worktree list` so a worktree whose
/// registration git has already pruned is still cleaned off disk. Returns an
/// empty list when the project has no worktree directory at all, which is the
/// common case — every cycle that finished normally removed its own.
fn surviving_workspaces(
    worktrees_root: &Path,
    project_name: &str,
    campaign: &str,
) -> Vec<(PathBuf, String)> {
    let dir = worktrees_root.join(workspace::slug(project_name));
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return vec![];
    };

    let mut found: Vec<(PathBuf, String)> = entries
        .filter_map(|entry| {
            let id = entry.ok()?.file_name().into_string().ok()?;
            workspace::is_campaign_workspace(campaign, &id)
                .then(|| workspace::task_workspace_paths_in(worktrees_root, project_name, &id))
        })
        .collect();
    // Deterministic order so the summary reads the same way across runs.
    found.sort();
    found
}

/// What disposal did with one orphaned worktree.
///
/// Reported so the cancellation can record it on the cycle's ledger item.
/// Nothing here changes what disposal preserves or discards — it is the same
/// work, described.
struct Disposed {
    /// The worktree disposal was pointed at.
    worktree: PathBuf,
    /// The durable ref the work was preserved on: a branch, or `bundle:{path}`.
    /// `None` when the work was discarded, or when preserving it failed.
    preservation_ref: Option<String>,
}

/// Disposal's outcome: the summary an operator reads, and what it did per
/// worktree.
struct Disposal {
    summary: String,
    worktrees: Vec<Disposed>,
}

async fn preserve_one(
    shell: &dyn ShellGateway,
    checkout: &Path,
    worktree: &Path,
    project: &str,
    branch: &str,
) -> (String, Option<String>) {
    if let Err(error) = workspace::commit_worktree(shell, worktree, project).await {
        tracing::warn!(%branch, error = %error, "could not commit cancelled campaign work");
        return (format!("{branch}: could not commit ({error})"), None);
    }
    match workspace::preserve(shell, worktree, project, branch).await {
        Ok(reference) => {
            workspace::remove_workspace(shell, checkout, worktree).await;
            (format!("{branch}: preserved at {reference}"), Some(reference))
        }
        Err(error) => {
            // Leave the worktree in place. It is the only remaining copy of
            // work we failed to push or bundle, so removing it would destroy
            // exactly what the operator asked us to keep.
            tracing::warn!(%branch, error = %error, "could not preserve cancelled campaign work");
            (
                format!(
                    "{branch}: could not preserve ({error}); worktree left at {}",
                    worktree.display()
                ),
                None,
            )
        }
    }
}

async fn dispose(
    shell: &dyn ShellGateway,
    worktrees_root: &Path,
    checkout: &Path,
    project: &str,
    campaign: &str,
    discard_work: bool,
) -> Disposal {
    let workspaces = surviving_workspaces(worktrees_root, project, campaign);
    if workspaces.is_empty() {
        return Disposal {
            summary: format!("campaign '{campaign}': no orphaned worktree to dispose of"),
            worktrees: vec![],
        };
    }

    let mut outcomes = Vec::new();
    let mut disposed = Vec::new();
    for (worktree, branch) in workspaces {
        let preservation_ref = if discard_work {
            workspace::discard_workspace(shell, checkout, &worktree, &branch).await;
            outcomes.push(format!("{branch}: discarded"));
            None
        } else {
            let (note, preservation_ref) =
                preserve_one(shell, checkout, &worktree, project, &branch).await;
            outcomes.push(note);
            preservation_ref
        };
        disposed.push(Disposed {
            worktree,
            preservation_ref,
        });
    }
    Disposal {
        summary: format!("campaign '{campaign}': {}", outcomes.join("; ")),
        worktrees: disposed,
    }
}

/// How the cancelled cycle's work ended, as the ledger records it.
///
/// Observed rather than asked for: the worktree is the one disposal was pointed
/// at, and whether it is gone is read off the filesystem *after* disposal ran —
/// so a discard that removed it, a preserve that removed it, and a failed
/// preserve that deliberately left it all report themselves honestly. `None`
/// when the cancellation found no worktree at all, which is the common case for
/// a cycle that had not yet built one.
fn cancellation_disposition(disposal: &Disposal) -> Option<WorkDisposition> {
    let disposed = disposal.worktrees.first()?;
    Some(WorkDisposition {
        verdict: None,
        landed_commit: None,
        preservation_ref: disposed.preservation_ref.clone(),
        worktree: Some(disposed.worktree.display().to_string()),
        worktree_removed: Some(!disposed.worktree.exists()),
    })
}

/// The `work_item_cancelled` event recording one settled cancellation.
///
/// Carried on the *aborted cycle's* trace, not the cancellation's: the item was
/// recorded under that trace and every other event about this unit of work is
/// on it, so that is where a reader looks. `Event::new` leaves the trace unset
/// and the engine only ever stamps what is unset, so setting it here is what
/// keeps it.
fn work_item_cancelled_event(project: &str, throttle: Throttle, item: &WorkItem) -> Event {
    super::event_from_infallible_payload(
        EventType::WorkItemCancelled,
        project,
        throttle,
        &WorkItemEventPayload::from_item(item),
    )
    .with_trace_id(item.trace_id.clone())
}

/// Settle the aborted cycle's ledger item `cancelled`, and return it.
///
/// Correlates by the aborted workflow's trace and nothing else, exactly as a
/// `TaskRunCompleted` settlement does: a trace no running task-shaped item
/// carries settles nothing, and there is no fallback to the project's newest
/// running item — settling an unrelated concurrent run as cancelled would be
/// worse than leaving this one for the restart sweep.
///
/// `None` when the cancellation aborted nothing, when no running item carries
/// that trace, or when the ledger cannot be read or written.
fn settle_cancelled_cycle(
    path: &Path,
    project: &str,
    payload: &CampaignCancelledPayload,
    disposal: &Disposal,
) -> Option<WorkItem> {
    let trace = payload.aborted_trace_id.as_deref()?;
    let disposition = cancellation_disposition(disposal);
    let _guard = super::work_ledger::ledger_lock()?;
    let mut store = super::work_ledger::load_ledger(path)?;
    let item = store.running_for_settlement(Some(trace), project)?;
    item.settle_cancelled(&payload.terminal.reason, disposition, Utc::now());
    let settled = item.clone();
    super::work_ledger::save_ledger(&store, path).then_some(settled)
}

impl TaskBlock for DisposeCampaignWork {
    task_block_meta! {
        name: "Dispose Campaign Work",
        kind: Observer,
        sinks_on: [CampaignCancelled],
    }

    fn accepts(&self, trigger: &Event) -> bool {
        trigger
            .parse_payload::<CampaignCancelledPayload>()
            .is_ok_and(|payload| payload.terminated_now)
    }

    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let payload = parse_payload!(trigger, CampaignCancelledPayload);
        let registry = Arc::clone(&self.registry);
        let shell = Arc::clone(&self.shell);
        let work_items_path = self.work_items_path.clone();
        let worktrees_root = foundry_sdk::paths::worktrees_dir();
        let throttle = trigger.throttle;

        Box::pin(async move {
            let project_name = payload.terminal.project.clone();
            let entry = super::read_registry(&registry)?
                .find_project(&project_name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("project '{project_name}' not found"))?;

            let disposal = dispose(
                &*shell,
                &worktrees_root,
                Path::new(&entry.path),
                &project_name,
                &payload.terminal.campaign,
                payload.discard_work,
            )
            .await;

            let settled =
                settle_cancelled_cycle(&work_items_path, &project_name, &payload, &disposal);
            let summary = match &settled {
                Some(item) => {
                    format!("{}; work item {} settled cancelled", disposal.summary, item.id)
                }
                None => disposal.summary.clone(),
            };
            let events = settled
                .as_ref()
                .map(|item| vec![work_item_cancelled_event(&project_name, throttle, item)])
                .unwrap_or_default();

            Ok(TaskBlockResult::success(summary, events))
        })
    }
}

#[cfg(test)]
mod tests {
    use foundry_sdk::gateway::fakes::FakeShellGateway;
    use foundry_sdk::payload::{CampaignCancelledPayload, CampaignTerminalPayload};
    use foundry_sdk::throttle::Throttle;

    use super::*;

    fn cancelled_event(terminated_now: bool, discard_work: bool) -> Event {
        let payload = CampaignCancelledPayload {
            terminal: CampaignTerminalPayload {
                campaign: "ship-billing".to_string(),
                project: "demo".to_string(),
                reason: "abandoned".to_string(),
                cycles_completed: 2,
                cycles_landed: 0,
            },
            terminated_now,
            discard_work,
            aborted_event_id: None,
            aborted_trace_id: None,
        };
        Event::new(
            EventType::CampaignCancelled,
            "demo".to_string(),
            Throttle::Full,
            serde_json::to_value(payload).unwrap(),
        )
    }

    /// A graceful cancellation has no orphaned work — `FinalizeTask` already
    /// ran — so the block must not even look.
    #[test]
    fn only_accepts_an_immediate_cancellation() {
        let block = DisposeCampaignWork::new(
            Arc::new(RwLock::new(Registry {
                version: 2,
                projects: vec![],
            })),
            PathBuf::from("/tmp/work-items.json"),
        );
        assert!(block.accepts(&cancelled_event(true, false)));
        assert!(!block.accepts(&cancelled_event(false, false)));
    }

    /// Build a worktrees root containing one directory per given workspace id.
    fn worktrees_root_with(dir: &Path, ids: &[&str]) -> PathBuf {
        let root = dir.join("worktrees");
        for id in ids {
            std::fs::create_dir_all(root.join("demo").join(id)).unwrap();
        }
        root
    }

    #[tokio::test]
    async fn absent_worktree_directory_disposes_nothing_and_runs_no_commands() {
        let dir = tempfile::tempdir().unwrap();
        let shell = FakeShellGateway::success();

        let summary = dispose(
            &*shell,
            &dir.path().join("missing"),
            dir.path(),
            "demo",
            "ship-billing",
            false,
        )
        .await
        .summary;

        assert!(summary.contains("no orphaned worktree"), "{summary}");
        assert!(shell.invocations().is_empty(), "must not shell out when there is nothing to do");
    }

    #[tokio::test]
    async fn discard_removes_the_worktree_and_branch_but_never_the_remote() {
        let dir = tempfile::tempdir().unwrap();
        let root = worktrees_root_with(dir.path(), &["ship-billing-c2-abcdef"]);

        let shell = FakeShellGateway::success();
        let summary =
            dispose(&*shell, &root, dir.path(), "demo", "ship-billing", true).await.summary;

        let issued: Vec<String> =
            shell.invocations().iter().map(|inv| inv.args.join(" ")).collect();
        assert!(summary.contains("discarded"), "{summary}");
        assert!(
            issued.iter().any(|a| a.contains("worktree remove --force")),
            "expected a forced worktree removal, got {issued:?}"
        );
        assert!(
            issued
                .iter()
                .any(|a| a.contains("branch -D foundry-task/demo-ship-billing-c2-abcdef")),
            "expected the local task branch to be deleted, got {issued:?}"
        );
        // The remote ref is the audit trail for any work an earlier cycle
        // already pushed; discarding uncommitted work must not touch it.
        assert!(
            !issued.iter().any(|a| a.contains("push origin --delete")),
            "must never delete the remote branch, got {issued:?}"
        );
    }

    #[tokio::test]
    async fn preserve_commits_pushes_then_removes_the_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let root = worktrees_root_with(dir.path(), &["ship-billing-c1-abcdef"]);

        // `status --porcelain` reports a dirty tree so the commit path runs;
        // every later call falls through to the repeated success result.
        let shell = FakeShellGateway::sequence(vec![
            foundry_sdk::gateway::CommandResult {
                stdout: " M src/main.rs".to_string(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
            foundry_sdk::gateway::CommandResult {
                stdout: String::new(),
                stderr: String::new(),
                exit_code: 0,
                success: true,
            },
        ]);
        let summary =
            dispose(&*shell, &root, dir.path(), "demo", "ship-billing", false).await.summary;

        let issued: Vec<String> =
            shell.invocations().iter().map(|inv| inv.args.join(" ")).collect();
        assert!(summary.contains("preserved at"), "{summary}");
        assert!(issued.iter().any(|a| a == "add -A"), "{issued:?}");
        assert!(issued.iter().any(|a| a.starts_with("commit -m")), "{issued:?}");
        assert!(
            issued
                .iter()
                .any(|a| a == "push -u origin foundry-task/demo-ship-billing-c1-abcdef"),
            "{issued:?}"
        );
        assert!(issued.iter().any(|a| a.contains("worktree remove")), "{issued:?}");
    }

    /// The safety property: cancelling one campaign must never dispose of
    /// another's work, including a concurrent plain task on the same project.
    #[tokio::test]
    async fn never_touches_another_campaigns_or_a_plain_tasks_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let root = worktrees_root_with(
            dir.path(),
            &[
                "ship-billing-c1-abcdef",
                "other-campaign-c1-999999",
                "abcdef123456",
            ],
        );

        let shell = FakeShellGateway::success();
        let summary =
            dispose(&*shell, &root, dir.path(), "demo", "ship-billing", true).await.summary;

        let issued = shell.invocations().iter().map(|inv| inv.args.join(" ")).collect::<Vec<_>>();
        assert!(summary.contains("ship-billing-c1-abcdef"), "{summary}");
        assert!(!summary.contains("other-campaign"), "{summary}");
        assert!(
            !issued
                .iter()
                .any(|a| a.contains("other-campaign") || a.contains("abcdef123456")),
            "disposal reached outside the cancelled campaign: {issued:?}"
        );
    }

    // ── The cancelled cycle's ledger item ────────────────────────────────────

    /// A shell that models the one effect of `git worktree remove` the ledger
    /// reads back: the directory is gone afterwards.
    ///
    /// `FakeShellGateway` records invocations and leaves the filesystem alone,
    /// and `worktree_removed` is *observed* off the filesystem rather than
    /// assumed — so without a fake that actually removes, the disposition could
    /// only ever report "still there".
    struct GitLikeShell {
        /// Fail every invocation whose joined args start with this.
        fail_prefix: Option<&'static str>,
    }

    impl foundry_sdk::gateway::ShellGateway for GitLikeShell {
        fn run<'a>(
            &'a self,
            _working_dir: &'a Path,
            _command: &'a str,
            args: &'a [&'a str],
            _env: Option<&'a [(String, String)]>,
            _timeout: Option<std::time::Duration>,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = anyhow::Result<foundry_sdk::gateway::CommandResult>,
                    > + Send
                    + 'a,
            >,
        > {
            let joined = args.join(" ");
            Box::pin(async move {
                if joined.starts_with("worktree remove")
                    && let Some(path) = args.last()
                {
                    // Best-effort: this fake stands in for git, and a removal it
                    // cannot perform is not the behaviour under test — the
                    // assertion reads the filesystem either way.
                    if let Err(error) = std::fs::remove_dir_all(path) {
                        tracing::warn!(%path, error = %error, "fake worktree removal failed");
                    }
                }
                let fails = self.fail_prefix.is_some_and(|prefix| joined.starts_with(prefix));
                Ok(foundry_sdk::gateway::CommandResult {
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: i32::from(fails),
                    success: !fails,
                })
            })
        }
    }

    const TRACE: &str = "cccccccccccccccccccccccccccccccc";

    /// A ledger holding one `running` campaign-cycle item on `TRACE`.
    fn ledger_with_running_cycle(dir: &Path) -> (PathBuf, String) {
        let path = dir.join("work-items.json");
        let item = WorkItem::dispatched(
            foundry_sdk::work_item::WorkItemSpec {
                project: "demo".to_string(),
                objective: "Tidy the billing module.".to_string(),
                kind: foundry_sdk::work_item::WorkItemKind::CampaignCycle,
                lane: foundry_sdk::work_item::WorkLane::Campaign,
                origin: "campaign ship-billing cycle 2".to_string(),
                trace_id: Some(TRACE.to_string()),
            },
            chrono::Utc::now(),
        );
        let id = item.id.clone();
        let mut store = foundry_sdk::work_item::WorkItemStore::default();
        store.upsert(item);
        store.save(&path).unwrap();
        (path, id)
    }

    fn payload(discard_work: bool, aborted_trace_id: Option<&str>) -> CampaignCancelledPayload {
        CampaignCancelledPayload {
            terminal: CampaignTerminalPayload {
                campaign: "ship-billing".to_string(),
                project: "demo".to_string(),
                reason: "superseded by the rewrite".to_string(),
                cycles_completed: 2,
                cycles_landed: 0,
            },
            terminated_now: true,
            discard_work,
            aborted_event_id: Some("evt_abc".to_string()),
            aborted_trace_id: aborted_trace_id.map(str::to_string),
        }
    }

    fn settled_item(path: &Path, id: &str) -> WorkItem {
        foundry_sdk::work_item::WorkItemStore::load(path)
            .unwrap()
            .find(id)
            .expect("the item is still in the ledger")
            .clone()
    }

    #[tokio::test]
    async fn discarding_settles_the_cycles_item_cancelled_with_the_operators_reason() {
        let dir = tempfile::tempdir().unwrap();
        let root = worktrees_root_with(dir.path(), &["ship-billing-c2-abcdef"]);
        let (ledger, id) = ledger_with_running_cycle(dir.path());
        let shell = GitLikeShell { fail_prefix: None };

        let disposal = dispose(&shell, &root, dir.path(), "demo", "ship-billing", true).await;
        let settled =
            settle_cancelled_cycle(&ledger, "demo", &payload(true, Some(TRACE)), &disposal)
                .expect("the running cycle item settles");

        assert_eq!(settled.id, id);
        assert_eq!(settled.state, foundry_sdk::work_item::WorkItemState::Cancelled);
        assert_eq!(settled.reason, "superseded by the rewrite");
        assert!(settled.settled_at.is_some());
        let d = settled.disposition.clone().expect("a disposed worktree is recorded");
        assert_eq!(
            d.worktree,
            Some(root.join("demo/ship-billing-c2-abcdef").display().to_string())
        );
        assert_eq!(d.worktree_removed, Some(true), "discard removed the worktree");
        assert_eq!(d.preservation_ref, None, "discarded work is on no ref");
        assert_eq!(settled_item(&ledger, &id), settled, "the settlement reached the file");
    }

    #[tokio::test]
    async fn preserving_records_the_ref_the_work_is_recoverable_from() {
        let dir = tempfile::tempdir().unwrap();
        let root = worktrees_root_with(dir.path(), &["ship-billing-c2-abcdef"]);
        let (ledger, id) = ledger_with_running_cycle(dir.path());
        let shell = GitLikeShell { fail_prefix: None };

        let disposal = dispose(&shell, &root, dir.path(), "demo", "ship-billing", false).await;
        let settled =
            settle_cancelled_cycle(&ledger, "demo", &payload(false, Some(TRACE)), &disposal)
                .expect("the running cycle item settles");

        let d = settled.disposition.expect("a disposed worktree is recorded");
        assert_eq!(
            d.preservation_ref.as_deref(),
            Some("foundry-task/demo-ship-billing-c2-abcdef"),
            "the pushed branch is how the work is recovered"
        );
        assert_eq!(d.worktree_removed, Some(true));
        assert_eq!(
            settled_item(&ledger, &id).state,
            foundry_sdk::work_item::WorkItemState::Cancelled
        );
    }

    /// A preserve that could not push *and* could not bundle leaves the worktree
    /// in place deliberately — the disposition has to say so, because that
    /// directory is then the only copy of the work.
    #[tokio::test]
    async fn a_failed_preserve_reports_the_worktree_it_left_behind() {
        let dir = tempfile::tempdir().unwrap();
        let root = worktrees_root_with(dir.path(), &["ship-billing-c2-abcdef"]);
        let (ledger, _id) = ledger_with_running_cycle(dir.path());
        // Fail at `status`, so `commit_worktree` errors before `preserve` runs
        // and nothing touches the preserved-bundle directory.
        let shell = GitLikeShell {
            fail_prefix: Some("status"),
        };

        let disposal = dispose(&shell, &root, dir.path(), "demo", "ship-billing", false).await;
        let settled =
            settle_cancelled_cycle(&ledger, "demo", &payload(false, Some(TRACE)), &disposal)
                .expect("the running cycle item settles");

        let d = settled.disposition.expect("a disposed worktree is recorded");
        assert_eq!(d.worktree_removed, Some(false), "the only copy of the work is still there");
        assert_eq!(d.preservation_ref, None);
    }

    #[tokio::test]
    async fn a_cancellation_that_aborted_nothing_settles_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = worktrees_root_with(dir.path(), &[]);
        let (ledger, id) = ledger_with_running_cycle(dir.path());
        let before = std::fs::read(&ledger).unwrap();

        let disposal =
            dispose(&shell_success(), &root, dir.path(), "demo", "ship-billing", true).await;
        assert!(settle_cancelled_cycle(&ledger, "demo", &payload(true, None), &disposal).is_none());

        assert_eq!(std::fs::read(&ledger).unwrap(), before, "the ledger file is untouched");
        assert!(settled_item(&ledger, &id).is_running());
    }

    #[tokio::test]
    async fn a_trace_no_running_item_carries_settles_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let root = worktrees_root_with(dir.path(), &[]);
        let (ledger, id) = ledger_with_running_cycle(dir.path());
        let before = std::fs::read(&ledger).unwrap();

        let disposal =
            dispose(&shell_success(), &root, dir.path(), "demo", "ship-billing", true).await;
        let other_trace = "d".repeat(32);
        assert!(
            settle_cancelled_cycle(&ledger, "demo", &payload(true, Some(&other_trace)), &disposal)
                .is_none(),
            "no fallback to the project's newest running item"
        );

        assert_eq!(std::fs::read(&ledger).unwrap(), before, "the ledger file is untouched");
        assert!(settled_item(&ledger, &id).is_running());
    }

    #[tokio::test]
    async fn an_unwritable_ledger_settles_nothing_and_faults_nowhere() {
        let dir = tempfile::tempdir().unwrap();
        let root = worktrees_root_with(dir.path(), &[]);
        let disposal =
            dispose(&shell_success(), &root, dir.path(), "demo", "ship-billing", true).await;

        // A directory where the ledger file should be: unreadable as JSON.
        let ledger = dir.path().join("not-a-file");
        std::fs::create_dir(&ledger).unwrap();
        assert!(
            settle_cancelled_cycle(&ledger, "demo", &payload(true, Some(TRACE)), &disposal)
                .is_none()
        );
    }

    #[test]
    fn the_cancelled_event_names_the_item_and_stays_on_its_trace() {
        let dir = tempfile::tempdir().unwrap();
        let (ledger, id) = ledger_with_running_cycle(dir.path());
        let mut item = settled_item(&ledger, &id);
        item.settle_cancelled("superseded by the rewrite", None, chrono::Utc::now());

        let event = work_item_cancelled_event("demo", Throttle::Full, &item);

        assert_eq!(event.event_type, EventType::WorkItemCancelled);
        assert_eq!(event.project, "demo");
        assert_eq!(event.trace_id.as_deref(), Some(TRACE));
        assert_eq!(event.payload["item_id"], id.as_str());
        assert_eq!(event.payload["kind"], "campaign_cycle");
        assert_eq!(event.payload["lane"], "campaign");
        assert_eq!(event.payload["state"], "cancelled");
        assert_eq!(event.payload["reason"], "superseded by the rewrite");
        assert_eq!(event.payload["origin"], "campaign ship-billing cycle 2");
    }

    fn shell_success() -> GitLikeShell {
        GitLikeShell { fail_prefix: None }
    }
}
