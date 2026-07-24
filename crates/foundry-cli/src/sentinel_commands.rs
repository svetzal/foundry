//! CLI handlers for the `foundry sentinel` subcommands.
//!
//! The default online path is daemon-authoritative for all four commands.
//! `--offline` is an explicit recovery mode that reads or mutates
//! `sentinels.json` directly when `foundryd` is not running.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Result, bail};
use foundry_sdk::event::EventType;
use foundry_sdk::sentinel::SentinelStore;
use foundry_sdk::throttle::Throttle;

use crate::daemon::{connect_daemon_required, status_to_anyhow};
use crate::proto::{
    Sentinel, SentinelDisableRequest, SentinelEnableRequest, SentinelListRequest,
    SentinelShowRequest,
};
use crate::render;

pub async fn list(sentinels_path: &Path, addr: &str, offline: bool) -> Result<()> {
    if offline {
        return list_offline(sentinels_path);
    }

    let mut client = connect_daemon_required(addr, &sentinel_offline_hint("list")).await?;
    let response = client
        .sentinel_list(SentinelListRequest {})
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    let entries = response.sentinels.iter().map(entry_from_proto).collect::<Result<Vec<_>>>()?;

    render_sentinel_table(&entries);
    Ok(())
}

fn list_offline(sentinels_path: &Path) -> Result<()> {
    let store = SentinelStore::load(sentinels_path)?;

    if store.sentinels.is_empty() {
        println!("No sentinels configured.");
        return Ok(());
    }

    print!("{}", render::sentinel::sentinel_table(&store.sentinels));
    Ok(())
}

pub async fn show(sentinels_path: &Path, addr: &str, offline: bool, name: &str) -> Result<()> {
    if offline {
        return show_offline(sentinels_path, name);
    }

    let mut client =
        connect_daemon_required(addr, &sentinel_offline_hint(&format!("show {name}"))).await?;
    let response = client
        .sentinel_show(SentinelShowRequest {
            name: name.to_string(),
        })
        .await
        .map_err(status_to_anyhow)?
        .into_inner();
    let sentinel = response
        .sentinel
        .ok_or_else(|| anyhow::anyhow!("daemon returned no sentinel for '{name}'"))?;

    print!("{}", render::sentinel::sentinel_detail(&entry_from_proto(&sentinel)?));
    Ok(())
}

fn show_offline(sentinels_path: &Path, name: &str) -> Result<()> {
    let store = SentinelStore::load(sentinels_path)?;
    let Some(entry) = store.find_sentinel(name) else {
        bail!("Sentinel '{name}' not found");
    };

    print!("{}", render::sentinel::sentinel_detail(entry));
    Ok(())
}

pub async fn enable(sentinels_path: &Path, addr: &str, offline: bool, name: &str) -> Result<()> {
    if offline {
        enable_offline(sentinels_path, name)?;
        println!("Enabled sentinel '{name}'.");
        return Ok(());
    }

    let mut client =
        connect_daemon_required(addr, &sentinel_offline_hint(&format!("enable {name}"))).await?;
    client
        .sentinel_enable(SentinelEnableRequest {
            name: name.to_string(),
        })
        .await
        .map_err(status_to_anyhow)?;
    println!("Enabled sentinel '{name}'.");
    Ok(())
}

pub async fn disable(sentinels_path: &Path, addr: &str, offline: bool, name: &str) -> Result<()> {
    if offline {
        disable_offline(sentinels_path, name)?;
        println!("Disabled sentinel '{name}'.");
        return Ok(());
    }

    let mut client =
        connect_daemon_required(addr, &sentinel_offline_hint(&format!("disable {name}"))).await?;
    client
        .sentinel_disable(SentinelDisableRequest {
            name: name.to_string(),
        })
        .await
        .map_err(status_to_anyhow)?;
    println!("Disabled sentinel '{name}'.");
    Ok(())
}

fn enable_offline(sentinels_path: &Path, name: &str) -> Result<()> {
    let mut store = SentinelStore::load(sentinels_path)?;
    store.enable(name).map_err(|e| anyhow::anyhow!("{e}"))?;
    store.save(sentinels_path)?;
    Ok(())
}

fn disable_offline(sentinels_path: &Path, name: &str) -> Result<()> {
    let mut store = SentinelStore::load(sentinels_path)?;
    store.disable(name).map_err(|e| anyhow::anyhow!("{e}"))?;
    store.save(sentinels_path)?;
    Ok(())
}

fn sentinel_offline_hint(command_suffix: &str) -> String {
    format!("foundry sentinel {command_suffix} --offline")
}

fn render_sentinel_table(entries: &[foundry_sdk::sentinel::SentinelEntry]) {
    if entries.is_empty() {
        println!("No sentinels configured.");
        return;
    }

    print!("{}", render::sentinel::sentinel_table(entries));
}

fn entry_from_proto(proto: &Sentinel) -> Result<foundry_sdk::sentinel::SentinelEntry> {
    let throttle = match proto.emit_throttle {
        0 => Throttle::Full,
        1 => Throttle::DryRun,
        other => bail!("invalid daemon sentinel throttle value: {other}"),
    };
    let payload = serde_json::from_str(&proto.emit_payload_json)
        .map_err(|source| anyhow::anyhow!("invalid daemon sentinel payload JSON: {source}"))?;

    Ok(foundry_sdk::sentinel::SentinelEntry {
        name: proto.name.clone(),
        schedule: foundry_sdk::sentinel::Schedule::Cron(proto.cron.clone()),
        emit: foundry_sdk::sentinel::EmitSpec {
            event_type: EventType::from_str(&proto.emit_event_type).map_err(|source| {
                anyhow::anyhow!("invalid daemon sentinel event type: {source}")
            })?,
            project: proto.emit_project.clone(),
            throttle,
            payload,
        },
        enabled: proto.enabled,
    })
}

#[cfg(test)]
mod tests {
    use foundry_sdk::sentinel::SentinelStore;
    use tempfile::NamedTempFile;

    use super::*;

    fn seed_path() -> NamedTempFile {
        let tmp = NamedTempFile::new().expect("tempfile");
        SentinelStore::default_seed().save(tmp.path()).unwrap();
        tmp
    }

    #[test]
    fn list_succeeds_against_default_seed_file() {
        let tmp = seed_path();
        list_offline(tmp.path()).expect("list should succeed against seed");
    }

    #[test]
    fn show_returns_error_for_unknown_name() {
        let tmp = seed_path();
        let err = show_offline(tmp.path(), "ghost").unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn show_succeeds_for_known_seed_entry() {
        let tmp = seed_path();
        show_offline(tmp.path(), "nightly-maintenance").expect("show should succeed");
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

    #[test]
    fn entry_from_proto_round_trips_seed_fields() {
        let entry = entry_from_proto(&Sentinel {
            name: "nightly-maintenance".to_string(),
            cron: "0 2 * * *".to_string(),
            emit_event_type: "maintenance_cycle_started".to_string(),
            emit_project: "system".to_string(),
            emit_throttle: 0,
            emit_payload_json: "{}".to_string(),
            enabled: true,
        })
        .expect("proto sentinel should parse");

        assert_eq!(entry.name, "nightly-maintenance");
        assert_eq!(entry.emit.event_type.as_str(), "maintenance_cycle_started");
    }
}
