use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::payload::TaskRunCompletedPayload;

fn default_version() -> u32 {
    1
}

fn default_max_cycles() -> u64 {
    20
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignStore {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub campaigns: Vec<Campaign>,
}

impl Default for CampaignStore {
    fn default() -> Self {
        Self {
            version: default_version(),
            campaigns: vec![],
        }
    }
}

impl CampaignStore {
    pub fn load(path: &Path) -> Result<Self, StoreError> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let content = std::fs::read_to_string(path).map_err(|source| StoreError::Io {
            path: path.to_owned(),
            source,
        })?;
        if content.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&content).map_err(|source| StoreError::Parse {
            path: path.to_owned(),
            source,
        })
    }

    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_owned(),
                source,
            })?;
        }
        let content = serde_json::to_string_pretty(self).map_err(|source| StoreError::Parse {
            path: path.to_owned(),
            source,
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, content).map_err(|source| StoreError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, path).map_err(|source| StoreError::Io {
            path: path.to_owned(),
            source,
        })
    }

    #[must_use]
    pub fn find(&self, name: &str) -> Option<&Campaign> {
        self.campaigns.iter().find(|campaign| campaign.name == name)
    }

    pub fn find_mut(&mut self, name: &str) -> Option<&mut Campaign> {
        self.campaigns.iter_mut().find(|campaign| campaign.name == name)
    }

    pub fn add(&mut self, campaign: Campaign) -> anyhow::Result<()> {
        if self.find(&campaign.name).is_some() {
            anyhow::bail!("campaign '{}' already exists", campaign.name);
        }
        campaign.validate()?;
        self.campaigns.push(campaign);
        Ok(())
    }

    /// Acquire the campaign store's cross-process advisory lock and load its
    /// latest contents. Every production read-modify-write operation must use
    /// this guard so CLI control commands cannot race daemon advancement.
    pub fn lock_exclusive(path: &Path) -> Result<CampaignStoreGuard, StoreError> {
        CampaignStoreGuard::load(path)
    }
}

/// Exclusive read-modify-write guard for the campaign store.
///
/// The adjacent `.lock` file has a stable inode even though campaign saves use
/// atomic rename, so the lock remains valid across store replacements.
pub struct CampaignStoreGuard {
    path: PathBuf,
    lock_file: File,
    pub store: CampaignStore,
}

impl CampaignStoreGuard {
    fn load(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: parent.to_owned(),
                source,
            })?;
        }
        let lock_path = path.with_extension("lock");
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| StoreError::Io {
                path: lock_path.clone(),
                source,
            })?;
        lock_file.lock_exclusive().map_err(|source| StoreError::Io {
            path: lock_path,
            source,
        })?;
        let store = CampaignStore::load(path)?;
        Ok(Self {
            path: path.to_owned(),
            lock_file,
            store,
        })
    }

    pub fn save(&self) -> Result<(), StoreError> {
        self.store.save(&self.path)
    }
}

impl Drop for CampaignStoreGuard {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.lock_file);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Campaign {
    pub name: String,
    pub project: String,
    pub mission: String,
    #[serde(default)]
    pub intent_refs: Vec<String>,
    #[serde(default)]
    pub context_paths: Vec<String>,
    #[serde(default)]
    pub done_evidence: Vec<DoneEvidence>,
    #[serde(default)]
    pub budget: CampaignBudget,
    #[serde(default)]
    pub escalation: Vec<String>,
    #[serde(default)]
    pub status: CampaignStatus,
    #[serde(default)]
    pub cycles_completed: u64,
    #[serde(default)]
    pub cycles_landed: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorized_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_event_id: Option<String>,
    /// Owner decisions recorded after escalations. These are append-only and
    /// fed back into future formation prompts as binding policy.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owner_decisions: Vec<OwnerDecision>,
    /// Typed result recorded while a campaign is paused. The next manual
    /// advance replays it so formation sees the reviewer gaps and the executor
    /// continues from its preservation ref. Consumed when a decision is made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_run_result: Option<TaskRunCompletedPayload>,
}

/// Commands that assert state on a host rather than in the repository, and so
/// can never be satisfied by work done inside a disposable task worktree.
const OFF_WORKTREE_COMMANDS: &[&str] = &[
    "ssh",
    "scp",
    "sftp",
    "rsync",
    "systemctl",
    "launchctl",
    "journalctl",
    "service",
];

/// Find an off-worktree command used in command position anywhere in a gate.
///
/// Segments the string on shell operators and inspects the leading word of each
/// segment, skipping `VAR=value` prefixes and `sudo`. This is deliberately not a
/// shell parser: over-segmentation only widens the search, which is the safe
/// direction for a check whose purpose is to catch host assertions.
fn off_worktree_command(command: &str) -> Option<&'static str> {
    command.split(['|', '&', ';', '\n']).find_map(|segment| {
        segment
            .split_whitespace()
            .find(|word| !word.contains('=') && *word != "sudo")
            .and_then(|word| {
                let leaf = word.trim_start_matches(['(', '"', '\'']);
                let leaf = leaf.rsplit('/').next().unwrap_or(leaf);
                OFF_WORKTREE_COMMANDS.iter().copied().find(|name| *name == leaf)
            })
    })
}

impl Campaign {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.name.trim().is_empty()
            || self.project.trim().is_empty()
            || self.mission.trim().is_empty()
        {
            anyhow::bail!("campaign name, project, and mission must be non-empty");
        }
        if self.done_evidence.is_empty() {
            anyhow::bail!("campaign '{}' requires at least one done_evidence item", self.name);
        }
        if self.budget.max_cycles == 0 {
            anyhow::bail!("campaign '{}' max_cycles must be greater than zero", self.name);
        }
        self.validate_required_gates_are_runnable()
    }

    /// A required gate is re-run every formation against the project checkout
    /// and gates the `done` decision outright, so one that asserts state on
    /// another host can never pass until the whole mission is already deployed —
    /// making the campaign structurally incapable of completing and burning its
    /// entire budget. Such evidence is real, but it is owner-verified
    /// deployment evidence, which is what a `review` statement is for.
    fn validate_required_gates_are_runnable(&self) -> anyhow::Result<()> {
        for item in &self.done_evidence {
            let DoneEvidence::Gate {
                command, required, ..
            } = item
            else {
                continue;
            };
            if !*required {
                continue;
            }
            if let Some(offender) = off_worktree_command(command) {
                anyhow::bail!(
                    "campaign '{}' has a required gate that cannot run in a task worktree: `{}` asserts state on another host via `{offender}`. \
                     A required gate is re-run every formation and blocks `done`, so this one can never pass and will exhaust the cycle budget. \
                     Express it as a `review` statement the owner verifies, or mark the gate `\"required\": false`.",
                    self.name,
                    command.trim()
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CampaignBudget {
    #[serde(default = "default_max_cycles")]
    pub max_cycles: u64,
}

impl Default for CampaignBudget {
    fn default() -> Self {
        Self {
            max_cycles: default_max_cycles(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum CampaignStatus {
    #[default]
    Staged,
    Active,
    Paused,
    Escalated,
    Completed,
}

impl std::fmt::Display for CampaignStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Staged => "staged",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Escalated => "escalated",
            Self::Completed => "completed",
        };
        f.write_str(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerDecision {
    pub decision: String,
    pub authorized_by: String,
    pub decided_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DoneEvidence {
    Gate {
        command: String,
        #[serde(default = "default_required")]
        required: bool,
        /// Repository-relative files or directories that must exist before the
        /// command is eligible to pass.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        artifacts: Vec<String>,
    },
    Review {
        statement: String,
    },
}

fn default_required() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn absent_store_loads_empty_and_round_trips_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("campaigns.json");
        let mut store = CampaignStore::load(&path).unwrap();
        assert!(store.campaigns.is_empty());
        store
            .add(Campaign {
                name: "one".to_string(),
                project: "p".to_string(),
                mission: "ship one thing".to_string(),
                intent_refs: vec![],
                context_paths: vec![],
                done_evidence: vec![DoneEvidence::Review {
                    statement: "it is shipped".to_string(),
                }],
                budget: CampaignBudget::default(),
                escalation: vec![],
                status: CampaignStatus::Staged,
                cycles_completed: 0,
                cycles_landed: 0,
                authorized_by: Some("tester".to_string()),
                agent_provider: None,
                last_run_event_id: None,
                owner_decisions: vec![],
                pending_run_result: None,
            })
            .unwrap();
        store.save(&path).unwrap();
        assert_eq!(CampaignStore::load(&path).unwrap().campaigns.len(), 1);
        assert!(!path.with_extension("json.tmp").exists());
    }

    fn campaign_with_gate(command: &str, required: bool) -> Campaign {
        Campaign {
            name: "c".to_string(),
            project: "p".to_string(),
            mission: "ship".to_string(),
            intent_refs: vec![],
            context_paths: vec![],
            done_evidence: vec![DoneEvidence::Gate {
                command: command.to_string(),
                required,
                artifacts: vec![],
            }],
            budget: CampaignBudget::default(),
            escalation: vec![],
            status: CampaignStatus::Staged,
            cycles_completed: 0,
            cycles_landed: 0,
            authorized_by: Some("tester".to_string()),
            agent_provider: None,
            last_run_event_id: None,
            owner_decisions: vec![],
            pending_run_result: None,
        }
    }

    /// The exact gate that made parite-remote-nvenc-conversion-v1 structurally
    /// incapable of completing: it can never pass until the whole mission is
    /// already deployed, so it consumed all 12 cycles.
    #[test]
    fn required_gate_asserting_remote_host_state_is_rejected() {
        let campaign = campaign_with_gate(
            "ssh -o BatchMode=yes odin.local 'systemctl is-active --quiet parite-converterd' && ssh -o BatchMode=yes parite.local 'systemctl is-active --quiet parited'",
            true,
        );
        let error = campaign.validate().unwrap_err().to_string();
        assert!(error.contains("cannot run in a task worktree"), "unexpected error: {error}");
        assert!(error.contains("ssh"), "error should name the offending command: {error}");
        assert!(
            error.contains("review"),
            "error should point at the correct home for deployment evidence: {error}"
        );
    }

    #[test]
    fn off_worktree_command_is_found_in_any_command_position() {
        for command in [
            "ssh host 'true'",
            "cargo test && ssh host 'systemctl is-active x'",
            "sudo systemctl is-active parited",
            "/usr/bin/ssh host true",
            "DEPLOY=1 rsync -a ./ host:/srv",
            "cargo build; launchctl list",
        ] {
            assert!(
                off_worktree_command(command).is_some(),
                "should have flagged an off-worktree command: {command}"
            );
        }
    }

    #[test]
    fn ordinary_repository_gates_remain_acceptable() {
        for command in [
            "cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test",
            "cargo test -- --skip ssh_transport_tests",
            "npm run typecheck && npm test -- --runInBand",
            "mix test && mix credo --strict",
            "cargo deny check && mdbook build book && git diff --check",
        ] {
            assert!(
                off_worktree_command(command).is_none(),
                "false positive on a legitimate repository gate: {command}"
            );
            assert!(campaign_with_gate(command, true).validate().is_ok());
        }
    }

    /// Only required gates block `done`; an advisory host probe is allowed.
    #[test]
    fn optional_gate_may_assert_host_state() {
        assert!(
            campaign_with_gate("ssh host 'systemctl is-active parited'", false)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn empty_file_loads_as_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("campaigns.json");
        std::fs::write(&path, "").unwrap();

        let store = CampaignStore::load(&path).unwrap();
        assert!(store.campaigns.is_empty());
    }

    #[test]
    fn legacy_campaign_defaults_pending_result_and_gate_artifacts() {
        let campaign: Campaign = serde_json::from_value(serde_json::json!({
            "name": "legacy",
            "project": "p",
            "mission": "ship",
            "done_evidence": [{
                "kind": "gate",
                "command": "cargo test",
                "required": true
            }]
        }))
        .unwrap();

        assert!(campaign.owner_decisions.is_empty());
        assert!(campaign.pending_run_result.is_none());
        let DoneEvidence::Gate { artifacts, .. } = &campaign.done_evidence[0] else {
            panic!("expected gate evidence");
        };
        assert!(artifacts.is_empty());
    }

    #[test]
    fn owner_decisions_round_trip_as_rfc3339_strings() {
        let campaign = Campaign {
            name: "one".to_string(),
            project: "p".to_string(),
            mission: "ship one thing".to_string(),
            intent_refs: vec![],
            context_paths: vec![],
            done_evidence: vec![DoneEvidence::Review {
                statement: "it is shipped".to_string(),
            }],
            budget: CampaignBudget::default(),
            escalation: vec![],
            status: CampaignStatus::Active,
            cycles_completed: 0,
            cycles_landed: 0,
            authorized_by: Some("tester".to_string()),
            agent_provider: None,
            last_run_event_id: None,
            owner_decisions: vec![OwnerDecision {
                decision: "Proceed with the gRPC path.".to_string(),
                authorized_by: "tester".to_string(),
                decided_at: chrono::DateTime::parse_from_rfc3339("2026-07-18T12:00:00Z")
                    .unwrap()
                    .with_timezone(&chrono::Utc),
            }],
            pending_run_result: None,
        };

        let json = serde_json::to_value(&campaign).unwrap();
        assert_eq!(
            json["owner_decisions"][0]["decided_at"],
            serde_json::json!("2026-07-18T12:00:00Z")
        );

        let round_trip: Campaign = serde_json::from_value(json).unwrap();
        assert_eq!(round_trip.owner_decisions, campaign.owner_decisions);
    }

    #[test]
    fn exclusive_guard_serializes_cross_thread_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("campaigns.json");
        let first = CampaignStore::lock_exclusive(&path).unwrap();
        let (started_tx, started_rx) = mpsc::channel();
        let (acquired_tx, acquired_rx) = mpsc::channel();
        let second_path = path.clone();
        let handle = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let _guard = CampaignStore::lock_exclusive(&second_path).unwrap();
            acquired_tx.send(()).unwrap();
        });

        started_rx.recv().unwrap();
        assert!(acquired_rx.recv_timeout(Duration::from_millis(100)).is_err());
        drop(first);
        acquired_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        handle.join().unwrap();
    }
}
