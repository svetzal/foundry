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
//! [`merge_default_seed_into`] fills missing provider/tier/effort keys and
//! migrates retired canonical model aliases. Custom model overrides remain
//! untouched. New providers, tiers, effort levels, and replacement defaults
//! therefore reach existing installs automatically.
//!
//! A provider may also declare optional per-tier `effort_caps` — a ceiling on
//! the reasoning effort any request for that tier may use (for example
//! `{"deep": "high", "balanced": "medium"}`). Caps are operator policy: the seed
//! carries none, and the seed merge never adds, removes, or rewrites them.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::gateway::{AgentProvider, ModelTier, ReasoningEffort};

/// Current agent-config store format version.
pub const AGENT_CONFIG_VERSION: u32 = 1;

/// Per-provider resolution maps: tier→model id, effort→CLI token, and optional
/// per-tier effort caps.
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
    /// Optional ceiling on reasoning effort per model tier. A request whose
    /// effort exceeds its tier's cap is lowered to the cap before it is mapped
    /// to a CLI token; a lower request is never raised. Absent tiers are
    /// uncapped. Omitted from the JSON when empty.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub effort_caps: BTreeMap<ModelTier, ReasoningEffort>,
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
                effort_caps: BTreeMap::new(),
            }
        }
        match provider {
            // claude --effort accepts low|medium|high; clamp the extremes.
            AgentProvider::Claude => build(
                &[
                    (ModelTier::Deep, "claude-opus-5"),
                    (ModelTier::Balanced, "claude-sonnet-5"),
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

    /// The effort a request for `tier` actually runs at: `requested`, lowered
    /// to the tier's cap when one is configured and `requested` exceeds it.
    /// Never raises a request; an uncapped tier returns `requested` unchanged.
    pub fn effective_effort(&self, tier: ModelTier, requested: ReasoningEffort) -> ReasoningEffort {
        self.effort_caps.get(&tier).map_or(requested, |cap| requested.min(*cap))
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
            resolved.effort_caps.clone_from(&overrides.effort_caps);
        }
        resolved
    }
}

/// Seed merge: fill any provider, tier, or effort key missing from `store` and
/// migrate retired canonical Claude model aliases to their replacements.
///
/// Returns `true` when anything changed (the caller should persist). Only
/// previously shipped canonical aliases are replaced; all other hand-edited
/// model ids and effort tokens survive. `effort_caps` are operator policy and
/// are never added, removed, or rewritten.
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
    if let Some(claude) = store.providers.get_mut(&AgentProvider::Claude) {
        changed |= migrate_canonical_model(
            &mut claude.models,
            ModelTier::Deep,
            &["claude-opus-4-6", "claude-opus-4-8"],
            "claude-opus-5",
        );
        changed |= migrate_canonical_model(
            &mut claude.models,
            ModelTier::Balanced,
            &["claude-sonnet-4-6"],
            "claude-sonnet-5",
        );
    }
    changed
}

fn migrate_canonical_model(
    models: &mut BTreeMap<ModelTier, String>,
    tier: ModelTier,
    retired: &[&str],
    replacement: &str,
) -> bool {
    let Some(current) = models.get_mut(&tier) else {
        return false;
    };
    if retired.contains(&current.as_str()) {
        *current = replacement.to_string();
        true
    } else {
        false
    }
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
    fn default_tier_models_match_current_provider_mappings() {
        let claude = ProviderModels::default_for(AgentProvider::Claude);
        assert_eq!(claude.model(ModelTier::Deep, AgentProvider::Claude), "claude-opus-5");
        assert_eq!(claude.model(ModelTier::Balanced, AgentProvider::Claude), "claude-sonnet-5");
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
        assert_eq!(resolved.model(ModelTier::Deep, AgentProvider::Claude), "claude-opus-5");
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
    fn merge_migrates_retired_canonical_claude_models() {
        let mut store = AgentConfigStore::default_seed();
        let claude = store.providers.get_mut(&AgentProvider::Claude).unwrap();
        claude.models.insert(ModelTier::Deep, "claude-opus-4-8".to_string());
        claude.models.insert(ModelTier::Balanced, "claude-sonnet-4-6".to_string());

        assert!(merge_default_seed_into(&mut store));
        let claude = &store.providers[&AgentProvider::Claude];
        assert_eq!(claude.models[&ModelTier::Deep], "claude-opus-5");
        assert_eq!(claude.models[&ModelTier::Balanced], "claude-sonnet-5");
        assert!(!merge_default_seed_into(&mut store));
    }

    #[test]
    fn merge_preserves_custom_claude_model_overrides() {
        let mut store = AgentConfigStore::default_seed();
        let claude = store.providers.get_mut(&AgentProvider::Claude).unwrap();
        claude.models.insert(ModelTier::Deep, "custom-opus".to_string());
        claude.models.insert(ModelTier::Balanced, "custom-sonnet".to_string());

        assert!(!merge_default_seed_into(&mut store));
        let claude = &store.providers[&AgentProvider::Claude];
        assert_eq!(claude.models[&ModelTier::Deep], "custom-opus");
        assert_eq!(claude.models[&ModelTier::Balanced], "custom-sonnet");
    }

    fn codex_with_caps() -> ProviderModels {
        let mut pm = ProviderModels::default_for(AgentProvider::Codex);
        pm.effort_caps.insert(ModelTier::Deep, ReasoningEffort::High);
        pm.effort_caps.insert(ModelTier::Balanced, ReasoningEffort::Medium);
        pm
    }

    #[test]
    fn effort_cap_lowers_a_higher_request() {
        let pm = codex_with_caps();
        assert_eq!(
            pm.effective_effort(ModelTier::Balanced, ReasoningEffort::High),
            ReasoningEffort::Medium
        );
        assert_eq!(
            pm.effective_effort(ModelTier::Deep, ReasoningEffort::Max),
            ReasoningEffort::High
        );
    }

    #[test]
    fn effort_cap_does_not_raise_a_lower_request() {
        let pm = codex_with_caps();
        assert_eq!(
            pm.effective_effort(ModelTier::Balanced, ReasoningEffort::Low),
            ReasoningEffort::Low
        );
        assert_eq!(
            pm.effective_effort(ModelTier::Deep, ReasoningEffort::High),
            ReasoningEffort::High
        );
    }

    #[test]
    fn missing_effort_cap_is_a_no_op() {
        let pm = codex_with_caps();
        assert_eq!(
            pm.effective_effort(ModelTier::Fast, ReasoningEffort::Max),
            ReasoningEffort::Max
        );
        let uncapped = ProviderModels::default_for(AgentProvider::Codex);
        for tier in ModelTier::ALL {
            for effort in ReasoningEffort::ALL {
                assert_eq!(uncapped.effective_effort(tier, effort), effort);
            }
        }
    }

    #[test]
    fn default_seed_carries_no_effort_caps_and_omits_the_key() {
        let seed = AgentConfigStore::default_seed();
        assert!(seed.providers.values().all(|pm| pm.effort_caps.is_empty()));
        let json = serde_json::to_string(&seed).unwrap();
        assert!(!json.contains("effort_caps"), "json: {json}");
    }

    #[test]
    fn resolved_carries_operator_effort_caps() {
        let json = r#"{"version":1,"providers":{"codex":{"effort_caps":{"deep":"high","balanced":"medium"}}}}"#;
        let store: AgentConfigStore = serde_json::from_str(json).unwrap();
        let resolved = store.resolved(AgentProvider::Codex);
        assert_eq!(resolved.effort_caps[&ModelTier::Deep], ReasoningEffort::High);
        assert_eq!(resolved.effort_caps[&ModelTier::Balanced], ReasoningEffort::Medium);
        // Caps on one provider do not leak to another.
        assert!(store.resolved(AgentProvider::Claude).effort_caps.is_empty());
    }

    #[test]
    fn merge_preserves_operator_effort_caps_and_adds_none() {
        let mut store = AgentConfigStore::default_seed();
        store
            .providers
            .get_mut(&AgentProvider::Codex)
            .unwrap()
            .effort_caps
            .insert(ModelTier::Balanced, ReasoningEffort::Medium);
        store.providers.get_mut(&AgentProvider::Claude).unwrap().effort.clear();

        assert!(merge_default_seed_into(&mut store));
        let codex = &store.providers[&AgentProvider::Codex];
        assert_eq!(codex.effort_caps.len(), 1);
        assert_eq!(codex.effort_caps[&ModelTier::Balanced], ReasoningEffort::Medium);
        assert!(store.providers[&AgentProvider::Claude].effort_caps.is_empty());
        assert!(store.providers[&AgentProvider::Opencode].effort_caps.is_empty());
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
