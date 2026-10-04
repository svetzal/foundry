# Inference costs

Foundry records token usage and a cost estimate on `agent_session_ended`.
`foundry campaign report <name>` shows the known list estimate for each stage.
JSON reports include unpriced models and pricing limitations. These figures are
USD at published API list prices. Subscription fees and negotiated discounts
require separate accounting.

## Rates verified on 2026-10-04

The first six rows are the models configured on `mojility-ops-01` at verification.
The remaining rows cover Foundry's built-in defaults and earlier configured
models. Prices are USD per million tokens, for standard processing.

| Model | Fresh input | Cache read | Cache write (5m) | Cache write (1h) | Output |
| --- | ---: | ---: | ---: | ---: | ---: |
| claude-fable-5-1 | 10 | 0.25 | 12.50 | 20 | 50 |
| claude-opus-5-5 | 4 | 0.20 | 5 | 8 | 20 |
| claude-sonnet-5-5 | 2 | 0.20 | 2.50 | 4 | 10 |
| gpt-6-astra | 10 | 1 | 12.50 | 12.50 | 50 |
| gpt-6.1-sol | 2 | 0.10 | 2.50 | 2.50 | 10 |
| gpt-6-luna | 0.10 | 0.01 | 0.125 | 0.125 | 0.50 |
| claude-opus-5 / opus-4-8 / opus-4-7 / opus-4-6 / opus-4-5 | 5 | 0.50 | 6.25 | 10 | 25 |
| claude-fable-5 | 10 | 1 | 12.50 | 20 | 50 |
| claude-sonnet-5 | 2 | 0.20 | 2.50 | 4 | 10 |
| claude-sonnet-4-6 / sonnet-4-5 | 3 | 0.30 | 3.75 | 6 | 15 |
| claude-haiku-4-5 (-20251001) | 1 | 0.10 | 1.25 | 2 | 5 |
| gpt-5.5 | 5 | 0.50 | 0 | 0 | 30 |
| gpt-5.4 | 2.50 | 0.25 | 0 | 0 | 15 |
| gpt-5.4-mini | 0.75 | 0.075 | 0 | 0 | 4.50 |
| gpt-6-sol | 2 | 0.20 | 2.50 | 2.50 | 10 |

Sources: [Fable 5.1](https://platform.claude.com/docs/en/models/fable-5-1/overview),
[Opus 5.5](https://platform.claude.com/docs/en/models/opus-5-5/overview),
[Sonnet 5.5](https://platform.claude.com/docs/en/models/sonnet-5-5/overview),
[Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing),
[OpenAI pricing](https://developers.openai.com/api/docs/pricing),
[GPT-5.5](https://developers.openai.com/api/docs/models/gpt-5.5),
[GPT-5.4](https://developers.openai.com/api/docs/models/gpt-5.4), and
[GPT-5.4 Mini](https://developers.openai.com/api/docs/models/gpt-5.4-mini).

OpenCode uses IDs such as `openai/gpt-5.4`. Lookup accepts that prefix, the
`anthropic/` prefix, dated snapshots and bracketed context suffixes. An exact
entry takes precedence over its normalised base model.

## Billing rules

Claude 4.6 and later have no long-context premium. Cache-write retention affects
Claude's price; the transcript's aggregate TTL split apportions writes across
models. Fable 5.1 and Opus 5.5 have lower cache-read rates than earlier models.
Anthropic's batch discount is 50%. Opus 5.5 fast mode doubles rates; inference
geography can add 10%. The provider's reported cost takes precedence over
Foundry's list calculation. [Anthropic pricing](https://platform.claude.com/docs/en/about-claude/pricing)

GPT-6 requests above 272,000 input tokens double fresh input, cache reads and
cache writes, and multiply output by 1.5. Fast processing doubles the applicable
rates. Batch and Flex halve standard rates. Regional processing can add 10%.
These modifiers apply to individual requests, so a multi-million-token session
does not by itself prove that long-context pricing applied.
[GPT-6.1 Sol](https://developers.openai.com/api/docs/models/gpt-6.1-sol),
[GPT-6 Astra](https://developers.openai.com/api/docs/models/gpt-6-astra),
[GPT-6 Luna](https://developers.openai.com/api/docs/models/gpt-6-luna)

GPT-5.4 and GPT-5.5 also have a 272,000-input-token threshold with 2x input and
1.5x output pricing. Their model pages describe the premium as applying to the
full session. Mini has no such premium documented on its model page.
[GPT-5.4](https://developers.openai.com/api/docs/models/gpt-5.4),
[GPT-5.5](https://developers.openai.com/api/docs/models/gpt-5.5)

OpenAI cache writes use their own rate instead of the fresh-input rate. Foundry
subtracts reported cache reads and writes from total input before pricing each
category. Reasoning tokens are already included in output and are never added
again. Cache retention does not change the GPT-6 write rate.
[OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)

## Measurement limits

Current Codex terminal summaries can report `cache_write_input_tokens`, including
an explicit zero. Older summaries omit it. Foundry keeps that distinction and
marks missing writes as a pricing limitation for models that charge for them.
The retained terminal record is used once; earlier records are not added again.

Terminal summaries do not report individual request sizes, processing tier or
regional premiums. Foundry uses standard short-context rates and records those
limitations. Such a list estimate has `basis: partially_priced`. The campaign
report prints the limitations and labels the figure partial. It does not apply
a long-context multiplier to cumulative session input. Server-side tool fees
are outside this token estimate unless included in a provider-reported cost.

A Claude provider-reported cost remains authoritative even if the list book
lacks a model. `unpriced_models` still exposes the incomplete list comparison.
Interrupted sessions without terminal usage remain unmeasured. An unknown
model is never treated as proof of free inference. OpenCode transcripts still
have no supported terminal-usage parser and remain unmeasured.

## Runtime price book

`~/.foundry/token-rates.json` is editable without a rebuild. Each rate records
its source URL and verification date. `until` is inclusive; `then` applies on
the following day. Missing models are added from the seed at daemon startup
and in memory when pricing a session.

The 2026-10-04 seed also refreshes entries that exactly match the 2026-08-04
seed, including prices, provenance and dating fields. Other entries survive.
Sonnet 5 retains Foundry's historical $3/$15 accounting through 2026-08-31 and
uses the vendor's current $2/$10 standard price from 2026-09-01. Existing
recorded events are not rewritten.
