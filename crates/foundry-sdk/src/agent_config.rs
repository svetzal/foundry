//! Agent model configuration — the per-provider mapping from abstract
//! [`ModelTier`] / [`ReasoningEffort`] to concrete model ids and provider CLI
//! tokens.
//!
//! Blocks request work in provider-neutral terms (a tier and an effort). Each
//! provider resolves those to concrete strings via this config. Defaults are
//! baked into [`ProviderModels::default_for`] so the daemon works with no file;
//! `~/.foundry/agents.json` (see [`crate::paths::agent_config_path`]) lets an
//! operator override any tier→model or effort→token entry without rebuilding.
//!
//! The store follows the same seed-merge discipline as sentinels: on first
//! start the full default seed is written; on later starts
//! [`merge_default_seed_into`] additively fills any provider or individual
//! tier/effort key missing from the user's file, leaving overridden entries
//! untouched. New providers, tiers, or effort levels shipped in a release
//! therefore reach existing installs automatically.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::gateway::{AgentProvider, ModelTier, ReasoningEffort};

/// Current agent-config store format version.
pub const AGENT_CONFIG_VERSION: u32 = 1;

/// Per-provider resolution maps: tier→model id and effort→CLI token.
///
/// On disk these may be partial; missing keys fall back to
/// [`ProviderModels::default_for`]. After [`AgentConfigStore::resolved`] they
/// are complete for every tier and effort level.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderModels {
    /// Abstract model tier → concrete model id passed to the provider CLI.
    #[serde(default)]
    pub models: BTreeMap<ModelTier, String>,
    /// Abstract reasoning effort → the provider CLI's reasoning-effort token.
    #[serde(default)]
    pub effort: BTreeMap<ReasoningEffort, String>,
}

impl ProviderModels {
    /// The baked-in default maps for a provider — always complete for every
    /// tier and effort level.
    ///
    /// Effort tokens reflect what each CLI accepts: opencode `--variant` and
    /// codex `model_reasoning_effort` take the literal level; claude `--effort`
    /// has no `minimal`/`max`, so those clamp to `low`/`high`.
    pub fn default_for(provider: AgentProvider) -> Self {
        fn build(
            models: &[(ModelTier, &str)],
            effort: &[(ReasoningEffort, &str)],
        ) -> ProviderModels {
            ProviderModels {
                models: models.iter().map(|(t, m)| (*t, (*m).to_string())).collect(),
                effort: effort.iter().map(|(e, v)| (*e, (*v).to_string())).collect(),
            }
        }
        match provider {
            // claude --effort accepts low|medium|high; clamp the extremes.
            AgentProvider::Claude => build(
                &[
                    (ModelTier::Deep, "claude-opus-4-8"),
                    (ModelTier::Balanced, "claude-sonnet-4-6"),
                    (ModelTier::Fast, "claude-haiku-4-5-20251001"),
                ],
                &[
                    (ReasoningEffort::Minimal, "low"),
                    (ReasoningEffort::Low, "low"),
                    (ReasoningEffort::Medium, "medium"),
                    (ReasoningEffort::High, "high"),
                    (ReasoningEffort::Max, "high"),
                ],
            ),
            AgentProvider::Opencode => build(
                &[
                    (ModelTier::Deep, "openai/gpt-5.5"),
                    (ModelTier::Balanced, "openai/gpt-5.4"),
                    (ModelTier::Fast, "openai/gpt-5.4-mini"),
                ],
                &[
                    (ReasoningEffort::Minimal, "minimal"),
                    (ReasoningEffort::Low, "low"),
                    (ReasoningEffort::Medium, "medium"),
                    (ReasoningEffort::High, "high"),
                    (ReasoningEffort::Max, "max"),
                ],
            ),
            // codex model_reasoning_effort accepts minimal|low|medium|high; clamp `max` to `high`.
            AgentProvider::Codex => build(
                &[
                    (ModelTier::Deep, "gpt-5.5"),
                    (ModelTier::Balanced, "gpt-5.4"),
                    (ModelTier::Fast, "gpt-5.4-mini"),
                ],
                &[
                    (ReasoningEffort::Minimal, "minimal"),
                    (ReasoningEffort::Low, "low"),
                    (ReasoningEffort::Medium, "medium"),
                    (ReasoningEffort::High, "high"),
                    (ReasoningEffort::Max, "high"),
                ],
            ),
        }
    }

    /// The concrete model id for a tier. Falls back to the provider default when
    /// the (overlaid) map lacks the key; `provider` is only used for that
    /// fallback. After [`AgentConfigStore::resolved`] the key is always present.
    pub fn model(&self, tier: ModelTier, provider: AgentProvider) -> String {
        self.models
            .get(&tier)
            .cloned()
            .unwrap_or_else(|| Self::default_for(provider).models[&tier].clone())
    }

    /// The provider CLI token for an effort level, with the same fallback
    /// semantics as [`ProviderModels::model`].
    pub fn effort_token(&self, effort: ReasoningEffort, provider: AgentProvider) -> String {
        self.effort
            .get(&effort)
            .cloned()
            .unwrap_or_else(|| Self::default_for(provider).effort[&effort].clone())
    }
}

/// The on-disk agent model configuration — the source of truth for how abstract
/// tiers and effort levels resolve to concrete provider model ids and tokens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfigStore {
    /// Format version. Bumped on schema-breaking changes.
    pub version: u32,
    /// Per-provider resolution maps.
    pub providers: BTreeMap<AgentProvider, ProviderModels>,
}

impl AgentConfigStore {
    /// Deserialize a store from a JSON file at the given path.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NotFound`] when the file does not exist,
    /// [`StoreError::Io`] on read failure, and [`StoreError::Parse`] when
    /// the file contains malformed JSON.
    pub fn load(path: &Path) -> Result<Self, StoreError> {
        if !path.exists() {
            return Err(StoreError::NotFound {
                path: path.to_owned(),
            });
        }
        let content = std::fs::read_to_string(path).map_err(|source| StoreError::Io {
            path: path.to_owned(),
            source,
        })?;
        let store: Self = serde_json::from_str(&content).map_err(|source| StoreError::Parse {
            path: path.to_owned(),
            source,
        })?;
        Ok(store)
    }

    /// Serialize the store to a JSON file, creating parent directories.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Io`] if parent directory creation or the file
    /// write fails, or [`StoreError::Parse`] if serialization fails (extremely
    /// rare in practice).
    pub fn save(&self, path: &Path) -> Result<(), StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Io {
                path: path.to_owned(),
                source,
            })?;
        }
        let content = serde_json::to_string_pretty(self).map_err(|source| StoreError::Parse {
            path: path.to_owned(),
            source,
        })?;
        std::fs::write(path, content).map_err(|source| StoreError::Io {
            path: path.to_owned(),
            source,
        })?;
        Ok(())
    }

    /// The canonical seed — complete default maps for every known provider.
    pub fn default_seed() -> Self {
        let providers = [
            AgentProvider::Claude,
            AgentProvider::Opencode,
            AgentProvider::Codex,
        ]
        .into_iter()
        .map(|p| (p, ProviderModels::default_for(p)))
        .collect();
        Self {
            version: AGENT_CONFIG_VERSION,
            providers,
        }
    }

    /// The fully-resolved maps for a provider: the baked defaults with any
    /// user-supplied overrides overlaid on top. Always complete.
    pub fn resolved(&self, provider: AgentProvider) -> ProviderModels {
        let mut resolved = ProviderModels::default_for(provider);
        if let Some(overrides) = self.providers.get(&provider) {
            for (tier, model) in &overrides.models {
                resolved.models.insert(*tier, model.clone());
            }
            for (effort, token) in &overrides.effort {
                resolved.effort.insert(*effort, token.clone());
            }
        }
        resolved
    }
}

/// Additive seed-merge: fill any provider, tier, or effort key missing from
/// `store` with the canonical default. Returns `true` when anything was added
/// (the caller should persist). Never overwrites a key the user already set, so
/// hand-edited model ids and effort tokens survive.
pub fn merge_default_seed_into(store: &mut AgentConfigStore) -> bool {
    let mut changed = false;
    for provider in [
        AgentProvider::Claude,
        AgentProvider::Opencode,
        AgentProvider::Codex,
    ] {
        let defaults = ProviderModels::default_for(provider);
        let entry = store.providers.entry(provider).or_insert_with(|| {
            changed = true;
            ProviderModels::default()
        });
        for (tier, model) in &defaults.models {
            if !entry.models.contains_key(tier) {
                entry.models.insert(*tier, model.clone());
                changed = true;
            }
        }
        for (effort, token) in &defaults.effort {
            if !entry.effort.contains_key(effort) {
                entry.effort.insert(*effort, token.clone());
                changed = true;
            }
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_for_is_complete_for_every_tier_and_effort() {
        for provider in [
            AgentProvider::Claude,
            AgentProvider::Opencode,
            AgentProvider::Codex,
        ] {
            let pm = ProviderModels::default_for(provider);
            for tier in ModelTier::ALL {
                assert!(pm.models.contains_key(&tier), "{provider} missing tier {tier}");
            }
            for effort in ReasoningEffort::ALL {
                assert!(pm.effort.contains_key(&effort), "{provider} missing effort {effort}");
            }
        }
    }

    #[test]
    fn default_tier_models_match_prior_hardcoded_mappings() {
        let claude = ProviderModels::default_for(AgentProvider::Claude);
        assert_eq!(claude.model(ModelTier::Deep, AgentProvider::Claude), "claude-opus-4-8");
        assert_eq!(claude.model(ModelTier::Balanced, AgentProvider::Claude), "claude-sonnet-4-6");
        assert_eq!(
            claude.model(ModelTier::Fast, AgentProvider::Claude),
            "claude-haiku-4-5-20251001"
        );

        let oc = ProviderModels::default_for(AgentProvider::Opencode);
        assert_eq!(oc.model(ModelTier::Deep, AgentProvider::Opencode), "openai/gpt-5.5");
        assert_eq!(oc.model(ModelTier::Fast, AgentProvider::Opencode), "openai/gpt-5.4-mini");

        let codex = ProviderModels::default_for(AgentProvider::Codex);
        assert_eq!(codex.model(ModelTier::Balanced, AgentProvider::Codex), "gpt-5.4");
    }

    #[test]
    fn claude_clamps_unsupported_effort_levels() {
        let claude = ProviderModels::default_for(AgentProvider::Claude);
        assert_eq!(claude.effort_token(ReasoningEffort::Minimal, AgentProvider::Claude), "low");
        assert_eq!(claude.effort_token(ReasoningEffort::Max, AgentProvider::Claude), "high");
        assert_eq!(claude.effort_token(ReasoningEffort::Medium, AgentProvider::Claude), "medium");
    }

    #[test]
    fn opencode_passes_effort_levels_through() {
        let oc = ProviderModels::default_for(AgentProvider::Opencode);
        assert_eq!(oc.effort_token(ReasoningEffort::Minimal, AgentProvider::Opencode), "minimal");
        assert_eq!(oc.effort_token(ReasoningEffort::Max, AgentProvider::Opencode), "max");
    }

    #[test]
    fn resolved_overlays_user_overrides_on_defaults() {
        let mut store = AgentConfigStore::default_seed();
        // User overrides only codex's deep model; everything else should remain.
        let codex = store.providers.get_mut(&AgentProvider::Codex).unwrap();
        codex.models.insert(ModelTier::Deep, "gpt-6-preview".to_string());

        let resolved = store.resolved(AgentProvider::Codex);
        assert_eq!(resolved.model(ModelTier::Deep, AgentProvider::Codex), "gpt-6-preview");
        // Untouched keys keep their defaults.
        assert_eq!(resolved.model(ModelTier::Fast, AgentProvider::Codex), "gpt-5.4-mini");
        assert_eq!(resolved.effort_token(ReasoningEffort::High, AgentProvider::Codex), "high");
    }

    #[test]
    fn resolved_falls_back_to_defaults_for_absent_provider() {
        let store = AgentConfigStore {
            version: AGENT_CONFIG_VERSION,
            providers: BTreeMap::new(),
        };
        let resolved = store.resolved(AgentProvider::Claude);
        assert_eq!(resolved.model(ModelTier::Deep, AgentProvider::Claude), "claude-opus-4-8");
    }

    #[test]
    fn merge_fills_missing_keys_without_overwriting() {
        // Start from a store where codex has only a custom deep model.
        let mut providers = BTreeMap::new();
        let mut codex = ProviderModels::default();
        codex.models.insert(ModelTier::Deep, "custom-deep".to_string());
        providers.insert(AgentProvider::Codex, codex);
        let mut store = AgentConfigStore {
            version: AGENT_CONFIG_VERSION,
            providers,
        };

        let changed = merge_default_seed_into(&mut store);
        assert!(changed);
        // Custom value preserved.
        assert_eq!(store.providers[&AgentProvider::Codex].models[&ModelTier::Deep], "custom-deep");
        // Missing keys filled.
        assert_eq!(store.providers[&AgentProvider::Codex].models[&ModelTier::Fast], "gpt-5.4-mini");
        // Missing providers added.
        assert!(store.providers.contains_key(&AgentProvider::Claude));
        assert!(store.providers.contains_key(&AgentProvider::Opencode));

        // Idempotent: a second merge changes nothing.
        assert!(!merge_default_seed_into(&mut store));
    }

    #[test]
    fn store_round_trips_through_json_with_enum_keys() {
        let seed = AgentConfigStore::default_seed();
        let json = serde_json::to_string(&seed).unwrap();
        // Enum keys serialize as their lowercase wire form.
        assert!(json.contains("\"deep\""), "json: {json}");
        assert!(json.contains("\"minimal\""), "json: {json}");
        let back: AgentConfigStore = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back.resolved(AgentProvider::Opencode)
                .model(ModelTier::Deep, AgentProvider::Opencode),
            "openai/gpt-5.5"
        );
    }
}
