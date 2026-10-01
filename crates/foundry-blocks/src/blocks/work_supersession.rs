//! Read-only Git proof for landing-triggered ledger supersession.
//!
//! No fetching, checkout, ref updates or preservation cleanup happens here.
//! Unavailable objects and ambiguous evidence are explicit unresolved results.

use std::path::Path;

use anyhow::{Result, bail, ensure};
use foundry_sdk::work_item::WorkItem;

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
        ensure!(reference.starts_with("refs/heads/"), "not an exact branch or commit");
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

pub(super) async fn prove_supersession(
    path: &Path,
    trunk: &str,
    item: &WorkItem,
) -> Result<String> {
    let reference = item
        .disposition
        .as_ref()
        .and_then(|d| d.preservation_ref.as_deref())
        .ok_or_else(|| anyhow::anyhow!("missing preservation evidence"))?;
    let preserved = preserved_commit(path, reference).await?;
    let ancestry = git(path, &["merge-base", "--is-ancestor", &preserved, trunk]).await?;
    if ancestry.success {
        return Ok(trunk.to_string());
    }
    ensure!(ancestry.exit_code == 1, "ancestry check failed: {}", ancestry.stderr.trim());
    ensure!(
        output(path, &["rev-parse", "--is-shallow-repository"]).await? == "false",
        "shallow history cannot prove patch equivalence"
    );
    let base = output(path, &["merge-base", trunk, &preserved]).await?;
    ensure!(valid_hash(&base), "no common history for patch comparison");
    let range = format!("{trunk}..{preserved}");
    let commits = output(path, &["rev-list", &range]).await?;
    ensure!(!commits.is_empty(), "no preserved commits to compare");
    let merges = output(path, &["rev-list", "--merges", &range]).await?;
    ensure!(merges.is_empty(), "merge commits cannot be proved by git cherry");
    let cherry = output(path, &["cherry", trunk, &preserved]).await?;
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
    Ok(trunk.to_string())
}
