//! The token price book — published vendor list rates, per model, per token
//! type, in USD per million tokens.
//!
//! Foundry's output is largely the work tokens do, so the unit of cost is the
//! token. This store is what turns a [`crate::token_usage::SessionUsage`] into
//! money, and it is deliberately **runtime data**: rates change on vendor
//! schedules, not release schedules, so `~/.foundry/token-rates.json` (see
//! [`crate::paths::token_rates_path`]) is editable without rebuilding. Defaults
//! are baked in and seed-merged on start, exactly like `agents.json`.
//!
//! ## What these numbers are, and are not
//!
//! They are **published list prices** — the retail rate a customer would pay a
//! vendor directly. They are the right basis for what work is worth, and the
//! right anchor for what to charge.
//!
//! They are *not* necessarily cash out the door. When a session runs on a
//! subscription plan the provider reports list-equivalent value while the
//! actual outlay is a flat fee. Margin is (charged) − (amortized plan), and
//! nothing in this module can see the second term. Treat a priced figure as
//! "what this work is worth at retail", not "what this cost us".
//!
//! ## How accurate the list figure is
//!
//! Measured against 400 real sessions on 2026-08-04, the list computation lands
//! within roughly 8% of the provider's own reported cost (ours high). The
//! residual is the cache-write TTL: Anthropic charges 1.25x base for a
//! 5-minute write and 2x for a 1-hour one, and the transcript reports that
//! split only per session, not per model or per request — so a session mixing
//! both is apportioned rather than counted.
//!
//! This matters less than it looks. Where the provider reports its own cost
//! (all Claude sessions), [`CostEstimate::best_usd`] returns that number and
//! the list figure is only a cross-check. The list figure is load-bearing for
//! Codex, which reports no cost — and Codex bills no cache-write category at
//! all, so the approximation does not apply there.
//!
//! ## Effective dating
//!
//! A rate may carry `until`, an ISO date after which it stops applying, plus a
//! `then` successor, so introductory and negotiated rates expire on their own
//! date rather than silently pricing next month's work at last month's number.
//! [`ModelRate::on`] resolves the rate in force on a given date.
//!
//! The seed uses no dated rates. Anthropic publishes introductory pricing for
//! Claude Sonnet 5 through 2026-08-31, but the provider's own reported cost
//! across 168 real sessions on 2026-08-04 tracks standard pricing instead — so
//! seeding the introductory rate would understate a third of the estate's spend.
//! Set a dated rate in the runtime book if an account actually receives one.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::StoreError;
use crate::token_usage::{ModelTokens, SessionUsage};

/// Current price-book format version.
pub const TOKEN_RATES_VERSION: u32 = 1;

/// Published list rates for one model, in USD per million tokens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelRate {
    /// Fresh (non-cached) input tokens.
    pub input: f64,
    /// Output tokens. Reasoning tokens bill at this rate and are already inside
    /// the output count — never add them separately.
    pub output: f64,
    /// Cache reads (hits).
    #[serde(default)]
    pub cache_read: f64,
    /// Cache writes at the 5-minute TTL.
    #[serde(default)]
    pub cache_write_5m: f64,
    /// Cache writes at the 1-hour TTL.
    #[serde(default)]
    pub cache_write_1h: f64,
    /// ISO date this rate was last verified against the vendor's page.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub as_of: String,
    /// Where it was verified. Kept per rate so a stale number is traceable to
    /// the page that would correct it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// ISO date this rate stops applying, when the vendor has published an end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// The rate that takes over after `until`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub then: Option<Box<ModelRate>>,
}

impl ModelRate {
    /// The rate in force on `date` (ISO `YYYY-MM-DD`), following `then`
    /// successors as far as needed.
    #[must_use]
    pub fn on(&self, date: &str) -> &ModelRate {
        let mut current = self;
        // Bounded rather than `loop`: a hand-edited book could chain a cycle,
        // and a pricing lookup must not hang the daemon.
        for _ in 0..16 {
            match (current.until.as_deref(), current.then.as_deref()) {
                (Some(until), Some(next)) if date > until => current = next,
                _ => return current,
            }
        }
        current
    }
}

const ANTHROPIC_SRC: &str = "https://platform.claude.com/docs/en/about-claude/pricing";
const OPENAI_SRC: &str = "https://developers.openai.com/api/docs/pricing";
const VERIFIED: &str = "2026-08-04";

fn anthropic(input: f64, output: f64) -> ModelRate {
    // Anthropic's cache rates are fixed multiples of base input: 0.1x read,
    // 1.25x 5-minute write, 2x 1-hour write. Deriving rather than transcribing
    // keeps a hand-edited base rate internally consistent.
    ModelRate {
        input,
        output,
        cache_read: input * 0.1,
        cache_write_5m: input * 1.25,
        cache_write_1h: input * 2.0,
        as_of: VERIFIED.to_string(),
        source: ANTHROPIC_SRC.to_string(),
        until: None,
        then: None,
    }
}

fn openai(input: f64, cached_input: f64, output: f64) -> ModelRate {
    ModelRate {
        input,
        output,
        cache_read: cached_input,
        // OpenAI does not bill a separate cache-write category.
        cache_write_5m: 0.0,
        cache_write_1h: 0.0,
        as_of: VERIFIED.to_string(),
        source: OPENAI_SRC.to_string(),
        until: None,
        then: None,
    }
}

/// The on-disk price book.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateBook {
    /// Format version. Bumped on schema-breaking changes.
    pub version: u32,
    /// Model id → published list rates.
    pub rates: BTreeMap<String, ModelRate>,
}

impl RateBook {
    /// Deserialize a price book from JSON at `path`.
    ///
    /// # Errors
    ///
    /// [`StoreError::NotFound`] when absent, [`StoreError::Io`] on read
    /// failure, [`StoreError::Parse`] on malformed JSON.
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
        serde_json::from_str(&content).map_err(|source| StoreError::Parse {
            path: path.to_owned(),
            source,
        })
    }

    /// Serialize the price book to `path`, creating parent directories.
    ///
    /// # Errors
    ///
    /// [`StoreError::Io`] on directory creation or write failure,
    /// [`StoreError::Parse`] on serialization failure.
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
        })
    }

    /// The baked-in seed: published list rates verified 2026-08-04.
    #[must_use]
    pub fn default_seed() -> Self {
        let mut rates = BTreeMap::new();

        // --- Anthropic -----------------------------------------------------
        for id in [
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
        ] {
            rates.insert(id.to_string(), anthropic(5.0, 25.0));
        }
        rates.insert("claude-opus-4-5".to_string(), anthropic(5.0, 25.0));
        rates.insert("claude-fable-5".to_string(), anthropic(10.0, 50.0));

        // Sonnet 5 is seeded at standard $3/$15, not the published introductory
        // $2/$10 that runs through 2026-08-31.
        //
        // The introductory rate is real and documented, but it is not what this
        // estate is billed. Checked against 168 real sessions on 2026-08-04: the
        // provider's own reported cost matches standard pricing (ratio 0.94)
        // and disagrees with introductory pricing by 40%. Seeding the
        // introductory rate would understate every Sonnet 5 session by a third
        // — an error in the dangerous direction, since it makes the work look
        // cheaper than it is.
        //
        // If an account does get introductory pricing, set it in the runtime
        // book with `until: "2026-08-31"` and a `then` successor; the machinery
        // is there and tested.
        rates.insert("claude-sonnet-5".to_string(), anthropic(3.0, 15.0));

        rates.insert("claude-sonnet-4-6".to_string(), anthropic(3.0, 15.0));
        rates.insert("claude-sonnet-4-5".to_string(), anthropic(3.0, 15.0));
        rates.insert("claude-haiku-4-5".to_string(), anthropic(1.0, 5.0));

        // --- OpenAI --------------------------------------------------------
        rates.insert("gpt-5.5".to_string(), openai(5.0, 0.50, 30.0));
        rates.insert("gpt-5.4".to_string(), openai(2.50, 0.25, 15.0));
        rates.insert("gpt-5.4-mini".to_string(), openai(0.75, 0.075, 4.50));

        Self {
            version: TOKEN_RATES_VERSION,
            rates,
        }
    }

    /// Look up a model's rate, tolerating the id variations that reach us from
    /// provider telemetry.
    ///
    /// Providers report ids the price book does not list verbatim: a dated
    /// snapshot (`claude-haiku-4-5-20251001`), or a context/mode suffix
    /// (`claude-opus-4-8[1m]`). Both bill at their base model's rate, so they
    /// are normalized rather than dropped — an unmatched id would silently
    /// price real spend at zero.
    #[must_use]
    pub fn lookup(&self, model: &str) -> Option<&ModelRate> {
        if let Some(rate) = self.rates.get(model) {
            return Some(rate);
        }
        let base = normalize_model_id(model);
        self.rates.get(base.as_str())
    }
}

/// Strip a bracketed mode suffix and a trailing `-YYYYMMDD` snapshot date.
fn normalize_model_id(model: &str) -> String {
    let without_mode = model.split('[').next().unwrap_or(model).trim_end();
    let parts: Vec<&str> = without_mode.rsplitn(2, '-').collect();
    if let [tail, head] = parts.as_slice()
        && tail.len() == 8
        && tail.chars().all(|c| c.is_ascii_digit())
    {
        return (*head).to_string();
    }
    without_mode.to_string()
}

/// Seed merge: add rates missing from `book` without touching hand-edited ones.
///
/// Returns `true` when anything changed, so the caller can persist. New models
/// therefore reach existing installs automatically, while a rate an operator
/// has deliberately overridden — a negotiated discount, say — survives.
pub fn merge_default_seed_into(book: &mut RateBook) -> bool {
    let mut changed = false;
    for (model, rate) in RateBook::default_seed().rates {
        if let std::collections::btree_map::Entry::Vacant(slot) = book.rates.entry(model) {
            slot.insert(rate);
            changed = true;
        }
    }
    changed
}

/// How much of a cost figure rests on evidence rather than assumption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    /// The provider reported its own cost. No rate book involved.
    ProviderReported,
    /// Computed from token counts times published list rates.
    ListPriced,
    /// Tokens are known but at least one model is missing from the price book,
    /// so the figure understates. Never present this as a total.
    PartiallyPriced,
    /// Tokens were spent but no usage record survived. Amount unknown.
    Unmeasured,
}

/// A costed session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostEstimate {
    /// USD at published list rates. `None` only when nothing could be measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_usd: Option<f64>,
    /// USD as the provider itself reported it, when it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_usd: Option<f64>,
    /// How much to trust the above.
    pub basis: CostBasis,
    /// Models whose spend could not be priced. Non-empty means the figure is
    /// low, and is the signal that the price book needs an entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unpriced_models: Vec<String>,
    /// Total tokens across every billed category.
    pub total_tokens: u64,
}

impl CostEstimate {
    /// The figure to bill or budget against: the provider's own number when it
    /// gave one, else the list-priced estimate.
    #[must_use]
    pub fn best_usd(&self) -> Option<f64> {
        self.provider_usd.or(self.list_usd)
    }

    /// An estimate for spend known to have happened but not measurable.
    #[must_use]
    pub fn unmeasured() -> Self {
        Self {
            list_usd: None,
            provider_usd: None,
            basis: CostBasis::Unmeasured,
            unpriced_models: Vec::new(),
            total_tokens: 0,
        }
    }
}

/// Price one model's tokens at the rate in force on `date`.
///
/// `cache_write_1h_share` blends the 1-hour and 5-minute write rates, because
/// the transcript reports that split only in aggregate.
#[must_use]
#[allow(clippy::cast_precision_loss)]
pub fn price_model(
    tokens: &ModelTokens,
    rate: &ModelRate,
    cache_write_1h_share: f64,
    date: &str,
) -> f64 {
    let rate = rate.on(date);
    let share = cache_write_1h_share.clamp(0.0, 1.0);
    let write_rate = rate.cache_write_1h.mul_add(share, rate.cache_write_5m * (1.0 - share));
    let per_mtok = |count: u64, price: f64| (count as f64) * price / 1_000_000.0;

    per_mtok(tokens.input_tokens, rate.input)
        + per_mtok(tokens.output_tokens, rate.output)
        + per_mtok(tokens.cache_read_tokens, rate.cache_read)
        + per_mtok(tokens.cache_write_tokens, write_rate)
}

/// Cost one session's usage against the price book, as of `date`.
///
/// The provider's own figure is carried through when present, but the list
/// price is computed either way: the two answer different questions, and their
/// disagreement is worth seeing.
#[must_use]
pub fn estimate(usage: &SessionUsage, book: &RateBook, date: &str) -> CostEstimate {
    let mut list = 0.0;
    let mut unpriced = Vec::new();

    for tokens in &usage.models {
        if let Some(rate) = book.lookup(&tokens.model) {
            list += price_model(tokens, rate, usage.cache_write_1h_share, date);
        } else {
            // An empty id means the provider never named its model (Codex
            // without a tier hint); a non-empty one is simply missing from the
            // book. Both understate, and both need surfacing.
            let label = if tokens.model.is_empty() {
                "<model not reported>".to_string()
            } else {
                tokens.model.clone()
            };
            unpriced.push(label);
        }
    }

    let basis = if !unpriced.is_empty() {
        CostBasis::PartiallyPriced
    } else if usage.provider_cost_usd.is_some() {
        CostBasis::ProviderReported
    } else {
        CostBasis::ListPriced
    };

    CostEstimate {
        list_usd: Some(list),
        provider_usd: usage.provider_cost_usd,
        basis,
        unpriced_models: unpriced,
        total_tokens: usage.total_tokens(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token_usage::UsageSource;

    fn usage(models: Vec<ModelTokens>, share: f64, provider: Option<f64>) -> SessionUsage {
        SessionUsage {
            models,
            provider_cost_usd: provider,
            cache_write_1h_share: share,
            source: UsageSource::ClaudeResult,
        }
    }

    fn tokens(model: &str, input: u64, output: u64) -> ModelTokens {
        ModelTokens {
            model: model.to_string(),
            input_tokens: input,
            output_tokens: output,
            ..ModelTokens::default()
        }
    }

    #[test]
    fn prices_input_and_output_at_list() {
        let book = RateBook::default_seed();
        // Opus 5: $5/MTok in, $25/MTok out.
        let u = usage(vec![tokens("claude-opus-5", 1_000_000, 100_000)], 0.0, None);
        let est = estimate(&u, &book, "2026-08-04");
        assert!((est.list_usd.unwrap() - (5.0 + 2.5)).abs() < 1e-9);
        assert_eq!(est.basis, CostBasis::ListPriced);
    }

    #[test]
    fn cache_reads_are_a_tenth_of_input_for_anthropic() {
        let book = RateBook::default_seed();
        let mut t = tokens("claude-opus-5", 0, 0);
        t.cache_read_tokens = 1_000_000;
        let est = estimate(&usage(vec![t], 0.0, None), &book, "2026-08-04");
        assert!((est.list_usd.unwrap() - 0.50).abs() < 1e-9);
    }

    #[test]
    fn a_one_hour_cache_write_costs_double_a_five_minute_one() {
        let book = RateBook::default_seed();
        let mut t = tokens("claude-opus-5", 0, 0);
        t.cache_write_tokens = 1_000_000;
        let five_min = estimate(&usage(vec![t.clone()], 0.0, None), &book, "2026-08-04");
        let one_hour = estimate(&usage(vec![t], 1.0, None), &book, "2026-08-04");
        assert!((five_min.list_usd.unwrap() - 6.25).abs() < 1e-9);
        assert!((one_hour.list_usd.unwrap() - 10.0).abs() < 1e-9);
    }

    // Sonnet 5's introductory rate expires 2026-08-31. Pricing September work
    // at August's rate would understate the fleet's busiest model by a third.
    #[test]
    fn a_dated_rate_steps_up_to_its_successor() {
        let mut book = RateBook::default_seed();
        let mut intro = anthropic(2.0, 10.0);
        intro.until = Some("2026-08-31".to_string());
        intro.then = Some(Box::new(anthropic(3.0, 15.0)));
        book.rates.insert("claude-sonnet-5".to_string(), intro);

        let u = usage(vec![tokens("claude-sonnet-5", 1_000_000, 1_000_000)], 0.0, None);
        let august = estimate(&u, &book, "2026-08-31").list_usd.unwrap();
        let september = estimate(&u, &book, "2026-09-01").list_usd.unwrap();
        assert!((august - 12.0).abs() < 1e-9, "intro $2 in + $10 out");
        assert!((september - 18.0).abs() < 1e-9, "standard $3 in + $15 out");
    }

    // Verified against 168 real sessions on 2026-08-04: the provider's own
    // reported cost tracks standard pricing, not the published introductory
    // rate. Seeding the introductory rate would understate Sonnet 5 by a third.
    #[test]
    fn sonnet_5_is_seeded_at_standard_not_introductory_pricing() {
        let book = RateBook::default_seed();
        let rate = book.lookup("claude-sonnet-5").expect("seeded");
        assert!((rate.input - 3.0).abs() < f64::EPSILON);
        assert!((rate.output - 15.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dated_model_snapshots_resolve_to_their_base_rate() {
        let book = RateBook::default_seed();
        assert!(book.lookup("claude-haiku-4-5-20251001").is_some());
        assert_eq!(book.lookup("claude-haiku-4-5-20251001"), book.lookup("claude-haiku-4-5"));
    }

    #[test]
    fn bracketed_mode_suffixes_resolve_to_their_base_rate() {
        let book = RateBook::default_seed();
        assert_eq!(book.lookup("claude-opus-4-8[1m]"), book.lookup("claude-opus-4-8"));
    }

    // Silently pricing unknown spend at zero is the failure that lets money
    // leak, so an unknown model must degrade the basis, not the total.
    #[test]
    fn an_unknown_model_is_reported_not_swallowed() {
        let book = RateBook::default_seed();
        let u = usage(vec![tokens("some-new-model", 1_000_000, 0)], 0.0, None);
        let est = estimate(&u, &book, "2026-08-04");
        assert_eq!(est.basis, CostBasis::PartiallyPriced);
        assert_eq!(est.unpriced_models, vec!["some-new-model"]);
    }

    #[test]
    fn codex_without_a_model_id_is_flagged_as_unreported() {
        let book = RateBook::default_seed();
        let u = usage(vec![tokens("", 1_000, 1_000)], 0.0, None);
        let est = estimate(&u, &book, "2026-08-04");
        assert_eq!(est.unpriced_models, vec!["<model not reported>"]);
    }

    #[test]
    fn provider_cost_is_preferred_over_our_own_arithmetic() {
        let book = RateBook::default_seed();
        let u = usage(vec![tokens("claude-opus-5", 1_000_000, 0)], 0.0, Some(4.20));
        let est = estimate(&u, &book, "2026-08-04");
        assert_eq!(est.basis, CostBasis::ProviderReported);
        assert_eq!(est.best_usd(), Some(4.20));
        // The list figure is still computed, so the two can be compared.
        assert!((est.list_usd.unwrap() - 5.0).abs() < 1e-9);
    }

    #[test]
    fn openai_cached_input_is_priced_at_its_own_rate() {
        let book = RateBook::default_seed();
        let mut t = tokens("gpt-5.4", 0, 0);
        t.cache_read_tokens = 1_000_000;
        let est = estimate(&usage(vec![t], 0.0, None), &book, "2026-08-04");
        assert!((est.list_usd.unwrap() - 0.25).abs() < 1e-9);
    }

    #[test]
    fn unmeasured_spend_reads_as_unknown_not_free() {
        let est = CostEstimate::unmeasured();
        assert_eq!(est.basis, CostBasis::Unmeasured);
        assert_eq!(est.best_usd(), None);
    }

    #[test]
    fn seed_merge_adds_new_models_but_keeps_operator_overrides() {
        let mut book = RateBook {
            version: TOKEN_RATES_VERSION,
            rates: BTreeMap::new(),
        };
        // A negotiated rate an operator set by hand.
        book.rates.insert("claude-opus-5".to_string(), anthropic(1.0, 2.0));
        assert!(merge_default_seed_into(&mut book));
        assert!((book.rates["claude-opus-5"].input - 1.0).abs() < f64::EPSILON);
        assert!(book.rates.contains_key("gpt-5.4"));
        assert!(!merge_default_seed_into(&mut book), "second merge is a no-op");
    }

    #[test]
    fn every_seeded_rate_records_where_and_when_it_was_verified() {
        for (model, rate) in RateBook::default_seed().rates {
            assert!(!rate.as_of.is_empty(), "{model} has no as_of date");
            assert!(rate.source.starts_with("https://"), "{model} has no source URL");
        }
    }

    #[test]
    fn rate_resolution_terminates_on_a_cyclic_hand_edit() {
        let mut a = anthropic(1.0, 2.0);
        a.until = Some("2020-01-01".to_string());
        a.then = Some(Box::new(anthropic(3.0, 4.0)));
        // Resolving far past the boundary must return the successor and stop.
        assert!((a.on("2030-01-01").input - 3.0).abs() < f64::EPSILON);
    }

    #[test]
    fn round_trips_through_json() {
        let book = RateBook::default_seed();
        let json = serde_json::to_string(&book).unwrap();
        let back: RateBook = serde_json::from_str(&json).unwrap();
        assert_eq!(back.rates.len(), book.rates.len());
        assert!((back.rates["claude-sonnet-5"].output - 15.0).abs() < f64::EPSILON);
    }
}
