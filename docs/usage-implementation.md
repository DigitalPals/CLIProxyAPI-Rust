# Usage & Costs implementation record

Baseline: `4a81fe3` (user's prior work committed on main). Feature: `feature/usage-costs`.
Local implementation only; synthetic histories/mock upstreams; no existing service restarts.

## Acceptance checklist

- [x] Inspect product/design, auth, CLI, request lifecycle, storage, dashboard and tests.
- [x] Discover runtime: Codex CLI 0.160.1; native collaboration.spawn_agent supports explicit model/effort.
- [x] Define source observation/token contracts and ownership before concurrent implementation.
- [ ] Persistent SQLite storage/migrations, deterministic reconciliation, bounded writer and health.
- [ ] Versioned local pricing, explicit unknowns and source-separated estimates.
- [ ] Proxy HTTP/stream/WS/retry/compaction integration and named authenticated clients.
- [ ] Opt-in Claude Code/Codex imports, checkpoints/idempotency and sanitized fixtures.
- [ ] Standalone collector enrollment, durable outbox, authenticated bounded ingestion and status.
- [ ] Management APIs, server aggregates, timezone ranges, exports, retention.
- [ ] Embedded Usage page with imports/collectors, quota separation and privacy states.
- [ ] Independent review, regression fixes, backend/browser/process acceptance and performance evidence.
- [ ] Format, clippy, tests, release build, documentation and local commits.

## Orchestration evidence

Root runtime: GPT-6 Astra / ultra (runtime supplied). Local user config selects gpt-6.1-sol / max with cliproxyapi provider; no custom agents/profiles were found in inspected config. This is not evidence of worker effective model selection. Available native overrides: gpt-6.1-sol, gpt-6-astra, gpt-6-sol, gpt-6-luna, gpt-5.6-sol; effort values from tool schema. Workers never recursively delegate. Maximum three workers. Each implementation worktree starts at the agreed contract commit; root alone integrates. Requested settings recorded below; effective settings unverified unless runtime reports them.

| Worker | Assignment | Requested model / effort | Effective |
|---|---|---|---|
| map | Read-only repository map | gpt-6-luna / high | unverified |
| formats_research | Official formats/pricing research | gpt-6.1-sol / high | unverified |

## Baseline coverage matrix

| Path | Current observer | Analytics integration |
|---|---|---|
| Translated HTTP, SSE, legacy completions, Gemini | proxy::Tracker; formats -> IR | Native metadata before flattening; bounded final observation per attempt |
| Pass-through JSON/SSE | proxy::execute_inner, passthrough_stream, collect_passthrough | Observe without payload changes |
| Retries/failover/auth refresh | Tracker::attempt, execute_inner loop | Close previous attempt; logical request once; failed usage retained |
| WS native | ws::native_turn, Tracker | One request per turn, response identities, cancel/terminal handling |
| WS HTTP fallback | proxy::execute | Same logical-request contract |
| Compaction/images/video create | media::with_accounts | Capture reported JSON usage; unsupported pricing explicit |
| Video status polling | media::video_status | No generation inference; document observation gap |
| count_tokens/model listing | local metadata endpoints | No billed generation; excluded from token accounting |
| Devin protobuf | translated IR | Reported flattened fields; richer metadata unavailable explicit |

Live ring is 300 records and is not historical storage. IR Usage currently uses noncached input, separate read/write, output, reasoning; fields are u64 defaulting to zero and TTL detail is lost. Analytics uses independent optional fields.

## Shared contract v1

`src/usage/types.rs` owns strict allowlisted Observation and Tokens. UTC event and ingestion milliseconds; bounded integers. Tokens input = ordinary non-cache input; cache_read/cache_write are disjoint input categories; write_5m/write_1h are subsets of cache_write; output includes reasoning; reasoning is subset and never added to output. Missing is None. Cache write TTL unknown stays unknown. Numeric metadata is an enumerated typed map, never arbitrary JSON. Unknown account/model/tier remains unknown. No transcript, project metadata or credential data.

Observation retains source (`proxy`, `claude_code`, `codex`), stable source_event_id, origin identity (local or server-bound collector), parser version, provider (`anthropic`, `openai`, otherwise provider name), actual/requested models, optional account/auth, request/response/session identities only if evidenced, attempt/logical ids and completion state. Provider request ID and response/message ID are distinct namespaces. Imported auth/account must not be inferred from current sign-ins. Server stamps ingestion and collector identity, strips untrusted proxy attribution, rejects claimed proxy source.

SQLite observations retain provenance separately from derived accounting entries. Idempotency key = source + origin + source_event_id. Copied native histories reconcile using native stable source event identity across origins. Reliable provider response/message IDs can associate proxy/history; no token/time matching. Proxy evidence wins over local import, which wins over collector, with deterministic tie-breaks. Imported-only source totals remain separate and explicitly possibly overlapping; the default defensible reconciled total contains proxy entries and their matching history evidence only, never a guessed cross-source grand total. Conflicts stay visible. Collector claims cannot override trusted proxy counts/accounts/models.

Pricing stored alongside derived entries at ingestion with catalogue version/basis; nanos of USD (i64, checked arithmetic) avoids per-event cents rounding. Unknown pricing is NULL, not zero. Historical events outside documented effective periods explicitly unpriced/current-rate-equivalent only if labelled. Never combine quota percentages/credits with API estimates.

SQLite uses WAL/FULL sync, versioned migrations, transactions and indexes. Bounded writer channel on a dedicated blocking worker; proxy enqueue never waits for disk, failed enqueue increments visible gaps. Enqueued events are not durable until commit; crash-loss bound = queued + current batch. Imports/collector ingestion await durable transaction ack, idempotent retry; checkpoint moves atomically with records. DB failure leaves proxy available with degraded analytics. Queries via spawn_blocking, bounded pagination/aggregation. Purge watermark blocks automatic resurrection; explicit reimport can deliberately reset it.

API prefix /api/usage under existing management auth. Collector-only /api/usage-ingest separately authenticated. Query range start/end RFC3339 or local dates + IANA timezone, filter provider/model/account/client/source; server groups local calendar dates. Source totals, reconciled proxy totals, unpriced/missing/partial/conflicts/possible overlap explicit. Detail/export pagination bounded. CSV escapes formulas. Collector enrollment returns credential once; hash only server-side; fixed prefix excluded from inference/management even when defaults allow anonymous loopback. Non-loopback collector destinations require HTTPS, default verified certs, redirects disabled. Batch version=1, at most 200 observations/512 KiB.

Configuration additive `usage` section: enabled default true for service, database optional (config-adjacent usage.sqlite3), retention_days=90, queue_capacity=1024. Local imports disabled by default; roots explicitly enabled via management/CLI and persisted in usage database. Collector mode initializes its own state dir/outbox and runs without App/provider pool. Client attribution additive `named-clients` list {id,label,key}; old api-keys unchanged; stable key scope is a fingerprint, not physical machine proof.

## Baseline checks

Default cargo test could not start: system Rust 1.85 < manifest/dependency 1.88. Use `rustup run 1.96.1 cargo ...` explicitly, no global toolchain changes. Full baseline rerun pending.
