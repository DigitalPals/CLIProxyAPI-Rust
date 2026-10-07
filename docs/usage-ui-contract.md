# Usage dashboard API contract

The vanilla `/ui/usage.js` script loads before `app.js`; the server must embed and
serve it as JavaScript. All endpoints below use the existing management Bearer
credential, supplied only in the Authorization header. No external resources or
rates are fetched. Monetary values are backend-derived signed USD nanounits;
null/missing values are unpriced, never implicitly zero.

## Range and filters

Summary, observations and exports receive `start=YYYY-MM-DD`,
`end=YYYY-MM-DD`, and `timezone=<browser IANA timezone>`.
Start is inclusive local midnight; end is exclusive local midnight. Today, 7 days
and 30 days include today. The custom UI accepts an inclusive “Through” date
and advances it by a calendar day, without adding a fixed 24-hour duration.
Optional `provider`, `model` (actual model), `account`, `client` filters use
stable server values. `source` is still accepted by the API but the UI no longer
offers it. The `client` facet includes named client IDs and server-bound
`collector:<id>` origins. Every read/action is authenticated.

Summary also accepts `stack=provider|model` (default `provider`), which sets the
grouping of `combined.trend`. Observations and exports accept
`view=combined|raw`; a missing `view` means `raw`. Any other value of `stack`
or `view` is a 400.

## GET /api/usage/summary

```json
{
  "range": {"start":"2026-10-01", "end":"2026-10-08", "timezone":"Europe/Amsterdam"},
  "proxy": {
    "logical_requests":3, "attempts":4, "observations":4,
    "estimated_cost_nanos":12345000,
    "tokens":{"input":1000,"output":200,"cache_read":100,"cache_write":null,"write_5m":null,"write_1h":null,"reasoning":20},
    "unpriced":1, "partial":1, "missing_usage":0, "conflicts":0
  },
  "sources":[
    {"source":"claude_code","observations":2,"estimated_cost_nanos":null,
     "unpriced":2,"partial":0,"missing_usage":0,"conflicts":0,"possibly_overlapping":true}
  ],
  "trend":[
    {"date":"2026-10-01","source":"proxy","observations":4,
     "logical_requests":3,"attempts":4,"estimated_cost_nanos":12345000,"unpriced":1}
  ],
  "facets": {
    "providers":["anthropic","openai"], "models":["claude-sonnet-4-5"],
    "accounts":[{"id":"stable-account-id","label":"Account label"}],
    "clients":[{"id":"stable-client-or-collector-id","label":"Work laptop"}],
    "sources":["proxy","claude_code","codex"]
  },
  "pricing":{"version":"local-catalogue-v1","basis":"Published API rates"},
  "combined": {
    "basis":"One entry per provider response. Imported entries that share a response ID with a proxy entry are excluded.",
    "totals":{"observations":1286,"estimated_cost_nanos":98123000000,"unpriced":3,"partial":0,
              "missing_usage":0,"tokens":{"input":1000,"output":200,"...":0},
              "history_only":1234,"matched":870,"weak_identity":12},
    "proxy_first_event_at_ms":1789344000000,
    "stack":"provider",
    "trend":[{"date":"2026-10-01","group":"anthropic","observations":412,
              "estimated_cost_nanos":98123000000,"unpriced":3,"partial":0,"tokens":{}}],
    "breakdowns":{
      "provider":[{"id":"anthropic","provider":"anthropic","accounts":2,"observations":412,"estimated_cost_nanos":98123000000}],
      "model":[], "account":[], "client":[]
    }
  }
}
```

Proxy requests and attempts are separate. Source entries may also contain
`tokens` with the same schema. The per-source `proxy`, `sources`, `trend` and
`breakdowns` fields remain for older clients and may overlap.

`combined` is the main number on the page. It counts the accounting entries in
the range and filters, then drops every imported entry whose provider response
ID also has a proxy entry. That check is not limited to the range, so a proxy
call and its imported copy on opposite sides of midnight count once; the proxy
entry wins. `totals`, each `trend` row and each breakdown row share the
aggregate shape of the source totals (`observations`, `estimated_cost_nanos`,
`known_cost_nanos`, `unpriced`, `partial`, `missing_usage`, `pricing_partial`,
`tokens`, `missing_token_counts`, `aggregation_overflow`, ...).

- `totals.history_only`: imported entries in the combined set.
- `totals.matched`: imported entries in range that were dropped because a proxy
  entry has the same response ID.
- `totals.weak_identity`: imported entries in the combined set without a
  response ID. They can never be matched, so they may still duplicate proxy
  traffic; the UI shows this count.
- `proxy_first_event_at_ms`: earliest proxy observation regardless of range or
  filters, or null. Days before it predate the proxy.
- `trend`: one row per local date (in `timezone`, DST-aware) and group, sorted
  by date then group. `group` is the provider or the actual model per `stack`;
  it can be null for an unknown model. Every group is returned; the UI keeps the
  top five and merges the rest into "Other".
- `breakdowns`: provider, model, account and client over the combined set, with
  no split by source. Each row adds `id` (may be null), `accounts` (distinct
  known accounts) and `provider` (lowest provider id in the group, for
  sub-lines). Client ids include `collector:<id>` origins. At most 500 rows per
  dimension, ordered by estimated cost, then observations, then id.

Usage older than the pricing catalogue is priced at today's rates; its
`pricing_snapshot.backdated` is true and the UI labels it a current-rate
equivalent, not what was paid at the time.
Missing/unknown quality counters render as Unknown. An absent/null estimate
renders Unpriced; a literal zero renders $0.00. All server strings are escaped.
Facets should represent the complete selectable filter domain for the range,
not only the current observation page.

## GET /api/usage/observations

Adds `limit=50&offset=<nonnegative offset>` to the same range and filter query.
The server must use stable ordering and apply filters identically to summary.

```json
{
  "items":[{
    "id":1,"event_at_ms":1790805600000,"source":"proxy","provider":"anthropic",
    "actual_model":"claude-sonnet-4-5","account_id":"stable-account-id",
    "account_label":"Account label","client_id":"stable-client-id","client_label":"Work laptop",
    "origin_id":"local","collector_label":null,
    "tokens":{"input":1000,"output":200,"cache_read":100,"cache_write":null,"write_5m":null,"write_1h":null,"reasoning":20},
    "estimated_cost_nanos":12345000,"pricing_basis":"Published API rates",
    "completeness":"partial","state":"complete"
  }],
  "total":1,"limit":50,"offset":0
}
```

Optional labels may be null. `origin_id` is `local` or `collector:<server-id>`; a
collector label takes precedence for presentation.

With `view=combined`, rows come from the combined set described above, ordered
by `event_at_ms` then id (both descending), and `total` is the combined count.
Each item has the raw fields plus:

- `matched_sources`: on proxy rows, the sorted imported sources that have the
  same response ID, for example `["claude_code"]`; `[]` otherwise.
- `origin_label`: on imported rows, the collector label, or `"This server"` for
  `origin_id = "local"`; null on proxy rows and for an unknown collector.

`superseded` is normally false in the combined view. `completeness` is
`complete`, `partial` or `missing`; completion `state` remains independently
visible. Input is non-cache input; cache reads and writes are separate. Write
5m/1h are subsets of cache_write. Reasoning is a subset of output. None of
these nested categories are added again by the UI.

## GET /api/usage/status

```json
{
  "health":{"state":"healthy","message":null,"queue_depth":0,"dropped":0,"writer_errors":0,"last_commit_at_ms":1790805600000},
  "imports":[{"source":"claude_code","root":"/explicit/server/root","enabled":true,"state":"idle","last_scan_at_ms":1790805600000,"last_error":null,"imported":2}],
  "collectors":[{"id":"collector-id","label":"Work laptop","revoked":false,"state":"healthy","last_contact_at_ms":1790805600000,"last_sync_at_ms":1790805600000,"pending":0}],
  "pricing":{"version":"local-catalogue-v1","basis":"Published API rates"}
}
```

Missing timestamps/counts are shown as Not reported/Unknown. Health state
and counts come from the backend; last contact is never inferred to mean
synchronized. Imports are disabled unless explicitly enabled. Existing
`S.accounts` is reused for provider allowances, including quota windows and
`quota.updated_at`, and is not converted into money or token estimates.

## Source controls

- `POST /api/usage/imports`: `{source:"claude_code"|"codex", root:string, enabled:boolean}`.
- `POST /api/usage/imports/scan`: `{source:"claude_code"|"codex"}`.
- `POST /api/usage/collectors`: `{label:string}`; returns
  `{collector:{id:string,label:string}, credential:string}`.
- `POST /api/usage/collectors/:id/rotate`: returns the same one-time credential shape.
- `POST /api/usage/collectors/:id/revoke`: returns any JSON success object.

After any successful mutation the UI refreshes all three reads. Rotate and
revoke have explicit consequence confirmations. Another enrollment/rotation
cannot overwrite an un-dismissed one-time credential. The credential remains
in browser memory until dismissed, and is never saved to browser storage.
No quota reset, credit purchase, or retention deletion controls are included.

## GET /api/usage/export

Adds `format=csv|json&limit=50&offset=<current page>` to the same query,
including `view=combined|raw`.
The response is downloadable metadata for the same page/filter/range, with
correct content type. Every button and success message explicitly says “this
page”; this is not a full-dataset export. Server CSV must neutralize formula
cells. Client fetches a Blob with Authorization then creates a temporary local
object URL. The management key is never a query parameter or download URL.

## Privacy, failures and polling

Usage privacy uses the existing `S.private`. Entire account/client/collector
labels and identifiers, saved roots, draft fields and one-time credentials are
masked. Identifier-bearing filter options and action buttons use numeric DOM
indexes; actual IDs stay in memory. Error details and health messages are
masked in privacy mode because they may contain paths. Copy credential still
copies the actual value. Exports deliberately contain the real metadata.

Read failures produce a visible error and retry control. Successful portions
may still render alongside that error. Management 401/403 returns the existing
lock view. Initial loading, empty histories, no matching records and missing
source status are explicit. Reads refresh once a minute while Usage is visible;
manual refresh and preset date ranges recalculate calendar boundaries.

Source summaries include `source_record_count` (raw observations/revisions) and `observations` (globally selected accounting entries). Collector-filtered raw copies may have their accounting evidence selected under another origin. `coverage_basis` explains this distinction. Breakdowns contain server aggregates for provider/model/account/client, capped at 500 groups per dimension. Collector status adds the strict per-source `progress` reports documented in [usage-imports-collectors.md](usage-imports-collectors.md).
