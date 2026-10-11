//! Git proof shared by landing-triggered and scheduled ledger supersession.
//!
//! No fetching, checkout, ref or index updates, or preservation cleanup
//! happens here. The only write is the content-containment proof's merged
//! tree, stored as unreferenced loose objects. Unavailable objects and
//! ambiguous evidence are explicit unresolved results.

use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use foundry_sdk::work_item::{WorkItem, WorkItemState};

async fn git(path: &Path, args: &[&str]) -> Result<foundry_sdk::gateway::CommandResult> {
    crate::shell::run(
        path,
        "git",
        args,
        Some(&[("GIT_NO_REPLACE_OBJECTS".into(), "1".into())]),
        None,
    )
    .await
}

async fn output(path: &Path, args: &[&str]) -> Result<String> {
    let result = git(path, args).await?;
    ensure!(result.success, "git {} failed: {}", args[0], result.stderr.trim());
    Ok(result.stdout.trim().to_string())
}

pub(super) async fn verified_commit(path: &Path, reference: &str) -> Result<String> {
    ensure!(!reference.trim().is_empty(), "missing preservation ref");
    if !valid_hash(reference) {
        ensure!(
            reference.starts_with("refs/heads/") || reference.starts_with("refs/remotes/origin/"),
            "not an exact branch or commit"
        );
        output(path, &["check-ref-format", reference]).await?;
    }
    let commit = output(
        path,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
    )
    .await?;
    ensure!(valid_hash(&commit), "Git returned no unambiguous commit");
    Ok(commit)
}

fn valid_hash(hash: &str) -> bool {
    matches!(hash.len(), 40 | 64) && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
}

async fn preserved_commit(path: &Path, reference: &str) -> Result<String> {
    if let Some(bundle) = reference.strip_prefix("bundle:") {
        // Bundles may advertise several heads. Never guess which one was preserved.
        let heads = output(path, &["bundle", "list-heads", bundle]).await?;
        let lines: Vec<_> = heads.lines().collect();
        ensure!(lines.len() == 1, "bundle does not identify exactly one preserved head");
        let hash = lines[0].split_whitespace().next().unwrap_or_default();
        ensure!(valid_hash(hash), "bundle has no valid preserved commit");
        // A bundle alone is insufficient if its objects are absent locally. Do
        // not import it into the project's authoritative object database.
        return verified_commit(path, hash).await;
    }
    if valid_hash(reference) {
        return verified_commit(path, reference).await;
    }
    let head = if reference.starts_with("refs/heads/") {
        reference.to_string()
    } else {
        format!("refs/heads/{reference}")
    };
    output(path, &["check-ref-format", &head]).await?;
    let local_ref = git(path, &["show-ref", "--verify", "--quiet", &head]).await?;
    let local = if local_ref.success {
        Some(verified_commit(path, &head).await?)
    } else {
        ensure!(
            local_ref.exit_code == 1,
            "local preservation check failed: {}",
            local_ref.stderr.trim()
        );
        None
    };
    // The stored branch name can denote both a local and a durable remote
    // preservation ref. Divergent heads are ambiguous: neither may stand in
    // for the other, even when one happens to be reachable from trunk.
    let origin = git(path, &["config", "--get", "remote.origin.url"]).await?;
    if !origin.success {
        ensure!(origin.exit_code == 1, "origin lookup failed: {}", origin.stderr.trim());
        return local
            .ok_or_else(|| anyhow::anyhow!("preservation ref unavailable; no origin configured"));
    }
    let remote = output(path, &["ls-remote", "--heads", "origin", &head]).await?;
    if remote.is_empty() {
        return local
            .ok_or_else(|| anyhow::anyhow!("preservation ref unavailable locally and remotely"));
    }
    let rows: Vec<_> = remote.lines().collect();
    ensure!(rows.len() == 1, "remote preservation ref is ambiguous");
    let fields: Vec<_> = rows[0].split_whitespace().collect();
    ensure!(
        fields.len() == 2 && fields[1] == head && valid_hash(fields[0]),
        "remote returned ambiguous preservation evidence"
    );
    if let Some(local) = local {
        ensure!(local == fields[0], "local and remote preservation heads disagree");
    }
    // Query without fetching or trusting a stale tracking branch. Unavailable
    // objects leave the obligation open rather than altering the repository.
    verified_commit(path, fields[0]).await
}

/// Which check proved that trunk supersedes a preserved commit.
///
/// Diagnostic only: every proof settles an item with the same reason,
/// `superseded by <commit>`, so readers and branch cleanup need not know it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SupersessionProof {
    /// The preserved commit is an ancestor of trunk.
    Ancestry,
    /// `git cherry` matched every preserved commit to a patch on trunk.
    PatchEquivalence,
    /// Merging the preserved commit into trunk cleanly leaves trunk's tree
    /// unchanged: trunk already holds every change the branch makes.
    ContentContainment,
}

impl SupersessionProof {
    /// How a finding names the relation between the commit and the trunk.
    pub(super) fn relation(self) -> &'static str {
        match self {
            Self::Ancestry => "ancestor of",
            Self::PatchEquivalence => "patch-equivalent to",
            Self::ContentContainment => "content contained in",
        }
    }
}

impl std::fmt::Display for SupersessionProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Ancestry => "ancestry",
            Self::PatchEquivalence => "patch equivalence",
            Self::ContentContainment => "content containment",
        })
    }
}

/// Whether supersession may settle `item`: it is open (`preserved`,
/// `needs_decision` or `failed`) and recorded a preservation ref.
///
/// Settled items, an owner's cancellation included, are terminal. An open item
/// with no ref preserved nothing a trunk could contain.
pub(super) fn is_supersession_candidate(item: &WorkItem) -> bool {
    item.state.is_open() && item.disposition.as_ref().is_some_and(|d| d.preservation_ref.is_some())
}

/// Prove that `trunk` supersedes the work an open item preserved.
pub(super) async fn prove_supersession(
    path: &Path,
    trunk: &str,
    item: &WorkItem,
) -> Result<SupersessionProof> {
    let reference = item
        .disposition
        .as_ref()
        .and_then(|d| d.preservation_ref.as_deref())
        .ok_or_else(|| anyhow::anyhow!("missing preservation evidence"))?;
    let preserved = preserved_commit(path, reference).await?;
    if item.state != WorkItemState::Preserved {
        ensure_commits_of_its_own(path, &preserved, item).await?;
    }
    prove_commit_supersession(path, trunk, &preserved).await
}

/// A `needs_decision` or `failed` run is preserved even when it made no
/// commits. Its branch then still points at the trunk commit it started from,
/// which every later trunk contains, so any proof would succeed. The obligation
/// of such an item is the question or the fault, not work that can land.
///
/// The ledger records no base commit, so the tip's committer time stands in
/// for it: only a tip committed after the item started, at whole seconds, is
/// work of the item's own.
async fn ensure_commits_of_its_own(path: &Path, preserved: &str, item: &WorkItem) -> Result<()> {
    let state = item.state.tag();
    let started = item.started_at.ok_or_else(|| {
        anyhow::anyhow!("{state} item records no start; its own commits cannot be told apart")
    })?;
    let committed = output(
        path,
        &[
            "log",
            "-1",
            "--no-show-signature",
            "--format=%ct",
            "--end-of-options",
            preserved,
        ],
    )
    .await?;
    let committed: i64 = committed
        .parse()
        .with_context(|| format!("unreadable commit time {committed:?}"))?;
    ensure!(
        committed > started.timestamp(),
        "{state} item's preserved branch has no commit made after the item started; \
         its obligation is not work that can land"
    );
    Ok(())
}

/// Apply the same conservative proof to an already resolved inventory commit.
///
/// Ancestry first, then patch equivalence, then content containment. When
/// every proof fails, the error names why each of the last two did.
pub(super) async fn prove_commit_supersession(
    path: &Path,
    trunk: &str,
    preserved: &str,
) -> Result<SupersessionProof> {
    let ancestry = git(path, &["merge-base", "--is-ancestor", preserved, trunk]).await?;
    if ancestry.success {
        return Ok(SupersessionProof::Ancestry);
    }
    ensure!(ancestry.exit_code == 1, "ancestry check failed: {}", ancestry.stderr.trim());
    ensure!(
        output(path, &["rev-parse", "--is-shallow-repository"]).await? == "false",
        "shallow history cannot prove supersession"
    );
    let base = git(path, &["merge-base", trunk, preserved]).await?;
    ensure!(base.success || base.exit_code != 1, "no common history with trunk");
    ensure!(base.success, "git merge-base failed: {}", base.stderr.trim());
    ensure!(valid_hash(base.stdout.trim()), "no common history with trunk");
    let unmatched = match patch_equivalence(path, trunk, preserved).await {
        Ok(()) => return Ok(SupersessionProof::PatchEquivalence),
        Err(error) => error,
    };
    match content_containment(path, trunk, preserved).await {
        Ok(()) => Ok(SupersessionProof::ContentContainment),
        Err(uncontained) => {
            bail!("patch equivalence: {unmatched:#}; content containment: {uncontained:#}")
        }
    }
}

/// Every preserved commit has an equivalent patch on trunk.
async fn patch_equivalence(path: &Path, trunk: &str, preserved: &str) -> Result<()> {
    let range = format!("{trunk}..{preserved}");
    let commits = output(path, &["rev-list", &range]).await?;
    ensure!(!commits.is_empty(), "no preserved commits to compare");
    let merges = output(path, &["rev-list", "--merges", &range]).await?;
    ensure!(merges.is_empty(), "merge commits cannot be proved by git cherry");
    let cherry = output(path, &["cherry", trunk, preserved]).await?;
    let rows: Vec<_> = cherry.lines().collect();
    ensure!(
        !rows.is_empty() && rows.len() == commits.lines().count(),
        "incomplete patch-equivalence evidence"
    );
    for row in rows {
        let fields: Vec<_> = row.split_whitespace().collect();
        ensure!(fields.len() == 2 && valid_hash(fields[1]), "malformed git cherry evidence");
        if fields[0] != "-" {
            bail!("preserved commits have unmatched patches");
        }
    }
    Ok(())
}

/// Trunk already holds every change the preserved commit makes relative to
/// their merge base, however the history between them was rewritten: a squash,
/// or a rebase that resolved conflicts the same way the branch did.
///
/// `git merge-tree --write-tree` (Git 2.38 or later) performs the three-way
/// merge without touching refs, the index or any worktree. It does write the
/// merged tree's objects into the object database as unreferenced loose
/// objects, which garbage collection removes. A conflict, any other failure or
/// unexpected output is never proof.
async fn content_containment(path: &Path, trunk: &str, preserved: &str) -> Result<()> {
    let merge = git(
        path,
        &[
            "merge-tree",
            "--write-tree",
            "--no-messages",
            trunk,
            preserved,
        ],
    )
    .await?;
    ensure!(merge.exit_code != 1, "merging the preserved commit into trunk conflicts");
    ensure!(merge.success, "git merge-tree failed: {}", merge.stderr.trim());
    let lines: Vec<_> = merge.stdout.lines().collect();
    ensure!(
        lines.len() == 1 && valid_hash(lines[0].trim()),
        "git merge-tree returned no unambiguous tree"
    );
    let merged = lines[0].trim();
    let tree = output(
        path,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{trunk}^{{tree}}"),
        ],
    )
    .await?;
    ensure!(valid_hash(&tree), "trunk has no unambiguous tree");
    ensure!(
        merged == tree,
        "trunk lacks preserved changes (merged tree {merged} differs from trunk tree {tree})"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use chrono::{DateTime, Utc};
    use foundry_sdk::work_item::{
        WorkDisposition, WorkItem, WorkItemKind, WorkItemSpec, WorkItemState, WorkLane,
    };

    use super::{
        SupersessionProof, patch_equivalence, prove_commit_supersession, prove_supersession,
    };

    /// Commit times are fixed so the start-time guard is deterministic.
    const BASE_TIME: i64 = 1_800_000_000;

    /// Run Git outside the developer's global and system configuration, with
    /// author and committer times at `time` seconds past the epoch.
    fn git_at(repo: &Path, time: i64, args: &[&str]) -> String {
        let date = format!("@{time} +0000");
        let output = std::process::Command::new("git")
            .current_dir(repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .args([
                "-c",
                "user.name=Regression",
                "-c",
                "user.email=regression@example.test",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        git_at(repo, BASE_TIME, args)
    }

    fn commit(repo: &Path, time: i64, files: &[(&str, &str)], message: &str) -> String {
        for (name, content) in files {
            std::fs::write(repo.join(name), content).unwrap();
        }
        git_at(repo, time, &["add", "--all"]);
        git_at(repo, time, &["commit", "--quiet", "-m", message]);
        git(repo, &["rev-parse", "HEAD"])
    }

    /// A repository whose `main` holds one base commit at `BASE_TIME`.
    fn repository(root: &Path) -> PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "--quiet", "-b", "main"]);
        commit(&repo, BASE_TIME, &[("app.txt", "one\ntwo\nthree\n")], "base");
        repo
    }

    /// Everything Git keeps outside the object database that a proof must not
    /// change: refs and the index.
    fn untouched_state(repo: &Path) -> (String, Option<Vec<u8>>) {
        (
            git(repo, &["for-each-ref", "--format=%(refname) %(objectname)"]),
            std::fs::read(repo.join(".git/index")).ok(),
        )
    }

    fn item(state: WorkItemState, started: Option<i64>, reference: &str) -> WorkItem {
        let mut item = WorkItem::submitted(
            WorkItemSpec {
                project: "regression".into(),
                objective: "preserved deliverable".into(),
                kind: WorkItemKind::Task,
                lane: WorkLane::Interactive,
                origin: "regression".into(),
                trace_id: None,
            },
            Utc::now(),
        );
        item.state = state;
        item.started_at = started.map(|time| DateTime::from_timestamp(time, 0).unwrap());
        item.disposition = Some(WorkDisposition {
            task_branch: None,
            branch_cleanup: Vec::new(),
            verdict: None,
            landed_commit: None,
            preservation_ref: Some(reference.into()),
            worktree: None,
            worktree_removed: None,
        });
        item
    }

    #[tokio::test]
    async fn a_squashed_continuation_is_proved_by_content_containment() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        git(&repo, &["checkout", "--quiet", "-b", "foundry-task/preserved"]);
        commit(&repo, BASE_TIME + 10, &[("app.txt", "ONE\ntwo\nthree\n")], "first half");
        let preserved = commit(&repo, BASE_TIME + 20, &[("added.txt", "new\n")], "second half");
        git(&repo, &["checkout", "--quiet", "main"]);
        commit(
            &repo,
            BASE_TIME + 30,
            &[("app.txt", "ONE\ntwo\nthree\n"), ("added.txt", "new\n")],
            "squashed continuation",
        );
        let trunk = commit(&repo, BASE_TIME + 40, &[("later.txt", "unrelated\n")], "later work");
        let before = untouched_state(&repo);

        let patch = patch_equivalence(&repo, &trunk, &preserved).await.unwrap_err();
        assert!(format!("{patch:#}").contains("unmatched patches"), "{patch:#}");
        let proof = prove_commit_supersession(&repo, &trunk, &preserved).await.unwrap();

        assert_eq!(proof, SupersessionProof::ContentContainment);
        assert_eq!(untouched_state(&repo), before);
    }

    #[tokio::test]
    async fn a_resolution_that_differs_on_a_line_the_branch_changed_stays_open() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        git(&repo, &["checkout", "--quiet", "-b", "foundry-task/preserved"]);
        let preserved =
            commit(&repo, BASE_TIME + 10, &[("app.txt", "one\nbranch\nthree\n")], "branch edit");
        git(&repo, &["checkout", "--quiet", "main"]);
        let trunk = commit(
            &repo,
            BASE_TIME + 20,
            &[("app.txt", "one\nresolved differently\nthree\n")],
            "rebased with a conflict resolution",
        );
        let before = untouched_state(&repo);

        let error = prove_commit_supersession(&repo, &trunk, &preserved).await.unwrap_err();

        let error = format!("{error:#}");
        assert!(error.contains("patch equivalence: preserved commits have unmatched patches"));
        assert!(
            error
                .contains("content containment: merging the preserved commit into trunk conflicts")
        );
        assert_eq!(untouched_state(&repo), before);
    }

    #[tokio::test]
    async fn a_clean_merge_that_would_still_change_trunk_is_not_containment() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        git(&repo, &["checkout", "--quiet", "-b", "foundry-task/preserved"]);
        let preserved = commit(
            &repo,
            BASE_TIME + 10,
            &[("landed.txt", "landed\n"), ("owed.txt", "still owed\n")],
            "two files",
        );
        git(&repo, &["checkout", "--quiet", "main"]);
        let trunk = commit(&repo, BASE_TIME + 20, &[("landed.txt", "landed\n")], "half of it");

        let error = prove_commit_supersession(&repo, &trunk, &preserved).await.unwrap_err();

        assert!(format!("{error:#}").contains("trunk lacks preserved changes"), "{error:#}");
    }

    #[tokio::test]
    async fn unrelated_history_is_never_proof() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        let trunk = git(&repo, &["rev-parse", "main"]);
        git(&repo, &["checkout", "--quiet", "--orphan", "foundry-task/unrelated"]);
        let preserved =
            commit(&repo, BASE_TIME + 10, &[("app.txt", "one\ntwo\nthree\n")], "same tree");

        let error = prove_commit_supersession(&repo, &trunk, &preserved).await.unwrap_err();

        assert!(format!("{error:#}").contains("no common history"), "{error:#}");
    }

    #[tokio::test]
    async fn needs_decision_and_failed_items_are_superseded_only_by_work_of_their_own() {
        let root = tempfile::tempdir().unwrap();
        let repo = repository(root.path());
        // A run that blocked or failed before committing preserves its base.
        git(&repo, &["branch", "foundry-task/no-commits", "main"]);
        git(&repo, &["checkout", "--quiet", "-b", "foundry-task/worked"]);
        commit(&repo, BASE_TIME + 100, &[("work.txt", "done\n")], "the item's own work");
        git(&repo, &["checkout", "--quiet", "main"]);
        git(&repo, &["merge", "--quiet", "--ff-only", "foundry-task/worked"]);
        let trunk = commit(&repo, BASE_TIME + 200, &[("later.txt", "later\n")], "later cycle");
        let started = Some(BASE_TIME + 50);

        for state in [WorkItemState::NeedsDecision, WorkItemState::Failed] {
            let worked = item(state, started, "foundry-task/worked");
            assert_eq!(
                prove_supersession(&repo, &trunk, &worked).await.unwrap(),
                SupersessionProof::Ancestry
            );
            let blocked = item(state, started, "foundry-task/no-commits");
            let error = prove_supersession(&repo, &trunk, &blocked).await.unwrap_err();
            assert!(
                format!("{error:#}").contains("no commit made after the item started"),
                "{error:#}"
            );
            let unstarted = item(state, None, "foundry-task/worked");
            let error = prove_supersession(&repo, &trunk, &unstarted).await.unwrap_err();
            assert!(format!("{error:#}").contains("records no start"), "{error:#}");
        }
        // Preserved work keeps its existing proof: an empty branch has nothing owed.
        let preserved = item(WorkItemState::Preserved, started, "foundry-task/no-commits");
        assert_eq!(
            prove_supersession(&repo, &trunk, &preserved).await.unwrap(),
            SupersessionProof::Ancestry
        );
    }
}
