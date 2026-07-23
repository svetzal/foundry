use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::payload::{TaskRunCompletedPayload, TaskVerdict};

/// How many recent objectives a campaign remembers.
///
/// This is formation's working memory, not an archive — every objective is
/// already durable in the `CampaignAdvanceCompleted` event stream. Eight
/// matches the depth of the trunk snapshot's own `git log -8`, stays small
/// against the default twenty-cycle budget, and is deep enough to expose a
/// restatement: an agent re-cutting cycle one's objective at cycle nine has
/// been re-cutting it at the cycles in between too.
const OBJECTIVE_HISTORY_LIMIT: usize = 8;

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
    /// Objectives this campaign has already cut, oldest first, retaining only
    /// the most recent few cycles. Fed back into formation so the decision
    /// agent can recognise that it is restating an earlier request instead of
    /// cutting new work.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub objective_history: Vec<CampaignCycle>,
}

/// How a campaign context path reaches formation.
///
/// The two are not interchangeable. A *binding* artifact is normative — its
/// exact wording must survive into the acceptance criteria — so it is inlined
/// verbatim. An *orienting* artifact only says where to look, and the formation
/// agent already holds `Read`, `Glob`, and `Grep` over the checkout, so
/// inlining one pays whole-file token cost for the few functions it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextRole {
    /// Inlined verbatim: the wording itself is binding.
    Binding,
    /// Listed as a path for the agent to read on demand.
    Orienting,
}

/// Extensions treated as orienting rather than binding.
///
/// Source is the clear case: it is read to understand current state, never
/// quoted as normative wording. Prose and data formats stay binding, which is
/// what `context_paths` was designed for — charters and intent projections.
const ORIENTING_EXTENSIONS: &[&str] = &[
    "rs", "ex", "exs", "ts", "tsx", "js", "jsx", "py", "go", "rb", "java", "kt", "swift", "c", "h",
    "cpp", "hpp", "cs", "sh", "sql",
];

/// Total bytes of *inlined* context a campaign may carry.
///
/// The hard ceiling is the platform's `ARG_MAX` (1 MiB on macOS), because every
/// provider passes the prompt as a command-line argument — exceed it and
/// `execve` fails before the agent starts, which surfaces only as an opaque
/// "unavailable" that retries cannot help. This budget sits far below that,
/// both to leave room for the rest of the prompt and because a formation
/// decision made against a megabyte of inlined source is diluted long before it
/// is impossible. Campaigns that landed well used 35–183 KB.
pub const MAX_INLINE_CONTEXT_BYTES: u64 = 262_144;

/// Whether a context path is inlined verbatim or listed for on-demand reading.
#[must_use]
pub fn context_role(path: &str) -> ContextRole {
    let extension = std::path::Path::new(path)
        .extension()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if ORIENTING_EXTENSIONS.contains(&extension.as_str()) {
        ContextRole::Orienting
    } else {
        ContextRole::Binding
    }
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
    /// Record an objective as dispatched for the cycle just counted.
    ///
    /// Older entries are dropped rather than kept, so the store a campaign
    /// rewrites on every advance stays bounded no matter how long the mission
    /// runs.
    pub fn record_dispatched_objective(&mut self, objective: String) {
        self.objective_history.push(CampaignCycle {
            cycle: self.cycles_completed,
            objective,
            outcome: None,
        });
        let overflow = self.objective_history.len().saturating_sub(OBJECTIVE_HISTORY_LIMIT);
        self.objective_history.drain(..overflow);
    }

    /// Stamp a typed run result onto the objective that produced it.
    ///
    /// A result always arrives on the advance after its dispatch, so the open
    /// entry is the last one. A result with no open entry to close — a campaign
    /// that predates this history, or a run replayed after its outcome was
    /// already recorded — is dropped rather than invented as a cycle nobody
    /// cut.
    pub fn record_cycle_outcome(&mut self, result: &TaskRunCompletedPayload) {
        if let Some(cycle) = self.objective_history.last_mut()
            && cycle.outcome.is_none()
        {
            cycle.outcome = Some(CycleOutcome {
                verdict: result.verdict.clone(),
                landed: result.landed,
            });
        }
    }

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

/// One objective this campaign cut, and how its execution ended.
///
/// Appended when the objective is dispatched; the outcome is stamped on when
/// that task's typed result arrives, which is on the following advance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CampaignCycle {
    pub cycle: u64,
    pub objective: String,
    /// Absent while the dispatched task is still running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<CycleOutcome>,
}

/// A dispatched cycle's typed verdict together with whether its work reached
/// trunk.
///
/// Both halves are needed to read a repeat correctly: a `remainder` that landed
/// is converging work with gaps left, while a `remainder` that landed nothing
/// is a cycle that produced no integrated progress at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleOutcome {
    #[serde(flatten)]
    pub verdict: TaskVerdict,
    pub landed: bool,
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
                objective_history: vec![],
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
            objective_history: vec![],
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
        assert!(campaign.objective_history.is_empty());
        let DoneEvidence::Gate { artifacts, .. } = &campaign.done_evidence[0] else {
            panic!("expected gate evidence");
        };
        assert!(artifacts.is_empty());
    }

    /// The live store holds campaigns written before objective history existed;
    /// a store that stops loading takes every one of them with it.
    #[test]
    fn store_written_before_objective_history_still_loads_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("campaigns.json");
        std::fs::write(
            &path,
            r#"{
              "version": 1,
              "campaigns": [{
                "name": "legacy",
                "project": "p",
                "mission": "ship",
                "done_evidence": [{"kind": "review", "statement": "shipped"}],
                "status": "active",
                "cycles_completed": 4,
                "authorized_by": "owner"
              }]
            }"#,
        )
        .unwrap();

        let store = CampaignStore::load(&path).unwrap();
        let campaign = store.find("legacy").unwrap();
        assert!(campaign.objective_history.is_empty());
        assert_eq!(campaign.cycles_completed, 4);

        // An untouched legacy campaign must not gain the key on rewrite either,
        // so a rollback still reads what the current build wrote.
        store.save(&path).unwrap();
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(
            !rewritten.contains("objective_history"),
            "an empty history must not be serialized: {rewritten}"
        );
        assert!(CampaignStore::load(&path).unwrap().find("legacy").is_some());
    }

    fn remainder_result(gap: &str, landed: bool) -> TaskRunCompletedPayload {
        TaskRunCompletedPayload {
            project: "p".to_string(),
            success: false,
            landed,
            summary: "converging".to_string(),
            preservation_ref: None,
            verdict: TaskVerdict::Remainder {
                gaps: vec![gap.to_string()],
            },
            context: crate::payload::LoopContext::default(),
        }
    }

    #[test]
    fn a_cycle_outcome_is_stamped_onto_the_objective_that_produced_it() {
        let mut campaign = campaign_with_gate("cargo test", true);
        campaign.cycles_completed = 1;
        campaign
            .record_dispatched_objective("Move registry list/show onto the daemon.".to_string());

        campaign.record_cycle_outcome(&remainder_result("the CLI still reads local files", true));

        let recorded = &campaign.objective_history[0];
        assert_eq!(recorded.cycle, 1);
        assert_eq!(
            recorded.outcome,
            Some(CycleOutcome {
                verdict: TaskVerdict::Remainder {
                    gaps: vec!["the CLI still reads local files".to_string()],
                },
                landed: true,
            })
        );
    }

    /// A replayed pending result must not overwrite an outcome already
    /// recorded, and a result arriving with no open dispatch must not invent a
    /// cycle nobody cut.
    #[test]
    fn a_second_result_does_not_rewrite_a_closed_or_absent_cycle() {
        let mut campaign = campaign_with_gate("cargo test", true);
        campaign.record_cycle_outcome(&remainder_result("nothing dispatched this", false));
        assert!(campaign.objective_history.is_empty());

        campaign.cycles_completed = 1;
        campaign.record_dispatched_objective("First slice.".to_string());
        campaign.record_cycle_outcome(&remainder_result("first gap", false));
        campaign.record_cycle_outcome(&remainder_result("second gap", true));

        let CycleOutcome {
            verdict: TaskVerdict::Remainder { gaps },
            landed,
        } = campaign.objective_history[0].outcome.clone().unwrap()
        else {
            panic!("expected a remainder outcome");
        };
        assert_eq!(gaps, vec!["first gap".to_string()]);
        assert!(!landed);
    }

    #[test]
    fn objective_history_retains_only_the_most_recent_cycles() {
        let mut campaign = campaign_with_gate("cargo test", true);
        for cycle in 1..=(OBJECTIVE_HISTORY_LIMIT as u64 + 3) {
            campaign.cycles_completed = cycle;
            campaign.record_dispatched_objective(format!("objective {cycle}"));
        }

        assert_eq!(campaign.objective_history.len(), OBJECTIVE_HISTORY_LIMIT);
        assert_eq!(campaign.objective_history[0].cycle, 4);
        assert_eq!(campaign.objective_history[0].objective, "objective 4");
        assert_eq!(campaign.objective_history.last().unwrap().cycle, 11);
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
            objective_history: vec![],
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

#[cfg(test)]
mod context_role_tests {
    use super::{ContextRole, MAX_INLINE_CONTEXT_BYTES, context_role};

    /// Source is read to understand current state; the agent has Read/Glob/Grep
    /// and only needs the path. Prose and data are normative wording and must
    /// reach the prompt verbatim.
    #[test]
    fn source_orients_while_prose_and_data_bind() {
        for path in [
            "parite-core/src/retrieval/coordinator.rs",
            "lib/ops_visualizer_web/live/foundry_live.ex",
            "src/apis/assessments.ts",
            "scripts/deploy.sh",
        ] {
            assert_eq!(context_role(path), ContextRole::Orienting, "should orient: {path}");
        }
        for path in [
            "CHARTER.md",
            "AGENTS.md",
            ".alloy/projections/AGENTS.generated.md",
            "campaigns/thing-v1.json",
            "docs/design.txt",
        ] {
            assert_eq!(context_role(path), ContextRole::Binding, "should bind: {path}");
        }
    }

    /// An extensionless file (LICENSE, Makefile) is prose until proven
    /// otherwise — binding is the safe default, since omitting normative
    /// wording is worse than inlining a small file.
    #[test]
    fn unknown_and_extensionless_paths_bind() {
        assert_eq!(context_role("LICENSE"), ContextRole::Binding);
        assert_eq!(context_role("Makefile"), ContextRole::Binding);
        assert_eq!(context_role("docs/notes.unknownext"), ContextRole::Binding);
    }

    #[test]
    fn extension_matching_is_case_insensitive() {
        assert_eq!(context_role("src/Main.RS"), ContextRole::Orienting);
    }

    /// The budget must stay well clear of the 1 MiB `ARG_MAX` that the whole
    /// command line has to fit inside, since context is only one of its parts.
    /// Checked at compile time so the constant can never drift into the cliff.
    #[test]
    fn inline_budget_leaves_room_for_the_rest_of_the_command_line() {
        const { assert!(MAX_INLINE_CONTEXT_BYTES < 1_048_576 / 2) };
    }
}
