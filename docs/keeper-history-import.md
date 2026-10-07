# Production keeper history import

All **192,716** retained CPA usage keeper events were imported into the production Fusebox database on 2026-10-07. Their original coverage is **2026-09-12 09:36:38 through 2026-10-02 15:52:16**, Europe/Amsterdam. Use **30 days** on the Usage page to see the full imported period. No records were available for October 3–6; native capture starts October 7 at 08:32:14. The import does not reconstruct the interval between the last keeper event and first native event.

The proxy remains on `0.3.2+gui.2849cdfb943e`. A separately staged CLI helper, `0.3.2+keeper.d3141cb807d6`, inserted the records through the existing storage, pricing and deduplication implementation. The proxy and persistent admission gate stayed running, with unchanged configuration, account settings and routing. Native recording continued with zero dropped/rejected records or writer errors.

## Source interpretation

The installed keeper was v1.15.4. Its source was checked at [commit f53fec8d3f7c0f189cf046aeae807375fd9b23e4](https://github.com/Willxup/cpa-usage-keeper/tree/f53fec8d3f7c0f189cf046aeae807375fd9b23e4):

- `internal/service/tokenprocessor/claude_executor.go` folds cache reads and creation into stored inclusive input.
- `internal/service/tokenprocessor/codex_executor.go` retains inclusive input/output, with reasoning inside output.
- `internal/helper/usage_cost.go` subtracts cache reads and writes before charging uncached input.
- `internal/service/redis_usage.go` uses the client request ID as `event_key`. Production reused these IDs across distinct events; all 192,716 rows have distinct request-ID/timestamp identities. Deduplicating by request ID alone would incorrectly discard 32,935 events.

The importer separates inclusive input into uncached input, cache reads and cache writes. Reasoning stays a subset of output. Applied response tiers take precedence: 40,750 requested-priority events returned the default tier. The stable `keeper:production-20260912-v1.15.4` origin and original row IDs identify events across retries or source-file copies. Request/attempt grouping and provider response identities remain unknown. Historical account references use namespaced hashes rather than assigning events to current accounts. Credentials, email labels, IP addresses, user agents and raw messages are excluded from the selected metadata export and imported observations.

## Result and pricing limits

The imported token categories reconcile exactly with the source:

| Category | Tokens |
| --- | ---: |
| Uncached input | 400,061,338 |
| Cache reads | 36,826,749,846 |
| Cache writes | 639,810,695 |
| Output, including reasoning | 122,884,032 |
| Reasoning subset | 42,513,381 |
| Total, excluding repeated subsets | 37,989,505,911 |

There are **102,990 priced** and **89,726 unpriced** observations. The known priced subset totals **$14,220.05819702 at catalogue 2026-10-07.3**. This is a partial equivalent at current API list prices, not historical spend or a subscription invoice. All historical prices retain explicit partial/backdated labels. Unpriced records comprise 89,536 with unknown cache-write TTL, 173 failed events with unavailable usage and 17 models without a catalogue rate. Cache-write TTL, inference region and Claude fast-mode speed were not retained in keeper; they are not reconstructed.

## Verification and recovery

323 Rust tests, formatting and clippy with warnings denied passed. New importer checks cover token normalization, applied tiers, read-only previews, privacy, malformed/unsupported records, retention, overlap refusal, idempotency and concurrent native recording. The relevant development service was rebuilt, restarted and health-checked on `thebeast`.

Before the live import, a full production-copy trial inserted all events and a second run inserted zero, reporting 192,716 duplicates. Both original databases have private, consistent SQLite backups on production. The live import used 100-record commits with a yield between batches. It preserved all **2,906 pre-import observations byte-for-byte**, including **2,674 priced snapshots**, and created exactly 192,716 raw, canonical and per-source entries. SQLite integrity, historical dashboard API totals, public HTTPS health, unchanged production units/settings, and continued native durability were verified without inference calls.

An initial public HTTPS verification used a helper restricted to the backend port. The check was corrected and verification repeated read-only; the live import was not repeated. The operation unit subsequently completed with `Result=success` and `ExecMainStatus=0`.

Private operational artifacts and the helper remain under `/opt/cli-proxy-api-rust/incoming/fusebox-keeper-history-20261007/`; backups are under `/var/lib/cliproxy-rust-update/keeper-history-20261007/`. Replays must use the same origin and source row identities. A retry after a partial batch commit is idempotent. Keep live SQLite backups consistent with their WAL rather than replacing the active database with a main-file-only copy.

The sanitized [production receipt](keeper-history-import-production.json) includes source/helper hashes, backup hashes, exact counts and preservation evidence. Its source archive identifies the helper build snapshot; this final report and receipt were added afterward.
