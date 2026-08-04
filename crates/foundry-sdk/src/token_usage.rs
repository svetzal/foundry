//! Token usage recovered from an agent session transcript.
//!
//! Every token Foundry spends is spent inside an agent CLI session, and every
//! session tees its transcript to `~/.foundry/agent-sessions/<id>.jsonl`. The
//! providers already write their own accounting into that stream; this module
//! reads it back so a session can report what it cost instead of only how many
//! bytes it wrote.
//!
//! ## Two provider shapes
//!
//! - **Claude CLI** emits a terminal `{"type":"result", …}` record carrying
//!   `total_cost_usd`, an aggregate `usage`, and a per-model `modelUsage` map.
//!   Because the CLI prices the work itself, its number is kept as
//!   [`SessionUsage::provider_cost_usd`] — authoritative, needing no rate book.
//! - **Codex CLI** emits `{"type":"turn.completed","usage":{…}}` with token
//!   counts only: no cost and, critically, **no model id**. Pricing a codex
//!   session therefore requires the caller to say which model ran, which
//!   Foundry knows from the request's tier (see `crate::agent_config`). That is
//!   why [`parse_transcript`] takes a `model_hint`.
//!
//! ## Counting conventions, which differ by vendor
//!
//! Normalized here so downstream pricing never has to ask:
//!
//! - `input_tokens` is always **fresh** input — cache reads excluded. Anthropic
//!   already reports it that way; `OpenAI` reports a total that *includes* cached
//!   input, so the cached portion is subtracted on the way in.
//! - `reasoning_tokens` is a **subset** of `output_tokens`, not an addition to
//!   it. Both vendors bill reasoning at the output rate, so pricing must use
//!   `output_tokens` alone and treat reasoning as informational.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Which provider's accounting a [`SessionUsage`] was recovered from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    /// Claude CLI terminal `result` record. Carries the provider's own cost.
    ClaudeResult,
    /// Codex CLI `turn.completed` record. Token counts only.
    CodexTurn,
}

/// Token counts for one model within a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTokens {
    /// Concrete model id, e.g. `claude-sonnet-5`. For codex this comes from the
    /// caller's hint, since the transcript never names the model.
    pub model: String,
    /// Fresh (non-cached) input tokens.
    pub input_tokens: u64,
    /// Output tokens, inclusive of `reasoning_tokens`.
    pub output_tokens: u64,
    /// Tokens served from cache, billed at the cache-read rate.
    pub cache_read_tokens: u64,
    /// Tokens written to cache. The 5-minute/1-hour split is a session-level
    /// figure — see [`SessionUsage::cache_write_1h_share`].
    pub cache_write_tokens: u64,
    /// Reasoning tokens, a **subset** of `output_tokens`. Informational only;
    /// pricing must not add these on top.
    pub reasoning_tokens: u64,
}

impl ModelTokens {
    /// Total tokens across every billed category. Reasoning is excluded because
    /// it is already inside `output_tokens`.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.cache_write_tokens)
    }
}

/// Everything one agent session spent, as recovered from its transcript.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionUsage {
    /// Per-model token counts. Ordered by model id for stable output.
    pub models: Vec<ModelTokens>,
    /// The provider's own cost figure, when it reports one. Present for Claude,
    /// absent for Codex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_cost_usd: Option<f64>,
    /// Fraction of cache-write tokens written at the 1-hour TTL (`0.0`–`1.0`).
    ///
    /// Anthropic charges 2x base for a 1-hour cache write against 1.25x for a
    /// 5-minute one, so the split is worth real money — but the transcript only
    /// reports it in aggregate, not per model. Pricing blends the two rates by
    /// this share. Defaults to `0.0` (all 5-minute) when unreported.
    #[serde(default)]
    pub cache_write_1h_share: f64,
    /// Which provider shape this came from.
    pub source: UsageSource,
}

impl SessionUsage {
    /// Summed tokens across every model.
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.models.iter().fold(0u64, |acc, m| acc.saturating_add(m.total()))
    }

    /// Every model id this session touched.
    #[must_use]
    pub fn model_ids(&self) -> Vec<String> {
        self.models.iter().map(|m| m.model.clone()).collect()
    }
}

/// Read a session transcript and recover what it spent.
///
/// `model_hint` names the model for providers whose transcript does not (Codex).
/// It is ignored when the transcript names its own models. Returns `Ok(None)`
/// when the file holds no terminal usage record at all — a session killed before
/// it reported, which is unmeasured spend the caller should surface rather than
/// silently treat as zero.
///
/// # Errors
///
/// Returns the underlying [`std::io::Error`] when the transcript cannot be read.
pub fn parse_transcript(
    path: &Path,
    model_hint: Option<&str>,
) -> std::io::Result<Option<SessionUsage>> {
    let content = std::fs::read_to_string(path)?;
    Ok(parse_transcript_str(&content, model_hint))
}

/// Pure form of [`parse_transcript`], operating on transcript text.
#[must_use]
pub fn parse_transcript_str(content: &str, model_hint: Option<&str>) -> Option<SessionUsage> {
    // Scan from the end: the terminal record is the last one written, and a
    // session that retried carries earlier, superseded records too.
    for line in content.lines().rev() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("result") => {
                if let Some(usage) = parse_claude_result(&value) {
                    return Some(usage);
                }
            }
            Some("turn.completed") => {
                if let Some(usage) = parse_codex_turn(&value, model_hint) {
                    return Some(usage);
                }
            }
            _ => {}
        }
    }
    None
}

fn u64_at(value: &serde_json::Value, pointer: &str) -> u64 {
    value.pointer(pointer).and_then(serde_json::Value::as_u64).unwrap_or(0)
}

fn parse_claude_result(value: &serde_json::Value) -> Option<SessionUsage> {
    let model_usage = value.get("modelUsage").and_then(serde_json::Value::as_object)?;

    let mut models: Vec<ModelTokens> = model_usage
        .iter()
        .map(|(model, v)| ModelTokens {
            model: model.clone(),
            input_tokens: u64_at(v, "/inputTokens"),
            output_tokens: u64_at(v, "/outputTokens"),
            cache_read_tokens: u64_at(v, "/cacheReadInputTokens"),
            cache_write_tokens: u64_at(v, "/cacheCreationInputTokens"),
            reasoning_tokens: 0,
        })
        .collect();
    models.sort_by(|a, b| a.model.cmp(&b.model));

    // The 5m/1h split lives on the aggregate usage block, not per model.
    let hour_write = u64_at(value, "/usage/cache_creation/ephemeral_1h_input_tokens");
    let minute_write = u64_at(value, "/usage/cache_creation/ephemeral_5m_input_tokens");
    let split_total = hour_write.saturating_add(minute_write);
    #[allow(clippy::cast_precision_loss)]
    let cache_write_1h_share = if split_total == 0 {
        0.0
    } else {
        hour_write as f64 / split_total as f64
    };

    Some(SessionUsage {
        models,
        provider_cost_usd: value.get("total_cost_usd").and_then(serde_json::Value::as_f64),
        cache_write_1h_share,
        source: UsageSource::ClaudeResult,
    })
}

fn parse_codex_turn(value: &serde_json::Value, model_hint: Option<&str>) -> Option<SessionUsage> {
    let usage = value.get("usage")?;
    let cached = u64_at(usage, "/cached_input_tokens");
    // OpenAI's `input_tokens` is the total including cached; normalize to fresh.
    let total_input = u64_at(usage, "/input_tokens");

    Some(SessionUsage {
        models: vec![ModelTokens {
            model: model_hint.unwrap_or_default().to_string(),
            input_tokens: total_input.saturating_sub(cached),
            output_tokens: u64_at(usage, "/output_tokens"),
            cache_read_tokens: cached,
            // Codex does not bill or report a separate cache-write category.
            cache_write_tokens: 0,
            reasoning_tokens: u64_at(usage, "/reasoning_output_tokens"),
        }],
        provider_cost_usd: None,
        cache_write_1h_share: 0.0,
        source: UsageSource::CodexTurn,
    })
}

/// Per-model token totals folded across many sessions.
#[must_use]
pub fn aggregate(sessions: &[SessionUsage]) -> BTreeMap<String, ModelTokens> {
    let mut out: BTreeMap<String, ModelTokens> = BTreeMap::new();
    for session in sessions {
        for m in &session.models {
            let entry = out.entry(m.model.clone()).or_insert_with(|| ModelTokens {
                model: m.model.clone(),
                ..ModelTokens::default()
            });
            entry.input_tokens = entry.input_tokens.saturating_add(m.input_tokens);
            entry.output_tokens = entry.output_tokens.saturating_add(m.output_tokens);
            entry.cache_read_tokens = entry.cache_read_tokens.saturating_add(m.cache_read_tokens);
            entry.cache_write_tokens =
                entry.cache_write_tokens.saturating_add(m.cache_write_tokens);
            entry.reasoning_tokens = entry.reasoning_tokens.saturating_add(m.reasoning_tokens);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE_RESULT: &str = r#"{"type":"result","total_cost_usd":0.0718422,"usage":{"input_tokens":42,"cache_creation_input_tokens":24744,"cache_read_input_tokens":194572,"output_tokens":571,"cache_creation":{"ephemeral_1h_input_tokens":24744,"ephemeral_5m_input_tokens":0}},"modelUsage":{"claude-haiku-4-5-20251001":{"inputTokens":42,"outputTokens":571,"cacheReadInputTokens":194572,"cacheCreationInputTokens":24744,"costUSD":0.0718422}}}"#;

    const CODEX_TURN: &str = r#"{"type":"turn.completed","usage":{"input_tokens":2652109,"cached_input_tokens":2423424,"output_tokens":8316,"reasoning_output_tokens":3412}}"#;

    #[test]
    fn parses_claude_result_with_provider_cost() {
        let usage = parse_transcript_str(CLAUDE_RESULT, None).expect("usage");
        assert_eq!(usage.source, UsageSource::ClaudeResult);
        assert_eq!(usage.provider_cost_usd, Some(0.071_842_2));
        assert_eq!(usage.models.len(), 1);
        let m = &usage.models[0];
        assert_eq!(m.model, "claude-haiku-4-5-20251001");
        assert_eq!(m.input_tokens, 42);
        assert_eq!(m.output_tokens, 571);
        assert_eq!(m.cache_read_tokens, 194_572);
        assert_eq!(m.cache_write_tokens, 24_744);
    }

    #[test]
    fn records_the_full_1h_cache_write_share() {
        // All 24744 write tokens were 1-hour; pricing them at the 5m rate would
        // understate the session by 60%.
        let usage = parse_transcript_str(CLAUDE_RESULT, None).expect("usage");
        assert!((usage.cache_write_1h_share - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn codex_input_is_normalized_to_fresh_tokens() {
        let usage = parse_transcript_str(CODEX_TURN, Some("gpt-5.4")).expect("usage");
        assert_eq!(usage.source, UsageSource::CodexTurn);
        let m = &usage.models[0];
        // OpenAI reports 2_652_109 total input of which 2_423_424 was cached.
        assert_eq!(m.input_tokens, 228_685);
        assert_eq!(m.cache_read_tokens, 2_423_424);
    }

    #[test]
    fn codex_reasoning_is_a_subset_of_output_not_an_addition() {
        let usage = parse_transcript_str(CODEX_TURN, Some("gpt-5.4")).expect("usage");
        let m = &usage.models[0];
        assert_eq!(m.output_tokens, 8_316);
        assert_eq!(m.reasoning_tokens, 3_412);
        assert!(m.reasoning_tokens < m.output_tokens);
        // total() must not double-count reasoning.
        assert_eq!(m.total(), 228_685 + 8_316 + 2_423_424);
    }

    #[test]
    fn codex_takes_its_model_from_the_hint_because_the_transcript_has_none() {
        let usage = parse_transcript_str(CODEX_TURN, Some("gpt-5.5")).expect("usage");
        assert_eq!(usage.models[0].model, "gpt-5.5");
        assert_eq!(usage.provider_cost_usd, None);
    }

    #[test]
    fn codex_without_a_hint_yields_an_unnamed_model_rather_than_a_wrong_one() {
        let usage = parse_transcript_str(CODEX_TURN, None).expect("usage");
        assert_eq!(usage.models[0].model, "");
    }

    #[test]
    fn later_terminal_record_wins_over_an_earlier_superseded_one() {
        let earlier = r#"{"type":"result","total_cost_usd":0.01,"modelUsage":{"claude-sonnet-5":{"inputTokens":1,"outputTokens":1}}}"#;
        let content = format!("{earlier}\n{CLAUDE_RESULT}\n");
        let usage = parse_transcript_str(&content, None).expect("usage");
        assert_eq!(usage.provider_cost_usd, Some(0.071_842_2));
    }

    #[test]
    fn non_json_and_blank_lines_are_skipped() {
        let content = format!("not json\n\n   \n{CODEX_TURN}\n\n");
        assert!(parse_transcript_str(&content, Some("gpt-5.4")).is_some());
    }

    // A session killed before its terminal record is unmeasured spend. It must
    // read as "unknown", never as zero.
    #[test]
    fn a_transcript_with_no_terminal_record_yields_none_not_zero() {
        let content = r#"{"type":"assistant","message":{"content":[]}}
{"type":"item.completed"}"#;
        assert!(parse_transcript_str(content, Some("gpt-5.4")).is_none());
    }

    #[test]
    fn empty_transcript_yields_none() {
        assert!(parse_transcript_str("", None).is_none());
    }

    #[test]
    fn multi_model_sessions_are_reported_per_model_sorted() {
        let line = r#"{"type":"result","total_cost_usd":1.0,"modelUsage":{"claude-sonnet-5":{"inputTokens":10,"outputTokens":20},"claude-haiku-4-5":{"inputTokens":1,"outputTokens":2}}}"#;
        let usage = parse_transcript_str(line, None).expect("usage");
        assert_eq!(usage.model_ids(), vec!["claude-haiku-4-5", "claude-sonnet-5"]);
    }

    #[test]
    fn aggregate_folds_models_across_sessions() {
        let a = parse_transcript_str(CLAUDE_RESULT, None).expect("usage");
        let b = parse_transcript_str(CLAUDE_RESULT, None).expect("usage");
        let folded = aggregate(&[a, b]);
        let m = &folded["claude-haiku-4-5-20251001"];
        assert_eq!(m.input_tokens, 84);
        assert_eq!(m.output_tokens, 1_142);
    }
}
