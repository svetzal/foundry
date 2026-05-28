//! CLI handlers for the `foundry sentinel` subcommands.
//!
//! Inspection commands (`list`, `show`) always read `sentinels.json` directly
//! — they never need the daemon. Mutation commands (`enable`, `disable`) try
//! the gRPC path first so the running daemon updates its in-memory store and
//! wakes the scheduler; if the daemon is unreachable they fall back to direct
//! file mutation (matching `foundry registry add | edit | remove`).

use std::path::Path;

use anyhow::{Result, bail};
use comfy_table::{ContentArrangement, Table};
use foundry_core::sentinel::{Schedule, SentinelStore};

use crate::proto::{SentinelDisableRequest, SentinelEnableRequest, foundry_client::FoundryClient};

pub fn list(sentinels_path: &Path) -> Result<()> {
    let store = SentinelStore::load(sentinels_path)?;

    if store.sentinels.is_empty() {
        println!("No sentinels configured.");
        return Ok(());
    }

    let mut table = Table::new();
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_header(vec!["Name", "Schedule", "Emits", "Project", "Enabled"]);

    for s in &store.sentinels {
        let schedule = schedule_summary(&s.schedule);
        let event_type = s.emit.event_type.as_str();
        table.add_row(vec![
            s.name.as_str(),
            schedule.as_str(),
            event_type.as_str(),
            s.emit.project.as_str(),
            if s.enabled { "yes" } else { "no" },
        ]);
    }

    println!("{table}");
    Ok(())
}

pub fn show(sentinels_path: &Path, name: &str) -> Result<()> {
    let store = SentinelStore::load(sentinels_path)?;
    let Some(entry) = store.find_sentinel(name) else {
        bail!("Sentinel '{name}' not found");
    };

    println!("Name:      {}", entry.name);
    println!("Schedule:  {}", schedule_summary(&entry.schedule));
    println!("Enabled:   {}", if entry.enabled { "yes" } else { "no" });
    println!("Emits:     {}", entry.emit.event_type.as_str());
    println!("Project:   {}", entry.emit.project);
    println!("Throttle:  {}", entry.emit.throttle);
    println!("Payload:   {}", entry.emit.payload);
    Ok(())
}

pub async fn enable(sentinels_path: &Path, addr: &str, offline: bool, name: &str) -> Result<()> {
    if !offline {
        match FoundryClient::connect(addr.to_string()).await {
            Ok(mut client) => {
                let req = SentinelEnableRequest {
                    name: name.to_string(),
                };
                client
                    .sentinel_enable(req)
                    .await
                    .map_err(|s| anyhow::anyhow!("daemon error: {} — {}", s.code(), s.message()))?;
                println!("Enabled sentinel '{name}'.");
                return Ok(());
            }
            Err(_) => {
                eprintln!("warning: daemon not reachable, falling back to direct file mutation");
            }
        }
    }

    enable_offline(sentinels_path, name)
}

pub async fn disable(sentinels_path: &Path, addr: &str, offline: bool, name: &str) -> Result<()> {
    if !offline {
        match FoundryClient::connect(addr.to_string()).await {
            Ok(mut client) => {
                let req = SentinelDisableRequest {
                    name: name.to_string(),
                };
                client
                    .sentinel_disable(req)
                    .await
                    .map_err(|s| anyhow::anyhow!("daemon error: {} — {}", s.code(), s.message()))?;
                println!("Disabled sentinel '{name}'.");
                return Ok(());
            }
            Err(_) => {
                eprintln!("warning: daemon not reachable, falling back to direct file mutation");
            }
        }
    }

    disable_offline(sentinels_path, name)
}

fn enable_offline(sentinels_path: &Path, name: &str) -> Result<()> {
    let mut store = SentinelStore::load(sentinels_path)?;
    store.enable(name).map_err(|e| anyhow::anyhow!("{e}"))?;
    store.save(sentinels_path)?;
    println!("Enabled sentinel '{name}'.");
    Ok(())
}

fn disable_offline(sentinels_path: &Path, name: &str) -> Result<()> {
    let mut store = SentinelStore::load(sentinels_path)?;
    store.disable(name).map_err(|e| anyhow::anyhow!("{e}"))?;
    store.save(sentinels_path)?;
    println!("Disabled sentinel '{name}'.");
    Ok(())
}

fn schedule_summary(schedule: &Schedule) -> String {
    match schedule {
        Schedule::Cron(expr) => format!("cron: {expr}"),
    }
}

#[cfg(test)]
mod tests {
    use foundry_core::sentinel::SentinelStore;
    use tempfile::NamedTempFile;

    use super::*;

    fn seed_path() -> NamedTempFile {
        let tmp = NamedTempFile::new().expect("tempfile");
        SentinelStore::default_seed().save(tmp.path()).unwrap();
        tmp
    }

    #[test]
    fn schedule_summary_renders_cron_prefix() {
        let s = Schedule::Cron("0 2 * * *".to_string());
        assert_eq!(schedule_summary(&s), "cron: 0 2 * * *");
    }

    #[test]
    fn list_succeeds_against_default_seed_file() {
        let tmp = seed_path();
        list(tmp.path()).expect("list should succeed against seed");
    }

    #[test]
    fn show_returns_error_for_unknown_name() {
        let tmp = seed_path();
        let err = show(tmp.path(), "ghost").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn show_succeeds_for_known_seed_entry() {
        let tmp = seed_path();
        show(tmp.path(), "nightly-maintenance").expect("show should succeed");
    }

    #[tokio::test]
    async fn offline_disable_flips_seed_entry_on_disk() {
        let tmp = seed_path();
        disable(tmp.path(), "ignored://addr", true, "nightly-maintenance")
            .await
            .expect("offline disable should succeed");
        let store = SentinelStore::load(tmp.path()).unwrap();
        assert!(!store.sentinels[0].enabled);
    }

    #[tokio::test]
    async fn offline_enable_flips_seed_entry_on_disk() {
        let tmp = seed_path();
        // Pre-disable so the enable is observable.
        {
            let mut store = SentinelStore::load(tmp.path()).unwrap();
            store.disable("nightly-maintenance").unwrap();
            store.save(tmp.path()).unwrap();
        }
        enable(tmp.path(), "ignored://addr", true, "nightly-maintenance")
            .await
            .expect("offline enable should succeed");
        let store = SentinelStore::load(tmp.path()).unwrap();
        assert!(store.sentinels[0].enabled);
    }

    #[tokio::test]
    async fn offline_enable_unknown_returns_error() {
        let tmp = seed_path();
        let err = enable(tmp.path(), "ignored://addr", true, "ghost").await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn offline_disable_unknown_returns_error() {
        let tmp = seed_path();
        let err = disable(tmp.path(), "ignored://addr", true, "ghost").await.unwrap_err();
        assert!(err.to_string().contains("not found"));
    }
}
