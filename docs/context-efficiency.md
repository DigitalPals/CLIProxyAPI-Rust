# Measuring context efficiency

`scripts/context-benchmark.py` compares two **caller-authored** `/v1/responses`
requests. It does not trim messages, summarize conversations, rewrite tools, or
change Fusebox's live request path. A shorter request is useful only when it
preserves the task's required information and answer quality.

The default is an offline dry run. Network requests require `--execute`; each
execution sends up to twice `--trials` requests and can spend tokens. The harness
uses Python 3.9+ and the standard library. Executing requests requires Unix timer
support, available on Debian.

## Prepare and inspect a comparison

Prepare `original.json` and `compacted.json` with a configured model, `input`, and
identical generation settings. Use `"stream": true` in **both** files for streaming
timings. Only `input` and `instructions` may differ. The harness sends the files'
original bytes without changing their contents.

```sh
python3 scripts/context-benchmark.py \
  --original /path/to/original.json \
  --compacted /path/to/compacted.json
```

The report includes SHA-256 fingerprints, transmitted byte counts, canonical JSON
byte counts, and the change in serialized `input`/`instructions` bytes. These are
**payload measurements, not token estimates or demonstrated provider savings**.
Whitespace changes can reduce transmitted bytes without changing model tokens.
No tokenizer, model, or endpoint is called in dry-run mode.

Use self-contained requests: `previous_response_id`, `conversation`, and background
generation are rejected. Function tool definitions are supported, but the harness
does not execute function calls. Hosted tools are rejected to avoid replaying
provider-side tool actions. A tool-only answer has no text TTFT and does not prove
that the original task was completed.

## Run an authorized measurement

```sh
python3 scripts/context-benchmark.py \
  --original /path/to/original.json \
  --compacted /path/to/compacted.json \
  --endpoint https://your-fusebox.example/v1/responses \
  --trials 5 --timeout 120 --execute
```

The command prompts privately for a client API key. For unattended use, provide
`FUSEBOX_API_KEY` through your secret manager or an already-exported environment
variable. Set it to an empty string only for an endpoint that needs no key.
There is no API-key command-line option. The endpoint must not contain credentials,
a query string, or a fragment. TLS certificates are verified; remote plaintext
HTTP needs explicit `--allow-insecure-http`. Loopback HTTP is supported for local
development. The harness connects directly and does not use HTTP proxy environment
variables, follow redirects, or retry requests.

Defaults are three pairs, a 120-second total deadline per request, a 2 MiB request
limit and an 8 MiB response limit. Maximums are 20 pairs, 600 seconds, 16 MiB and
64 MiB respectively. Set a suitable output limit in **both** request files if the
chosen provider supports it. Body and time limits bound the benchmark process;
they are not a provider billing limit. Closing a connection on timeout or Ctrl+C
does not guarantee that provider-side work or billing stops.

The harness prints metadata-only JSON to stdout and writes no request or response
transcript. It discards generated text, tool arguments, response identifiers, and
provider error messages. Redirect stdout yourself if you want a saved report.
Your supplied input files and the endpoint's own logging/storage policies remain
your responsibility.

## Read the measurements

Each pair runs original then compacted on odd pairs, and compacted then original
on even pairs. There are no unreported warmup calls. Inspect individual trials:
early cold requests and later warmed requests can differ substantially.

Every trial sends the same freshly generated `x-fusebox-session-id` header to
retain one account assignment when Fusebox session affinity is enabled. The report
includes the journal fingerprint of Fusebox's credential-scoped session identity
for correlation with the sanitized request journal. The raw session header, client
scope and API key are not printed. This does not change either request body. Only the last **planned** request sends
`x-fusebox-session-end: true`. An early stop sends no cleanup request; its assignment
can remain until the server's configured idle expiry. Session pinning does not
prevent quota fallback, account unavailability, or routing changes when affinity
is disabled, and cannot guarantee a particular provider cache state.

| Field | Meaning |
| --- | --- |
| `latency_ms` | Wall time from starting the request through a terminal response, JSON body completion, or failure; includes connection setup. |
| `ttft_ms` | Time to first nonempty streamed text/refusal delta received by this client. Creation events, reasoning events, and tool arguments do not count. Null for JSON or no text delta. |
| `first_output_delta_ms` | Time to the first text/refusal, function-argument, or supported audio delta. |
| `usage` | Numeric input, output, total, cached-input, cache-write and reasoning counters reported in the terminal response, when present. |
| `usage_warnings` | Missing or invalid counters, or an inconsistent cache count. |
| `status` | Completion, provider failure/incompletion, cancellation, timeout, protocol failure, or HTTP/transport error. |

TTFT is a client-observed approximation to first text arrival, not an instrument
inside the provider. Several events can arrive in the same network read. Network,
proxy routing, concurrency, and a fresh connection for every trial affect latency.

Usage comes from **response usage fields exposed by Fusebox**, not from counting
characters. Upstream adapters may normalize or synthesize fields; this harness
cannot independently establish their upstream provenance or verify a bill. An
absent/invalid field remains `null`; an explicit numeric zero remains `0`. A
cached-input count greater than reported total input is invalidated and excluded
from cache summaries, with a warning. A
missing `total_tokens` is not fabricated from other fields. Cached input is usually
a subset of total input on this endpoint; do not add it to input again. Inspect
native usage provenance when validating a provider adapter's accounting.

A successful HTTP status or `[DONE]` alone is insufficient: SSE needs a valid
completed terminal response. Truncated streams, malformed events, incomplete,
failed and cancelled responses are recorded separately. Reported usage on failed
responses remains visible in the trial, since failure may still incur usage.
Summaries include **completed responses only**, with a separate observation count
for every metric. Missing usage does not become a zero-cost response.

`paired_compacted_minus_original` reports within-pair differences: a negative input
delta means the compacted trial reported fewer input tokens. Check
`all_planned_pairs_observed` and each variant's attempted/completed/planned counts
before comparing summaries. Missing observations and failed trials prevent a full
comparison. `provider_metadata_varied` flags differing reported model/tier values;
unknown metadata cannot verify consistent routing. Record the Fusebox revision,
provider/account routing, settings, run time, and other traffic separately when
you need repeatable operational results.

The exit code is 0 for a valid dry run or all completed trials, 1 for trial
failures, 2 for invalid configuration/files, and 130 for user cancellation.
Completed responses without usage still exit 0; inspect metric coverage. The run
stops early on 401, 403, 404, 429, or Ctrl+C. Other failed trials are included without
retries. Ctrl+C during a request emits the partial report and marks that trial as
cancelled; interruption before/between requests may only emit a cancellation error.

## Preserve quality and account for caching

Before compacting, define what a successful answer must retain: the current task,
constraints, decisions, unresolved questions, tool results needed for the next
step, identifiers, and relevant code/errors. Remove redundant material in the
caller where its meaning is understood. Keep original inputs available for review.
Do not silently discard earlier requirements or replace exact evidence with an
unverified summary.

Run several representative tasks and assess their answers against the same rubric.
This metadata-only harness deliberately reports `quality_evaluated: false` and
does not retain answers; perform answer review with your approved evaluation
workflow. A completion marker cannot establish factual accuracy or task quality.

Large stable prefixes may already have a high cache-hit rate. Rewriting a prefix
can reduce total input while losing cached input, increasing uncached work, cost,
or first-token latency on that run. Repeated trials warm caches, and original and
compacted requests can share prefixes and influence each other's cache state.
Alternating order reduces ordering bias but does not isolate a cold-cache effect.
Do not flush production caches for this benchmark. Report cold and warmed behavior
separately when the environment allows it, and inspect cached/input/output counters
alongside latency. Use the chosen provider's applicable rates and account contract
for any cost calculation; this harness intentionally makes no dollar-savings claim.

No production token-reduction result is supplied here. Validation uses synthetic
loopback responses only:

```sh
python3 -m unittest discover -s tests -p test_context_benchmark.py -v
```
