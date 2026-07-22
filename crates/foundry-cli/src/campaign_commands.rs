//! CLI handlers for the `foundry campaign` subcommands.
//!
//! Inspection commands (`add`, `list`, `show`) always operate on `campaigns.json`
//! directly — they never need the daemon.
//!
//! The `pause` and `resume` mutation commands mirror the sentinel
//! daemon-or-offline protocol: the online path calls the `PauseCampaign` /
//! `ResumeCampaign` gRPC RPC and renders the response from the typed detail
//! field; the offline fallback (and graceful-degradation path when the daemon
//! is unreachable) mutates the store file directly via
//! [`CampaignStore::lock_exclusive`].
//!
//! The `decide` mutation command is intentionally stricter: without explicit
//! `--offline` it requires a reachable daemon and must not mutate the store
//! file when the daemon is unavailable.
//!
//! The `advance` command is out of scope for daemon fallback changes in this
//! slice — its workflow dispatch semantics are handled separately.

use std::path::{Component, Path};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use foundry_sdk::campaign::{Campaign, CampaignStatus, CampaignStore, OwnerDecision};
use foundry_sdk::registry::Registry;

use crate::daemon::{connect_daemon_required, status_to_anyhow, with_daemon_or_offline_render};
use crate::proto::{
    CompleteCampaignRequest, DecideCampaignRequest, PauseCampaignRequest, ResumeCampaignRequest,
};
use crate::render;
use crate::workflow_commands::WorkflowRunner;

pub fn add(store_path: &Path, registry_path: &Path, file: &Path) -> Result<()> {
    let content = std::fs::read_to_string(file)
        .with_context(|| format!("read campaign definition {}", file.display()))?;
    let campaign: Campaign = serde_json::from_str(&content)
        .with_context(|| format!("parse campaign definition {}", file.display()))?;
    validate_campaign_definition(&campaign, registry_path)?;
    let name = campaign.name.clone();
    let mut guard = CampaignStore::lock_exclusive(store_path)?;
    guard.store.add(campaign)?;
    guard.save()?;
    println!("Added campaign '{name}'.");
    Ok(())
}

fn validate_campaign_definition(campaign: &Campaign, registry_path: &Path) -> Result<()> {
    campaign.validate()?;
    let registry = Registry::load(registry_path)
        .with_context(|| format!("load registry {}", registry_path.display()))?;
    let project = registry.find_project(&campaign.project).ok_or_else(|| {
        anyhow::anyhow!(
            "campaign '{}' references unknown registered project '{}'",
            campaign.name,
            campaign.project
        )
    })?;
    validate_context_paths(campaign, Path::new(&project.path))
}

fn validate_context_paths(campaign: &Campaign, repo_path: &Path) -> Result<()> {
    let repo = repo_path.canonicalize().with_context(|| {
        format!(
            "campaign '{}' project '{}' checkout is unreadable: {}",
            campaign.name,
            campaign.project,
            repo_path.display()
        )
    })?;
    for context_path in &campaign.context_paths {
        validate_context_path(campaign, &repo, context_path)?;
    }
    Ok(())
}

fn validate_context_path(campaign: &Campaign, repo: &Path, context_path: &str) -> Result<()> {
    let relative = Path::new(context_path);
    if relative.is_absolute() {
        bail!(
            "campaign '{}' context path must be repository-relative: {}",
            campaign.name,
            context_path
        );
    }
    if relative.components().any(|component| matches!(component, Component::ParentDir)) {
        bail!(
            "campaign '{}' context path must not traverse parent directories: {}",
            campaign.name,
            context_path
        );
    }

    let candidate = repo.join(relative);
    if !candidate.exists() {
        bail!("campaign '{}' context path missing: {}", campaign.name, context_path);
    }

    let canonical = candidate.canonicalize().with_context(|| {
        format!("campaign '{}' context path is unreadable: {}", campaign.name, context_path)
    })?;
    if !canonical.starts_with(repo) {
        bail!(
            "campaign '{}' context path escapes project checkout: {}",
            campaign.name,
            context_path
        );
    }
    if !canonical.is_file() {
        bail!("campaign '{}' context path must be a file: {}", campaign.name, context_path);
    }
    Ok(())
}

pub fn list(store_path: &Path) -> Result<()> {
    let store = CampaignStore::load(store_path)?;
    if store.campaigns.is_empty() {
        println!("No campaigns configured.");
    } else {
        print!("{}", render::campaign::campaign_table(&store.campaigns));
    }
    Ok(())
}

pub fn show(store_path: &Path, name: &str) -> Result<()> {
    let store = CampaignStore::load(store_path)?;
    let campaign =
        store.find(name).ok_or_else(|| anyhow::anyhow!("campaign '{name}' not found"))?;
    print!("{}", render::campaign::campaign_detail(campaign));
    Ok(())
}

/// Pause a campaign via the daemon (online path) or the campaign store
/// (offline/fallback path).
///
/// Returns the rendered output string. Callers print it so that tests can
/// inspect the value without capturing stdout.
///
/// **Online path**: calls `PauseCampaign` gRPC, renders from
/// `PauseCampaignResponse.campaign` — no store reads on this path.
///
/// **Offline / fallback path**: acquires an exclusive lock on the store file
/// and mutates `status` directly, mirroring the behaviour of
/// `CampaignStore::lock_exclusive` used by the daemon itself.
pub async fn pause_and_render(
    store_path: &Path,
    addr: &str,
    offline: bool,
    name: &str,
) -> Result<String> {
    // Clone owned values so each closure can capture independently.
    let name_for_daemon = name.to_string();
    let name_for_file = name.to_string();
    let store_path_for_file = store_path.to_path_buf();
    with_daemon_or_offline_render(
        addr,
        offline,
        move |mut client| async move {
            let req = PauseCampaignRequest {
                name: name_for_daemon,
            };
            let resp = client.pause_campaign(req).await.map_err(status_to_anyhow)?;
            let detail = resp.into_inner().campaign.ok_or_else(|| {
                anyhow::anyhow!("daemon returned no campaign in PauseCampaignResponse")
            })?;
            // Render from the typed proto response — never re-read from disk.
            Ok(render::campaign::campaign_detail_proto(&detail))
        },
        move || {
            // Offline / graceful-degradation fallback: mutate the store directly.
            pause_offline(&store_path_for_file, &name_for_file)?;
            Ok(format!("Campaign '{name_for_file}' is now paused.\n"))
        },
    )
    .await
}

/// Pause a campaign, printing the result to stdout.
///
/// See [`pause_and_render`] for the full description of the online/offline
/// dispatch logic.
pub async fn pause(store_path: &Path, addr: &str, offline: bool, name: &str) -> Result<()> {
    let output = pause_and_render(store_path, addr, offline, name).await?;
    print!("{output}");
    Ok(())
}

/// Record an owner decision via the daemon (online path) or the campaign
/// store (explicit offline path).
pub async fn decide_and_render(
    store_path: &Path,
    addr: &str,
    offline: bool,
    name: &str,
    decision: &str,
) -> Result<String> {
    if offline {
        return decide_offline_and_render(store_path, name, decision);
    }

    let mut client =
        connect_daemon_required(addr, &format!("foundry campaign decide {name} --offline")).await?;
    let req = DecideCampaignRequest {
        name: name.to_string(),
        decision: decision.to_string(),
    };
    let resp = client.decide_campaign(req).await.map_err(status_to_anyhow)?;
    let detail = resp
        .into_inner()
        .campaign
        .ok_or_else(|| anyhow::anyhow!("daemon returned no campaign in DecideCampaignResponse"))?;
    Ok(render::campaign::campaign_detail_proto(&detail))
}

pub async fn decide(
    store_path: &Path,
    addr: &str,
    offline: bool,
    name: &str,
    decision: &str,
) -> Result<()> {
    let output = decide_and_render(store_path, addr, offline, name, decision).await?;
    print!("{output}");
    Ok(())
}

/// Mark an authorized campaign complete and retain the owner's reason.
pub async fn complete_and_render(
    store_path: &Path,
    addr: &str,
    offline: bool,
    name: &str,
    reason: &str,
) -> Result<String> {
    if offline {
        return complete_offline_and_render(store_path, name, reason);
    }

    let mut client = connect_daemon_required(
        addr,
        &format!("foundry campaign complete {name} --reason <reason> --offline"),
    )
    .await?;
    let resp = client
        .complete_campaign(CompleteCampaignRequest {
            name: name.to_string(),
            reason: reason.to_string(),
        })
        .await
        .map_err(status_to_anyhow)?;
    let detail = resp.into_inner().campaign.ok_or_else(|| {
        anyhow::anyhow!("daemon returned no campaign in CompleteCampaignResponse")
    })?;
    Ok(render::campaign::campaign_detail_proto(&detail))
}

pub async fn complete(
    store_path: &Path,
    addr: &str,
    offline: bool,
    name: &str,
    reason: &str,
) -> Result<()> {
    let output = complete_and_render(store_path, addr, offline, name, reason).await?;
    print!("{output}");
    Ok(())
}

/// Direct-store pause used by the offline / fallback path.
///
/// Acquires an exclusive lock, sets `status = Paused`, and saves.
/// Does NOT emit any events; that is the daemon's responsibility.
fn pause_offline(store_path: &Path, name: &str) -> Result<()> {
    let mut guard = CampaignStore::lock_exclusive(store_path)?;
    let campaign = guard
        .store
        .find_mut(name)
        .ok_or_else(|| anyhow::anyhow!("campaign '{name}' not found"))?;
    campaign.status = CampaignStatus::Paused;
    guard.save()?;
    Ok(())
}

fn decide_offline_and_render(store_path: &Path, name: &str, decision: &str) -> Result<String> {
    let decision = decision.trim();
    if decision.is_empty() {
        bail!("decision must be non-empty");
    }
    let mut guard = CampaignStore::lock_exclusive(store_path)?;
    let campaign = guard
        .store
        .find_mut(name)
        .ok_or_else(|| anyhow::anyhow!("campaign '{name}' not found"))?;
    let authorized_by = campaign.authorized_by.clone().ok_or_else(|| {
        anyhow::anyhow!("campaign '{name}' has not been authorized; decide requires authorized_by")
    })?;
    if campaign.status != CampaignStatus::Escalated {
        bail!("campaign '{name}' is '{}'; decide requires Escalated status", campaign.status);
    }
    campaign.owner_decisions.push(OwnerDecision {
        decision: decision.to_string(),
        authorized_by,
        decided_at: Utc::now(),
    });
    campaign.status = CampaignStatus::Active;
    let rendered = render::campaign::campaign_detail(campaign);
    guard.save()?;
    Ok(rendered)
}

fn complete_offline_and_render(store_path: &Path, name: &str, reason: &str) -> Result<String> {
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("reason must be non-empty");
    }
    let mut guard = CampaignStore::lock_exclusive(store_path)?;
    let campaign = guard
        .store
        .find_mut(name)
        .ok_or_else(|| anyhow::anyhow!("campaign '{name}' not found"))?;
    let authorized_by = campaign.authorized_by.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "campaign '{name}' has not been authorized; complete requires authorized_by"
        )
    })?;
    if campaign.status == CampaignStatus::Completed {
        return Ok(render::campaign::campaign_detail(campaign));
    }
    campaign.owner_decisions.push(OwnerDecision {
        decision: format!("Completed externally: {reason}"),
        authorized_by,
        decided_at: Utc::now(),
    });
    campaign.status = CampaignStatus::Completed;
    campaign.pending_run_result = None;
    let rendered = render::campaign::campaign_detail(campaign);
    guard.save()?;
    Ok(rendered)
}

/// Resume a campaign via the daemon (online path) or the campaign store
/// (offline/fallback path).
///
/// Returns the rendered output string. Callers print it so that tests can
/// inspect the value without capturing stdout.
///
/// **Online path**: calls `ResumeCampaign` gRPC, renders from
/// `ResumeCampaignResponse.campaign` — no store reads on this path.
///
/// **Offline / fallback path**: acquires an exclusive lock on the store file
/// and mutates `status`, `budget.max_cycles` directly, mirroring the behaviour
/// of `CampaignStore::lock_exclusive` used by the daemon itself.
/// `pending_run_result` is never cleared or overwritten on this path.
pub async fn resume_and_render(
    store_path: &Path,
    addr: &str,
    offline: bool,
    name: &str,
    add_cycles: u64,
) -> Result<String> {
    // Clone owned values so each closure can capture independently.
    let name_for_daemon = name.to_string();
    let name_for_file = name.to_string();
    let store_path_for_file = store_path.to_path_buf();
    with_daemon_or_offline_render(
        addr,
        offline,
        move |mut client| async move {
            let req = ResumeCampaignRequest {
                name: name_for_daemon,
                add_cycles,
            };
            let resp = client.resume_campaign(req).await.map_err(status_to_anyhow)?;
            let detail = resp.into_inner().campaign.ok_or_else(|| {
                anyhow::anyhow!("daemon returned no campaign in ResumeCampaignResponse")
            })?;
            // Render from the typed proto response — never re-read from disk.
            Ok(render::campaign::campaign_detail_proto(&detail))
        },
        move || {
            // Offline / graceful-degradation fallback: mutate the store directly.
            resume_offline(&store_path_for_file, &name_for_file, add_cycles)?;
            Ok(format!("Campaign '{name_for_file}' is now active.\n"))
        },
    )
    .await
}

/// Resume a campaign, printing the result to stdout.
///
/// See [`resume_and_render`] for the full description of the online/offline
/// dispatch logic.
pub async fn resume(
    store_path: &Path,
    addr: &str,
    offline: bool,
    name: &str,
    add_cycles: u64,
) -> Result<()> {
    let output = resume_and_render(store_path, addr, offline, name, add_cycles).await?;
    print!("{output}");
    Ok(())
}

/// Direct-store resume used by the offline / fallback path.
///
/// Acquires an exclusive lock, validates `authorized_by` and the exhausted-budget
/// guard, applies `add_cycles` to `budget.max_cycles`, sets `status = Active`,
/// and saves.  `pending_run_result` is left untouched.
/// Does NOT emit any events; that is the daemon's responsibility.
fn resume_offline(store_path: &Path, name: &str, add_cycles: u64) -> Result<()> {
    let mut guard = CampaignStore::lock_exclusive(store_path)?;
    let campaign = guard
        .store
        .find_mut(name)
        .ok_or_else(|| anyhow::anyhow!("campaign '{name}' not found"))?;
    if campaign.authorized_by.is_none() {
        bail!("campaign '{name}' cannot resume until authorized_by is set");
    }
    if add_cycles == 0 && campaign.cycles_completed >= campaign.budget.max_cycles {
        bail!(
            "campaign '{name}' exhausted its cycle budget; pass --add-cycles N to authorize more work"
        );
    }
    campaign.budget.max_cycles = campaign
        .budget
        .max_cycles
        .checked_add(add_cycles)
        .ok_or_else(|| anyhow::anyhow!("campaign '{name}' cycle budget overflow"))?;
    campaign.status = CampaignStatus::Active;
    // pending_run_result is intentionally left untouched.
    guard.save()?;
    Ok(())
}

pub async fn advance(addr: &str, store_path: &Path, name: &str) -> Result<()> {
    let store = CampaignStore::load(store_path)?;
    let campaign =
        store.find(name).ok_or_else(|| anyhow::anyhow!("campaign '{name}' not found"))?;
    if campaign.status == CampaignStatus::Paused {
        bail!(
            "campaign '{name}' is {}; run `foundry campaign resume {name}` before advancing",
            campaign.status
        );
    }
    if campaign.status == CampaignStatus::Escalated {
        bail!(
            "campaign '{name}' is escalated; record owner policy with `foundry campaign decide {name} --decision \"...\"` or use `foundry campaign resume {name}` when the escalation was budget-only"
        );
    }
    if campaign.status == CampaignStatus::Completed {
        bail!("campaign '{name}' is already completed");
    }
    let project = campaign.project.clone();
    let runner = WorkflowRunner::new(addr, &project);
    let (event_id, _) = runner
        .run_workflow(
            "campaign_advance_requested",
            serde_json::json!({"campaign": name}),
            |event_type, _| event_type == "campaign_advance_completed",
        )
        .await?;
    runner.show_trace(&event_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use foundry_sdk::registry::{ActionFlags, ProjectEntry, Registry, Stack};

    fn write_registry(dir: &tempfile::TempDir, project: &str, repo: &Path) -> std::path::PathBuf {
        let registry_path = dir.path().join("registry.json");
        Registry {
            version: 2,
            projects: vec![ProjectEntry {
                name: project.to_string(),
                path: repo.display().to_string(),
                stack: Stack::Rust,
                agent: "codex".to_string(),
                repo: "owner/repo".to_string(),
                branch: "main".to_string(),
                skip: None,
                actions: ActionFlags::default(),
                install: None,
                installs_skill: None,
                notes: None,
                timeout_secs: None,
                audit_exceptions: vec![],
            }],
        }
        .save(&registry_path)
        .unwrap();
        registry_path
    }

    fn write_campaign_file(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let path = dir.path().join("campaign.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    // Offline path: pause_and_render with offline=true must not touch gRPC
    // and must flip the status to Paused in the store file.
    #[tokio::test]
    async fn offline_pause_sets_status_paused_in_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("campaigns.json");
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let registry_path = write_registry(&dir, "p", &repo);
        let file = write_campaign_file(
            &dir,
            r#"{
                "name":"c", "project":"p", "mission":"ship",
                "done_evidence":[{"kind":"review","statement":"shipped"}],
                "authorized_by":"tester"
            }"#,
        );
        add(&store, &registry_path, &file).unwrap();

        let output = pause_and_render(&store, "http://127.0.0.1:0", true, "c").await.unwrap();

        assert_eq!(
            CampaignStore::load(&store).unwrap().find("c").unwrap().status,
            CampaignStatus::Paused,
            "store must reflect Paused after offline pause"
        );
        assert!(
            output.contains("paused"),
            "offline output must mention 'paused'; got: {output:?}"
        );
    }

    // Offline round-trip: add → list → show → pause all succeed against the
    // same store file.
    #[tokio::test]
    async fn add_list_show_pause_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("campaigns.json");
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let registry_path = write_registry(&dir, "p", &repo);
        let file = write_campaign_file(
            &dir,
            r#"{
                "name":"c", "project":"p", "mission":"ship",
                "done_evidence":[{"kind":"review","statement":"shipped"}],
                "authorized_by":"tester"
            }"#,
        );
        add(&store, &registry_path, &file).unwrap();
        list(&store).unwrap();
        show(&store, "c").unwrap();
        pause(&store, "http://127.0.0.1:0", true, "c").await.unwrap();
        assert_eq!(
            CampaignStore::load(&store).unwrap().find("c").unwrap().status,
            CampaignStatus::Paused
        );
    }

    #[tokio::test]
    async fn offline_complete_records_reason_and_clears_pending_result() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("campaigns.json");
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let registry_path = write_registry(&dir, "p", &repo);
        let file = write_campaign_file(
            &dir,
            r#"{
                "name":"c", "project":"p", "mission":"ship",
                "done_evidence":[{"kind":"review","statement":"shipped"}],
                "authorized_by":"tester", "status":"paused",
                "pending_run_result":{
                    "project":"p", "success":true, "landed":true,
                    "summary":"done", "preservation_ref":null,
                    "verdict":"complete", "campaign":"c"
                }
            }"#,
        );
        add(&store, &registry_path, &file).unwrap();

        let output = complete_and_render(
            &store,
            "http://127.0.0.1:0",
            true,
            "c",
            "Production evidence confirms the mission shipped.",
        )
        .await
        .unwrap();

        let saved = CampaignStore::load(&store).unwrap();
        let campaign = saved.find("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Completed);
        assert!(campaign.pending_run_result.is_none());
        assert_eq!(campaign.owner_decisions.len(), 1);
        assert_eq!(
            campaign.owner_decisions[0].decision,
            "Completed externally: Production evidence confirms the mission shipped."
        );
        assert!(output.contains("completed"));
    }

    #[tokio::test]
    async fn offline_complete_requires_owner_authorization_and_reason() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("campaigns.json");
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let registry_path = write_registry(&dir, "p", &repo);
        let file = write_campaign_file(
            &dir,
            r#"{
                "name":"c", "project":"p", "mission":"ship",
                "done_evidence":[{"kind":"review","statement":"shipped"}],
                "status":"paused"
            }"#,
        );
        add(&store, &registry_path, &file).unwrap();

        let unauthorized = complete_and_render(&store, "http://127.0.0.1:0", true, "c", "shipped")
            .await
            .unwrap_err();
        assert!(unauthorized.to_string().contains("authorized"));

        let empty = complete_and_render(&store, "http://127.0.0.1:0", true, "c", "   ")
            .await
            .unwrap_err();
        assert!(empty.to_string().contains("non-empty"));
    }

    #[tokio::test]
    async fn exhausted_campaign_requires_and_applies_explicit_added_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                name: "c".to_string(),
                project: "p".to_string(),
                mission: "ship".to_string(),
                intent_refs: vec![],
                context_paths: vec![],
                done_evidence: vec![foundry_sdk::campaign::DoneEvidence::Review {
                    statement: "shipped".to_string(),
                }],
                budget: foundry_sdk::campaign::CampaignBudget { max_cycles: 2 },
                escalation: vec![],
                status: CampaignStatus::Escalated,
                cycles_completed: 2,
                cycles_landed: 0,
                authorized_by: Some("tester".to_string()),
                agent_provider: None,
                last_run_event_id: None,
                owner_decisions: vec![],
                pending_run_result: None,
            })
            .unwrap();
        store.save(&store_path).unwrap();

        // Offline path rejects add_cycles=0 when budget is exhausted.
        let error = resume_and_render(&store_path, "http://127.0.0.1:0", true, "c", 0)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("--add-cycles"));

        // Offline path applies add_cycles=1 and sets status to Active.
        resume_and_render(&store_path, "http://127.0.0.1:0", true, "c", 1)
            .await
            .unwrap();
        let resumed = CampaignStore::load(&store_path).unwrap();
        let campaign = resumed.find("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Active);
        assert_eq!(campaign.budget.max_cycles, 3);
    }

    // Offline path: resume_and_render with offline=true must not touch gRPC,
    // must flip the status to Active in the store file, and must reject
    // add_cycles=0 when the campaign budget is exhausted.
    #[tokio::test]
    async fn offline_resume_mutates_store_and_rejects_exhausted_with_zero_add_cycles() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("campaigns.json");
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let registry_path = write_registry(&dir, "p", &repo);
        // Seed a paused campaign with an exhausted budget.
        let file = write_campaign_file(
            &dir,
            r#"{
                "name":"c", "project":"p", "mission":"ship",
                "done_evidence":[{"kind":"review","statement":"shipped"}],
                "authorized_by":"tester",
                "budget":{"max_cycles":2},
                "cycles_completed":2,
                "status":"paused"
            }"#,
        );
        add(&store, &registry_path, &file).unwrap();

        // Exhausted + add_cycles=0 must be rejected — store must be unchanged.
        let err = resume_and_render(&store, "http://127.0.0.1:0", true, "c", 0).await.unwrap_err();
        assert!(
            err.to_string().contains("--add-cycles"),
            "error must mention --add-cycles; got: {err}"
        );
        assert_eq!(
            CampaignStore::load(&store).unwrap().find("c").unwrap().status,
            CampaignStatus::Paused,
            "store must remain Paused after rejected resume"
        );

        // add_cycles=1 must succeed and set status to Active.
        let output = resume_and_render(&store, "http://127.0.0.1:0", true, "c", 1).await.unwrap();
        assert!(
            output.contains("active"),
            "offline output must mention 'active'; got: {output:?}"
        );
        let after = CampaignStore::load(&store).unwrap();
        let campaign = after.find("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Active);
        assert_eq!(campaign.budget.max_cycles, 3, "budget must be extended by add_cycles");
    }

    #[test]
    fn add_rejects_unknown_registered_project_distinctly() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("campaigns.json");
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let registry_path = write_registry(&dir, "known-project", &repo);
        let file = write_campaign_file(
            &dir,
            r#"{
                "name":"c", "project":"missing-project", "mission":"ship",
                "done_evidence":[{"kind":"review","statement":"shipped"}]
            }"#,
        );

        let err = add(&store, &registry_path, &file).unwrap_err();

        assert_eq!(
            err.to_string(),
            "campaign 'c' references unknown registered project 'missing-project'"
        );
        assert!(!store.exists(), "rejected add must not create the store");
    }

    #[test]
    fn add_rejects_invalid_context_paths_without_mutating_existing_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("campaigns.json");
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let registry_path = write_registry(&dir, "p", &repo);
        let outside_file = dir.path().join("outside.md");
        std::fs::write(&outside_file, "outside").unwrap();
        let linked_escape = repo.join("linked-outside.md");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside_file, &linked_escape).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(&outside_file, &linked_escape).unwrap();
        let original = concat!(
            "{\n",
            "  \"version\": 1,\n",
            "  \"campaigns\": [\n",
            "    {\n",
            "      \"name\": \"existing\",\n",
            "      \"project\": \"p\",\n",
            "      \"mission\": \"keep\",\n",
            "      \"done_evidence\": [\n",
            "        {\n",
            "          \"kind\": \"review\",\n",
            "          \"statement\": \"kept\"\n",
            "        }\n",
            "      ]\n",
            "    }\n",
            "  ]\n",
            "}\n"
        );
        std::fs::write(&store, original).unwrap();

        let cases = [
            ("missing.md", "campaign 'c' context path missing: missing.md"),
            (
                "/tmp/absolute.md",
                "campaign 'c' context path must be repository-relative: /tmp/absolute.md",
            ),
            (
                "../parent.md",
                "campaign 'c' context path must not traverse parent directories: ../parent.md",
            ),
            (
                "linked-outside.md",
                "campaign 'c' context path escapes project checkout: linked-outside.md",
            ),
        ];

        for (context_path, expected_error) in cases {
            let file = write_campaign_file(
                &dir,
                &format!(
                    "{{\n  \"name\":\"c\",\n  \"project\":\"p\",\n  \"mission\":\"ship\",\n  \"context_paths\":[\"{context_path}\"],\n  \"done_evidence\":[{{\"kind\":\"review\",\"statement\":\"shipped\"}}]\n}}"
                ),
            );

            let err = add(&store, &registry_path, &file).unwrap_err();

            assert_eq!(err.to_string(), expected_error);
            assert_eq!(
                std::fs::read(&store).unwrap(),
                original.as_bytes(),
                "rejected add must leave campaigns.json byte-identical for {context_path}"
            );
        }
    }

    #[test]
    fn add_persists_when_all_context_paths_are_existing_repository_relative_files() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("campaigns.json");
        let repo = dir.path().join("repo");
        let docs = repo.join("docs");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&docs).unwrap();
        std::fs::write(repo.join("CHARTER.md"), "charter").unwrap();
        std::fs::write(docs.join("context.md"), "context").unwrap();
        let registry_path = write_registry(&dir, "p", &repo);
        let file = write_campaign_file(
            &dir,
            r#"{
                "name":"c",
                "project":"p",
                "mission":"ship",
                "context_paths":["CHARTER.md","docs/context.md"],
                "done_evidence":[{"kind":"review","statement":"shipped"}]
            }"#,
        );

        add(&store, &registry_path, &file).unwrap();

        let stored = CampaignStore::load(&store).unwrap();
        let campaign = stored.find("c").unwrap();
        assert_eq!(campaign.context_paths, vec!["CHARTER.md", "docs/context.md"]);
    }

    #[tokio::test]
    async fn offline_decide_appends_owner_decision_and_reactivates_campaign() {
        let dir = tempfile::tempdir().unwrap();
        let store_path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::default();
        store
            .add(Campaign {
                name: "c".to_string(),
                project: "p".to_string(),
                mission: "ship".to_string(),
                intent_refs: vec![],
                context_paths: vec![],
                done_evidence: vec![foundry_sdk::campaign::DoneEvidence::Review {
                    statement: "shipped".to_string(),
                }],
                budget: foundry_sdk::campaign::CampaignBudget { max_cycles: 2 },
                escalation: vec![],
                status: CampaignStatus::Escalated,
                cycles_completed: 2,
                cycles_landed: 1,
                authorized_by: Some("tester".to_string()),
                agent_provider: None,
                last_run_event_id: Some("run-2".to_string()),
                owner_decisions: vec![],
                pending_run_result: None,
            })
            .unwrap();
        store.save(&store_path).unwrap();

        let rendered = decide_and_render(
            &store_path,
            "http://127.0.0.1:0",
            true,
            "c",
            "Use the typed daemon mutation path.",
        )
        .await
        .unwrap();

        let stored = CampaignStore::load(&store_path).unwrap();
        let campaign = stored.find("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Active);
        assert_eq!(campaign.owner_decisions.len(), 1);
        assert_eq!(campaign.owner_decisions[0].decision, "Use the typed daemon mutation path.");
        assert_eq!(campaign.owner_decisions[0].authorized_by, "tester");
        assert!(rendered.contains("Owner decisions:"));
        assert!(rendered.contains("Use the typed daemon mutation path."));
    }
}
