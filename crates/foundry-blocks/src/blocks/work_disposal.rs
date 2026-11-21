//! Best-effort disposal of what a settled work item no longer needs: its
//! worktree, its local `foundry-task/*` branch and the origin branch Foundry
//! pushed for it.
//!
//! The rules (Stacey, 2026-11-21: Foundry cleans up after itself):
//!
//! - A **landed** item loses its worktree and its branches. Each branch is
//!   deleted only with fresh proof that the registered trunk contains it.
//! - A **cancelled** item (an owner cancel or close, a campaign carry or
//!   cancellation) loses its worktree and its branches without trunk proof:
//!   the ledger records that the work is not wanted.
//! - An item a later landing **superseded** through its lineage
//!   (`resumes`, `continues`) is disposed of like a cancelled one: the
//!   successor carried its work to trunk.
//! - An **open** or **waiting** item (preserved, `needs_decision`, failed,
//!   queued, held, running) keeps everything. Its preservation ref is its
//!   durable record of unlanded work.
//! - A ref that another open or waiting item still references (it owns it,
//!   or starts from it as its `base_ref`) is kept, in any registered project
//!   that shares the repository. When that item later settles, the refs it
//!   referenced are examined again ([`dispose_released`]).
//!
//! Only refs under `refs/heads/foundry-task/` and worktrees under the Foundry
//! worktrees root are touched. A failure never fails the settlement: it is
//! logged and recorded on the item as what was left.
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Result, ensure};
use foundry_sdk::registry::{ProjectEntry, Registry};
use foundry_sdk::work_item::{
    BranchCleanup, WorkItem, WorkItemState, WorkItemStore, ledger_write_gate,
};

use super::work_supersession::{prove_commit_supersession, verified_commit};

/// The prefix of every branch Foundry creates for a task, as a full ref.
const TASK_REF_PREFIX: &str = "refs/heads/foundry-task/";

/// How much proof a settled item's refs need before they are deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Disposal {
    /// The item's own run landed it: delete a ref only with fresh proof that
    /// the registered trunk contains it.
    Landed,
    /// The item was cancelled, or a later landing superseded it: delete
    /// without trunk proof.
    Discarded,
}

impl Disposal {
    /// The disposal a settled item's state calls for. `None` for every state
    /// that still holds work.
    pub(super) fn of(item: &WorkItem) -> Option<Self> {
        match item.state {
            WorkItemState::Landed => Some(Self::Landed),
            WorkItemState::Cancelled => Some(Self::Discarded),
            _ => None,
        }
    }
}

async fn git(path: &Path, args: &[&str]) -> Result<String> {
    let result = crate::shell::run(
        path,
        "git",
        args,
        Some(&[("GIT_NO_REPLACE_OBJECTS".into(), "1".into())]),
        None,
    )
    .await?;
    ensure!(result.success, "git {}: {}", args[0], result.stderr.trim());
    Ok(result.stdout.trim().to_owned())
}

/// The `refs/heads/foundry-task/*` ref a recorded reference names, or `None`
/// for a bundle, a commit, or any branch Foundry did not create.
fn task_ref(reference: &str) -> Option<String> {
    let name = reference
        .strip_prefix("refs/heads/")
        .or_else(|| reference.strip_prefix("refs/remotes/origin/"))
        .unwrap_or(reference);
    if reference.starts_with("bundle:") || name.starts_with("refs/") {
        return None;
    }
    let full = format!("refs/heads/{name}");
    full.starts_with(TASK_REF_PREFIX).then_some(full)
}

/// The refs an item owns: its task branch locally, and its preservation
/// branch locally and on origin. `(ref, remote)` pairs.
fn owned(item: &WorkItem) -> Vec<(String, bool)> {
    let Some(d) = &item.disposition else {
        return Vec::new();
    };
    let mut refs = Vec::new();
    if let Some(reference) = d.task_branch.as_deref().and_then(task_ref) {
        refs.push((reference, false));
    }
    if let Some(reference) = d.preservation_ref.as_deref().and_then(task_ref) {
        if !refs.contains(&(reference.clone(), false)) {
            refs.push((reference.clone(), false));
        }
        refs.push((reference, true));
    }
    refs
}

/// Every task ref an item depends on: the refs it owns, and the ref its run
/// starts from (`base_ref` on its pending or started root).
pub(super) fn referenced(item: &WorkItem) -> Vec<String> {
    let mut refs: Vec<String> = owned(item).into_iter().map(|(reference, _)| reference).collect();
    let bases = item
        .pending_root
        .iter()
        .chain(item.started_root.iter())
        .filter_map(|root| root.payload_str("base_ref"))
        .filter_map(task_ref);
    for base in bases {
        if !refs.contains(&base) {
            refs.push(base);
        }
    }
    refs
}

/// Whether an item still holds the refs it references: every state except
/// landed and cancelled does.
pub(super) fn holds_refs(item: &WorkItem) -> bool {
    !matches!(item.state, WorkItemState::Landed | WorkItemState::Cancelled)
}

/// The commit of `reference` on origin, `None` when origin has no such ref.
async fn remote_commit(path: &Path, reference: &str) -> Result<Option<String>> {
    let rows = git(path, &["ls-remote", "--heads", "origin", reference]).await?;
    if rows.is_empty() {
        return Ok(None);
    }
    let fields: Vec<_> = rows.split_whitespace().collect();
    ensure!(fields.len() == 2 && fields[1] == reference, "remote ref ambiguous");
    let hash = fields[0];
    ensure!(
        matches!(hash.len(), 40 | 64) && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "remote returned an invalid commit"
    );
    Ok(Some(hash.to_owned()))
}

/// The commit of the local `reference`, `None` when the ref does not exist.
async fn local_commit(path: &Path, reference: &str) -> Result<Option<String>> {
    let result = crate::shell::run(
        path,
        "git",
        &["show-ref", "--verify", "--quiet", reference],
        Some(&[("GIT_NO_REPLACE_OBJECTS".into(), "1".into())]),
        None,
    )
    .await?;
    match result.exit_code {
        0 => Ok(Some(verified_commit(path, reference).await?)),
        1 => Ok(None),
        code => anyhow::bail!("git show-ref ({code}): {}", result.stderr.trim()),
    }
}

/// Prove that the registered trunk, locally or on origin, contains `commit`.
async fn prove_on_trunk(project: &ProjectEntry, commit: &str) -> Result<()> {
    let path = Path::new(&project.path);
    let trunk_ref = format!("refs/heads/{}", project.branch);
    // Best-effort: a failed local proof may still be proved by the live origin trunk.
    match verified_commit(path, &trunk_ref).await {
        Ok(trunk) => match prove_commit_supersession(path, &trunk, commit).await {
            Ok(_) => return Ok(()),
            Err(error) => tracing::warn!(%error, "local trunk did not prove branch cleanup"),
        },
        Err(error) => tracing::warn!(%error, "local cleanup trunk unavailable"),
    }
    let trunk = remote_commit(path, &trunk_ref)
        .await?
        .ok_or_else(|| anyhow::anyhow!("origin has no {trunk_ref}"))?;
    prove_commit_supersession(path, &trunk, commit).await?;
    Ok(())
}

/// Delete one task ref. `Ok(false)` when it is already gone, which makes a
/// repeated disposal a no-op.
///
/// Guards, in order: the ref is a `foundry-task/*` branch, not the trunk;
/// trunk proof when `proof_required`; no worktree has it checked out; origin
/// pushes where it fetches; and the deletion leases the commit observed here,
/// so a ref that moved meanwhile is refused.
async fn delete(
    project: &ProjectEntry,
    evidence: &mut BranchCleanup,
    proof_required: bool,
) -> Result<bool> {
    let path = Path::new(&project.path);
    ensure!(
        evidence.reference.starts_with(TASK_REF_PREFIX),
        "only foundry-task branches are deleted"
    );
    git(path, &["check-ref-format", &evidence.reference]).await?;
    let observed = if evidence.remote {
        remote_commit(path, &evidence.reference).await?
    } else {
        local_commit(path, &evidence.reference).await?
    };
    let Some(commit) = observed else {
        return Ok(false);
    };
    evidence.commit = Some(commit.clone());
    let trunk_ref = format!("refs/heads/{}", project.branch);
    ensure!(evidence.reference != trunk_ref, "registered trunk cannot be deleted");
    if proof_required {
        verified_commit(path, &commit).await?;
        prove_on_trunk(project, &commit).await?;
    }
    let worktrees = git(path, &["worktree", "list", "--porcelain"]).await?;
    ensure!(
        !worktrees.lines().any(|line| line == format!("branch {}", evidence.reference)),
        "branch is checked out in a worktree"
    );
    if evidence.remote {
        let fetch_url = git(path, &["remote", "get-url", "origin"]).await?;
        let push_urls = git(path, &["remote", "get-url", "--push", "--all", "origin"]).await?;
        ensure!(push_urls == fetch_url, "origin push destination differs from the proved remote");
        // The lease refuses a ref that moved after the observation. A deletion
        // carries no code, so `--no-verify` skips the project's pre-push hook:
        // some run their whole gate suite on every push, which turned
        // deletions into failures and five-minute timeouts. The remote's own
        // hooks still apply.
        git(
            path,
            &[
                "push",
                "--no-verify",
                &format!("--force-with-lease={}:{}", evidence.reference, commit),
                "origin",
                &format!(":{}", evidence.reference),
            ],
        )
        .await?;
    } else {
        git(
            path,
            &[
                "update-ref",
                "--no-deref",
                "-d",
                &evidence.reference,
                &commit,
            ],
        )
        .await?;
    }
    evidence.deleted = true;
    Ok(true)
}

/// Delete one `foundry-task/*` ref that no ledger item holds. `reference` is
/// the branch as `refs/heads/<name>`; `remote` selects the origin ref rather
/// than the local one. With `proof_required`, the registered trunk must
/// contain it ([`delete`] has the other guards).
pub(super) async fn prune(
    project: &ProjectEntry,
    reference: &str,
    remote: bool,
    proof_required: bool,
) -> Result<()> {
    let mut evidence = BranchCleanup {
        reference: reference.to_owned(),
        commit: None,
        remote,
        deleted: false,
        error: None,
    };
    ensure!(delete(project, &mut evidence, proof_required).await?, "ref already gone");
    Ok(())
}

/// Remove a Foundry worktree from `checkout` and from disk. `Ok(())` when it
/// is already gone. Refuses any path outside `worktrees_root`.
pub(super) async fn remove_worktree(
    checkout: &Path,
    worktree: &Path,
    worktrees_root: &Path,
) -> Result<()> {
    let root = std::fs::canonicalize(worktrees_root)
        .map_err(|error| anyhow::anyhow!("worktrees root {}: {error}", worktrees_root.display()))?;
    let resolved = match std::fs::canonicalize(worktree) {
        Ok(resolved) => resolved,
        // An absent directory is resolved through its parent, so a registration
        // git still holds for it is removed too.
        Err(_) => match (worktree.parent(), worktree.file_name()) {
            (Some(parent), Some(name)) => match std::fs::canonicalize(parent) {
                Ok(parent) => parent.join(name),
                Err(_) => return Ok(()),
            },
            _ => anyhow::bail!("worktree path has no parent: {}", worktree.display()),
        },
    };
    ensure!(
        resolved.starts_with(&root) && resolved != root,
        "worktree {} is outside the Foundry worktrees root {}",
        worktree.display(),
        root.display()
    );
    let registered = git(checkout, &["worktree", "list", "--porcelain", "-z"])
        .await?
        .split('\0')
        .filter_map(|line| line.strip_prefix("worktree "))
        .any(|path| {
            std::fs::canonicalize(path)
                .map_or_else(|_| Path::new(path) == resolved, |p| p == resolved)
        });
    let text = resolved.to_string_lossy().to_string();
    if registered {
        // Owner removal: disposal of a settled item's worktree.
        git(checkout, &["worktree", "remove", "--force", &text]).await?;
    }
    if resolved.exists() {
        let target = resolved.clone();
        tokio::task::spawn_blocking(move || std::fs::remove_dir_all(target)).await??;
    }
    Ok(())
}

/// Dispose of `item` with `disposal` and record what happened on it, in the
/// ledger at `ledger`. Does nothing for an item that still holds work.
///
/// Run after the settlement is persisted and before its event is built, so
/// the event carries the evidence. Ledger decisions and outcome writes use
/// short blocking critical sections; no Git operation runs while the shared
/// ledger gate is held.
pub(super) async fn dispose(
    ledger: &Path,
    registry: Option<&Arc<RwLock<Registry>>>,
    worktrees_root: &Path,
    item: &mut WorkItem,
    disposal: Disposal,
) {
    if holds_refs(item) {
        return;
    }
    let has_worktree = item
        .disposition
        .as_ref()
        .is_some_and(|d| d.worktree.is_some() && d.worktree_removed != Some(true));
    if owned(item).is_empty() && !has_worktree {
        return;
    }
    let projects = registry.and_then(|registry| match registry.read() {
        Ok(registry) => Some(registry.projects.clone()),
        Err(error) => {
            // Best-effort: without registry authority, retain everything.
            tracing::warn!(%error, "disposal registry unavailable");
            None
        }
    });
    let Some(projects) = projects else { return };
    let Some(project) = projects.iter().find(|project| project.name == item.project) else {
        return;
    };
    match dispose_owned(ledger, &projects, project, worktrees_root, item, disposal).await {
        Ok(updated) => *item = updated,
        Err(error) => {
            // Best-effort: the settlement is already durable; evidence
            // persistence or worker failure must not turn it into a failure.
            tracing::warn!(item_id = %item.id, %error, "disposal could not record evidence");
        }
    }
}

/// Settled items whose refs `settled` was holding, and which now have a ref
/// to dispose of: one `settled` referenced, not yet deleted, and referenced
/// by no item that still holds refs.
///
/// An item keeps a ref another open item references (a campaign cycle formed
/// from it, a task resuming it). When that other item settles, the refs it
/// referenced are released, and their owners are examined again.
pub(super) fn released_by(store: &WorkItemStore, settled: &WorkItem) -> Vec<WorkItem> {
    if holds_refs(settled) {
        return Vec::new();
    }
    let released = referenced(settled);
    let held_elsewhere = |owner: &WorkItem, reference: &String| {
        store.items.iter().any(|other| {
            other.id != owner.id && holds_refs(other) && referenced(other).contains(reference)
        })
    };
    store
        .items
        .iter()
        .filter(|item| item.id != settled.id && !holds_refs(item))
        .filter(|item| {
            let recorded = item.disposition.as_ref().map_or(&[][..], |d| &d.branch_cleanup);
            owned(item).iter().any(|(reference, remote)| {
                released.contains(reference)
                    && !recorded
                        .iter()
                        .any(|e| e.deleted && e.reference == *reference && e.remote == *remote)
                    && !held_elsewhere(item, reference)
            })
        })
        .cloned()
        .collect()
}

/// Dispose of every item [`released_by`] `settled`, each by its own state.
/// Returns those items with their recorded evidence.
pub(super) async fn dispose_released(
    ledger: &Path,
    registry: Option<&Arc<RwLock<Registry>>>,
    worktrees_root: &Path,
    settled: &WorkItem,
) -> Vec<WorkItem> {
    let path = ledger.to_owned();
    let snapshot = settled.clone();
    let candidates = tokio::task::spawn_blocking(move || -> Result<Vec<WorkItem>> {
        let _guard = ledger_write_gate().lock().map_err(|e| anyhow::anyhow!("ledger gate: {e}"))?;
        Ok(released_by(&WorkItemStore::load(&path)?, &snapshot))
    })
    .await
    .map_err(anyhow::Error::from)
    .and_then(std::convert::identity);
    let mut candidates = match candidates {
        Ok(candidates) => candidates,
        Err(error) => {
            // Best-effort: released refs stay until the reconciler's sweep.
            tracing::warn!(item_id = %settled.id, %error, "released ref disposal unavailable");
            return Vec::new();
        }
    };
    for item in &mut candidates {
        if let Some(disposal) = Disposal::of(item) {
            dispose(ledger, registry, worktrees_root, item, disposal).await;
        }
    }
    candidates
}

/// One line for a block's summary naming what disposal did across `items`:
/// how many refs it deleted, and each ref it retained with the first line of
/// its reason. `None` when no item recorded branch cleanup.
pub(super) fn describe(items: &[WorkItem]) -> Option<String> {
    let evidence: Vec<(&WorkItem, &BranchCleanup)> = items
        .iter()
        .filter_map(|item| Some((item, item.disposition.as_ref()?)))
        .flat_map(|(item, d)| d.branch_cleanup.iter().map(move |e| (item, e)))
        .collect();
    if evidence.is_empty() {
        return None;
    }
    let deleted = evidence.iter().filter(|(_, e)| e.deleted).count();
    let retained: Vec<String> = evidence
        .iter()
        .filter(|(_, e)| !e.deleted)
        .map(|(item, e)| {
            let place = if e.remote { "origin" } else { "local" };
            let reason = e.error.as_deref().and_then(|r| r.lines().next()).unwrap_or("unknown");
            let reason: String = reason.chars().take(160).collect();
            format!("{place} {} of {} ({reason})", e.reference, item.id)
        })
        .collect();
    let line = format!("branch cleanup deleted {deleted} ref(s)");
    if retained.is_empty() {
        return Some(line);
    }
    Some(format!("{line}; retained {}: {}", retained.len(), retained.join("; ")))
}

async fn common_directory(project: &ProjectEntry) -> Result<PathBuf> {
    let directory = git(
        Path::new(&project.path),
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    Ok(std::fs::canonicalize(directory)?)
}

/// Registered projects that share `project`'s repository: the same registry
/// slug (separate clones sharing origin refs) or the same Git common
/// directory (aliases and linked worktrees).
async fn repository_projects(
    projects: &[ProjectEntry],
    project: &ProjectEntry,
) -> Result<Vec<String>> {
    let directory = common_directory(project).await?;
    let mut shared = Vec::new();
    for other in projects {
        let same = if other.repo.eq_ignore_ascii_case(&project.repo) {
            true
        } else {
            match common_directory(other).await {
                Ok(other_directory) => other_directory == directory,
                Err(error) => {
                    // Best-effort: unresolved repository identity cannot authorize
                    // deleting a ref that this project's open item also references.
                    tracing::warn!(project = %other.name, %error, "cleanup repository unknown");
                    true
                }
            }
        };
        if same {
            shared.push(other.name.clone());
        }
    }
    Ok(shared)
}

/// Merge this disposal's ref observations into the evidence already
/// recorded. An observation replaces the earlier one for the same ref; a ref
/// that was already gone keeps whatever was recorded about it.
fn merge_evidence(recorded: &[BranchCleanup], observed: Vec<BranchCleanup>) -> Vec<BranchCleanup> {
    let mut merged: Vec<BranchCleanup> = recorded
        .iter()
        .filter(|old| {
            !observed
                .iter()
                .any(|new| new.reference == old.reference && new.remote == old.remote)
        })
        .cloned()
        .collect();
    merged.extend(observed);
    merged
}

async fn dispose_owned(
    ledger: &Path,
    projects: &[ProjectEntry],
    project: &ProjectEntry,
    worktrees_root: &Path,
    snapshot: &WorkItem,
    disposal: Disposal,
) -> Result<WorkItem> {
    let repository_projects = repository_projects(projects, project).await?;
    let path = ledger.to_owned();
    let expected = snapshot.clone();
    let decisions = tokio::task::spawn_blocking(move || -> Result<_> {
        let _guard = ledger_write_gate().lock().map_err(|e| anyhow::anyhow!("ledger gate: {e}"))?;
        let store = WorkItemStore::load(&path)?;
        ensure!(store.find(&expected.id) == Some(&expected), "ledger changed before disposal");
        Ok(owned(&expected)
            .into_iter()
            .map(|(reference, remote)| {
                let held = store.items.iter().any(|other| {
                    other.id != expected.id
                        && repository_projects.contains(&other.project)
                        && holds_refs(other)
                        && referenced(other).contains(&reference)
                });
                (reference, remote, held)
            })
            .collect::<Vec<_>>())
    })
    .await??;

    let checkout = Path::new(&project.path);
    let mut worktree_removed = snapshot.disposition.as_ref().and_then(|d| d.worktree_removed);
    if let Some(worktree) = snapshot.disposition.as_ref().and_then(|d| d.worktree.clone())
        && worktree_removed != Some(true)
    {
        match remove_worktree(checkout, Path::new(&worktree), worktrees_root).await {
            Ok(()) => worktree_removed = Some(true),
            Err(error) => {
                // Best-effort: the settlement stands; the reconciler's sweep
                // and the recorded `worktree_removed: false` name what was left.
                tracing::warn!(item_id = %snapshot.id, %worktree, %error, "worktree retained");
                worktree_removed = Some(false);
            }
        }
    }

    let mut observations = Vec::new();
    for (reference, remote, held) in decisions {
        let mut evidence = BranchCleanup {
            reference,
            remote,
            commit: None,
            deleted: false,
            error: None,
        };
        let outcome = if held {
            Err(anyhow::anyhow!("ref still referenced by an open item in this repository"))
        } else {
            delete(project, &mut evidence, disposal == Disposal::Landed).await
        };
        match outcome {
            Ok(true) => observations.push(evidence),
            // Already gone: nothing removed now, nothing left.
            Ok(false) => {}
            Err(error) => {
                // Best-effort: disposal cannot undo a durable settlement.
                tracing::warn!(item_id = %snapshot.id, reference = %evidence.reference, %error, "branch retained");
                evidence.error = Some(error.to_string());
                observations.push(evidence);
            }
        }
    }

    let path = ledger.to_owned();
    let expected = snapshot.clone();
    tokio::task::spawn_blocking(move || -> Result<WorkItem> {
        let _guard = ledger_write_gate().lock().map_err(|e| anyhow::anyhow!("ledger gate: {e}"))?;
        let mut store = WorkItemStore::load(&path)?;
        let current = store
            .items
            .iter_mut()
            .find(|item| item.id == expected.id)
            .ok_or_else(|| anyhow::anyhow!("item removed during disposal"))?;
        if current.state != expected.state || owned(current) != owned(&expected) {
            // Best-effort: a concurrent state or ownership change takes priority
            // over disposal evidence; never restore an obsolete settlement.
            tracing::warn!(item_id = %expected.id, "item changed during disposal");
            return Ok(current.clone());
        }
        if let Some(disposition) = current.disposition.as_mut() {
            disposition.branch_cleanup = merge_evidence(&disposition.branch_cleanup, observations);
            disposition.worktree_removed = worktree_removed;
        }
        let updated = current.clone();
        store.save(&path)?;
        Ok(updated)
    })
    .await?
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "isolated Git test fixtures"
)]
mod tests {
    use super::*;
    use foundry_sdk::work_item::{WorkDisposition, WorkItemKind, WorkItemSpec, WorkLane};
    use foundry_sdk::work_source::WorkSource;

    async fn isolated(name: &str) -> bool {
        if std::env::var_os("FOUNDRY_BRANCH_CLEANUP_TEST_CHILD").is_some() {
            return false;
        }
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("FOUNDRY_BRANCH_CLEANUP_TEST_CHILD", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    async fn fixture() -> (tempfile::TempDir, ProjectEntry, Arc<RwLock<Registry>>, PathBuf, WorkItem)
    {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("checkout");
        std::fs::create_dir(&path).unwrap();
        git(&path, &["init", "-b", "main"]).await.unwrap();
        git(&path, &["config", "user.name", "Test"]).await.unwrap();
        git(&path, &["config", "user.email", "test@example.com"]).await.unwrap();
        git(&path, &["commit", "--allow-empty", "-m", "base"]).await.unwrap();
        let origin = dir.path().join("origin.git");
        git(dir.path(), &["init", "--bare", origin.to_str().unwrap()]).await.unwrap();
        git(&path, &["remote", "add", "origin", origin.to_str().unwrap()])
            .await
            .unwrap();
        git(&path, &["push", "origin", "main"]).await.unwrap();
        git(&path, &["branch", "foundry-task/owned"]).await.unwrap();
        let mut registry = Registry {
            version: 2,
            projects: Vec::new(),
        };
        // Deserialize the public registry contract rather than depend on optional fields.
        let project: ProjectEntry = serde_json::from_value(serde_json::json!({
            "name":"test", "path":path, "branch":"main", "stack":"rust", "agent":"test", "repo":"test/test"
        }))
        .unwrap();
        registry.projects.push(project.clone());
        let mut item = WorkItem::dispatched(
            WorkItemSpec {
                project: "test".into(),
                objective: "test cleanup".into(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "test".into(),
                trace_id: None,
            },
            foundry_sdk::time::UtcDateTime::now(),
        );
        item.settle_landed("landed", foundry_sdk::time::UtcDateTime::now());
        item.disposition = Some(WorkDisposition {
            task_branch: Some("foundry-task/owned".into()),
            branch_cleanup: Vec::new(),
            verdict: Some("complete".into()),
            landed_commit: Some(git(&path, &["rev-parse", "HEAD"]).await.unwrap()),
            preservation_ref: None,
            worktree: None,
            worktree_removed: None,
        });
        let ledger = dir.path().join("ledger.json");
        (dir, project, Arc::new(RwLock::new(registry)), ledger, item)
    }

    /// The Foundry worktrees root of a fixture's ledger.
    fn worktrees_root(ledger: &Path) -> PathBuf {
        let root = ledger.parent().unwrap().join("worktrees");
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Dispose of `item` as its state calls for.
    async fn cleanup(ledger: &Path, registry: Option<&Arc<RwLock<Registry>>>, item: &mut WorkItem) {
        if let Some(disposal) = Disposal::of(item) {
            dispose(ledger, registry, &worktrees_root(ledger), item, disposal).await;
        }
    }

    fn save(ledger: &Path, item: &WorkItem) {
        WorkItemStore {
            version: 1,
            items: vec![item.clone()],
        }
        .save(ledger)
        .unwrap();
    }

    #[tokio::test]
    async fn unlanded_owner_in_another_registered_project_keeps_shared_refs() {
        if isolated("blocks::work_disposal::tests::unlanded_owner_in_another_registered_project_keeps_shared_refs").await { return; }
        for separate_clone in [false, true] {
            let (dir, project, registry, ledger, mut item) = fixture().await;
            let path = Path::new(&project.path);
            git(path, &["push", "origin", "foundry-task/owned"]).await.unwrap();
            item.disposition.as_mut().unwrap().preservation_ref = Some("foundry-task/owned".into());
            let mut other_project = project.clone();
            other_project.name = "another-project".into();
            if separate_clone {
                let clone = dir.path().join("another-clone");
                git(
                    dir.path(),
                    &[
                        "clone",
                        dir.path().join("origin.git").to_str().unwrap(),
                        clone.to_str().unwrap(),
                    ],
                )
                .await
                .unwrap();
                other_project.path = clone.to_str().unwrap().into();
            } else {
                let worktree = dir.path().join("another-worktree");
                git(path, &["worktree", "add", "--detach", worktree.to_str().unwrap()])
                    .await
                    .unwrap();
                other_project.path = worktree.to_str().unwrap().into();
                // Common-directory identity must work even with different slugs.
                other_project.repo = "another/alias".into();
            }
            registry.write().unwrap().projects.push(other_project.clone());
            let mut other = item.clone();
            other.id = "wi_other".into();
            other.project = other_project.name;
            other.state = WorkItemState::Preserved;
            other.disposition.as_mut().unwrap().task_branch = None;
            other.disposition.as_mut().unwrap().preservation_ref =
                Some("refs/remotes/origin/foundry-task/owned".into());
            WorkItemStore {
                version: 1,
                items: vec![item.clone(), other.clone()],
            }
            .save(&ledger)
            .unwrap();
            cleanup(&ledger, Some(&registry), &mut item).await;
            assert!(verified_commit(path, "refs/heads/foundry-task/owned").await.is_ok());
            assert!(remote_commit(path, "refs/heads/foundry-task/owned").await.unwrap().is_some());
            let evidence = &item.disposition.as_ref().unwrap().branch_cleanup;
            assert_eq!(evidence.len(), 2);
            assert!(
                evidence
                    .iter()
                    .all(|e| !e.deleted && e.error.as_deref().unwrap().contains("open item"))
            );
            assert_eq!(WorkItemStore::load(&ledger).unwrap().find(&other.id), Some(&other));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_cleanup_allows_concurrent_ledger_write_and_preserves_its_changes() {
        if isolated("blocks::work_disposal::tests::git_cleanup_allows_concurrent_ledger_write_and_preserves_its_changes").await { return; }
        let (dir, project, registry, ledger, mut item) = fixture().await;
        git(Path::new(&project.path), &["push", "origin", "foundry-task/owned"])
            .await
            .unwrap();
        item.disposition.as_mut().unwrap().preservation_ref = Some("foundry-task/owned".into());
        save(&ledger, &item);
        let origin = dir.path().join("origin.git");
        let hook = origin.join("hooks/pre-receive");
        // Pause a real Git operation until a synchronous writer releases it.
        // The bounded loop lets a failing regression exit without hanging Git.
        foundry_sdk::test_support::write_executable(&hook, "#!/bin/sh\ntouch cleanup-ready\ni=0\nwhile [ ! -e cleanup-release ] && [ \"$i\" -lt 100 ]; do sleep 0.05; i=$((i+1)); done\ntest -e cleanup-release\n").unwrap();
        let writer_ledger = ledger.clone();
        let item_id = item.id.clone();
        let writer = tokio::spawn(async move {
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while !origin.join("cleanup-ready").exists() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            // Check first so the old implementation fails rather than deadlocking
            // this single-worker runtime at the synchronous lock acquisition.
            let guard = ledger_write_gate().try_lock().expect("Git must not hold the ledger gate");
            drop(guard);
            let _guard = ledger_write_gate().lock().unwrap();
            let mut store = WorkItemStore::load(&writer_ledger).unwrap();
            let current = store.items.iter_mut().find(|item| item.id == item_id).unwrap();
            current.reason = "concurrent ledger update".into();
            store.save(&writer_ledger).unwrap();
            std::fs::write(origin.join("cleanup-release"), "release").unwrap();
        });
        cleanup(&ledger, Some(&registry), &mut item).await;
        writer.await.unwrap();
        assert_eq!(item.reason, "concurrent ledger update");
        assert_eq!(item.state, WorkItemState::Landed);
        assert!(item.disposition.as_ref().unwrap().branch_cleanup.iter().all(|e| e.deleted));
        assert_eq!(WorkItemStore::load(&ledger).unwrap().find(&item.id), Some(&item));
    }

    #[tokio::test]
    async fn landed_task_deletes_only_its_owned_local_branch_and_records_commit() {
        if isolated("blocks::work_disposal::tests::landed_task_deletes_only_its_owned_local_branch_and_records_commit").await { return; }
        let (_dir, project, registry, ledger, mut item) = fixture().await;
        let path = Path::new(&project.path);
        git(path, &["branch", "foundry-task/unowned"]).await.unwrap();
        save(&ledger, &item);
        cleanup(&ledger, Some(&registry), &mut item).await;
        assert!(verified_commit(path, "refs/heads/foundry-task/owned").await.is_err());
        assert!(verified_commit(path, "refs/heads/foundry-task/unowned").await.is_ok());
        let d = item.disposition.as_ref().unwrap();
        assert!(d.branch_cleanup[0].deleted);
        assert_eq!(d.branch_cleanup[0].commit, d.landed_commit);
        assert_eq!(WorkItemStore::load(&ledger).unwrap().find(&item.id), Some(&item));
        let event = foundry_sdk::payload::WorkItemEventPayload::from_item(&item);
        assert_eq!(event.disposition.unwrap().branch_cleanup, d.branch_cleanup);
    }

    #[tokio::test]
    async fn unproven_checked_out_and_unlanded_refs_are_retained() {
        if isolated(
            "blocks::work_disposal::tests::unproven_checked_out_and_unlanded_refs_are_retained",
        )
        .await
        {
            return;
        }
        let (dir, project, registry, ledger, mut item) = fixture().await;
        let path = Path::new(&project.path);
        let worktree = dir.path().join("worktree");
        git(
            path,
            &[
                "worktree",
                "add",
                worktree.to_str().unwrap(),
                "foundry-task/owned",
            ],
        )
        .await
        .unwrap();
        save(&ledger, &item);
        cleanup(&ledger, Some(&registry), &mut item).await;
        assert!(!item.disposition.as_ref().unwrap().branch_cleanup[0].deleted);
        std::fs::write(worktree.join("new"), "unlanded").unwrap();
        git(&worktree, &["add", "new"]).await.unwrap();
        git(&worktree, &["commit", "-m", "unlanded"]).await.unwrap();
        // Test fixture owns the worktree it created.
        git(path, &["worktree", "remove", worktree.to_str().unwrap()]).await.unwrap();
        save(&ledger, &item);
        cleanup(&ledger, Some(&registry), &mut item).await;
        assert!(!item.disposition.as_ref().unwrap().branch_cleanup[0].deleted);
        assert!(verified_commit(path, "refs/heads/foundry-task/owned").await.is_ok());
        item.state = WorkItemState::Preserved;
        save(&ledger, &item);
        let before = item.clone();
        cleanup(&ledger, Some(&registry), &mut item).await;
        assert_eq!(before, item);
    }

    #[tokio::test]
    async fn landed_parent_deletes_origin_ref_and_remote_rejection_keeps_landing() {
        if isolated("blocks::work_disposal::tests::landed_parent_deletes_origin_ref_and_remote_rejection_keeps_landing").await { return; }
        for reject in [false, true] {
            let (dir, project, registry, ledger, mut item) = fixture().await;
            let path = Path::new(&project.path);
            git(path, &["push", "origin", "foundry-task/owned"]).await.unwrap();
            item.disposition.as_mut().unwrap().preservation_ref = Some("foundry-task/owned".into());
            if reject {
                git(&dir.path().join("origin.git"), &["config", "receive.denyDeletes", "true"])
                    .await
                    .unwrap();
            }
            save(&ledger, &item);
            cleanup(&ledger, Some(&registry), &mut item).await;
            assert_eq!(item.state, WorkItemState::Landed);
            let d = item.disposition.as_ref().unwrap();
            assert!(d.branch_cleanup[0].deleted);
            assert_eq!(d.branch_cleanup[1].deleted, !reject);
            let remote = git(
                path,
                &[
                    "ls-remote",
                    "--heads",
                    "origin",
                    "refs/heads/foundry-task/owned",
                ],
            )
            .await
            .unwrap();
            assert_eq!(remote.is_empty(), !reject);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn origin_deletion_skips_the_projects_pre_push_hook() {
        if isolated(
            "blocks::work_disposal::tests::origin_deletion_skips_the_projects_pre_push_hook",
        )
        .await
        {
            return;
        }
        let (_dir, project, registry, ledger, mut item) = fixture().await;
        let path = Path::new(&project.path);
        git(path, &["push", "origin", "foundry-task/owned"]).await.unwrap();
        item.disposition.as_mut().unwrap().preservation_ref = Some("foundry-task/owned".into());
        // A project whose pre-push hook runs its whole gate suite would fail or
        // time out here. A deletion carries no code, so the hook must not run.
        foundry_sdk::test_support::write_executable(
            &path.join(".git/hooks/pre-push"),
            "#!/bin/sh\necho 'gate suite failed' >&2\nexit 1\n",
        )
        .unwrap();
        save(&ledger, &item);
        cleanup(&ledger, Some(&registry), &mut item).await;
        let remote = &item.disposition.as_ref().unwrap().branch_cleanup[1];
        assert!(remote.remote);
        assert!(remote.deleted, "hook ran or deletion failed: {:?}", remote.error);
        let remote_heads = git(
            path,
            &[
                "ls-remote",
                "--heads",
                "origin",
                "refs/heads/foundry-task/owned",
            ],
        )
        .await
        .unwrap();
        assert_eq!(remote_heads, "");
    }

    /// A cycle of `campaign` in project `test`, owning `branch` locally and
    /// as its preservation ref on origin.
    fn cycle(id: &str, campaign: &str, state: WorkItemState, branch: Option<&str>) -> WorkItem {
        let mut item = WorkItem::dispatched(
            WorkItemSpec {
                project: "test".into(),
                objective: "cycle".into(),
                kind: WorkItemKind::CampaignCycle,
                lane: WorkLane::Interactive,
                origin: "test".into(),
                trace_id: None,
            },
            foundry_sdk::time::UtcDateTime::now(),
        )
        .with_source(Some(WorkSource::campaign(campaign, 1)));
        item.id = id.into();
        item.state = state;
        item.disposition = branch.map(|branch| WorkDisposition {
            task_branch: Some(branch.into()),
            preservation_ref: Some(branch.into()),
            ..WorkDisposition::default()
        });
        item
    }

    /// `item` started from `base`, as a campaign forms a cycle from the
    /// previous cycle's preserved ref.
    fn started_from(mut item: WorkItem, base: &str) -> WorkItem {
        item.started_root = Some(foundry_sdk::event::Event::new(
            foundry_sdk::event::EventType::ExecutionRequested,
            "test".into(),
            foundry_sdk::throttle::Throttle::Full,
            serde_json::json!({ "base_ref": base }),
        ));
        item
    }

    #[test]
    fn a_settled_item_releases_the_settled_owners_of_the_refs_it_referenced() {
        let carrier = started_from(
            cycle("wi_carrier", "ship", WorkItemState::Landed, Some("foundry-task/c5")),
            "foundry-task/c4",
        );
        let base = cycle("wi_c4", "ship", WorkItemState::Cancelled, Some("foundry-task/c4"));
        let unrelated = cycle("wi_c1", "ship", WorkItemState::Cancelled, Some("foundry-task/c1"));
        let mut done = cycle("wi_done", "ship", WorkItemState::Cancelled, Some("foundry-task/c4"));
        done.disposition.as_mut().unwrap().branch_cleanup = [false, true]
            .into_iter()
            .map(|remote| BranchCleanup {
                reference: "refs/heads/foundry-task/c4".into(),
                commit: None,
                remote,
                deleted: true,
                error: None,
            })
            .collect();
        let open = cycle("wi_open", "ship", WorkItemState::Preserved, Some("foundry-task/c5"));
        let mut store = WorkItemStore {
            version: 1,
            items: vec![carrier.clone(), base.clone(), unrelated, done, open],
        };

        let ids: Vec<_> = released_by(&store, &carrier).into_iter().map(|item| item.id).collect();
        assert_eq!(ids, vec![base.id], "only a settled owner with refs left is released");

        // Another open item still starting from the ref keeps it held.
        store.items.push(started_from(
            cycle("wi_retry", "ship", WorkItemState::Queued, None),
            "foundry-task/c4",
        ));
        assert_eq!(released_by(&store, &carrier), Vec::<WorkItem>::new());

        // An item that still holds its refs releases nothing.
        for state in [
            WorkItemState::Preserved,
            WorkItemState::Running,
            WorkItemState::Queued,
        ] {
            let mut holding = carrier.clone();
            holding.state = state;
            assert!(released_by(&store, &holding).is_empty(), "{state:?} released refs");
        }
    }

    #[test]
    fn only_foundry_task_branches_are_owned() {
        let mut item = cycle("wi", "ship", WorkItemState::Cancelled, Some("foundry-task/c1"));
        assert_eq!(
            owned(&item),
            vec![
                ("refs/heads/foundry-task/c1".to_string(), false),
                ("refs/heads/foundry-task/c1".to_string(), true),
            ]
        );
        for reference in [
            "main",
            "release/1.0",
            "bundle:/tmp/x.bundle",
            "refs/tags/v1",
        ] {
            item.disposition = Some(WorkDisposition {
                task_branch: Some(reference.into()),
                preservation_ref: Some(reference.into()),
                ..WorkDisposition::default()
            });
            assert!(owned(&item).is_empty(), "{reference} is not Foundry's to delete");
        }
    }

    #[test]
    fn describe_counts_deletions_and_names_each_retained_ref_with_its_reason() {
        assert_eq!(describe(&[]), None);
        let untouched = cycle("wi_none", "ship", WorkItemState::Landed, Some("foundry-task/n"));
        assert_eq!(describe(std::slice::from_ref(&untouched)), None);

        let mut item = cycle("wi_c1", "ship", WorkItemState::Landed, Some("foundry-task/c1"));
        let evidence = |remote, deleted, error: Option<&str>| BranchCleanup {
            reference: "refs/heads/foundry-task/c1".into(),
            commit: None,
            remote,
            deleted,
            error: error.map(Into::into),
        };
        item.disposition.as_mut().unwrap().branch_cleanup = vec![
            evidence(false, true, None),
            evidence(true, false, Some("command timed out\nsecond line")),
        ];
        assert_eq!(
            describe(&[item]).as_deref(),
            Some(
                "branch cleanup deleted 1 ref(s); retained 1: origin \
                 refs/heads/foundry-task/c1 of wi_c1 (command timed out)"
            )
        );
    }

    /// Commit `file` on a new branch `branch` from `start`, leaving the
    /// checkout on `main`.
    async fn commit_on(path: &Path, branch: &str, start: &str, file: &str) {
        git(path, &["checkout", "-q", "-b", branch, start]).await.unwrap();
        std::fs::write(path.join(file), file).unwrap();
        git(path, &["add", file]).await.unwrap();
        git(path, &["commit", "-q", "-m", file]).await.unwrap();
        git(path, &["checkout", "-q", "main"]).await.unwrap();
    }

    /// A Foundry worktree for `branch` under the fixture's worktrees root,
    /// with an untracked file so only a forced removal takes it.
    async fn foundry_worktree(ledger: &Path, checkout: &Path, branch: &str) -> PathBuf {
        let worktree = worktrees_root(ledger).join("test").join(branch.replace('/', "-"));
        std::fs::create_dir_all(worktree.parent().unwrap()).unwrap();
        git(checkout, &["worktree", "add", "-q", worktree.to_str().unwrap(), branch])
            .await
            .unwrap();
        std::fs::write(worktree.join("scratch.txt"), "left by the agent").unwrap();
        worktree
    }

    async fn on_origin(path: &Path, branch: &str) -> bool {
        !git(
            path,
            &[
                "ls-remote",
                "--heads",
                "origin",
                &format!("refs/heads/{branch}"),
            ],
        )
        .await
        .unwrap()
        .is_empty()
    }

    async fn local(path: &Path, branch: &str) -> bool {
        local_commit(path, &format!("refs/heads/{branch}")).await.unwrap().is_some()
    }

    fn evidence_for(item: &WorkItem, remote: bool) -> &BranchCleanup {
        item.disposition
            .as_ref()
            .unwrap()
            .branch_cleanup
            .iter()
            .find(|e| e.remote == remote)
            .unwrap()
    }

    #[tokio::test]
    async fn a_landed_item_loses_its_worktree_and_task_branch() {
        if isolated(
            "blocks::work_disposal::tests::a_landed_item_loses_its_worktree_and_task_branch",
        )
        .await
        {
            return;
        }
        let (_dir, project, registry, ledger, mut item) = fixture().await;
        let path = Path::new(&project.path);
        let worktree = foundry_worktree(&ledger, path, "foundry-task/owned").await;
        item.disposition.as_mut().unwrap().worktree = Some(worktree.display().to_string());
        item.disposition.as_mut().unwrap().worktree_removed = Some(false);
        save(&ledger, &item);

        cleanup(&ledger, Some(&registry), &mut item).await;

        assert!(!worktree.exists(), "the landed worktree is removed");
        assert!(!git(path, &["worktree", "list"]).await.unwrap().contains("foundry-task/owned"));
        assert!(!local(path, "foundry-task/owned").await);
        let d = item.disposition.as_ref().unwrap();
        assert_eq!(d.worktree_removed, Some(true));
        assert_eq!(d.branch_cleanup.len(), 1, "a landing pushed no task branch: {d:?}");
        assert_eq!(
            d.disposal_summary().as_deref(),
            Some(
                format!(
                    "removed worktree {}, local refs/heads/foundry-task/owned",
                    worktree.display()
                )
                .as_str()
            )
        );
        assert_eq!(WorkItemStore::load(&ledger).unwrap().find(&item.id), Some(&item));

        // Disposal is idempotent: a second pass finds nothing and changes nothing.
        let before = item.clone();
        cleanup(&ledger, Some(&registry), &mut item).await;
        assert_eq!(item, before);
    }

    #[tokio::test]
    async fn a_cancelled_item_loses_unlanded_work_without_trunk_proof() {
        if isolated("blocks::work_disposal::tests::a_cancelled_item_loses_unlanded_work_without_trunk_proof").await { return; }
        let (_dir, project, registry, ledger, mut item) = fixture().await;
        let path = Path::new(&project.path);
        commit_on(path, "foundry-task/cancelled", "main", "unwanted").await;
        git(path, &["push", "-q", "origin", "foundry-task/cancelled"]).await.unwrap();
        let worktree = foundry_worktree(&ledger, path, "foundry-task/cancelled").await;
        git(path, &["branch", "feature/kept"]).await.unwrap();
        item.settle_cancelled(
            "owner cancelled",
            Some(WorkDisposition {
                task_branch: Some("foundry-task/cancelled".into()),
                preservation_ref: Some("foundry-task/cancelled".into()),
                worktree: Some(worktree.display().to_string()),
                worktree_removed: Some(false),
                ..WorkDisposition::default()
            }),
            foundry_sdk::time::UtcDateTime::now(),
        );
        save(&ledger, &item);

        cleanup(&ledger, Some(&registry), &mut item).await;

        assert!(!worktree.exists());
        assert!(!local(path, "foundry-task/cancelled").await);
        assert!(!on_origin(path, "foundry-task/cancelled").await);
        assert!(local(path, "feature/kept").await, "a non-Foundry branch is never touched");
        assert!(evidence_for(&item, false).deleted);
        assert!(evidence_for(&item, true).deleted);
        assert_eq!(item.state, WorkItemState::Cancelled);
    }

    #[tokio::test]
    async fn a_worktree_outside_the_foundry_root_is_left_and_recorded() {
        if isolated("blocks::work_disposal::tests::a_worktree_outside_the_foundry_root_is_left_and_recorded").await { return; }
        let (dir, project, registry, ledger, mut item) = fixture().await;
        let path = Path::new(&project.path);
        let outside = dir.path().join("elsewhere");
        git(
            path,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                outside.to_str().unwrap(),
            ],
        )
        .await
        .unwrap();
        item.disposition.as_mut().unwrap().worktree = Some(outside.display().to_string());
        save(&ledger, &item);

        cleanup(&ledger, Some(&registry), &mut item).await;

        assert!(outside.exists());
        let d = item.disposition.as_ref().unwrap();
        assert_eq!(d.worktree_removed, Some(false));
        assert!(
            d.disposal_summary()
                .unwrap()
                .contains(&format!("left worktree {}", outside.display()))
        );
    }

    #[tokio::test]
    async fn a_missing_origin_is_a_recorded_warning_not_a_failure() {
        if isolated(
            "blocks::work_disposal::tests::a_missing_origin_is_a_recorded_warning_not_a_failure",
        )
        .await
        {
            return;
        }
        let (_dir, project, registry, ledger, mut item) = fixture().await;
        let path = Path::new(&project.path);
        git(path, &["remote", "remove", "origin"]).await.unwrap();
        item.settle_cancelled(
            "owner cancelled",
            Some(WorkDisposition {
                task_branch: Some("foundry-task/owned".into()),
                preservation_ref: Some("foundry-task/owned".into()),
                ..WorkDisposition::default()
            }),
            foundry_sdk::time::UtcDateTime::now(),
        );
        save(&ledger, &item);

        cleanup(&ledger, Some(&registry), &mut item).await;

        assert!(evidence_for(&item, false).deleted, "the local branch still goes");
        let remote = evidence_for(&item, true);
        assert!(!remote.deleted);
        assert!(remote.error.is_some(), "{remote:?}");
        assert_eq!(item.state, WorkItemState::Cancelled);
    }

    /// The campaign carry, and a resume: an item formed from a cancelled
    /// item's branch keeps that branch while it is open. When it settles,
    /// the branch is released and goes, whether or not trunk contains it.
    #[tokio::test]
    async fn a_ref_an_open_item_starts_from_is_kept_until_that_item_settles() {
        if isolated("blocks::work_disposal::tests::a_ref_an_open_item_starts_from_is_kept_until_that_item_settles").await { return; }
        let (_dir, project, registry, ledger, _) = fixture().await;
        let path = Path::new(&project.path);
        commit_on(path, "foundry-task/c1", "main", "first-cycle").await;
        git(path, &["push", "-q", "origin", "foundry-task/c1"]).await.unwrap();
        let carried = cycle("wi_c1", "ship", WorkItemState::Cancelled, Some("foundry-task/c1"));
        let mut successor =
            started_from(cycle("wi_c2", "ship", WorkItemState::Running, None), "foundry-task/c1");
        WorkItemStore {
            version: 1,
            items: vec![carried.clone(), successor.clone()],
        }
        .save(&ledger)
        .unwrap();

        let mut first = carried.clone();
        cleanup(&ledger, Some(&registry), &mut first).await;
        assert!(local(path, "foundry-task/c1").await, "a running successor starts from it");
        assert!(on_origin(path, "foundry-task/c1").await);
        assert!(
            evidence_for(&first, true).error.as_deref().unwrap().contains("open item"),
            "{first:?}"
        );

        // The successor lands (or is cancelled): what it referenced is released.
        let mut store = WorkItemStore::load(&ledger).unwrap();
        let settled = store.items.iter_mut().find(|item| item.id == successor.id).unwrap();
        settled.settle_landed("landed", foundry_sdk::time::UtcDateTime::now());
        successor = settled.clone();
        store.save(&ledger).unwrap();
        let released =
            dispose_released(&ledger, Some(&registry), &worktrees_root(&ledger), &successor).await;

        assert_eq!(released.len(), 1);
        assert!(!local(path, "foundry-task/c1").await);
        assert!(!on_origin(path, "foundry-task/c1").await);
        assert!(evidence_for(&released[0], false).deleted);
        assert!(evidence_for(&released[0], true).deleted);
        assert_eq!(WorkItemStore::load(&ledger).unwrap().find("wi_c1"), Some(&released[0]));
        // Nothing is left to release a second time.
        assert_eq!(
            dispose_released(&ledger, Some(&registry), &worktrees_root(&ledger), &successor).await,
            Vec::<WorkItem>::new()
        );
    }

    /// Preserved, resumed, and the resuming task landed: settlement
    /// discharges the preserved item through its lineage, and its branch goes
    /// even though the landed history differs from it.
    #[tokio::test]
    async fn a_discharged_item_loses_its_preserved_branch() {
        if isolated("blocks::work_disposal::tests::a_discharged_item_loses_its_preserved_branch")
            .await
        {
            return;
        }
        let (_dir, project, registry, ledger, mut item) = fixture().await;
        let path = Path::new(&project.path);
        commit_on(path, "foundry-task/preserved", "main", "preserved-draft").await;
        git(path, &["push", "-q", "origin", "foundry-task/preserved"]).await.unwrap();
        item.disposition = Some(WorkDisposition {
            task_branch: Some("foundry-task/preserved".into()),
            preservation_ref: Some("foundry-task/preserved".into()),
            ..WorkDisposition::default()
        });
        save(&ledger, &item);

        dispose(
            &ledger,
            Some(&registry),
            &worktrees_root(&ledger),
            &mut item,
            Disposal::Discarded,
        )
        .await;

        assert!(!local(path, "foundry-task/preserved").await);
        assert!(!on_origin(path, "foundry-task/preserved").await);
    }

    #[tokio::test]
    async fn an_open_item_keeps_everything() {
        if isolated("blocks::work_disposal::tests::an_open_item_keeps_everything").await {
            return;
        }
        let (_dir, project, registry, ledger, mut item) = fixture().await;
        let path = Path::new(&project.path);
        let worktree = foundry_worktree(&ledger, path, "foundry-task/owned").await;
        item.state = WorkItemState::Preserved;
        item.disposition.as_mut().unwrap().worktree = Some(worktree.display().to_string());
        save(&ledger, &item);
        let before = item.clone();

        dispose(
            &ledger,
            Some(&registry),
            &worktrees_root(&ledger),
            &mut item,
            Disposal::Discarded,
        )
        .await;

        assert_eq!(item, before);
        assert!(worktree.exists());
        assert!(local(path, "foundry-task/owned").await);
    }

    /// Preserved, then closed by its owner: the cancellation event drives
    /// disposal, and `work_item_disposed` records what went.
    #[tokio::test]
    async fn a_closed_preserved_item_is_disposed_of_by_its_cancellation() {
        use foundry_sdk::task_block::TaskBlock as _;
        if isolated("blocks::work_disposal::tests::a_closed_preserved_item_is_disposed_of_by_its_cancellation").await { return; }
        let (_dir, project, registry, ledger, mut item) = fixture().await;
        let path = Path::new(&project.path);
        commit_on(path, "foundry-task/preserved", "main", "unfinished").await;
        git(path, &["push", "-q", "origin", "foundry-task/preserved"]).await.unwrap();
        let worktree = foundry_worktree(&ledger, path, "foundry-task/preserved").await;
        item.state = WorkItemState::Preserved;
        item.disposition = Some(WorkDisposition {
            task_branch: Some("foundry-task/preserved".into()),
            preservation_ref: Some("foundry-task/preserved".into()),
            worktree: Some(worktree.display().to_string()),
            worktree_removed: Some(false),
            ..WorkDisposition::default()
        });
        let disposition = item.disposition.clone();
        item.settle_cancelled("owner closed", disposition, foundry_sdk::time::UtcDateTime::now());
        save(&ledger, &item);
        let block = super::super::DisposeCancelledWork::new(
            Arc::clone(&registry),
            ledger.clone(),
            worktrees_root(&ledger),
        );
        let trigger = super::super::work_ledger::work_item_event(
            foundry_sdk::event::EventType::WorkItemCancelled,
            &foundry_sdk::event::Event::new(
                foundry_sdk::event::EventType::WorkItemCancelled,
                "test".into(),
                foundry_sdk::throttle::Throttle::Full,
                serde_json::json!({}),
            ),
            &item,
        );

        let result = block.execute(&trigger).await.unwrap();

        assert!(result.success);
        assert!(!worktree.exists());
        assert!(!local(path, "foundry-task/preserved").await);
        assert!(!on_origin(path, "foundry-task/preserved").await);
        assert_eq!(result.events.len(), 1);
        let event = &result.events[0];
        assert_eq!(event.event_type, foundry_sdk::event::EventType::WorkItemDisposed);
        let payload: foundry_sdk::payload::WorkItemEventPayload = event.parse_payload().unwrap();
        assert_eq!(payload.item_id, item.id);
        let summary = payload.disposition.unwrap().disposal_summary().unwrap();
        assert!(summary.starts_with("removed worktree"), "{summary}");
        assert!(summary.contains("origin refs/heads/foundry-task/preserved"), "{summary}");
        assert!(!summary.contains("left"), "{summary}");

        // A second cancellation event finds nothing left and announces nothing.
        let again = block.execute(&trigger).await.unwrap();
        assert!(again.events.is_empty(), "{:?}", again.events);
    }
}
