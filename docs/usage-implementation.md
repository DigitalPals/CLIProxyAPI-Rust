# Usage and Costs implementation record

Baseline: `4a81fe3` preserved the prior uncommitted work on local `main`, as requested. Implementation is on local `feature/usage-costs`; no push, merge, deployment, or existing service restart was performed. This record tracks implementation and verification separately; the root agent owns final acceptance results.

## Implementation checklist

- [x] Map request, streaming, WebSocket, media, auth, persistence, dashboard, CLI and config paths.
- [x] Define a strict metadata-only observation contract and source/token provenance.
- [x] Add versioned SQLite storage, migrations, deterministic reconciliation, pricing snapshots, bounded writes, health counters, retention and query/export APIs.
- [x] Capture proxy HTTP/stream/WebSocket/retry/media usage and add named inference client attribution.
- [x] Add explicitly enabled Claude Code and Codex imports with parser checkpoints and idempotent ingestion.
- [x] Add standalone collectors with one-time enrollment credentials, durable local outbox and authenticated bounded ingestion.
- [x] Add management APIs and the embedded Usage view while keeping provider subscription quota reporting separate.
- [x] Document configuration, source coverage, pricing limits, imports, collectors, privacy, retention, backup and recovery.
- [x] Final independent review, regression fixes, full checks and performance acceptance report.

Final executed evidence is recorded in [usage-verification.md](usage-verification.md), with measured release results in [usage-benchmark-results.json](usage-benchmark-results.json).

## Orchestration record

Runtime discovery: installed Codex CLI `0.160.1`; native `collaboration.spawn_agent` exposes supported model and effort overrides. Root runtime is GPT-6 Astra / ultra. Inspected local configuration selected `gpt-6.1-sol` / max through `cliproxyapi`, with no custom agent profiles found; configuration was unchanged. Available overrides included GPT-6.1 Sol, GPT-6 Astra, GPT-6 Sol, GPT-6 Luna, and GPT-5.6 Sol. Implementation workers used separate worktrees rooted at the shared contract baseline; root alone cherry-picked and integrated changes. Later bounded fixes used explicit file ownership. At most three workers ran concurrently, with no recursive delegation.

Worker effective models and effort are not exposed authoritatively in this session. Requested settings are recorded where known; every effective value remains **unverified**.

| Worker | Assignment | Requested model / effort | Effective |
|---|---|---|---|
| map | Repository mapping and usage documentation | gpt-6-luna / high | unverified |
| formats_research | Official format/pricing research, importers and collectors | gpt-6.1-sol / high | unverified |
| store | SQLite, reconciliation, pricing, persistence regressions | gpt-6.1-sol / high | unverified |
| dashboard | Embedded dashboard and JavaScript tests | gpt-6.1-sol / high | unverified |
| review | Independent accounting/security/failure review | gpt-6-astra / xhigh | unverified |

The baseline coverage was 203 Rust tests and 2 JavaScript checks. This is baseline coverage information, not a report that the feature branch's final checks passed.

## Integration map

| Path | Implementation | Coverage notes |
|---|---|---|
| Translated HTTP, SSE, legacy completions, Gemini | `proxy::Tracker`; format parsers and renderers use the IR | Metadata captured before client-format rendering where possible. |
| Pass-through JSON/SSE | `proxy::execute_inner`, `passthrough_stream`, `collect_passthrough` | Provider output is observed without changing pass-through payloads. |
| Retries, failover, refresh | `Tracker::attempt`, `proxy::execute_inner` | Attempts remain distinct; source-reported failure usage is retained. |
| Responses WebSocket | `ws::native_turn` and shared `Tracker`; fallback enters `proxy::execute` | One observation per turn, including terminal usage and cancellation state. |
| Compaction and image/video creation | `media::with_accounts` | Captured when upstream returns usage. Video status polls do not imply generation use. |
| Token count and model listing | Local metadata endpoints | Excluded from generation accounting. |
| Devin | Translated IR usage where reported | Its current protobuf path does not expose richer usage/tier metadata. |
| Native imports | `usage::imports` | Claude Code `message.usage`; Codex durable response records and supported legacy cumulative snapshots. |
| Standalone collector | `usage::collector` | Metadata-only outbox; server replaces untrusted collector attribution and request claims. |

Final root integration: `6c060b1`.

The existing request ring remains an in-memory, 300-row live view. The persistent usage database is the historical source. Provider allowance/quota remains in the existing account and quota state.

## Data and version contracts

`src/usage/types.rs` defines the strict, allowlisted observation. Observation envelope `schema_version` is 1. Collector wire `Batch.version` is also 1. The SQLite database's `user_version` is 3: migrations step 0→1→2→3 in one transaction, where 1→2 adds writer-session health accounting and 2→3 adds the `usage_entries_association` index; a newer or foreign application database is rejected. These versions describe different layers.

Tokens use disjoint uncached input, cache read, cache write, and output categories. Cache-write TTL values are subsets of cache write; reasoning is a subset of output. Missing is `null`, not zero. Observations retain source, origin, parser version, provider/model, event/ingest time, optional evidenced identities, completion, and allowlisted numeric metadata. Prompt/completion bodies, tools, transcripts, credentials, and arbitrary provider JSON are not persisted.

Idempotency uses source + origin + source event identity. Reliable provider response IDs can associate proxy and imported evidence; token/time similarity is never enough. Reconciliation selects trusted evidence deterministically, with proxy before local import before collector, and surfaces conflicts. Per-source totals remain available and possibly overlapping. The combined total takes the accounting entries in range and drops every imported entry whose association key also has a proxy entry (not limited to the range), so each provider response counts once and proxy evidence wins. Imported entries without a response ID are counted and reported as weak identity.

## Pricing provenance

Catalogue `2026-10-07.2` records USD rates in integer nanodollars per token. Its list-price source URLs and model rows are in `src/usage/rates.json`; schema, matching and tier logic are in `src/usage/pricing.rs`. Undocumented effective dates are null and therefore apply only from `verified_at`, displayed as current-rate equivalents. No old event is repriced using a later rate. Unknown rates remain null. A local override is USD-only and its content digest is incorporated into the catalogue version stored with price snapshots.

The bundled tier multipliers are explicit and limited: OpenAI GPT-6 and GPT-5.6 rows support Batch/Flex at 1/2, Fast/Priority at 2/1, and GPT-6 Astra Ultrafast at 6/1. The listed Anthropic rows support Batch at 1/2; Fast at 2/1 is limited to Opus 5.5, Opus 5, and Opus 4.8. Unsupported model/tier combinations are unpriced. The 272,000 input threshold is present only for listed OpenAI models with threshold rate fields. Pricing is an API estimate, never a provider invoice or subscription-plan charge.

## Storage, health, retention and imports

SQLite uses WAL, `synchronous=FULL`, transactions, foreign keys, application identity, and transactional migrations. A dedicated writer batches up to 200 observations and uses a bounded nonblocking ingress queue. Enqueue acceptance is not durable acknowledgment. If the queue is full or capture ingress cannot acquire its gate, the observation is dropped; invalid records are rejected; a failed insertion batch contributes drops and writer errors. The crash-loss bound is queued observations plus the current uncommitted batch.

Health separates counters for the current writer from `historical_gap` counters recovered from earlier writer sessions. Counter persistence is best-effort between commands and on the idle timer, so a busy transaction or disk failure can delay or prevent updates. An unclosed prior session is a recovery warning because it can indicate forced shutdown or a concurrent process. Do not treat a healthy proxy response as proof of complete analytics when health is degraded.

Normal shutdown stops the native import poller, closes usage ingress, drains queued writer commands, then persists and marks the writer session clean after acknowledgment. Shutdown errors are logged. A crash/forced kill may leave an unclosed session and lose queued/uncommitted observations. The test-only flush barrier reports prior writer errors rather than clearing gaps; production commands use acknowledged shutdown.

Retention purges at database open and about hourly during the writer loop using the configured retention horizon. A manual purge advances a watermark that blocks automatic resurrection. Explicit `usage backfill` clears enabled roots' checkpoints and temporarily allows their imported records past that watermark; the watermark is restored, and age-based retention still applies at a later purge. The server scans enabled roots about every 30 seconds; CLI scans and backfills wait for transaction acknowledgment. Imported records and source checkpoints commit atomically. Collector outbox records and checkpoints also commit atomically, and the server acknowledges only after durable ingestion.

## Baseline and final verification

Baseline coverage: 203 Rust tests and 2 JavaScript checks. Final feature checks: 272 Rust tests, 14 JavaScript tests, 6 real-browser acceptance groups and 9 isolated process checks passed. Formatting, clippy with denied warnings, syntax checks and release build passed. The 100k-record test ran without ignoring tests. See the verification report for methodology and limitations. Existing services were not restarted; only isolated synthetic test processes were rebuilt/restarted.
