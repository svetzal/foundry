//! Best-effort disposal of ledger-owned branches after durable landing.
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Result, ensure};
use foundry_sdk::registry::{ProjectEntry, Registry};
use foundry_sdk::work_item::{
    BranchCleanup, WorkItem, WorkItemState, WorkItemStore, ledger_write_gate,
};

use super::work_supersession::{prove_commit_supersession, verified_commit};

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

fn branch(reference: &str) -> Option<String> {
    let name = reference
        .strip_prefix("refs/heads/")
        .or_else(|| reference.strip_prefix("refs/remotes/origin/"))
        .unwrap_or(reference);
    if reference.starts_with("bundle:")
        || name.starts_with("refs/")
        || (matches!(name.len(), 40 | 64) && name.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return None;
    }
    Some(format!("refs/heads/{name}"))
}

fn owned(item: &WorkItem) -> Vec<(String, bool)> {
    let Some(d) = &item.disposition else {
        return Vec::new();
    };
    let mut refs = Vec::new();
    if let Some(reference) = d.task_branch.as_deref().and_then(branch) {
        refs.push((reference, false));
    }
    if let Some(reference) = d.preservation_ref.as_deref().and_then(branch) {
        if !refs.contains(&(reference.clone(), false)) {
            refs.push((reference.clone(), false));
        }
        refs.push((reference, true));
    }
    refs
}

async fn remote_commit(path: &Path, reference: &str) -> Result<String> {
    let rows = git(path, &["ls-remote", "--heads", "origin", reference]).await?;
    let fields: Vec<_> = rows.split_whitespace().collect();
    ensure!(fields.len() == 2 && fields[1] == reference, "remote ref absent or ambiguous");
    let hash = fields[0];
    ensure!(
        matches!(hash.len(), 40 | 64) && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "remote returned an invalid commit"
    );
    Ok(hash.to_owned())
}

async fn delete(project: &ProjectEntry, evidence: &mut BranchCleanup) -> Result<()> {
    let path = Path::new(&project.path);
    git(path, &["check-ref-format", &evidence.reference]).await?;
    let commit = if evidence.remote {
        remote_commit(path, &evidence.reference).await?
    } else {
        verified_commit(path, &evidence.reference).await?
    };
    evidence.commit = Some(commit.clone());
    verified_commit(path, &commit).await?;
    let trunk_ref = format!("refs/heads/{}", project.branch);
    ensure!(evidence.reference != trunk_ref, "registered trunk cannot be deleted");
    // Best-effort: a failed local proof may still be proved by the live origin trunk.
    let local = verified_commit(path, &trunk_ref).await;
    let proved = match local {
        Ok(trunk) => match prove_commit_supersession(path, &trunk, &commit).await {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(%error, "local trunk did not prove branch cleanup");
                false
            }
        },
        Err(error) => {
            tracing::warn!(%error, "local cleanup trunk unavailable");
            false
        }
    };
    if !proved {
        let trunk = remote_commit(path, &trunk_ref).await?;
        prove_commit_supersession(path, &trunk, &commit).await?;
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
        // The lease refuses a ref that moved after the proof.
        git(
            path,
            &[
                "push",
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
    Ok(())
}

/// Run after landing is persisted, and before constructing the settlement event.
/// Ledger decisions and outcome writes use short blocking critical sections;
/// no Git operation runs while the shared ledger gate is held.
pub(super) async fn cleanup(
    ledger: &Path,
    registry: Option<&Arc<RwLock<Registry>>>,
    item: &mut WorkItem,
) {
    if item.state != WorkItemState::Landed || owned(item).is_empty() {
        return;
    }
    let projects = registry.and_then(|registry| match registry.read() {
        Ok(registry) => Some(registry.projects.clone()),
        Err(error) => {
            // Best-effort: without registry authority, retain refs and the durable landing.
            tracing::warn!(%error, "branch cleanup registry unavailable");
            None
        }
    });
    let Some(projects) = projects else { return };
    let Some(project) = projects.iter().find(|project| project.name == item.project) else {
        return;
    };
    let result = cleanup_owned(ledger, &projects, project, item).await;
    match result {
        Ok(updated) => *item = updated,
        Err(error) => {
            // Best-effort: landing is already durable; evidence persistence or
            // worker failure must not turn a completed task into a failure.
            tracing::warn!(%error, "branch cleanup could not record evidence");
        }
    }
}

async fn common_directory(project: &ProjectEntry) -> Result<PathBuf> {
    let directory = git(
        Path::new(&project.path),
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    Ok(std::fs::canonicalize(directory)?)
}

async fn cleanup_owned(
    ledger: &Path,
    projects: &[ProjectEntry],
    project: &ProjectEntry,
    snapshot: &WorkItem,
) -> Result<WorkItem> {
    let directory = common_directory(project).await?;
    let mut repository_projects = Vec::new();
    for other in projects {
        // The registry slug also covers separate clones sharing origin refs.
        // The common directory covers aliases and linked worktrees.
        let shared = if other.repo.eq_ignore_ascii_case(&project.repo) {
            true
        } else {
            match common_directory(other).await {
                Ok(other_directory) => other_directory == directory,
                Err(error) => {
                    // Best-effort: unresolved repository identity cannot authorize
                    // deleting a ref that this project's unlanded item also owns.
                    tracing::warn!(project = %other.name, %error, "cleanup repository unknown");
                    true
                }
            }
        };
        if shared {
            repository_projects.push(other.name.clone());
        }
    }
    let path = ledger.to_owned();
    let expected = snapshot.clone();
    let decisions = tokio::task::spawn_blocking(move || -> Result<_> {
        let _guard = ledger_write_gate().lock().map_err(|e| anyhow::anyhow!("ledger gate: {e}"))?;
        let store = WorkItemStore::load(&path)?;
        ensure!(
            store.find(&expected.id) == Some(&expected),
            "ledger changed before branch cleanup"
        );
        Ok(owned(&expected)
            .into_iter()
            .map(|(reference, remote)| {
                let shared = store.items.iter().any(|other| {
                    other.id != expected.id
                        && repository_projects.contains(&other.project)
                        && other.state != WorkItemState::Landed
                        && owned(other).iter().any(|(other_ref, _)| *other_ref == reference)
                });
                (reference, remote, shared)
            })
            .collect::<Vec<_>>())
    })
    .await??;

    let mut observations = Vec::new();
    for (reference, remote, shared) in decisions {
        let mut evidence = BranchCleanup {
            reference,
            remote,
            commit: None,
            deleted: false,
            error: None,
        };
        let outcome = if shared {
            Err(anyhow::anyhow!("ref also owned by an unlanded item in this repository"))
        } else {
            delete(project, &mut evidence).await
        };
        if let Err(error) = outcome {
            // Best-effort: cleanup cannot undo a durable landing.
            tracing::warn!(item_id = %snapshot.id, reference = %evidence.reference, %error, "branch retained");
            evidence.error = Some(error.to_string());
        }
        observations.push(evidence);
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
            .ok_or_else(|| anyhow::anyhow!("item removed during branch cleanup"))?;
        if current.state != WorkItemState::Landed || owned(current) != owned(&expected) {
            // Best-effort: a concurrent state or ownership change takes priority
            // over cleanup evidence; never restore an obsolete settlement.
            tracing::warn!(item_id = %expected.id, "item changed during branch cleanup");
            return Ok(current.clone());
        }
        if let Some(disposition) = current.disposition.as_mut() {
            disposition.branch_cleanup = observations;
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
            chrono::Utc::now(),
        );
        item.settle_landed("landed", chrono::Utc::now());
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
        if isolated("blocks::work_branch_cleanup::tests::unlanded_owner_in_another_registered_project_keeps_shared_refs").await { return; }
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
            assert!(remote_commit(path, "refs/heads/foundry-task/owned").await.is_ok());
            let evidence = &item.disposition.as_ref().unwrap().branch_cleanup;
            assert_eq!(evidence.len(), 2);
            assert!(
                evidence
                    .iter()
                    .all(|e| !e.deleted && e.error.as_deref().unwrap().contains("unlanded item"))
            );
            assert_eq!(WorkItemStore::load(&ledger).unwrap().find(&other.id), Some(&other));
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn git_cleanup_allows_concurrent_ledger_write_and_preserves_its_changes() {
        use std::os::unix::fs::PermissionsExt;
        if isolated("blocks::work_branch_cleanup::tests::git_cleanup_allows_concurrent_ledger_write_and_preserves_its_changes").await { return; }
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
        std::fs::write(&hook, "#!/bin/sh\ntouch cleanup-ready\ni=0\nwhile [ ! -e cleanup-release ] && [ \"$i\" -lt 100 ]; do sleep 0.05; i=$((i+1)); done\ntest -e cleanup-release\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
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
        if isolated("blocks::work_branch_cleanup::tests::landed_task_deletes_only_its_owned_local_branch_and_records_commit").await { return; }
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
        if isolated("blocks::work_branch_cleanup::tests::unproven_checked_out_and_unlanded_refs_are_retained").await { return; }
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
        if isolated("blocks::work_branch_cleanup::tests::landed_parent_deletes_origin_ref_and_remote_rejection_keeps_landing").await { return; }
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
}
