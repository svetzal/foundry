//! CLI handlers for the `foundry sentinel` subcommands.
//!
//! Inspection commands (`list`, `show`) always read `sentinels.json` directly
//! — they never need the daemon. Mutation commands (`enable`, `disable`) use
//! [`crate::daemon::with_daemon_or_offline`] as the single source of the
//! fallback protocol: try gRPC first; warn and fall back to direct file
//! mutation when the daemon is unreachable.

use std::path::Path;

use anyhow::{Result, bail};
use foundry_sdk::sentinel::SentinelStore;

use crate::daemon::{status_to_anyhow, with_daemon_or_offline};
use crate::proto::{SentinelDisableRequest, SentinelEnableRequest};
use crate::render;

pub fn list(sentinels_path: &Path) -> Result<()> {
    let store = SentinelStore::load(sentinels_path)?;

    if store.sentinels.is_empty() {
        println!("No sentinels configured.");
        return Ok(());
    }

    print!("{}", render::sentinel::sentinel_table(&store.sentinels));
    Ok(())
}

pub fn show(sentinels_path: &Path, name: &str) -> Result<()> {
    let store = SentinelStore::load(sentinels_path)?;
    let Some(entry) = store.find_sentinel(name) else {
        bail!("Sentinel '{name}' not found");
    };

    print!("{}", render::sentinel::sentinel_detail(entry));
    Ok(())
}

pub async fn enable(sentinels_path: &Path, addr: &str, offline: bool, name: &str) -> Result<()> {
    with_daemon_or_offline(
        addr,
        offline,
        &format!("Enabled sentinel '{name}'."),
        |mut client| async move {
            let req = SentinelEnableRequest {
                name: name.to_string(),
            };
            client.sentinel_enable(req).await.map(|_| ()).map_err(status_to_anyhow)
        },
        || enable_offline(sentinels_path, name),
    )
    .await
}

pub async fn disable(sentinels_path: &Path, addr: &str, offline: bool, name: &str) -> Result<()> {
    with_daemon_or_offline(
        addr,
        offline,
        &format!("Disabled sentinel '{name}'."),
        |mut client| async move {
            let req = SentinelDisableRequest {
                name: name.to_string(),
            };
            client.sentinel_disable(req).await.map(|_| ()).map_err(status_to_anyhow)
        },
        || disable_offline(sentinels_path, name),
    )
    .await
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
