# Usage accounting comparison with T3 Code

Research date: 2026-10-07. Upstream was pinned to
[`cd41c4ada0c70cc2eec95ecd7266f3dab010c58c`](https://github.com/pingdotgg/t3code/tree/cd41c4ada0c70cc2eec95ecd7266f3dab010c58c),
the GitHub HEAD observed during this task. Inspection used
`~/Code/t3code-usage-research`; the existing T3 Code installation was not modified.

## Accounting paths and sources

T3 Code's Usage page scans provider history into priced records and calendar
buckets. Its live adapters separately calculate usage for the active agent turn.
A thread's cumulative usage, a provider response, a multi-response agent turn,
and the current context window are different measurements.

The primary sources inspected were:

- [Transcript parsers](https://github.com/pingdotgg/t3code/blob/cd41c4ada0c70cc2eec95ecd7266f3dab010c58c/apps/server/src/usage/usageTranscripts.ts), including Claude message identities, Codex last-response counters, speed settings, and copied fork histories.
- [Live Codex turn accounting](https://github.com/pingdotgg/t3code/blob/cd41c4ada0c70cc2eec95ecd7266f3dab010c58c/apps/server/src/provider/CodexTurnTokenUsage.ts) and [Claude turn normalization](https://github.com/pingdotgg/t3code/blob/cd41c4ada0c70cc2eec95ecd7266f3dab010c58c/apps/server/src/provider/ClaudeTurnTokenUsage.ts), including resume baselines, terminal completeness, main-agent scope, and thinking subsets.
- [Rate lookup and arithmetic](https://github.com/pingdotgg/t3code/blob/cd41c4ada0c70cc2eec95ecd7266f3dab010c58c/apps/server/src/usage/usagePricing.ts), including LiteLLM rates, speed premiums, category splits, overrides, and cache savings.
- [Aggregation](https://github.com/pingdotgg/t3code/blob/cd41c4ada0c70cc2eec95ecd7266f3dab010c58c/apps/server/src/usage/usageAggregation.ts), [incremental reader](https://github.com/pingdotgg/t3code/blob/cd41c4ada0c70cc2eec95ecd7266f3dab010c58c/apps/server/src/usage/usageTranscriptReader.ts), [versioned scan cache](https://github.com/pingdotgg/t3code/blob/cd41c4ada0c70cc2eec95ecd7266f3dab010c58c/apps/server/src/usage/usageScanCache.ts), and [UsageService](https://github.com/pingdotgg/t3code/blob/cd41c4ada0c70cc2eec95ecd7266f3dab010c58c/apps/server/src/usage/UsageService.ts).
- [Token/cost contracts](https://github.com/pingdotgg/t3code/blob/cd41c4ada0c70cc2eec95ecd7266f3dab010c58c/packages/contracts/src/usage.ts), plus the tests beside the parsers, reader, pricing, aggregation, and live adapters.

T3's normalization is:

```text
total input = uncached input + cache reads + cache writes
total tokens = total input + output
reasoning is a subset of output
```

Claude reports uncached input directly. Codex reports inclusive input, so cache
reads and writes are subtracted to obtain uncached input. Cache categories are
replacements within input; reasoning is not added to output again. This agrees
with Fusebox's ledger contract and the official
[OpenAI spending-controller guidance](https://developers.openai.com/cookbook/articles/per_run_spending_controller_responses_api#limits-and-other-costs).
The current [Anthropic Messages schema](https://platform.claude.com/docs/en/api/messages)
documents `output_tokens_details.thinking_tokens` as a subset of inclusive billed
`output_tokens`.

## Findings and changes

| Area | T3 Code evidence | Fusebox change or verified behavior |
| --- | --- | --- |
| Ordinary OpenAI usage | Historical Codex writes default to zero when absent; input is inclusive. | Native OpenAI capture now treats omitted writes as zero when input is reported. Null/invalid writes and missing read detail stay unknown. Arbitrary compatible providers retain unknown omitted categories. |
| Translation | Contracts keep reads/writes disjoint and reasoning inclusive. | Chat/Responses parsers previously discarded writes and classified them as uncached input. Both parsers now subtract writes and preserve their category; buffered/SSE renderers carry it through. |
| Claude reasoning | The live adapter recognizes `thinking_tokens`; the history-page parser does not expose it. | Proxy capture, native imports, and Claude translation now recognize thinking detail without increasing output totals. |
| Claude fast mode | Native `usage.speed` is distinct from `usage.service_tier`. | Capture maps fast speed to explicit fast rates. Incompatible Batch/Fast or unknown speed combinations remain unpriced. |
| Codex speed changes | Applied thread settings carry priority/ultrafast. An omitted tier in an applied snapshot resets standard speed. | Checkpoints retain the active tier and the tier for each known turn. A delayed response keeps its original tier after a switch. |
| Resumed counters | T3's live accumulator uses `last` without a prior total. Its Usage parser reads `last_token_usage`. | The first legacy counter uses validated last-response usage, avoiding attribution of all prior history to its timestamp/model. Later last values must match cumulative growth for per-request pricing. |
| Repeated notifications | T3 skips consecutive identical last-usage payloads. | Fusebox skips unchanged cumulative totals. Two real responses with identical last counts and advancing totals are both counted. |
| Gaps and thresholds | Cumulative growth can aggregate several responses; T3's page omits long-context variants. | Validated last-response usage is priced against that request's inclusive input. A jump disagreeing with last usage remains an unpriced aggregate delta. |
| Forks/subagents | T3 detects `forked_from_id` and nested spawn metadata, then suppresses an initial copied burst using a one-second gap heuristic. | Fusebox now detects nested spawn metadata too. Ambiguous legacy fork counters stay visibly unsupported; response-ID records are imported/deduplicated. Timing is not used as proof of a charge. |
| Claude repeated blocks | Each content block can repeat the whole usage object. T3 globally deduplicates message/request pairs and keeps the first. | Fusebox already coalesces message identities, preserves revisions, and prefers more complete/larger-output evidence. Distinct main/sidechain identities remain observations. |
| Rate provenance | T3 fetches LiteLLM on a 24-hour TTL, supports exact custom IDs, and falls back to standard for unsupported speed rates. | Fusebox retains cited vendor snapshots, integer nanodollars, explicit supported tiers, TTL-aware Claude writes, and unknown prices. Assumptions remain visible. |
| Claude region sentinel | T3 applies list rates without regional distinction. Production Fusebox had 135 complete Claude records with `not_available`, all unpriced. | This sentinel now receives an explicit global-rate assumption and partial estimate. Original metadata is retained; unrecognized regions stay unpriced. |
| Persistence | T3 versions scan caches when parser semantics change. | Native parser is `native-2026-10-07-v2`; rate catalogue is `2026-10-07.3`. Startup/CLI reprice repair affected unpriced date/region snapshots idempotently. Saved prices are retained. |
| Dates/scans | T3 buckets event timestamps in the requested timezone and checkpoints complete lines with a prefix guard. | Fusebox already has event dates, IANA/DST boundaries, bounded scans, plain/zstd sibling handling, complete-line checkpoints, and durable transactions. |
| Combined sources | T3 deduplicates across transcript files and tracks source paths. | Fusebox already prefers matching proxy response evidence, retains raw local/collector rows, excludes matching imports from combined totals, and reports weak identities/conflicts. Counts/timestamps do not establish response identity. |

## Exact examples

A Responses object with 100 inclusive input tokens, 20 cached tokens, 40 output,
and no cache-write field previously left uncached input and writes unknown. It
now records 80 uncached input, 20 reads, zero writes, and 40 output: 140 tokens.
At bundled GPT-5.4-mini rates this is 241,500 nanodollars ($0.0002415). An
explicitly null write count remains partial and unpriced.

A response with 100 inclusive input, 20 reads, 10 writes, 40 output, and 30
reasoning tokens retains those categories across Chat, Responses, and Claude
buffered/SSE translation: 70 uncached input and 140 total tokens. Reasoning stays
inside output.

A resumed Codex transcript can first report 500,000 cumulative input tokens but
only 1,000 for its newest response. The importer records the 1,000-token response
and evaluates its context threshold against 1,000. It moves the baseline to
500,000 for subsequent deduplication. Legacy per-request prices remain partial
estimates; durable response-ID records supersede that evidence.

## Boundaries and retained protections

These figures are API list-price equivalents. Subscription quota windows and
subscription charges are separate measurements, as T3's contracts also state.
Fusebox preserves missing prices, conflicting evidence, and numeric validation.

The `not_available` sentinel was established from production metadata, not a
published enumeration promising the actual region. A global rate is therefore
an assumption recorded in every repaired snapshot:
`inference_region_unavailable_global_rate_assumed`. It does not establish a bill.

Claude `costUSD` can itself be a client estimate or override. T3 calls this field
`providerReported`; this task does not treat it as an authoritative invoice.
Unknown providers/models, including custom Codex providers, retain their identity.

Counter-only gaps, resets, or copied forks do not establish every individual
request. Their evidence stays unpriced/unsupported. Modern response-ID import
continues after a legacy reset. No new history roots are enabled, prompt/tool
text is not ingested, and account quota calculations are unchanged.

Previously imported v1 history retains its observations/checkpoint. New fields
apply to subsequent rows; reconstructing pre-checkpoint settings requires an
explicit backfill. Saved prices remain immutable. Startup correction only retries
unpriced date/region records with sufficient stored metadata. It does not infer
missing tokens or old speed settings.

## Verification and production

Regression coverage includes omitted/null/invalid writes, read uncertainty,
speed switches across checkpoints/delayed turns, thinking bounds, resumed
counters, unchanged notifications, identical real responses, cumulative gaps,
malformed-row isolation, subagent copies, full/SSE translation, and idempotent
startup correction preserving saved prices.

Validation used Rust 1.96.1 explicitly, preserving the system toolchain:

- All 315 Rust tests passed, including the 100,000-record store check and mocked
  HTTP/SSE/WebSocket integrations. Formatting and clippy with denied warnings passed.
- All 24 dashboard JavaScript tests and 30 Python regression tests passed.
- The isolated collector process acceptance passed all 11 checks.
- A real Chromium browser against a synthetic backend verified two observations
  with a combined 12,460,000-nanodollar estimate, correct resumed-counter input,
  120 inclusive reasoning tokens, two explicit partial estimates, no unpriced
  rows, no runtime errors, and no desktop/mobile overflow. T3 preview status/open
  reported that this headless environment has no automation host, so shell-based
  Chromium was used.
- The existing development service was rebuilt, restarted, and health-checked.

Production was deployed and verified on 2026-10-07 as
`0.3.2+gui.2849cdfb943e`. The user-authorized forced restart proceeded with one
active request. The durable updater retained rollback release
`0.3.2+gui.d002ee92a380` and backed up the stopped usage database together with the
existing service state. Admission gate, account configuration, and routing
settings passed preservation checks; all 66 saved session assignments remained.

Read-only postflight verified the exact running binary, public HTTPS health and
asset hashes, management authentication boundaries, native usage commits, and
SQLite integrity. All 135 previously unpriced Claude region observations were
repaired, totaling 11,994,608,800 nanodollars ($11.9946088) in explicitly assumed
API-equivalent estimates. The SHA-256 of all 2,364 previously priced snapshots
matched the preflight digest exactly. The new writer reported zero dropped,
rejected, failed, or unclosed sessions. Verification generated zero inference
calls.

The sanitized [production receipt](usage-calculation-production.json) contains
artifact hashes, validation counts, saved-price preservation evidence, and final
health checks. The source archive hash identifies the build snapshot; this final
production record was appended after that snapshot was built.
