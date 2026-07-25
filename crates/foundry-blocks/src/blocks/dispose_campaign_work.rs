use std::path::{Path, PathBuf};
use std::sync::Arc;

use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::CampaignCancelledPayload;
use foundry_sdk::registry::Registry;
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};

use crate::gateway::ShellGateway;
use crate::workspace;

task_block_new! {
    /// Disposes of the worktree a cancelled campaign left behind.
    ///
    /// Observer — sinks on `CampaignCancelled`, and only when the operator
    /// passed `--now`. A graceful cancellation lets the in-flight cycle finish,
    /// so `FinalizeTask` has already committed and preserved (or landed) its
    /// work and there is nothing orphaned to dispose of.
    ///
    /// An immediate cancellation aborts the workflow mid-agent, so
    /// `FinalizeTask` never runs and the worktree is left with uncommitted
    /// changes. This block resolves those worktrees from the campaign's
    /// workspace-id convention (see the crate-internal `workspace` module) and
    /// either preserves the work or throws it away, per `discard_work`.
    ///
    /// Emits no downstream events — pure housekeeping. Failures are logged and
    /// reported in the block summary rather than failing the workflow: the
    /// campaign is already cancelled, and nothing is served by making the
    /// cancellation itself look unsuccessful.
    pub struct DisposeCampaignWork {
        shell: ShellGateway = crate::gateway::ProcessShellGateway
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

async fn preserve_one(
    shell: &dyn ShellGateway,
    checkout: &Path,
    worktree: &Path,
    project: &str,
    branch: &str,
) -> String {
    if let Err(error) = workspace::commit_worktree(shell, worktree, project).await {
        tracing::warn!(%branch, error = %error, "could not commit cancelled campaign work");
        return format!("{branch}: could not commit ({error})");
    }
    match workspace::preserve(shell, worktree, project, branch).await {
        Ok(reference) => {
            workspace::remove_workspace(shell, checkout, worktree).await;
            format!("{branch}: preserved at {reference}")
        }
        Err(error) => {
            // Leave the worktree in place. It is the only remaining copy of
            // work we failed to push or bundle, so removing it would destroy
            // exactly what the operator asked us to keep.
            tracing::warn!(%branch, error = %error, "could not preserve cancelled campaign work");
            format!(
                "{branch}: could not preserve ({error}); worktree left at {}",
                worktree.display()
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
) -> String {
    let workspaces = surviving_workspaces(worktrees_root, project, campaign);
    if workspaces.is_empty() {
        return format!("campaign '{campaign}': no orphaned worktree to dispose of");
    }

    let mut outcomes = Vec::new();
    for (worktree, branch) in workspaces {
        if discard_work {
            workspace::discard_workspace(shell, checkout, &worktree, &branch).await;
            outcomes.push(format!("{branch}: discarded"));
        } else {
            outcomes.push(preserve_one(shell, checkout, &worktree, project, &branch).await);
        }
    }
    format!("campaign '{campaign}': {}", outcomes.join("; "))
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

        Box::pin(async move {
            let project_name = payload.terminal.project.clone();
            let entry = super::read_registry(&registry)?
                .find_project(&project_name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("project '{project_name}' not found"))?;

            let summary = dispose(
                &*shell,
                &foundry_sdk::paths::worktrees_dir(),
                Path::new(&entry.path),
                &project_name,
                &payload.terminal.campaign,
                payload.discard_work,
            )
            .await;

            Ok(TaskBlockResult::success(summary, vec![]))
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
        let block = DisposeCampaignWork::new(Arc::new(std::sync::RwLock::new(Registry {
            version: 2,
            projects: vec![],
        })));
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
        .await;

        assert!(summary.contains("no orphaned worktree"), "{summary}");
        assert!(shell.invocations().is_empty(), "must not shell out when there is nothing to do");
    }

    #[tokio::test]
    async fn discard_removes_the_worktree_and_branch_but_never_the_remote() {
        let dir = tempfile::tempdir().unwrap();
        let root = worktrees_root_with(dir.path(), &["ship-billing-c2-abcdef"]);

        let shell = FakeShellGateway::success();
        let summary = dispose(&*shell, &root, dir.path(), "demo", "ship-billing", true).await;

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
        let summary = dispose(&*shell, &root, dir.path(), "demo", "ship-billing", false).await;

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
        let summary = dispose(&*shell, &root, dir.path(), "demo", "ship-billing", true).await;

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
}
