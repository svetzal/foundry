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
    let mut store = CampaignStore::load(store_path)?;
    store.add(campaign)?;
    store.save(store_path)?;
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

pub fn resume(store_path: &Path, name: &str) -> Result<()> {
    let store = CampaignStore::load(store_path)?;
    let campaign =
        store.find(name).ok_or_else(|| anyhow::anyhow!("campaign '{name}' not found"))?;
    if campaign.authorized_by.is_none() {
        bail!("campaign '{name}' cannot resume until authorized_by is set");
    }
    drop(store);
    set_status(store_path, name, CampaignStatus::Active)
}

fn set_status(store_path: &Path, name: &str, status: CampaignStatus) -> Result<()> {
    let mut store = CampaignStore::load(store_path)?;
    let campaign = store
        .find_mut(name)
        .ok_or_else(|| anyhow::anyhow!("campaign '{name}' not found"))?;
    campaign.status = status;
    store.save(store_path)?;
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
}
