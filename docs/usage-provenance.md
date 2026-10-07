# Fusebox native usage and pricing research

Verified on **2026-10-07** against fetched official docs and public vendor source. No local personal history/config/auth inspected, no provider inference calls. Requested execution model `gpt-6.1-sol`, effort `high`: **effective execution model/effort unverified** (this delegated session does not expose an authoritative effective model result). Public model docs verify this model supports `high`; this is distinct from proving the session actually used it.

## Native source evidence

### Codex

Official docs first: [Advanced configuration, Config and state locations](https://developers.openai.com/codex/config-advanced#config-and-state-locations) says `CODEX_HOME` defaults to `~/.codex`. `history.jsonl` is prompt history, **not** a usage ledger. [App-server](https://developers.openai.com/codex/app-server) links the official open source implementation.

Pinned current public main: **5a3140176e668a2f72f3c098490eb7f7052d9d85**, commit timestamp **2026-10-07T04:24:56Z**. This is implementation evidence, not a claim that every installed released CLI already has every feature.

All following Codex paths are under https://github.com/openai/codex/blob/5a3140176e668a2f72f3c098490eb7f7052d9d85/ :

- `codex-rs/rollout/src/lib.rs` L86-87: `sessions` and `archived_sessions` directories. Collector should recurse, including date partitions; archived layout may be flat.
- `codex-rs/history/src/lib.rs` L359-367: JSONL envelope `timestamp`, optional `ordinal`, flattened item. `codex-rs/history/src/rollout_payload.rs` L33 onward defines snake-case `type` and `payload`, including `session_meta`, `turn_context`, `event_msg`, **`token_usage_record`**.
- `codex-rs/protocol/src/protocol.rs` L2295-2308: `TokenUsage` has inclusive `input_tokens`, `cached_input_tokens`, new `cache_write_input_tokens` (serde default 0), inclusive `output_tokens`, `reasoning_output_tokens`, `total_tokens`.
- Same L2316-2326: **durable per-completed-response** `TokenUsageRecord`: `thread_id`, `turn_id`, `session_id`, `root_turn_id`, `response_id`, `usage`, `turn_token_usage`, `thread_token_usage`. Prefer `usage` and deduplicate by provider/source namespace + `response_id`; never sum the two cumulative totals too.
- Same L2330-2367: `TokenUsageInfo` includes cumulative `total_token_usage`, latest `last_token_usage`, optional `model_context_window`; `append_last_usage` adds the latter to former. L1461-1463: absent info means **unknown**, not zero. L2370-2391 can replace counts with context-window estimates, so generic `token_count` is not guaranteed invoice-exact.
- Same L1970-1977: `raw_response_completed` explicitly reports exact upstream completion usage, unlike accumulated/estimated/replayed `TokenCountEvent`; **rollout policy excludes RawResponseCompleted** (`codex-rs/rollout/src/policy.rs` L184-188), while persisting TokenUsageRecord L44 and legacy TokenCount L133. Do not require raw_response_completed in native saved logs.
- Same L3177-3188: `SessionMeta.id` is thread identity; `session_id` equals root thread ID. Creator user/account metadata are optional and capture **creation**, not current resumed-account identity; do not infer a billing account from them. L3313-3318 maps legacy missing `session_id` to `id`.
- `codex-rs/rollout/src/recorder.rs` L98-104: revert keeps stable thread ID but filename gains a separate rollout ID (`rollout-<time>-<thread>_<rollout>.jsonl`). L1149-1154: **first** session_meta is canonical; later session_meta can be copied fork history.
- `codex-rs/protocol/src/protocol.rs` L3349-3388: `turn_context` has optional turn/root-turn IDs, cwd, model, effort. Per-response record lacks a model field: join matching turn context; never price all history using today's user config or the last model in file.
- `codex-rs/codex-api/src/sse/responses.rs` L118-150 and test L846-869: API `input_tokens_details.cached_tokens` and `cache_write_tokens` map to native flat fields; output detail reasoning maps separately. Test has input=100, cache-read=40, cache-write=60, output=10, reasoning=5, total=110: input includes both cache categories, output includes reasoning.
- `codex-rs/rollout/src/compression.rs` L207-226, L1279-1280, L1315-1322: `.jsonl.zst` compressed logs exist; when plain `.jsonl` and compressed sibling both exist, **plain wins**, don't import twice. Skipping compressed files is a material coverage limitation that should be disclosed.

Synthetic projection (usage-related fields only; omitted unrelated native context fields):

```jsonl
{"timestamp":"2026-10-07T10:00:00Z","type":"session_meta","payload":{"id":"thread-A","session_id":"root-A","cwd":"/srv/project","cli_version":"synthetic","model_provider":"openai","parent_thread_id":"root-A"}}
{"timestamp":"2026-10-07T10:00:01Z","type":"turn_context","payload":{"turn_id":"turn-A","root_turn_id":"turn-root","model":"gpt-6.1-sol","effort":"high","cwd":"/srv/project"}}
{"timestamp":"2026-10-07T10:00:02Z","ordinal":3,"type":"token_usage_record","payload":{"thread_id":"thread-A","turn_id":"turn-A","session_id":"root-A","root_turn_id":"turn-root","response_id":"resp-synthetic","usage":{"input_tokens":1000,"cached_input_tokens":600,"cache_write_input_tokens":100,"output_tokens":200,"reasoning_output_tokens":80,"total_tokens":1200},"turn_token_usage":{"input_tokens":1000,"cached_input_tokens":600,"cache_write_input_tokens":100,"output_tokens":200,"reasoning_output_tokens":80,"total_tokens":1200},"thread_token_usage":{"input_tokens":1000,"cached_input_tokens":600,"cache_write_input_tokens":100,"output_tokens":200,"reasoning_output_tokens":80,"total_tokens":1200}}}
{"timestamp":"2026-10-07T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":1000,"cached_input_tokens":600,"output_tokens":200,"reasoning_output_tokens":80,"total_tokens":1200},"last_token_usage":{"input_tokens":1000,"cached_input_tokens":600,"output_tokens":200,"reasoning_output_tokens":80,"total_tokens":1200},"model_context_window":1050000},"rate_limits":null}}
```

Legacy fallback: process cumulative snapshots in file order, compute componentwise deltas against prior snapshot; repeated totals yield no new usage. Don't sum `last_token_usage` on every event: quota-only events may repeat it. Missing info/invalid counters should record diagnostics. Decreases/resets, copied fork prefixes, and estimates require explicit quality labels; don't manufacture negative deltas or silently claim invoice precision. New record and legacy snapshots cover overlapping usage and must not both create observations. Dedupe copied response records across forks/reverts/archives by durable response identity. Physical ordinal alone is insufficient across copied prefixes or rollout replacements.

### Claude Code

Official [Agent SDK sessions](https://platform.claude.com/docs/en/agent-sdk/sessions) documents `~/.claude/projects/<encoded-cwd>/*.jsonl`, overridden by `$CLAUDE_CONFIG_DIR/projects/`. Directory encoding replaces non-alphanumeric chars with `-`; names exceeding 200 chars are truncated + hashed. New SDKs can use `CLAUDE_CODE_PROJECT_DIR_NAME`. Use **record cwd**, don't reverse directory encoding as authoritative project identity.

Official [settings](https://code.claude.com/docs/en/settings) says `CLAUDE_CONFIG_DIR` changes settings, session history and plugin storage. [Hooks, SubagentStop](https://code.claude.com/docs/en/hooks#subagentstop) distinguishes main `transcript_path` from `agent_transcript_path`, e.g. `.../<session>/subagents/agent-<agent>.jsonl`; internal agents also exist.

Official public Python Agent SDK pin **23bb0157f51f83c21a7c47fbed1e5741ad581082**, commit **2026-10-07T02:04:56Z**:

- https://github.com/anthropics/claude-agent-sdk-python/blob/23bb0157f51f83c21a7c47fbed1e5741ad581082/src/claude_agent_sdk/_internal/sessions.py L122-141 implements config override; L825-833 describes loose native transcript fields: `type`, `uuid`, `parentUuid`, `sessionId`, `message`, `isSidechain`, `isMeta`, `isCompactSummary`, `teamName`.
- Same L931-1040 rebuilds **visible conversation** from latest parentUuid chain, excludes sidechain/meta/team rows; this is a UI projection, not a correct spend ledger. Previously sampled responses still cost money after compaction/rewind or branching. L1202-1203, L1292-1294 confirms nested subagent transcripts (including workflow subdirectories).
- https://github.com/anthropics/claude-agent-sdk-python/blob/23bb0157f51f83c21a7c47fbed1e5741ad581082/tests/test_sessions.py L653-664 gives an official fixture envelope matching CLI JSONL.
- API usage [prompt caching, Track cache performance](https://platform.claude.com/docs/en/build-with-claude/prompt-caching): input is **uncached only**; total input = `input_tokens + cache_creation_input_tokens + cache_read_input_tokens`. TTL object values sum to top-level creation count; do not add TTL counts to creation count again.
- https://github.com/anthropics/anthropic-sdk-python/blob/main/src/anthropic/types/usage.py says `output_tokens` is inclusive authoritative billing total; `output_tokens_details` is decomposition. Optional service_tier is standard/priority/batch, inference_geo is separately reported, server_tool_use is separate.

Synthetic usage-bearing transcript projection:

```jsonl
{"type":"assistant","uuid":"row-synthetic","parentUuid":"user-synthetic","sessionId":"session-synthetic","isSidechain":false,"timestamp":"2026-10-07T10:01:00Z","cwd":"/srv/project","requestId":"req-synthetic","message":{"id":"msg-synthetic","type":"message","role":"assistant","model":"claude-sonnet-4-6","content":[],"usage":{"input_tokens":100,"output_tokens":50,"cache_read_input_tokens":600,"cache_creation_input_tokens":300,"cache_creation":{"ephemeral_5m_input_tokens":200,"ephemeral_1h_input_tokens":100},"service_tier":"standard"}}}
```

Confidence boundary: official SDK proves envelope and API docs prove `message.usage` semantics; a published stable complete native Claude Code JSONL billing schema and universal `requestId` guarantee were **not established**. Preserve message/provider identity when present, use envelope uuid as fallback, and mark weaker dedupe. Multiple persisted content blocks or streaming updates for a single message must be coalesced, not charged repeatedly. A repeated message ID can carry evolving output usage; choose latest consistent finalized usage, not sum snapshots. `isSidechain` is not inherently non-billable. Avoid dropping subagent spend; avoid counting parent summarized subagent totals and child leaf usage simultaneously. Missing usage means unknown, not zero.

## Pricing and provenance

**Current verification date is not rate effective date.** The fetched current tables mostly do not state historical effective dates. Store `verified_at=2026-10-07`, `effective_from=null` (or explicitly labeled assumed current estimate), source URL, currency USD, unit 1M tokens, model/tier/context/region, and rate-card version. Do not backdate new current prices over historical imports and claim historical actual spend. Unknown models/tiers/categories should remain unpriced, not $0. Local USD estimates are not subscription-plan charges, invoices, or provider truth.

### OpenAI official current API pricing

Sources: https://developers.openai.com/api/docs/pricing ; https://developers.openai.com/api/docs/guides/prompt-caching ; https://developers.openai.com/api/docs/models/gpt-6.1-sol ; https://developers.openai.com/api/docs/guides/reasoning ; https://developers.openai.com/api/docs/guides/token-counting#understand-output-token-counts . All fetched.

Standard USD/MTok. Columns `input / cache-read / cache-write / output`:

| Model | Short ≤272k request input | Long >272k request input |
|---|---|---|
| gpt-6-astra | 10 / 1 / 12.5 / 50 | 20 / 2 / 25 / 75 |
| gpt-6.1-sol | 2 / 0.1 / 2.5 / 10 | 4 / 0.2 / 5 / 15 |
| gpt-6-sol | 2 / 0.2 / 2.5 / 10 | 4 / 0.4 / 5 / 15 |
| gpt-6-luna | 0.1 / 0.01 / 0.125 / 0.5 | 0.2 / 0.02 / 0.25 / 0.75 |
| gpt-5.6-sol | 4 / 0.4 / 5 / 20 | 8 / 0.8 / 10 / 30 |
| gpt-5.6-terra | 2 / 0.2 / 2.5 / 12 | 4 / 0.4 / 5 / 18 |
| gpt-5.6-luna | 0.2 / 0.02 / 0.25 / 1.2 | 0.4 / 0.04 / 0.5 / 1.8 |
| gpt-5.5 | 5 / 0.5 / n/a / 30 | 10 / 1 / n/a / 45 |
| gpt-5.4 | 2.5 / 0.25 / n/a / 15 | 5 / 0.5 / n/a / 22.5 |
| gpt-5.4-mini | 0.75 / 0.075 / n/a / 4.5 | n/a |
| gpt-5.4-nano | 0.2 / 0.02 / n/a / 1.25 | n/a |
| gpt-5.3-codex | 1.75 / 0.175 / n/a / 14 | no separate rate established |
| gpt-5.2 | 1.75 / 0.175 / n/a / 14 | no separate rate established |
| gpt-5.1, gpt-5 | 1.25 / 0.125 / n/a / 10 | no separate rate established |
| gpt-5-mini | 0.25 / 0.025 / n/a / 2 | no separate rate established |
| gpt-5-nano | 0.05 / 0.005 / n/a / 0.4 | no separate rate established |

For GPT-5.6+ and GPT-6 family above, Batch/Flex are half Standard, Fast is twice Standard; **model-specific rows govern** older families (do not multiply all models blindly). Astra Ultrafast is six times standard. Priority renamed Fast on **2026-07-30**; store alias semantics, not an invented universal rate. GPT-5.6 Sol promo table is stated available at least through **2026-11-21**, not its effective start.

Long-context threshold uses **individual request total input**, never cumulative session totals or model_context_window. Threshold applies premium to **whole request**, not marginal tokens beyond threshold. Regional processing for models released on/after **2026-03-05** and FedRAMP add 10% where eligible; model capability/context window is not proof this premium occurred.

OpenAI cost formula: `(input-cache_read-cache_write)*input_rate + cache_read*read_rate + cache_write*write_rate + output*output_rate`, divided by 1e6. Cache write is **replacement category, not additive fee**. Older models with absent write rate/zero write count retain ordinary uncached input. Reasoning subset is **already included** in output; don't add it a second time. Don't silently clamp impossible category totals and present them as verified costs.

Official Codex subscription source https://learn.chatgpt.com/docs/pricing explicitly says API token prices are **separate from subscription usage** and should not estimate included tasks. Credit-based plans have their own rate table. ChatGPT Work and Codex share usage. Display imported list-price USD as an estimate/equivalent cost, not actual subscription spend or credit balance.

### Anthropic official current API pricing

Source fetched: https://platform.claude.com/docs/en/about-claude/pricing and https://platform.claude.com/docs/en/build-with-claude/prompt-caching . Table columns `uncached input / output / 5m write / 1h write / cache-read`, USD/MTok:

| Model | Rates |
|---|---|
| Claude Fable 5.1, Mythos 5.1 | 10 / 50 / 12.5 / 20 / 0.25 |
| Claude Fable 5, Mythos 5 | 10 / 50 / 12.5 / 20 / 1 |
| Claude Opus 5.5 | 4 / 20 / 5 / 8 / 0.2 |
| Claude Opus 5, 4.8, 4.7, 4.6, 4.5 | 5 / 25 / 6.25 / 10 / 0.5 |
| Claude Sonnet 5.5, 5 | 2 / 10 / 2.5 / 4 / 0.2 |
| Claude Sonnet 4.6, 4.5, 4 | 3 / 15 / 3.75 / 6 / 0.3 |
| Claude Haiku 4.5 | 1 / 5 / 1.25 / 2 / 0.1 |
| Claude Haiku 3.5 | 0.8 / 4 / 1 / 1.6 / 0.08 |
| Claude Opus 4.1, 4 | 15 / 75 / 18.75 / 30 / 1.5 |

**Claude 4.6 and later** include full 1M context at standard pricing, including 900k requests. Do not retain a blanket >200k premium from older prices. Historical >200k prices for older models were not established from the current table; use explicit historical/override rate cards.

Cache 5m write=1.25x input, 1h write=2x; cache reads are **model-specific** (Opus 5.5 .05x; Fable/Mythos 5.1 .025x; most others .1x). Batch discount is 50% input/output, and cache modifiers stack. US-only inference_geo on Claude 4.6+ adds 1.1x to all categories. Priority tiers may be contracted; unknown actual rate remains unknown. Fast is currently Opus 5.5 input/output 8/40 and Opus 5/4.8 10/50, all context; cache/region modifiers stack; no Batch+Fast. Opus 4.7 fast errors, Opus 4.6 runs standard. Don't apply old Opus 4.6 Fast assumptions.

Anthropic formula: `uncached_input*input_rate + output*output_rate + read*read_rate + write5m*write5m_rate + write1h*write1h_rate`, /1e6. If positive aggregate creation lacks TTL split, precise TTL-weighted pricing is unknown; a 5m assumption is an **explicit estimate**, not evidence. Output includes thinking/reasoning. Server tools may incur separate prices (e.g. web search $10/1k); token-only local logs cannot claim full invoices.

https://code.claude.com/docs/en/costs says `/cost` computes **local estimates** at list rates or organization modelPricing overrides; authoritative billing is Console. Managed modelPricing requires v2.1.242+, markup v2.1.271+; it changes reported estimates, not Anthropic charges. Cost totals now reset on /clear since v2.1.211. The page's `dateModified` was **2026-10-06T19:57:53.445Z**, which is document modification, not rate effective date.

## Recommended meaningful fixtures

1. Repeated Codex cumulative totals/rate-limit-only rows; only true increments count.
2. Both token_usage_record + token_count for same response; one spend event.
3. Fork copying parent records, revert filename change, archive moves, .jsonl/.zst siblings; no duplicates.
4. Codex cumulative decrease/reset and unknown info; explicit diagnostics/quality.
5. Model changes during session, child session/root thread IDs, per-request context crossing threshold.
6. Claude repeated message ID with revised output, duplicate uuid/file copies, missing message ID/request ID.
7. Claude main + nested sidechain/subagent usage; no parent summary double count.
8. Mixed 5m/1h cache creation; aggregate equals split sum; inclusive reasoning output.
9. Unknown model/tier/rate effective date, incomplete JSONL last line, malformed row between valid rows.
10. Current list-price estimates distinguish subscription usage, configured rates, actual invoices; source-verification date separate from effective date.
