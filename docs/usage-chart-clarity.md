# Usage chart scope and Claude Fable

Production SQL and the dashboard agree on the screenshot's October 1 readout:
2,446 Claude calls, a known priced subtotal of $0.00028, and 2,444 unpriced
calls. The UI rounds that subtotal to $0.0003. Two calls are priced; 2,442
lack cache-write lifetime and two lack token usage. This is a single day's
subtotal, whereas the headline and model breakdown cover the selected thirty
days. The graph did not miscalculate that amount, but the presentation made
the different periods and very incomplete pricing easy to overlook.

The canonical database and the original keeper metadata both contain 1,896
`claude-fable-5-1` calls, from September 12 at 12:09:35 to September 30 at
14:52:53, Europe/Amsterdam. Requested and actual model names agree. The model
was eleventh in the fifteen-model breakdown, below the initial eight rows.
One call has a $0.0069 current-rate equivalent; 1,879 have unknown cache-write
lifetime and sixteen lack token usage. No model identity or cost was lost in
the import. These known subtotals do not represent all Claude spend.

The presentation now labels readouts as a day and breakdowns with the entire
selected range. Each provider/model readout retains pricing coverage, including
wholly unpriced groups and the coverage of models merged into Other. A known
zero stays zero; an unknown cost says Unpriced, including the headline for
an entirely unpriced range. When no day has priced cost, the default readout
shows the busiest day rather than an empty date. Model breakdowns initially
show twenty rows, making all fifteen current models visible, and mixed
coverage prioritizes unpriced counts. Full coverage details remain in titles.

This change uses the existing fast dashboard snapshot and introduces no
additional requests or cost assumptions. The Rust runtime and rate catalogue
match the previous deployed source.

## Validation and live deployment

All 32 JavaScript checks passed, including 23 usage checks. Browser checks used
the rebuilt backend's actual assets and sanitized production model/provider
aggregates. Desktop and mobile both show all fifteen models, support Fable
filtering, retain per-group unpriced coverage, and show Unpriced for an unknown
range total. The headline still renders before delayed status/records, chart
switches make no request, and there were no runtime errors or page overflow.
The collaborative preview became unavailable, so the browser checks used
headless Chromium from the shell after that explicit error.

Production is on `0.3.2+gui.3697a4b98262`. The authorized forced maintenance
completed with zero active requests at stop and a clean writer shutdown.
The durable updater made separate private configuration and usage backups,
retained the persistent gate, committed the release, and released admission.
All **196,389 existing observations**, including **106,357 priced snapshots**,
were preserved byte-for-byte. Five account identities, sixty routing
assignments, configuration, and the closed-range dashboard values were
verified unchanged. Live October 1 and Fable figures above were checked again,
and provider totals reconcile exactly with the daily series.

Public HTTPS assets match the release digests and management authentication
checks pass. Median loading time across eight requests was **10.8 ms today**,
**72.15 ms for seven days**, and **679.9 ms for thirty days**; another records
page took **23.4 ms**. Native recording continued with zero drops, rejections,
writer errors, or unclosed sessions. The required development service on
`thebeast` was rebuilt, restarted, and health-checked; the temporary browser
fixture was stopped after validation.

The sanitized [production receipt](usage-chart-clarity-production.json) records
the source, binary and asset digests, backups, transaction, exact preservation,
live model/provider evidence, and browser/performance checks. Private rollout
artifacts are in `target/usage-graph-20261007/` and the matching production
incoming directory. This final validation and receipt were added after the
release source snapshot.
