use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::StoreError;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DoneEvidence {
    Gate {
        command: String,
        #[serde(default = "default_required")]
        required: bool,
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
            })
            .unwrap();
        store.save(&path).unwrap();
        assert_eq!(CampaignStore::load(&path).unwrap().campaigns.len(), 1);
        assert!(!path.with_extension("json.tmp").exists());
    }
}
