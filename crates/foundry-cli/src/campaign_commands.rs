//! CLI handlers for the `foundry campaign` subcommands.
//!
//! Inspection commands (`add`, `list`, `show`) always operate on `campaigns.json`
//! directly — they never need the daemon.
//!
//! The `pause` mutation command mirrors the registry/sentinel daemon-or-offline
//! protocol: the online path calls the `PauseCampaign` gRPC RPC and renders the
//! response from the typed `PauseCampaignResponse.campaign` detail; the offline
//! fallback (and graceful-degradation path when the daemon is unreachable) mutates
//! the store file directly via [`CampaignStore::lock_exclusive`].
//!
//! The `resume` and `advance` commands are out of scope for this slice — their
//! daemon-boundary migration carries decision-bearing semantic changes and is
//! handled separately.

use std::path::Path;

use anyhow::{Context, Result, bail};
use foundry_sdk::campaign::{Campaign, CampaignStatus, CampaignStore};

use crate::daemon::{status_to_anyhow, with_daemon_or_offline_render};
use crate::proto::PauseCampaignRequest;
use crate::render;
use crate::workflow_commands::WorkflowRunner;

pub fn add(store_path: &Path, file: &Path) -> Result<()> {
    let content = std::fs::read_to_string(file)
        .with_context(|| format!("read campaign definition {}", file.display()))?;
    let campaign: Campaign = serde_json::from_str(&content)
        .with_context(|| format!("parse campaign definition {}", file.display()))?;
    let name = campaign.name.clone();
    let mut guard = CampaignStore::lock_exclusive(store_path)?;
    guard.store.add(campaign)?;
    guard.save()?;
    println!("Added campaign '{name}'.");
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

pub fn resume(store_path: &Path, name: &str, add_cycles: u64) -> Result<()> {
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
    let max_cycles = campaign.budget.max_cycles;
    guard.save()?;
    println!("Campaign '{name}' is now active with a {max_cycles}-cycle budget.");
    Ok(())
}

pub async fn advance(addr: &str, store_path: &Path, name: &str) -> Result<()> {
    let store = CampaignStore::load(store_path)?;
    let campaign =
        store.find(name).ok_or_else(|| anyhow::anyhow!("campaign '{name}' not found"))?;
    if matches!(campaign.status, CampaignStatus::Paused | CampaignStatus::Escalated) {
        bail!(
            "campaign '{name}' is {}; run `foundry campaign resume {name}` before advancing",
            campaign.status
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

    // Offline path: pause_and_render with offline=true must not touch gRPC
    // and must flip the status to Paused in the store file.
    #[tokio::test]
    async fn offline_pause_sets_status_paused_in_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = dir.path().join("campaigns.json");
        let file = dir.path().join("campaign.json");
        std::fs::write(
            &file,
            r#"{
                "name":"c", "project":"p", "mission":"ship",
                "done_evidence":[{"kind":"review","statement":"shipped"}],
                "authorized_by":"tester"
            }"#,
        )
        .unwrap();
        add(&store, &file).unwrap();

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
        let file = dir.path().join("campaign.json");
        std::fs::write(
            &file,
            r#"{
                "name":"c", "project":"p", "mission":"ship",
                "done_evidence":[{"kind":"review","statement":"shipped"}],
                "authorized_by":"tester"
            }"#,
        )
        .unwrap();
        add(&store, &file).unwrap();
        list(&store).unwrap();
        show(&store, "c").unwrap();
        pause(&store, "http://127.0.0.1:0", true, "c").await.unwrap();
        assert_eq!(
            CampaignStore::load(&store).unwrap().find("c").unwrap().status,
            CampaignStatus::Paused
        );
    }

    #[test]
    fn exhausted_campaign_requires_and_applies_explicit_added_cycles() {
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
                pending_run_result: None,
            })
            .unwrap();
        store.save(&store_path).unwrap();

        let error = resume(&store_path, "c", 0).unwrap_err();
        assert!(error.to_string().contains("--add-cycles"));

        resume(&store_path, "c", 1).unwrap();
        let resumed = CampaignStore::load(&store_path).unwrap();
        let campaign = resumed.find("c").unwrap();
        assert_eq!(campaign.status, CampaignStatus::Active);
        assert_eq!(campaign.budget.max_cycles, 3);
    }
}
