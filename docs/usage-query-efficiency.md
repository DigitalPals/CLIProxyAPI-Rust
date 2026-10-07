# Usage query retirement and efficiency — 7 October 2026

The old production summary calculation has been removed. The management-only
`/api/usage/summary` URL returns HTTP 410 with a pointer to
`/api/usage/dashboard`, without opening the journal or taking a reader slot.
The acceptance and benchmark scripts use the dashboard response. An independent
SQL reference remains behind `cfg(test)` to preserve the existing accounting,
request identity, conflict, deduplication and daylight-saving regression checks;
it is excluded from production builds.

## Cancellation and bounded work

Usage reads retain four independent SQLite WAL snapshots. Waiting for a slot
is limited to two seconds; execution, including blocking-pool queue time, is
limited to ten seconds. SQLite progress checks, interruption, cancellable lock
waits and cooperative Rust checks stop abandoned work. Expected cancellation
and timeouts do not mark the usage writer unhealthy. Queue and execution
timeouts return HTTP 503 and 504 respectively.

The Usage page now aborts superseded network requests. Changing filters or
refreshing cancels the previous reads; pagination cancels only the preceding
records request. Leaving the page cancels pending reads, and returning resumes
loading. Sequence checks still reject late responses from transports that ignore
cancellation. Chart grouping still switches without another request.

## Measured improvements

A covering timestamp index lets filter choices read only scalar metadata instead
of fetching large observation payload pages. The upgrade is transactional and
idempotent, retains schema version 3 and the original index name, and remains
readable by the retained binary. Collector outboxes keep their compact index.
Grouping also reuses existing dimension keys and account identifiers and borrows
the completeness value instead of allocating another string for every record.

On an isolated snapshot containing 196,964 production observations, complete
responses matched the retained binary exactly for nine date/filter/grouping
cases:

| Range | Retained binary median | Final candidate median |
| --- | ---: | ---: |
| Today | 15.7 ms | 6.3 ms |
| Seven days | 92.9 ms | 34.8 ms |
| Thirty days | 606.7 ms | 234.5 ms |

Startup including index construction took 1.22 seconds. The index added
29,118,464 bytes (2.18%) to the copied database. A separate synthetic raw-insert
benchmark found no measurable write regression; it does not establish a write
speedup. An alternating comparison with identical indexes and cancellation code
measured a further 3.46% median improvement for seven-day reports from allocation
changes alone, winning 14 of 15 pairs. Larger-range allocation-only timings were
noisy. All 270 measured comparison responses were identical.

These changes reduce dashboard processing and allocation costs. They preserve
token accounting and do not change upstream model token consumption. There is no
TTL result cache or approximate total. Imported overlap checks still consult the
whole journal, and raw-only collectors remain discoverable.

## Validation

The exact release source passed 342 Rust tests, 36 UI tests, 30 Python tests,
formatting, Clippy with warnings denied, and twelve end-to-end collector checks
against both debug and release binaries. The migrated benchmark also completed
with a synthetic local provider and 1,000 imported observations.

A real Chromium check served the candidate's exact assets and observed ten
cancelled native fetches and ten corresponding closed server connections. It
verified filter changes, paging, navigation, independent section rendering and
no extra request for chart switching, with no JavaScript exceptions. The T3
preview explicitly reported that no automation host was available, so the check
used isolated headless Chromium.

All four accounting tables on the production copy retained identical row counts
and content hashes. Its one pre-existing open writer session came from copying
a live database; the candidate added no writer errors or dropped observations.

The release source archive is pinned independently of the shared working tree.
It includes the starting source at `84d1b5f` and this task's usage changes;
concurrent overview layout edits were excluded. The rebuilt development service
uses the current workspace, including those separately committed layout edits.
Private build, benchmark, browser and rollout artifacts are under
`target/usage-retirement-20261007/`.

## Production deployment

Production is running `0.3.2+gui.c96b49fe73ba`. After the bounded quiet wait,
the authorized forced restart began with four active requests. Shutdown was
clean. The durable updater committed the release, retained the previous binary,
verified a separate 1,355,105,152-byte journal backup, and released admission.
The persistent gate was unchanged.

All 197,547 observations captured before deployment, including 196,916 priced
snapshots, were preserved byte for byte. Closed-range dashboard values matched
exactly. Five account identities, 62 routing assignments, configuration and
management authentication were verified. Native recording resumed with zero
drops, rejections, writer errors or unclosed sessions. The first audited 22
completed requests included 19 successes and three downstream cancellations,
with no request failures.

Eight authenticated public HTTPS requests per range measured these live medians:

| Range | Median | Maximum |
| --- | ---: | ---: |
| Today | 8.5 ms | 9.8 ms |
| Seven days | 37.2 ms | 44.7 ms |
| Thirty days | 311.6 ms | 357.7 ms |

Another records page took 21.4 ms. The pre-deployment thirty-day median was
697.5 ms over five backend HTTP requests; that measurement used a different
transport and slightly fewer records, so the same-snapshot comparison above
provides the cleaner performance comparison. Public TLS and every embedded
dashboard asset match the release. Validation made no paid inference calls.

The [sanitized production receipt](usage-query-efficiency-production.json)
records the release hashes, preserved data, test results and measured timings.
