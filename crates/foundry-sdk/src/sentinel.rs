//! Sentinels — declarative, named, scheduled triggers that emit events
//! into the engine when their schedule fires.
//!
//! A sentinel is essentially a timer description plus the event it should
//! emit when due. The daemon's scheduler reads the store, watches each
//! enabled sentinel's next firing, and on fire constructs an `Event` from
//! the [`EmitSpec`] and hands it to the engine. This internalises proactive
//! workflows that previously lived in `launchd` plists.
//!
//! Slice 1 supports a single schedule kind (`cron`) and a single seeded
//! sentinel (`nightly-maintenance`). The [`Schedule`] enum is shaped so
//! future kinds (`interval`, `event_silence`) can be added without breaking
//! the wire format.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::event::EventType;
use crate::throttle::Throttle;

/// Current sentinel store format version.
pub const SENTINEL_STORE_VERSION: u32 = 1;

/// The on-disk sentinel store — the source of truth for which scheduled
/// triggers the daemon is honouring.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentinelStore {
    /// Format version. Bumped on schema-breaking changes.
    pub version: u32,
    /// All sentinels declared in this store.
    pub sentinels: Vec<SentinelEntry>,
}

impl SentinelStore {
    /// Deserialize a sentinel store from a JSON file at the given path.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let store: Self = serde_json::from_str(&content)?;
        Ok(store)
    }

    /// Serialize the store to a JSON file at the given path, creating
    /// parent directories as needed.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// Look up a sentinel by name.
    pub fn find_sentinel(&self, name: &str) -> Option<&SentinelEntry> {
        self.sentinels.iter().find(|s| s.name == name)
    }

    /// Return only the sentinels currently enabled.
    pub fn enabled_sentinels(&self) -> Vec<&SentinelEntry> {
        self.sentinels.iter().filter(|s| s.enabled).collect()
    }

    /// Mark the named sentinel as enabled.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when no sentinel with that name exists.
    pub fn enable(&mut self, name: &str) -> Result<&SentinelEntry, SentinelMutationError> {
        let entry = self
            .sentinels
            .iter_mut()
            .find(|s| s.name == name)
            .ok_or_else(|| SentinelMutationError::NotFound(name.to_string()))?;
        entry.enabled = true;
        Ok(entry)
    }

    /// Mark the named sentinel as disabled.
    ///
    /// # Errors
    ///
    /// Returns `NotFound` when no sentinel with that name exists.
    pub fn disable(&mut self, name: &str) -> Result<&SentinelEntry, SentinelMutationError> {
        let entry = self
            .sentinels
            .iter_mut()
            .find(|s| s.name == name)
            .ok_or_else(|| SentinelMutationError::NotFound(name.to_string()))?;
        entry.enabled = false;
        Ok(entry)
    }

    /// The canonical seed set — every sentinel Foundry ships with by default.
    ///
    /// On first daemon start (`sentinels.json` does not yet exist) the whole
    /// seed is written verbatim. On subsequent starts the daemon calls
    /// [`merge_default_seed_into`] to **additively** append any seed entries
    /// missing from the user's file by name, without touching entries that
    /// already exist. New Foundry releases that ship additional canonical
    /// sentinels therefore reach existing installs automatically.
    ///
    /// Current seed:
    /// - `nightly-maintenance` — `MaintenanceCycleStarted` at 02:00 local,
    ///   internalises the schedule that previously lived in
    ///   `launchd/com.mojility.foundry-maintenance.plist`.
    /// - `daily-commit-digest` — `CommitDigestStarted` at 17:00 local,
    ///   the daily commit summary across registered projects.
    /// - `ops-digest` — `OpsDigestStarted` at every 3rd hour (`0 */3 * * *`),
    ///   a periodic summary of MBOS operational events since the last watermark.
    /// - `nightly-supply-chain` — `SupplyChainScanStarted` at 06:00 local, a
    ///   working-tree dependency-advisory scan across every managed project,
    ///   classified against each repo's committed `.supply-chain-allow.json`.
    ///   Advisory and read-only; offset past the 02:00 maintenance run.
    pub fn default_seed() -> Self {
        Self {
            version: SENTINEL_STORE_VERSION,
            sentinels: vec![
                SentinelEntry {
                    name: "nightly-maintenance".to_string(),
                    schedule: Schedule::Cron("0 2 * * *".to_string()),
                    emit: EmitSpec {
                        event_type: EventType::MaintenanceCycleStarted,
                        project: "system".to_string(),
                        throttle: Throttle::default(),
                        payload: serde_json::Value::Object(serde_json::Map::new()),
                    },
                    enabled: true,
                },
                SentinelEntry {
                    name: "daily-commit-digest".to_string(),
                    schedule: Schedule::Cron("0 17 * * *".to_string()),
                    emit: EmitSpec {
                        event_type: EventType::CommitDigestStarted,
                        project: "system".to_string(),
                        throttle: Throttle::default(),
                        payload: serde_json::Value::Object(serde_json::Map::new()),
                    },
                    enabled: true,
                },
                SentinelEntry {
                    name: "ops-digest".to_string(),
                    schedule: Schedule::Cron("0 */3 * * *".to_string()),
                    emit: EmitSpec {
                        event_type: EventType::OpsDigestStarted,
                        project: "system".to_string(),
                        throttle: Throttle::default(),
                        payload: serde_json::Value::Object(serde_json::Map::new()),
                    },
                    enabled: true,
                },
                SentinelEntry {
                    name: "nightly-supply-chain".to_string(),
                    schedule: Schedule::Cron("0 6 * * *".to_string()),
                    emit: EmitSpec {
                        event_type: EventType::SupplyChainScanStarted,
                        project: "system".to_string(),
                        throttle: Throttle::default(),
                        payload: serde_json::Value::Object(serde_json::Map::new()),
                    },
                    enabled: true,
                },
            ],
        }
    }
}

/// Additive seed-merge: append any canonical seed entries whose names are not
/// already present in `store`. Returns `true` when at least one entry was
/// appended (the caller should persist the store back to disk in that case).
///
/// **The function only inspects names.** It never mutates entries that are
/// already present, so user toggles, hand-edited cron expressions, and
/// payload customisations on existing entries survive untouched. Entries the
/// user added that are not in the canonical seed are also preserved.
pub fn merge_default_seed_into(store: &mut SentinelStore) -> bool {
    let mut changed = false;
    let seed = SentinelStore::default_seed();
    let existing: std::collections::HashSet<String> =
        store.sentinels.iter().map(|s| s.name.clone()).collect();
    for entry in seed.sentinels {
        if !existing.contains(&entry.name) {
            store.sentinels.push(entry);
            changed = true;
        }
    }
    changed
}

/// A single sentinel — a name, a schedule, and the event it emits when due.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SentinelEntry {
    /// Unique human-readable name (e.g., `"nightly-maintenance"`).
    pub name: String,
    /// When this sentinel should fire.
    pub schedule: Schedule,
    /// What event to emit on fire.
    pub emit: EmitSpec,
    /// Whether the scheduler should honour this sentinel. Disabled sentinels
    /// remain in the store but never fire.
    pub enabled: bool,
}

/// When a sentinel fires. Modeled as an externally-tagged enum so new
/// schedule kinds (`interval`, `event_silence`, ...) can be added later
/// without breaking the wire format.
///
/// Wire shape today is `{"cron": "0 2 * * *"}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Schedule {
    /// Standard 5-field cron expression evaluated in `chrono::Local`.
    Cron(String),
}

/// What event a sentinel emits when its schedule fires.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmitSpec {
    /// Strongly-typed event variant. Wire form is `snake_case`, matching the
    /// rest of the `EventType` serialisation contract.
    pub event_type: EventType,
    /// Project name on the emitted event (e.g., `"system"` for cycle-roots).
    pub project: String,
    /// Throttle level propagated into the emitted event. Defaults to
    /// `Throttle::Full` so existing sentinels behave identically to
    /// `foundry run` without an explicit `--throttle` flag.
    #[serde(default)]
    pub throttle: Throttle,
    /// Event-type-specific payload. Mirrors the wire shape used by `EmitRequest`.
    pub payload: serde_json::Value,
}

/// Errors that can occur when mutating the sentinel store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SentinelMutationError {
    /// No sentinel with this name exists.
    NotFound(String),
}

impl std::fmt::Display for SentinelMutationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(name) => write!(f, "sentinel '{name}' not found"),
        }
    }
}

impl std::error::Error for SentinelMutationError {}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use tempfile::NamedTempFile;

    use super::*;

    const SEED_JSON: &str = r#"{
      "version": 1,
      "sentinels": [
        {
          "name": "nightly-maintenance",
          "schedule": { "cron": "0 2 * * *" },
          "emit": {
            "event_type": "maintenance_cycle_started",
            "project": "system",
            "throttle": "full",
            "payload": {}
          },
          "enabled": true
        }
      ]
    }"#;

    // ---------------------------------------------------------------------
    // default_seed
    // ---------------------------------------------------------------------

    #[test]
    fn default_seed_includes_nightly_maintenance_daily_commit_digest_and_ops_digest() {
        let store = SentinelStore::default_seed();
        assert_eq!(store.version, SENTINEL_STORE_VERSION);
        assert_eq!(store.sentinels.len(), 4);

        let nightly = store
            .find_sentinel("nightly-maintenance")
            .expect("nightly-maintenance present in seed");
        assert!(nightly.enabled);
        assert_eq!(nightly.schedule, Schedule::Cron("0 2 * * *".to_string()));
        assert_eq!(nightly.emit.event_type, EventType::MaintenanceCycleStarted);
        assert_eq!(nightly.emit.project, "system");
        assert_eq!(nightly.emit.throttle, Throttle::Full);
        assert!(nightly.emit.payload.is_object());

        let digest = store
            .find_sentinel("daily-commit-digest")
            .expect("daily-commit-digest present in seed");
        assert!(digest.enabled);
        assert_eq!(digest.schedule, Schedule::Cron("0 17 * * *".to_string()));
        assert_eq!(digest.emit.event_type, EventType::CommitDigestStarted);
        assert_eq!(digest.emit.project, "system");
        assert_eq!(digest.emit.throttle, Throttle::Full);
        assert!(digest.emit.payload.is_object());

        let ops = store.find_sentinel("ops-digest").expect("ops-digest present in seed");
        assert!(ops.enabled);
        assert_eq!(ops.schedule, Schedule::Cron("0 */3 * * *".to_string()));
        assert_eq!(ops.emit.event_type, EventType::OpsDigestStarted);
        assert_eq!(ops.emit.project, "system");
        assert_eq!(ops.emit.throttle, Throttle::Full);
        assert!(ops.emit.payload.is_object());

        let supply_chain = store
            .find_sentinel("nightly-supply-chain")
            .expect("nightly-supply-chain present in seed");
        assert!(supply_chain.enabled);
        assert_eq!(supply_chain.schedule, Schedule::Cron("0 6 * * *".to_string()));
        assert_eq!(supply_chain.emit.event_type, EventType::SupplyChainScanStarted);
        assert_eq!(supply_chain.emit.project, "system");
        assert_eq!(supply_chain.emit.throttle, Throttle::Full);
        assert!(supply_chain.emit.payload.is_object());
    }

    #[test]
    fn default_seed_serializes_to_documented_shape() {
        let store = SentinelStore::default_seed();
        let json = serde_json::to_value(&store).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["sentinels"].as_array().unwrap().len(), 4);

        let nightly = &json["sentinels"][0];
        assert_eq!(nightly["name"], "nightly-maintenance");
        assert_eq!(nightly["schedule"], serde_json::json!({ "cron": "0 2 * * *" }));
        assert_eq!(nightly["emit"]["event_type"], "maintenance_cycle_started");
        assert_eq!(nightly["emit"]["project"], "system");
        assert_eq!(nightly["emit"]["throttle"], "full");
        assert_eq!(nightly["emit"]["payload"], serde_json::json!({}));
        assert_eq!(nightly["enabled"], true);

        let digest = &json["sentinels"][1];
        assert_eq!(digest["name"], "daily-commit-digest");
        assert_eq!(digest["schedule"], serde_json::json!({ "cron": "0 17 * * *" }));
        assert_eq!(digest["emit"]["event_type"], "commit_digest_started");
        assert_eq!(digest["emit"]["project"], "system");
        assert_eq!(digest["enabled"], true);

        let ops = &json["sentinels"][2];
        assert_eq!(ops["name"], "ops-digest");
        assert_eq!(ops["schedule"], serde_json::json!({ "cron": "0 */3 * * *" }));
        assert_eq!(ops["emit"]["event_type"], "ops_digest_started");
        assert_eq!(ops["emit"]["project"], "system");
        assert_eq!(ops["enabled"], true);

        let supply_chain = &json["sentinels"][3];
        assert_eq!(supply_chain["name"], "nightly-supply-chain");
        assert_eq!(supply_chain["schedule"], serde_json::json!({ "cron": "0 6 * * *" }));
        assert_eq!(supply_chain["emit"]["event_type"], "supply_chain_scan_started");
        assert_eq!(supply_chain["emit"]["project"], "system");
        assert_eq!(supply_chain["enabled"], true);
    }

    // ---------------------------------------------------------------------
    // Schedule wire shape
    // ---------------------------------------------------------------------

    #[test]
    fn schedule_cron_serializes_as_externally_tagged_variant() {
        let schedule = Schedule::Cron("*/5 * * * *".to_string());
        let json = serde_json::to_value(&schedule).unwrap();
        assert_eq!(json, serde_json::json!({ "cron": "*/5 * * * *" }));
    }

    #[test]
    fn schedule_cron_deserializes_from_externally_tagged_variant() {
        let json = r#"{"cron":"0 9 * * 1"}"#;
        let parsed: Schedule = serde_json::from_str(json).unwrap();
        assert_eq!(parsed, Schedule::Cron("0 9 * * 1".to_string()));
    }

    // ---------------------------------------------------------------------
    // EmitSpec defaults
    // ---------------------------------------------------------------------

    #[test]
    fn emit_spec_defaults_throttle_to_full_when_absent() {
        let json = r#"{
            "event_type": "maintenance_cycle_started",
            "project": "system",
            "payload": {}
        }"#;
        let spec: EmitSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.throttle, Throttle::Full);
    }

    #[test]
    fn emit_spec_unknown_event_type_deserializes_to_custom() {
        // The EventType contract is open: an event name the core platform does
        // not recognize is preserved as a `Custom` event rather than rejected,
        // so a sentinel can fire a contributor-defined workflow.
        let json = r#"{
            "event_type": "third_party_workflow_requested",
            "project": "system",
            "throttle": "full",
            "payload": {}
        }"#;
        let spec = serde_json::from_str::<EmitSpec>(json).expect("custom event type is accepted");
        assert_eq!(
            spec.event_type,
            crate::event::EventType::Custom("third_party_workflow_requested".to_string()),
        );
    }

    // ---------------------------------------------------------------------
    // load / save round-trip
    // ---------------------------------------------------------------------

    #[test]
    fn deserialize_seed_json_matches_default_seed_in_memory() {
        let parsed: SentinelStore = serde_json::from_str(SEED_JSON).unwrap();
        let seeded = SentinelStore::default_seed();
        assert_eq!(parsed.version, seeded.version);
        assert_eq!(parsed.sentinels.len(), 1);
        assert_eq!(parsed.sentinels[0].name, seeded.sentinels[0].name);
        assert_eq!(parsed.sentinels[0].schedule, seeded.sentinels[0].schedule);
        assert_eq!(parsed.sentinels[0].emit.event_type, seeded.sentinels[0].emit.event_type);
        assert_eq!(parsed.sentinels[0].enabled, seeded.sentinels[0].enabled);
    }

    #[test]
    fn load_reads_store_from_file() {
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(SEED_JSON.as_bytes()).unwrap();
        file.flush().unwrap();
        let store = SentinelStore::load(file.path()).unwrap();
        assert_eq!(store.sentinels.len(), 1);
        assert_eq!(store.sentinels[0].name, "nightly-maintenance");
    }

    #[test]
    fn save_and_load_round_trip_preserves_seed() {
        let original = SentinelStore::default_seed();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sentinels.json");

        original.save(&path).unwrap();
        let loaded = SentinelStore::load(&path).unwrap();

        assert_eq!(loaded.version, original.version);
        assert_eq!(loaded.sentinels.len(), original.sentinels.len());
        assert_eq!(loaded.sentinels[0].name, original.sentinels[0].name);
        assert_eq!(loaded.sentinels[0].schedule, original.sentinels[0].schedule);
        assert_eq!(loaded.sentinels[0].emit.event_type, original.sentinels[0].emit.event_type);
        assert_eq!(loaded.sentinels[0].enabled, original.sentinels[0].enabled);
    }

    #[test]
    fn save_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("sentinels.json");
        SentinelStore::default_seed().save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn load_missing_file_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.json");
        assert!(SentinelStore::load(&path).is_err());
    }

    // ---------------------------------------------------------------------
    // find_sentinel / enabled_sentinels
    // ---------------------------------------------------------------------

    #[test]
    fn find_sentinel_returns_some_for_known_name() {
        let store = SentinelStore::default_seed();
        let found = store.find_sentinel("nightly-maintenance");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "nightly-maintenance");
    }

    #[test]
    fn find_sentinel_returns_none_for_unknown_name() {
        let store = SentinelStore::default_seed();
        assert!(store.find_sentinel("does-not-exist").is_none());
    }

    #[test]
    fn enabled_sentinels_filters_disabled_entries() {
        let mut store = SentinelStore::default_seed();
        let enabled_at_start = store.enabled_sentinels().len();
        store.sentinels.push(SentinelEntry {
            name: "off".to_string(),
            schedule: Schedule::Cron("0 0 * * *".to_string()),
            emit: EmitSpec {
                event_type: EventType::MaintenanceCycleStarted,
                project: "system".to_string(),
                throttle: Throttle::default(),
                payload: serde_json::json!({}),
            },
            enabled: false,
        });

        let enabled = store.enabled_sentinels();
        assert_eq!(enabled.len(), enabled_at_start, "disabled push must not affect enabled count");
        assert!(enabled.iter().all(|s| s.enabled));
    }

    // ---------------------------------------------------------------------
    // enable / disable
    // ---------------------------------------------------------------------

    #[test]
    fn enable_idempotently_flips_disabled_entry_to_enabled() {
        let mut store = SentinelStore::default_seed();
        store.sentinels[0].enabled = false;
        let entry = store.enable("nightly-maintenance").unwrap();
        assert!(entry.enabled);
        assert!(store.sentinels[0].enabled);
    }

    #[test]
    fn enable_on_already_enabled_entry_is_a_no_op_success() {
        let mut store = SentinelStore::default_seed();
        let entry = store.enable("nightly-maintenance").unwrap();
        assert!(entry.enabled);
    }

    #[test]
    fn disable_flips_enabled_entry_to_disabled() {
        let mut store = SentinelStore::default_seed();
        let entry = store.disable("nightly-maintenance").unwrap();
        assert!(!entry.enabled);
        assert!(!store.sentinels[0].enabled);
    }

    #[test]
    fn enable_unknown_returns_not_found() {
        let mut store = SentinelStore::default_seed();
        let err = store.enable("ghost").unwrap_err();
        assert_eq!(err, SentinelMutationError::NotFound("ghost".to_string()));
    }

    #[test]
    fn disable_unknown_returns_not_found() {
        let mut store = SentinelStore::default_seed();
        let err = store.disable("ghost").unwrap_err();
        assert_eq!(err, SentinelMutationError::NotFound("ghost".to_string()));
    }

    #[test]
    fn mutation_error_display_mentions_name() {
        let err = SentinelMutationError::NotFound("foo".to_string());
        assert!(err.to_string().contains("foo"));
        assert!(err.to_string().contains("not found"));
    }

    // ---------------------------------------------------------------------
    // merge_default_seed_into
    // ---------------------------------------------------------------------

    /// Build a store containing only the legacy Slice-1 nightly entry. This
    /// is exactly what an existing install upgrading from Slice 1 has on disk.
    fn legacy_slice1_store() -> SentinelStore {
        SentinelStore {
            version: SENTINEL_STORE_VERSION,
            sentinels: vec![SentinelEntry {
                name: "nightly-maintenance".to_string(),
                schedule: Schedule::Cron("0 2 * * *".to_string()),
                emit: EmitSpec {
                    event_type: EventType::MaintenanceCycleStarted,
                    project: "system".to_string(),
                    throttle: Throttle::Full,
                    payload: serde_json::json!({}),
                },
                enabled: true,
            }],
        }
    }

    #[test]
    fn merge_default_seed_into_returns_false_when_already_complete() {
        let mut store = SentinelStore::default_seed();
        let len_before = store.sentinels.len();
        let changed = merge_default_seed_into(&mut store);
        assert!(!changed, "no missing seed entries → no change");
        assert_eq!(store.sentinels.len(), len_before);
    }

    #[test]
    fn merge_default_seed_into_appends_missing_entries_on_slice1_store() {
        let mut store = legacy_slice1_store();
        let changed = merge_default_seed_into(&mut store);
        assert!(changed, "digest entries were missing → store should be mutated");
        assert_eq!(store.sentinels.len(), 4);
        assert!(store.find_sentinel("nightly-maintenance").is_some());
        assert!(store.find_sentinel("daily-commit-digest").is_some());
        assert!(store.find_sentinel("ops-digest").is_some());
        assert!(store.find_sentinel("nightly-supply-chain").is_some());
    }

    #[test]
    fn merge_default_seed_into_appends_all_canonical_entries_when_empty() {
        let mut store = SentinelStore {
            version: SENTINEL_STORE_VERSION,
            sentinels: vec![],
        };
        let changed = merge_default_seed_into(&mut store);
        assert!(changed);
        assert_eq!(store.sentinels.len(), SentinelStore::default_seed().sentinels.len());
    }

    #[test]
    fn merge_default_seed_into_preserves_user_disabled_state_on_existing_entries() {
        let mut store = legacy_slice1_store();
        // Simulate Stacey disabling the nightly run before the Slice 2 upgrade.
        store.sentinels[0].enabled = false;

        merge_default_seed_into(&mut store);

        let nightly = store.find_sentinel("nightly-maintenance").unwrap();
        assert!(!nightly.enabled, "user toggle on existing entry must not be reset");
    }

    #[test]
    fn merge_default_seed_into_preserves_user_added_entries() {
        let mut store = legacy_slice1_store();
        store.sentinels.push(SentinelEntry {
            name: "weekly-finance-audit".to_string(),
            schedule: Schedule::Cron("0 8 * * 1".to_string()),
            emit: EmitSpec {
                event_type: EventType::MaintenanceCycleStarted, // placeholder
                project: "system".to_string(),
                throttle: Throttle::Full,
                payload: serde_json::json!({}),
            },
            enabled: true,
        });

        merge_default_seed_into(&mut store);

        assert!(
            store.find_sentinel("weekly-finance-audit").is_some(),
            "user-defined entry must survive the merge"
        );
        // All canonical entries are now present alongside.
        assert!(store.find_sentinel("daily-commit-digest").is_some());
        assert!(store.find_sentinel("ops-digest").is_some());
    }

    #[test]
    fn merge_default_seed_into_preserves_cron_edits_on_existing_entries() {
        let mut store = legacy_slice1_store();
        // Simulate Stacey moving the nightly run to 03:30.
        store.sentinels[0].schedule = Schedule::Cron("30 3 * * *".to_string());

        merge_default_seed_into(&mut store);

        let nightly = store.find_sentinel("nightly-maintenance").unwrap();
        assert_eq!(
            nightly.schedule,
            Schedule::Cron("30 3 * * *".to_string()),
            "hand-edited cron must not be overwritten by the merge"
        );
    }
}
