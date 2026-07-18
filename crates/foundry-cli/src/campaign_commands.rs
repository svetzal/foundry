use std::path::Path;

use anyhow::{Context, Result, bail};
use foundry_sdk::campaign::{Campaign, CampaignStatus, CampaignStore};

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

pub fn pause(store_path: &Path, name: &str) -> Result<()> {
    set_status(store_path, name, CampaignStatus::Paused)
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

fn set_status(store_path: &Path, name: &str, status: CampaignStatus) -> Result<()> {
    let mut guard = CampaignStore::lock_exclusive(store_path)?;
    let campaign = guard
        .store
        .find_mut(name)
        .ok_or_else(|| anyhow::anyhow!("campaign '{name}' not found"))?;
    campaign.status = status;
    guard.save()?;
    println!("Campaign '{name}' is now {status}.");
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

    #[test]
    fn add_list_show_pause_round_trip() {
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
        pause(&store, "c").unwrap();
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
