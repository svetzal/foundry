//! Inventory and conservative scheduled settlement. Never invokes cleanup.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result, ensure};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{ReconcileFinding, WorkReconcileCompletedPayload};
use foundry_sdk::registry::{ProjectEntry, Registry};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::{WorkItem, WorkItemState, WorkItemStore, ledger_write_gate};

use super::work_supersession::{prove_commit_supersession, prove_supersession, verified_commit};
use super::{SimulatedSuccess, work_ledger::work_item_event};

/// Reconciles registered repositories with the daemon-owned ledger.
/// All paths are injectable so production boundaries can be tested without live state.
pub struct ReconcileWork {
    registry: Arc<RwLock<Registry>>,
    ledger: PathBuf,
    worktrees: PathBuf,
    events: PathBuf,
    output: PathBuf,
}

impl ReconcileWork {
    pub fn new(
        registry: Arc<RwLock<Registry>>,
        ledger: PathBuf,
        worktrees: PathBuf,
        events: PathBuf,
        output: PathBuf,
    ) -> Self {
        Self {
            registry,
            ledger,
            worktrees,
            events,
            output,
        }
    }

    async fn reconcile(&self, trigger: &Event) -> Result<TaskBlockResult> {
        let mut report = WorkReconcileCompletedPayload::default();
        let mut events = Vec::new();
        match self.inspect(&mut report).await {
            Ok(verified) => match apply_verified_async(self.ledger.clone(), verified.clone()).await
            {
                Ok(settled) => {
                    for item in settled {
                        report.settled_ids.push(item.id.clone());
                        finding(&mut report, &item.project, "landed", &item.id, &item.reason);
                        let mut event = work_item_event(EventType::WorkItemSettled, trigger, &item)
                            .with_trace_id(item.trace_id.clone());
                        event.project = item.project;
                        events.push(event);
                    }
                    for (item, _) in verified {
                        if !report.settled_ids.contains(&item.id) {
                            finding(
                                &mut report,
                                &item.project,
                                "unresolved",
                                &item.id,
                                "ledger changed during inspection; stale proof not applied",
                            );
                        }
                    }
                }
                Err(error) => report.errors.push(format!("ledger settlement: {error:#}")),
            },
            Err(error) => report.errors.push(format!("inventory: {error:#}")),
        }
        report.orphan_worktrees = count(&report, "orphan_worktree");
        report.orphan_branches = count(&report, "orphan_branch");
        report.broken_items = count(&report, "broken_item");
        report.unresolved = count(&report, "unresolved");
        report.success = report.errors.is_empty();
        render(&mut report);
        let destination = self.output.join(format!("{}.md", chrono::Utc::now().format("%Y-%m-%d")));
        if let Err(error) = write_report(&destination, &report.markdown).await {
            report.success = false;
            report.errors.push(format!("digest write {}: {error:#}", destination.display()));
            render(&mut report);
        } else {
            report.digest_path = Some(destination.display().to_string());
        }
        events.push(trigger.with_payload(EventType::WorkReconcileCompleted, &report)?);
        // Inspection failures are recorded in the completion, allowing ops observation
        // and the synchronous RPC to surface them even after a successful settlement.
        Ok(TaskBlockResult::success("Work reconciliation completed", events))
    }

    async fn inspect(
        &self,
        report: &mut WorkReconcileCompletedPayload,
    ) -> Result<Vec<(WorkItem, String)>> {
        let projects = super::read_registry(&self.registry)?.projects.clone();
        let ledger = self.ledger.clone();
        let store = tokio::task::spawn_blocking(move || load_snapshot(&ledger)).await??;
        let mut verified = Vec::new();
        for project in projects {
            let items: Vec<_> = store
                .items
                .iter()
                .filter(|item| {
                    item.project == project.name && (item.state.is_open() || item.is_running())
                })
                .cloned()
                .collect();
            if let Err(error) = self.inspect_project(&project, &items, report, &mut verified).await
            {
                report.errors.push(format!("{} ({}): {error:#}", project.name, project.path));
                for item in items {
                    finding(
                        report,
                        &project.name,
                        "unresolved",
                        &item.id,
                        "project inspection failed",
                    );
                }
            }
        }
        for item in store.items.iter().filter(|item| item.state.is_open() || item.is_running()) {
            if !super::read_registry(&self.registry)?
                .projects
                .iter()
                .any(|p| p.name == item.project)
            {
                finding(report, &item.project, "broken_item", &item.id, "project not registered");
            }
        }
        Ok(verified)
    }

    async fn inspect_project(
        &self,
        project: &ProjectEntry,
        items: &[WorkItem],
        report: &mut WorkReconcileCompletedPayload,
        verified: &mut Vec<(WorkItem, String)>,
    ) -> Result<()> {
        let checkout = Path::new(&project.path);
        // Startup and continuation resolve FETCH_HEAD after their own fetch.
        // Inventory must leave it, stale tracking refs and local tags intact,
        // including when global or origin configuration enables pruning.
        let fetched = git(
            checkout,
            &[
                "fetch",
                "--no-prune",
                "--porcelain",
                "--verbose",
                "--no-write-fetch-head",
                "--no-tags",
                "--no-prune-tags",
                "origin",
                "+refs/heads/*:refs/remotes/origin/*",
            ],
        )
        .await;
        if let Err(error) = &fetched {
            report.errors.push(format!("{} fetch: {error:#}", project.name));
        }
        let trunk = verified_commit(checkout, &format!("refs/heads/{}", project.branch)).await;
        if let Err(error) = &trunk {
            report
                .errors
                .push(format!("{} registered trunk {}: {error:#}", project.name, project.branch));
        }
        let mut trunks = Vec::new();
        if let Ok(output) = &fetched {
            if let Ok(commit) = &trunk {
                trunks.push((
                    format!("registered trunk refs/heads/{}", project.branch),
                    commit.clone(),
                ));
            }
            let reference = format!("refs/remotes/origin/{}", project.branch);
            // Porcelain reports even up-to-date refs. A retained tracking ref
            // absent from this fetch must never become supersession evidence.
            let observed = output.lines().find_map(|line| {
                let fields: Vec<_> = line.get(1..)?.split_whitespace().collect();
                (fields.len() == 3 && fields[2] == reference).then(|| fields[1])
            });
            let origin_trunk = match observed {
                Some(commit) => verified_commit(checkout, commit).await,
                None => Err(anyhow::anyhow!("not observed by this fetch")),
            };
            match origin_trunk {
                Ok(commit) => {
                    if let Ok(local) = &trunk
                        && local != &commit
                    {
                        finding(
                            report,
                            &project.name,
                            "informational",
                            &reference,
                            &format!(
                                "trunks differ: refs/heads/{}={local}; {reference}={commit}",
                                project.branch
                            ),
                        );
                    }
                    trunks.push((format!("origin trunk {reference}"), commit));
                }
                Err(error) => report
                    .errors
                    .push(format!("{} origin trunk {reference}: {error:#}", project.name)),
            }
        }
        let status = git(checkout, &["status", "--porcelain"]).await?;
        if !status.is_empty() {
            finding(report, &project.name, "dirty_checkout", &project.path, &status);
        }
        let inventory = git(checkout, &["worktree", "list", "--porcelain", "-z"]).await?;
        let mut trees = BTreeMap::new();
        let mut path = String::new();
        for line in inventory.split('\0') {
            if let Some(value) = line.strip_prefix("worktree ") {
                path = value.to_string();
                trees.insert(path.clone(), String::new());
            } else if let Some(value) = line.strip_prefix("branch ") {
                trees.insert(path.clone(), value.to_string());
            }
        }
        let refs = git(
            checkout,
            &[
                "for-each-ref",
                "--format=%(refname) %(objectname)",
                "refs/heads/foundry-task/",
                "refs/remotes/origin/foundry-task/",
            ],
        )
        .await?;
        let (owned_paths, owned_branches, unknown_running) =
            self.inspect_items(project, items, report, verified, (&trees, &trunks)).await;
        let root = self.worktrees.join(crate::workspace::slug(&project.name));
        let directory_root = root.clone();
        let directories = if let Some(directories) =
            tokio::task::spawn_blocking(move || directory_inventory(&directory_root)).await??
        {
            directories
        } else {
            finding(
                report,
                &project.name,
                "informational",
                &root.display().to_string(),
                "Foundry worktree directory absent",
            );
            Vec::new()
        };
        for path in trees.keys().chain(directories.iter()).collect::<BTreeSet<_>>() {
            if path == &project.path || owned_paths.contains(path) {
                continue;
            }
            let foundry = Path::new(path).starts_with(&root)
                || trees
                    .get(path)
                    .is_some_and(|branch| branch.starts_with("refs/heads/foundry-task/"));
            let category = if !foundry {
                "informational"
            } else if unknown_running {
                "unresolved"
            } else {
                "orphan_worktree"
            };
            finding(
                report,
                &project.name,
                category,
                path,
                &format!(
                    "branch={}; registered={}; directory={}",
                    trees.get(path).map_or("detached/unknown", String::as_str),
                    trees.contains_key(path),
                    Path::new(path).exists()
                ),
            );
        }
        for row in refs.lines() {
            let (reference, hash) = row.split_once(' ').context("malformed branch inventory")?;
            let branch = reference
                .strip_prefix("refs/heads/")
                .or_else(|| reference.strip_prefix("refs/remotes/origin/"))
                .context("unexpected inventory ref")?;
            if owned_branches.contains(branch) {
                continue;
            }
            let mut trunk_status = "unresolved: trunk or origin observation failed".to_string();
            let mut failures = Vec::new();
            for (name, commit) in &trunks {
                let result =
                    git_command(checkout, &["merge-base", "--is-ancestor", hash, commit]).await?;
                match result.exit_code {
                    0 => {
                        trunk_status = format!("ancestor of {name}");
                        break;
                    }
                    1 => match prove_commit_supersession(checkout, commit, hash).await {
                        Ok(_) => {
                            trunk_status = format!("patch-equivalent to {name}");
                            break;
                        }
                        Err(error) => failures.push(format!("not superseded by {name}: {error:#}")),
                    },
                    _ => failures
                        .push(format!("unresolved against {name}: {}", result.stderr.trim())),
                }
                trunk_status = failures.join("; ");
            }
            finding(
                report,
                &project.name,
                if unknown_running {
                    "unresolved"
                } else {
                    "orphan_branch"
                },
                reference,
                &format!("commit={hash}; trunk={}; {trunk_status}", project.branch),
            );
        }
        Ok(())
    }
    async fn inspect_items(
        &self,
        project: &ProjectEntry,
        items: &[WorkItem],
        report: &mut WorkReconcileCompletedPayload,
        verified: &mut Vec<(WorkItem, String)>,
        observations: (&BTreeMap<String, String>, &[(String, String)]),
    ) -> (BTreeSet<String>, BTreeSet<String>, bool) {
        let mut owned_paths = BTreeSet::new();
        let mut owned_branches = BTreeSet::new();
        let mut unknown_running = false;
        let event_dir = self.events.clone();
        let snapshot = items.to_vec();
        let running_evidence =
            tokio::task::spawn_blocking(move || running_evidence(&event_dir, &snapshot))
                .await
                .map_err(anyhow::Error::from)
                .and_then(std::convert::identity);
        if let Err(error) = &running_evidence {
            report.errors.push(format!("{} running evidence: {error:#}", project.name));
        }
        let checkout = Path::new(&project.path);
        let (trees, trunks) = observations;
        for item in items {
            let disposition = item.disposition.as_ref();
            let recorded_path = disposition.and_then(|d| d.worktree.clone()).or_else(|| {
                running_evidence
                    .as_ref()
                    .ok()
                    .and_then(|e| e.get(&item.id))
                    .and_then(|e| e.0.clone())
            });
            let recorded_branch =
                disposition.and_then(|d| d.preservation_ref.clone()).or_else(|| {
                    running_evidence
                        .as_ref()
                        .ok()
                        .and_then(|e| e.get(&item.id))
                        .and_then(|e| e.1.clone())
                });
            if let Some(reference) = &recorded_branch {
                owned_branches
                    .insert(reference.strip_prefix("refs/heads/").unwrap_or(reference).to_string());
            }
            if let Some(path) = &recorded_path {
                owned_paths.insert(path.clone());
                if let Some(branch) = trees.get(path) {
                    owned_branches.insert(branch.trim_start_matches("refs/heads/").to_string());
                }
                if !Path::new(path).exists() || !trees.contains_key(path) {
                    // Removal recorded at settlement is normal, not a broken worktree.
                    if disposition.and_then(|d| d.worktree_removed) != Some(true) {
                        finding(
                            report,
                            &project.name,
                            "broken_item",
                            &item.id,
                            &format!("worktree missing from disk or Git inventory: {path}"),
                        );
                    }
                }
            } else if item.is_running() {
                unknown_running = true;
                finding(
                    report,
                    &project.name,
                    "unresolved",
                    &item.id,
                    "active running item has no exact workspace evidence; inventory ownership unresolved",
                );
            }
            if item.state == WorkItemState::Preserved {
                if disposition.and_then(|d| d.preservation_ref.as_ref()).is_none() {
                    finding(
                        report,
                        &project.name,
                        "broken_item",
                        &item.id,
                        "preserved item has no preservation ref",
                    );
                }
                match prove_item_against_trunks(checkout, item, trunks).await {
                    Ok((name, commit)) => {
                        finding(
                            report,
                            &project.name,
                            "informational",
                            &item.id,
                            &format!("superseded by {name} at {commit}"),
                        );
                        verified.push((item.clone(), commit));
                    }
                    Err(error) => finding(
                        report,
                        &project.name,
                        "unresolved",
                        &item.id,
                        &format!("{error:#}"),
                    ),
                }
            }
        }
        (owned_paths, owned_branches, unknown_running)
    }
}

async fn prove_item_against_trunks(
    checkout: &Path,
    item: &WorkItem,
    trunks: &[(String, String)],
) -> Result<(String, String)> {
    ensure!(!trunks.is_empty(), "registered trunk or fetched origin unavailable");
    let mut failures = Vec::new();
    for (name, commit) in trunks {
        match prove_supersession(checkout, commit, item).await {
            Ok(commit) => return Ok((name.clone(), commit)),
            Err(error) => failures.push(format!("{name}: {error:#}")),
        }
    }
    Err(anyhow::anyhow!(failures.join("; ")))
}

async fn git_command(path: &Path, args: &[&str]) -> Result<foundry_sdk::gateway::CommandResult> {
    crate::shell::run(
        path,
        "git",
        args,
        Some(&[
            ("GIT_NO_REPLACE_OBJECTS".into(), "1".into()),
            // Observations must not refresh the checkout's index as a side effect.
            ("GIT_OPTIONAL_LOCKS".into(), "0".into()),
        ]),
        None,
    )
    .await
}

async fn git(path: &Path, args: &[&str]) -> Result<String> {
    let result = git_command(path, args).await?;
    ensure!(result.success, "git {}: {}", args.join(" "), result.stderr.trim());
    Ok(result.stdout.trim_end().to_string())
}

fn load_snapshot(path: &Path) -> Result<WorkItemStore> {
    let _guard = ledger_write_gate().lock().map_err(|e| anyhow::anyhow!("ledger gate: {e}"))?;
    Ok(WorkItemStore::load(path)?)
}

async fn apply_verified_async(
    path: PathBuf,
    verified: Vec<(WorkItem, String)>,
) -> Result<Vec<WorkItem>> {
    tokio::task::spawn_blocking(move || apply_verified(&path, &verified)).await?
}

fn apply_verified(path: &Path, verified: &[(WorkItem, String)]) -> Result<Vec<WorkItem>> {
    let _guard = ledger_write_gate().lock().map_err(|e| anyhow::anyhow!("ledger gate: {e}"))?;
    let mut store = WorkItemStore::load(path)?;
    let mut settled = Vec::new();
    for (candidate, commit) in verified {
        if let Some(item) = store.items.iter_mut().find(|item| **item == *candidate)
            && item.state == WorkItemState::Preserved
            && let Some(disposition) = item.disposition.as_mut()
        {
            disposition.landed_commit = Some(commit.clone());
            item.settle_landed(&format!("superseded by {commit}"), chrono::Utc::now());
            settled.push(item.clone());
        }
    }
    if !settled.is_empty() {
        store.save(path)?;
    }
    Ok(settled)
}

fn directory_inventory(root: &Path) -> Result<Option<Vec<String>>> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("worktree directory {}", root.display()));
        }
    };
    let mut directories = Vec::new();
    for entry in entries {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() || kind.is_symlink() {
            directories.push(entry.path().display().to_string());
        }
    }
    directories.sort();
    Ok(Some(directories))
}

type WorkspaceEvidence = BTreeMap<String, (Option<String>, Option<String>)>;

fn running_evidence(events: &Path, items: &[WorkItem]) -> Result<WorkspaceEvidence> {
    let mut evidence = BTreeMap::new();
    if !items.iter().any(WorkItem::is_running) {
        return Ok(evidence);
    }
    let files = match std::fs::read_dir(events) {
        Ok(files) => files,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(evidence),
        Err(error) => return Err(error.into()),
    };
    // Sort monthly files so newer exact trace evidence wins.
    let mut paths = files.map(|entry| Ok(entry?.path())).collect::<Result<Vec<_>>>()?;
    paths.sort();
    for file in paths
        .into_iter()
        .filter(|file| file.extension().is_some_and(|ext| ext == "jsonl"))
    {
        for line in std::fs::read_to_string(&file)?.lines() {
            let event: Event = serde_json::from_str(line)
                .with_context(|| format!("malformed event in {}", file.display()))?;
            for item in items.iter().filter(|item| {
                item.is_running()
                    && item.trace_id.is_some()
                    && item.trace_id == event.trace_id
                    && item.project == event.project
            }) {
                let entry = evidence.entry(item.id.clone()).or_insert((None, None));
                for (key, value) in [
                    ("task_worktree", &mut entry.0),
                    ("task_branch", &mut entry.1),
                ] {
                    if let Some(text) = event.payload.get(key).and_then(serde_json::Value::as_str) {
                        *value = Some(text.to_string());
                    }
                }
            }
        }
    }
    Ok(evidence)
}

fn finding(
    report: &mut WorkReconcileCompletedPayload,
    project: &str,
    category: &str,
    identity: &str,
    detail: &str,
) {
    report.findings.push(ReconcileFinding {
        project: project.into(),
        category: category.into(),
        identity: identity.into(),
        detail: detail.into(),
    });
}
fn count(report: &WorkReconcileCompletedPayload, category: &str) -> usize {
    report.findings.iter().filter(|finding| finding.category == category).count()
}
fn render(report: &mut WorkReconcileCompletedPayload) {
    let mut lines = vec![
        "# Work reconciliation".to_string(),
        format!(
            "Settled: {}; orphan worktrees: {}; orphan branches: {}; broken items: {}; unresolved: {}",
            report.settled_ids.len(),
            report.orphan_worktrees,
            report.orphan_branches,
            report.broken_items,
            report.unresolved
        ),
    ];
    lines.extend(report.settled_ids.iter().map(|id| format!("- landed: `{id}`")));
    lines.extend(
        report
            .findings
            .iter()
            .map(|f| format!("- {} / {}: `{}` — {}", f.project, f.category, f.identity, f.detail)),
    );
    lines.extend(report.errors.iter().map(|error| format!("- inspection error: {error}")));
    report.markdown = format!("{}\n", lines.join("\n\n"));
}
async fn write_report(destination: &Path, markdown: &str) -> Result<()> {
    let parent = destination.parent().context("digest has no directory")?;
    tokio::fs::create_dir_all(parent).await?;
    let temporary = parent.join(format!(".reconcile-{}.tmp", uuid::Uuid::new_v4()));
    tokio::fs::write(&temporary, markdown).await?;
    if let Err(error) = tokio::fs::rename(&temporary, destination).await {
        // Best-effort: the rename error is propagated; temporary report cleanup
        // cannot change the already durable ledger or hide the primary error.
        if let Err(cleanup) = tokio::fs::remove_file(&temporary).await {
            tracing::warn!(path = %temporary.display(), error = %cleanup, "could not remove temporary reconciliation report");
        }
        return Err(error.into());
    }
    Ok(())
}

impl SimulatedSuccess for ReconcileWork {
    type Outcome = WorkReconcileCompletedPayload;
    fn simulate(&self, _trigger: &Event) -> Self::Outcome {
        WorkReconcileCompletedPayload {
            success: true,
            markdown: "Dry run: reconciliation not performed\n".into(),
            ..Default::default()
        }
    }
    fn success_events(&self, trigger: &Event, outcome: &Self::Outcome) -> Vec<Event> {
        vec![super::event_from_infallible_payload(
            EventType::WorkReconcileCompleted,
            &trigger.project,
            trigger.throttle,
            outcome,
        )]
    }
}
impl TaskBlock for ReconcileWork {
    task_block_meta! { name: "Reconcile Work", kind: Mutator, sinks_on: [WorkReconcileStarted], }
    dry_run_via_simulation!();
    fn execute(&self, trigger: &Event) -> foundry_sdk::task_block::BlockFuture<'_> {
        let trigger = trigger.clone();
        Box::pin(async move { self.reconcile(&trigger).await })
    }
}

#[cfg(test)]
mod tests;
