# Usage loading performance — 7 October 2026

Deployed `0.3.2+gui.31af9badd922` to production. The delay came from repeated aggregation
of saved usage history, rather than recalculating prices on page refresh. The imported
history expanded the journal to approximately 196,000 observations and 1.3 GB.

## Production measurements

| Range | Previous summary | New first request | New median | New maximum | Improvement |
| --- | ---: | ---: | ---: | ---: | ---: |
| today | 167.9 ms | 15.8 ms | 10.6 ms | 15.8 ms | 15.8× |
| 7days | 2,104.5 ms | 82.0 ms | 75.8 ms | 84.6 ms | 27.8× |
| 30days | 27,782.5 ms | 704.2 ms | 683.9 ms | 704.2 ms | 40.6× |

The previous measurements are one authenticated HTTPS request per range. The new
measurements are eight requests per range through the public HTTPS admission gate,
including a fresh TLS connection for every request. Measurements use Amsterdam
calendar dates and include today. Native ingestion continued, so the later record
counts are higher. These timings measure API response time; browser and network
conditions add to visible page loading time. The next records page took
19.4 ms. No inference calls were made for validation.

## Implementation

- `GET /api/usage/dashboard` reads one independent WAL snapshot. One streaming pass
  over scalar accounting entries computes totals, both chart groupings and all four
  breakdowns. A separate pass over raw dimension tuples computes facets.
- The UI renders counts, records and collection status as each read finishes.
  Paging requests only records. Provider/model chart switches use both groupings
  returned in the same snapshot. New filters clear prior results and reject late
  responses from earlier requests.
- The existing full summary endpoint retains source reconciliation and logical
  request/attempt diagnostics for API clients. These expensive sections are not
  requested by the dashboard.
- There is no TTL result cache, price recalculation on reads or background estimate.
  Costs remain exact integer nanounits; missing values remain unknown. Overflows
  remain explicit null totals with overflow evidence. Reasoning and TTL subsets are
  not added twice.
- Imported overlap checks still consult proxy evidence outside the selected range
  and filters. Raw-only collectors remain discoverable in facets. DST dates,
  breakdown ordering and 500-row facet/breakdown limits remain equivalent.

## Validation and preservation

All 325 Rust tests and 18 Usage-page tests passed, together with formatting and
Clippy checks with warnings denied. Tests compare the independent full SQL and
streaming calculations for overlaps, filtered origins, revisions/copies, missing
usage, empty ranges, overflow, DST, large facets and live writes.

The final release was also checked on an isolated copy of 195,586
production observations. All displayed totals, facets, trends and breakdowns
matched the existing summary, including real provider/model/account/client filters.
Four concurrent 7-day reads completed in approximately
190–197 ms. Every copied observation remained unchanged.
Browser checks confirmed that the headline appears while status and records are
artificially delayed by one second, chart switching sends no request, desktop and
mobile layouts fit, and no runtime errors occur. The T3 preview explicitly reported
that its automation host became unavailable; headless Chromium completed these checks.

On production, all 195,972 pre-deployment observations were
verified byte for byte, including 105,977 priced records and all
unpriced records. A closed historical range matched the previous production
summary's displayed fields exactly. All 5 accounts and
60 routing assignments were preserved.
Configuration, catalogue snapshots, schema version 3 and the persistent admission
gate were verified. The writer reported zero drops, rejections, errors or unclosed
sessions, and native ingestion resumed. The dev service on `thebeast` was rebuilt
and restarted as required.

## Deployment backup

The updater rejected an initial unsupported version label before activation. After
rebuilding with its supported `+gui` provenance label, the first activation
stopped during backup: the imported journal exceeded the installed updater's
512 MiB backup bound. The candidate was not swapped; automatic recovery restarted
the previous release with a healthy journal and released admission.

The successful retry used the same tested implementation and a separate private,
streaming journal archive bound to the same updater transaction. It requires a
stopped writer, caps journal data at 4 GiB, rejects nonregular files, fsyncs the
archive and receipt, and verifies a streamed SHA-256 before candidate restart and
verification. The installed updater's package/core backup limits remain unchanged.
A 2,653,851,599-byte isolated fixture
verified the large backup, running-writer rejection and corruption detection.

The successful rollout forced a clean restart with
2 active requests, as authorized. The protected
journal backup holds 1,337,668,480 bytes. Its receipt and
archive remain under the private transaction directory
`/var/lib/cliproxy-rust-update/backups/7d476dcfbcbf4362957690f35c03a427/`.
The original release is retained for rollback. Public TLS, authenticated dashboard
access and every deployed UI asset were verified after commit.

Evidence: [sanitized production receipt](usage-loading-production.json),
[API contract](usage-ui-contract.md), and private deployment/benchmark artifacts
under `target/usage-speed-20261007/` and the matching production incoming directory.
