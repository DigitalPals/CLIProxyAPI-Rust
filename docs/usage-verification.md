# Usage & Costs acceptance evidence

Verified locally on 2026-10-07. All histories were synthetic; providers were loopback mocks. No personal histories, production accounts, paid provider calls, deployments, pushes, merges, live credential changes, or existing service restarts were used. Temporary backend/collector processes were started and stopped for acceptance.

## Local delivery

The prior uncommitted work was preserved on local `main` as `4a81fe3`. Implementation is on `feature/usage-costs`. Integrated implementation commit: `6c060b1`; supporting commits contain the shared contracts, embedded UI, store, importers, collector hardening and review fixes. No remote operation was performed.

Primary implementation areas are `src/usage/` (contracts, capture, journal, pricing, imports, collectors, API and tests), the existing proxy/WS/media lifecycle and authentication/config modules, `ui/usage.js` plus embedded dashboard navigation/styles, `tests/`, and `scripts/`. The single executable remains self-contained; bundled SQLite and timezone data add no database server or runtime requirement. The verified Linux release executable is 17,613,672 bytes.

See [setup and operations](usage-costs.md) for tested server/import/collector commands and [orchestration record](usage-implementation.md) for all worker requests and effective-setting limitations. The built server entry point is:

```sh
./target/release/fusebox --config /absolute/path/to/config.yaml serve
```

Server analytics defaults on; imports and remote collectors require explicit enrollment/root configuration. That command is supplied for operator use and was not run against an existing installation during implementation.

## Integrated checks

The repository's system Rust 1.85 is too old for its existing Rust requirement. Commands used the already installed Rust 1.96.1 explicitly; the global toolchain and Codex configuration were unchanged.

```sh
rustup run 1.96.1 cargo fmt --all -- --check
rustup run 1.96.1 cargo clippy --all-targets -- -D warnings
rustup run 1.96.1 cargo test -- --nocapture
node --test tests/*.test.cjs
node --check ui/app.js
node --check ui/config.js
node --check ui/usage.js
node --check tests/usage-browser.acceptance.js
rustup run 1.96.1 cargo build --release
python3 scripts/usage-e2e.py --binary target/release/fusebox
python3 scripts/usage-benchmark.py --binary target/release/fusebox
git diff --check
```

Rust: **272 passed, 0 failed, 0 ignored**. JavaScript: **14 passed, 0 failed, 0 skipped**. Formatter, clippy with warnings denied, JavaScript/Python syntax checks and release build passed. Baseline before the feature was 203 Rust tests and 2 JavaScript tests; no unresolved pre-existing test failure was identified.

| Release gate | Executed evidence |
|---|---|
| Persistence beyond the live ring | Real HTTP/SSE/compaction test records 306 logical requests, restarts App against the same SQLite file, and retains totals. Store test separately persists 601 observations. |
| Transport/lifecycle | Actual mock HTTP, translated Anthropic response, duplicate SSE finals, native WebSocket turn, compaction, failed usage before retry, cancellation, interrupted stream and missing usage. Existing routing/WS suite also passes. |
| Accounting/pricing | Disjoint cache fields, TTL categories, inclusive reasoning, exact nanodollar fixtures, threshold boundary, service tiers, historical date boundary, local override digest, unsupported and partial estimates. Checked aggregate overflow returns unknown plus diagnostics. |
| Native parsers | Claude and Codex fixtures cover malformed/trailing fragments, incremental restart, copies, rotation/truncation, zstd/plain siblings, resumed cumulative counters, resets, child-thread identities, modern supersession, invalid-row state isolation and unknown fields. |
| Reconciliation | Proxy/history identity match, reversed insertion order, conflicting collector claims, equal-token distinct events, copied origins, late revisions, superseded counters and filtered raw coverage. Request identities remain independent of selected token charges. |
| Collectors | Two separate processes/identities, durable replay acknowledgement, offline process restart, copied histories, revocation, rotation/re-enrollment, spoofed attribution stripping, credential boundaries, rate/batch bounds, progress validation and atomicity. |
| Privacy | Synthetic prompt/tool/project markers absent from transmitted allowlisted records, SQLite dumps, exports, logs and UI. Credential hashes only on server; sensitive UI labels escaped. |
| Access controls | Inference credentials cannot read analytics; collector credentials cannot manage accounts or infer, including otherwise-open localhost authentication configurations. |
| Failure/compatibility | Default/existing config tests, fresh schema/v1 migration, future schema rejection, retention watermark and explicit reimport, 23/25-hour DST days, queue saturation, 32 concurrent publishers, real SQLITE_FULL, read-only failure, clean/unclean restart, graceful CLI/server shutdown, collector startup allocation bounds and rollback-journal symlink rejection. |
| Browser | Product-native collaborative browser exercised the actual isolated backend. Six repeatable acceptance groups passed, plus manual multiple-root, narrow viewport, focus and unauthorized-login checks. |

`scripts/usage-e2e.py` performs nine real-process checks and asserts that reported collector progress includes actual scans. It also checks DELETE-mode collector storage limits and paginated/filtered exports. The script creates and removes its own temporary state.

## Browser acceptance

`tests/usage-browser.acceptance.js` is an in-page acceptance runner for the collaborative browser. Run it only against an isolated instance with synthetic `claude/` and `codex/` roots containing the committed fixtures. Evaluate the file, then invoke:

```js
await runUsageAcceptance({
  fixtureRoot: '/absolute/temporary/synthetic-fixtures',
  managementKey: 'synthetic-test-management-key'
})
```

It drives the actual forms and controls and asserts native import results, overlap warnings, filtering, custom dates, collector enrollment/contact/rotation/revocation, label escaping, privacy markup, real JSON/CSV downloads and import toggling. It does not mock backend data. Additional interactive checks verified a real standalone collector reporting scan progress to the browser backend, copied collector evidence showing one raw record and zero selected entries with an overlap explanation, three independently visible roots, a 390-pixel mobile layout without horizontal document overflow, keyboard-focusable filters, and locked-dashboard authentication. The first repeatable run caught an HTTP query-decoding defect in paginated exports; the fix is covered by browser, process and HTTP integration regressions.

The collaborative browser runs on a different host from this checkout: loopback navigation was refused. The same isolated test server was reached through the host's private network address. This was a test-only process with synthetic keys and data.

## Measurements

Host: Linux 6.12.107 Debian 13, AMD Ryzen Threadripper 9970X (32 cores / 64 threads), Rust 1.96.1, Node 22.22.3. No CPU pinning or production-traffic simulation. Measurements include local scheduling and filesystem noise; they are observations, not latency guarantees.

The full Rust test run generated **100,000 observations** over about 28 hours, with duplicates/reconciliation and a concurrent writer. Debug/test build results:

| Operation | Measured |
|---|---:|
| Insert 100,000 observations | 12.762 s |
| Full summary | 3.722 s |
| Empty range | 1.953 ms |
| Narrow two-record range | 2.349 ms |
| Detail page of 100 | 13.919 ms |
| Database | 265,269,248 bytes |
| Additional concurrent enqueues | 1,000, zero dropped |

The release benchmark generates 10,000 Claude records and runs a local HTTP provider with 20 warmups followed by three samples of 200 sequential requests per mode. It measures analytics off/on separately, then imports through bounded scan APIs and runs ten summary queries. Final measured import: 0.995 s for 10,000 records (4,157,780 fixture bytes, five scan calls). Summary median: 107.412 ms, maximum: 108.284 ms. Sequential request sample medians: 0.829 ms analytics off, 0.838 ms on (difference 0.009 ms). All 620 observed proxy requests committed, with zero drops. Database size: 36,249,600 bytes. Raw results are recorded in `usage-benchmark-results.json`. The measured difference includes process/network noise and is not a claimed universal proxy overhead.

## Independent review and limits

The independent reviewer did not author the implementation. It requested GPT-6 Astra / xhigh; the tool accepted these spawn parameters, but effective model/effort is unverified. Material findings were corrected and regression-tested: invalid usage snapshots, malformed-parser mutation, child-thread counter collisions, lifecycle counts, numeric aggregation overflow, persistent gap reporting, concurrent producer admission, collector filtering, and collector startup cap sequencing. Follow-up review accepted the final accounting/security/failure-handling changes. Root inspected the integrated diffs and ran the complete suite independently.

Coverage remains evidence-based: unreported usage is unknown, video status polling does not establish new spend, unsupported providers/rates remain unpriced, older undocumented price periods remain unpriced, and legacy Codex cumulative records cannot establish per-request threshold pricing. CLI history cannot identify an upstream account without evidence. Copied evidence may be selected under another origin; raw filtered records remain visible. Source totals can overlap, so there is no exact cross-source grand total. Provider allowance and subscription credits remain separate from API-equivalent estimates. This feature does not claim invoice precision or complete provider-history coverage.
