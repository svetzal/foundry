//! Inventory, conservative scheduled settlement, guarded landed-branch
//! cleanup, and the sweep of aged orphan worktrees and task branches.
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, ensure};
use foundry_sdk::event::{Event, EventType};
use foundry_sdk::payload::{
    ReconcileFinding, WorkReconcileCompletedPayload, WorkReconcileStartedPayload,
};
use foundry_sdk::registry::{ProjectEntry, Registry};
use foundry_sdk::task_block::{BlockKind, TaskBlock, TaskBlockResult};
use foundry_sdk::work_item::{WorkItem, WorkItemState, WorkItemStore, ledger_write_gate};

use super::work_supersession::{
    SupersessionProof, is_supersession_candidate, prove_commit_supersession, prove_supersession,
    verified_commit,
};
use super::{SimulatedSuccess, work_ledger::work_item_event};

/// Reconciles registered repositories with the daemon-owned ledger.
/// All paths are injectable so production boundaries can be tested without live state.
pub struct ReconcileWork {
    registry: Arc<RwLock<Registry>>,
    ledger: PathBuf,
    worktrees: PathBuf,
    events: PathBuf,
    output: PathBuf,
    /// How old an orphan must be before the sweep removes it; `None` turns
    /// the sweep off.
    sweep_after: Option<Duration>,
}

/// Environment variable that sets the orphan sweep age, in days.
pub const ORPHAN_SWEEP_DAYS_ENV: &str = "FOUNDRY_ORPHAN_SWEEP_DAYS";

/// Default orphan sweep age, in days.
///
/// Settlement disposes of what an item no longer needs, so the sweep only
/// catches what that missed: a crash, a failed removal, a ledger fault, and
/// the backlog from before disposal existed. Seven days is longer than any
/// task or campaign cycle runs and longer than a weekend. Each reconcile run
/// (every three hours) reports a young orphan as an anomaly, so a person sees
/// it about 56 times before anything is deleted.
pub const DEFAULT_ORPHAN_SWEEP_DAYS: u64 = 7;

const DAY: Duration = Duration::from_secs(24 * 60 * 60);

/// The sweep age from `FOUNDRY_ORPHAN_SWEEP_DAYS`, or `None` when the sweep
/// is off. Unset or unparseable means the default; `0` turns it off.
#[must_use]
pub fn sweep_after_from_env() -> Option<Duration> {
    parse_sweep_after(std::env::var(ORPHAN_SWEEP_DAYS_ENV).ok().as_deref())
}

fn parse_sweep_after(value: Option<&str>) -> Option<Duration> {
    let days = value
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map_or(DEFAULT_ORPHAN_SWEEP_DAYS, u64::from);
    (days > 0).then(|| DAY * u32::try_from(days).unwrap_or(u32::MAX))
}

/// Whether something last written at `written` is older than `max_age`. A
/// write in the future (clock skew) is young.
fn older_than(written: SystemTime, max_age: Duration) -> bool {
    SystemTime::now().duration_since(written).is_ok_and(|age| age > max_age)
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
            sweep_after: sweep_after_from_env(),
        }
    }

    /// Sweep orphans older than `sweep_after`, or never with `None`, rather
    /// than as `FOUNDRY_ORPHAN_SWEEP_DAYS` says.
    #[must_use]
    pub fn with_sweep_after(mut self, sweep_after: Option<Duration>) -> Self {
        self.sweep_after = sweep_after;
        self
    }

    async fn reconcile(&self, trigger: &Event) -> Result<TaskBlockResult> {
        let mut report = WorkReconcileCompletedPayload::default();
        let mut events = Vec::new();
        // A scheduled run carries no request; an unreadable one never prunes.
        let prune = trigger
            .parse_payload::<WorkReconcileStartedPayload>()
            .is_ok_and(|request| request.prune_branches);
        match self.inspect(&mut report, prune).await {
            Ok(verified) => match apply_verified_async(self.ledger.clone(), verified.clone()).await
            {
                Ok(settled) => {
                    for mut item in settled {
                        super::work_disposal::dispose(
                            &self.ledger,
                            Some(&self.registry),
                            &self.worktrees,
                            &mut item,
                            super::work_disposal::Disposal::Landed,
                        )
                        .await;
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
        let pruned = count(&report, "pruned_branch");
        let swept = (count(&report, "swept_worktree"), count(&report, "swept_branch"));
        report.broken_items = count(&report, "broken_item");
        report.unresolved = count(&report, "unresolved");
        report.success = report.errors.is_empty();
        render(&mut report, pruned, swept);
        let destination = self
            .output
            .join(format!("{}.md", foundry_sdk::time::UtcDateTime::now().format("%Y-%m-%d")));
        if let Err(error) = write_report(&destination, &report.markdown).await {
            report.success = false;
            report.errors.push(format!("digest write {}: {error:#}", destination.display()));
            render(&mut report, pruned, swept);
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
        prune: bool,
    ) -> Result<Vec<(WorkItem, String)>> {
        let projects = super::read_registry(&self.registry)?.projects.clone();
        let ledger = self.ledger.clone();
        let store = tokio::task::spawn_blocking(move || load_snapshot(&ledger)).await??;
        let mut verified = Vec::new();
        // A ref any waiting, running or open item owns or starts from is
        // never an orphan, in whichever registered project it is recorded.
        let held: BTreeSet<String> = store
            .items
            .iter()
            .filter(|item| super::work_disposal::holds_refs(item))
            .flat_map(super::work_disposal::referenced)
            .map(|reference| reference.trim_start_matches("refs/heads/").to_string())
            .collect();
        for project in projects {
            let items: Vec<_> = store
                .items
                .iter()
                .filter(|item| {
                    item.project == project.name && (item.state.is_open() || item.is_running())
                })
                .cloned()
                .collect();
            if let Err(error) = self
                .inspect_project(&project, &items, &held, report, &mut verified, prune)
                .await
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
        held: &BTreeSet<String>,
        report: &mut WorkReconcileCompletedPayload,
        verified: &mut Vec<(WorkItem, String)>,
        prune: bool,
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
        let origin_trunk_name = format!("origin trunk refs/remotes/origin/{}", project.branch);
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
                    trunks.push((origin_trunk_name.clone(), commit));
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
        let (owned_paths, mut owned_branches, unknown_running) =
            self.inspect_items(project, items, report, verified, (&trees, &trunks)).await;
        owned_branches.extend(held.iter().cloned());
        let sweep_after = self.sweep_after.filter(|_| !unknown_running);
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
            let mut detail = format!(
                "branch={}; registered={}; directory={}",
                trees.get(path).map_or("detached/unknown", String::as_str),
                trees.contains_key(path),
                Path::new(path).exists()
            );
            let category = match sweep_after {
                Some(max_age)
                    if category == "orphan_worktree"
                        && Path::new(path).starts_with(&root)
                        && newest_write(Path::new(path)).is_none_or(|w| older_than(w, max_age)) =>
                {
                    match super::work_disposal::remove_worktree(
                        checkout,
                        Path::new(path),
                        &self.worktrees,
                    )
                    .await
                    {
                        Ok(()) => "swept_worktree",
                        Err(error) => {
                            // Best-effort: the orphan stays reported with the reason.
                            tracing::warn!(project = %project.name, %path, %error, "sweep retained worktree");
                            detail = format!("{detail}; sweep failed: {error:#}");
                            category
                        }
                    }
                }
                _ => category,
            };
            finding(report, &project.name, category, path, &detail);
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
            let mut proved_by = None;
            for (name, commit) in &trunks {
                let result =
                    git_command(checkout, &["merge-base", "--is-ancestor", hash, commit]).await?;
                match result.exit_code {
                    0 => {
                        trunk_status = format!("ancestor of {name}");
                        proved_by = Some(name);
                        break;
                    }
                    1 => match prove_commit_supersession(checkout, commit, hash).await {
                        Ok(proof) => {
                            trunk_status = format!("{} {name}", proof.relation());
                            proved_by = Some(name);
                            break;
                        }
                        Err(error) => failures.push(format!("not superseded by {name}: {error:#}")),
                    },
                    _ => failures
                        .push(format!("unresolved against {name}: {}", result.stderr.trim())),
                }
                trunk_status = failures.join("; ");
            }
            let detail = format!("commit={hash}; trunk={}; {trunk_status}", project.branch);
            // Pruning acts only on what origin's registered branch holds: a
            // local trunk ahead of origin is not where the work is durable.
            // The report names the first trunk that proved it, usually the
            // local one, so origin is proved separately when it was not that.
            let on_origin = match proved_by {
                _ if !prune || unknown_running => false,
                None => false,
                Some(name) if *name == origin_trunk_name => true,
                Some(_) => match trunks.iter().find(|(name, _)| *name == origin_trunk_name) {
                    Some((_, commit)) => {
                        prove_commit_supersession(checkout, commit, hash).await.is_ok()
                    }
                    None => false,
                },
            };
            if on_origin {
                let remote = reference.starts_with("refs/remotes/origin/");
                let name = format!("refs/heads/{branch}");
                match super::work_disposal::prune(project, &name, remote, true).await {
                    Ok(()) => {
                        finding(report, &project.name, "pruned_branch", reference, &detail);
                        continue;
                    }
                    Err(error) => {
                        // Best-effort: a failed prune leaves the branch reported.
                        tracing::warn!(project = %project.name, %reference, %error, "prune retained branch");
                        trunk_status = format!("{trunk_status}; prune failed: {error:#}");
                    }
                }
            }
            if let Some(max_age) = sweep_after
                && branch.starts_with("foundry-task/")
                && older_than(ref_written(checkout, reference, hash).await, max_age)
            {
                let remote = reference.starts_with("refs/remotes/origin/");
                match sweep_branch(project, branch, reference, hash, remote).await {
                    Ok(()) => {
                        finding(report, &project.name, "swept_branch", reference, &detail);
                        continue;
                    }
                    Err(error) => {
                        // Best-effort: a failed sweep leaves the branch reported.
                        tracing::warn!(project = %project.name, %reference, %error, "sweep retained branch");
                        trunk_status = format!("{trunk_status}; sweep failed: {error:#}");
                    }
                }
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
            if item.state == WorkItemState::Preserved
                && disposition.and_then(|d| d.preservation_ref.as_ref()).is_none()
            {
                finding(
                    report,
                    &project.name,
                    "broken_item",
                    &item.id,
                    "preserved item has no preservation ref",
                );
            }
            if is_supersession_candidate(item) {
                match prove_item_against_trunks(checkout, item, trunks).await {
                    Ok((name, commit, proof)) => {
                        finding(
                            report,
                            &project.name,
                            "informational",
                            &item.id,
                            &format!("superseded by {name} at {commit} ({proof})"),
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
) -> Result<(String, String, SupersessionProof)> {
    ensure!(!trunks.is_empty(), "registered trunk or fetched origin unavailable");
    let mut failures = Vec::new();
    for (name, commit) in trunks {
        match prove_supersession(checkout, commit, item).await {
            Ok(proof) => return Ok((name.clone(), commit.clone(), proof)),
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

/// The newest modification time of `path` and its immediate entries, or
/// `None` when the directory is gone.
fn newest_write(path: &Path) -> Option<SystemTime> {
    let own = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    let entries = std::fs::read_dir(path).into_iter().flatten().flatten();
    Some(
        entries
            .filter_map(|entry| entry.metadata().and_then(|m| m.modified()).ok())
            .fold(own, SystemTime::max),
    )
}

/// When `reference` last changed: the later of its tip's committer date and
/// its newest reflog entry. A branch just created at an old commit is young.
async fn ref_written(checkout: &Path, reference: &str, hash: &str) -> SystemTime {
    let seconds = |text: &str| {
        text.trim()
            .parse::<u64>()
            .ok()
            .map(|s| SystemTime::UNIX_EPOCH + Duration::from_secs(s))
    };
    let committed = git(checkout, &["show", "-s", "--format=%ct", hash])
        .await
        .ok()
        .and_then(|t| seconds(&t));
    let logged = git(checkout, &["reflog", "show", "-1", "--format=%ct", reference, "--"])
        .await
        .ok()
        .and_then(|t| seconds(&t));
    // Unknown means young: the sweep never deletes what it cannot date.
    committed.max(logged).unwrap_or_else(SystemTime::now)
}

/// Delete an aged orphan `foundry-task/*` branch without trunk proof. For an
/// origin branch already gone from origin, the stale tracking ref is removed
/// instead, so it is not reported again.
async fn sweep_branch(
    project: &ProjectEntry,
    branch: &str,
    reference: &str,
    hash: &str,
    remote: bool,
) -> Result<()> {
    let name = format!("refs/heads/{branch}");
    match super::work_disposal::prune(project, &name, remote, false).await {
        Ok(()) => Ok(()),
        Err(error) if remote && error.to_string() == "ref already gone" => {
            git(Path::new(&project.path), &["update-ref", "-d", reference, hash]).await?;
            Ok(())
        }
        Err(error) => Err(error),
    }
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
        // Only an item still exactly as inspected, and still open, settles. An
        // owner's close or cancel, or any other write, wins over the proof.
        let Some(item) = store.items.iter_mut().find(|item| **item == *candidate) else {
            continue;
        };
        if !item.state.is_open() {
            continue;
        }
        let Some(disposition) = item.disposition.as_mut() else {
            continue;
        };
        disposition.landed_commit = Some(commit.clone());
        item.settle_landed(
            &format!("superseded by {commit}"),
            foundry_sdk::time::UtcDateTime::now(),
        );
        let superseded = item.id.clone();
        settled.push(item.clone());
        // What the superseded item resumed or continued is discharged too.
        settled.extend(store.settle_lineage(
            &superseded,
            commit,
            foundry_sdk::time::UtcDateTime::now(),
        ));
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
fn render(report: &mut WorkReconcileCompletedPayload, pruned: usize, swept: (usize, usize)) {
    let mut totals = format!(
        "Settled: {}; orphan worktrees: {}; orphan branches: {}; broken items: {}; unresolved: {}",
        report.settled_ids.len(),
        report.orphan_worktrees,
        report.orphan_branches,
        report.broken_items,
        report.unresolved
    );
    if pruned > 0 {
        totals = format!("{totals}; pruned branches: {pruned}");
    }
    let (swept_worktrees, swept_branches) = swept;
    if swept_worktrees > 0 {
        totals = format!("{totals}; swept worktrees: {swept_worktrees}");
    }
    if swept_branches > 0 {
        totals = format!("{totals}; swept branches: {swept_branches}");
    }
    let mut lines = vec!["# Work reconciliation".to_string(), totals];
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
