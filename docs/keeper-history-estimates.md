# Estimated costs for imported keeper history

The user authorized estimating historical costs from the metadata that remains
and storing those estimates in production. The old keeper retained complete
token categories for 89,536 unpriced Claude calls but did not retain the
five-minute/one-hour cache-write split. Production's 134 native Claude calls
with known positive cache writes all report five-minute writes (570,568 tokens)
and zero one-hour writes. A five-minute assumption therefore provides a
reasonable, explicitly labelled estimate for this history.

The scoped `usage estimate-keeper-cache` command prices each affected call
through the existing integer-only model, tier and context rules. It calculates
both lifetime scenarios, stores the five-minute estimate, and retains the
one-hour sensitivity in `historical_estimate` snapshot metadata. The method is
`keeper-cache-5m-2026-10-07-v1`. Required token counts must be present; missing
usage and unsupported models remain unpriced. The assumption is opt-in for a
specific verified keeper origin. It does not change native capture or future
pricing defaults.

Original tokens, unknown lifetime fields, payloads, fingerprints, timestamps,
identities and all already-priced snapshots are preserved. Only the pricing
fields of previously unpriced eligible observations and their accounting
entries change. Each snapshot includes the original pricing basis and digest,
an explicit five-minute assumption, partial/backdated labels, and the method
and application time. The entire pre-operation database is retained as a
consistent private backup. Atomic 100-record batches allow native recording to
continue; committed batches are safe to resume and subsequent runs update zero.

## Production result

All **89,536** eligible observations were updated live, adding
**$10,084.68139965** at catalogue `2026-10-07.3`. The corresponding one-hour
scenario is **$12,083.55791340**, with all other assumptions held fixed.

| Model | Calls newly estimated | Added five-minute estimate | One-hour scenario |
| --- | ---: | ---: | ---: |
| claude-opus-5-5 | 80,349 | $8,998.94 | $10,799.05 |
| claude-fable-5-1 | 1,879 | $524.09 | $676.80 |
| claude-opus-5 | 1,867 | $317.47 | $338.75 |
| claude-sonnet-5 | 5,214 | $191.02 | $208.44 |
| claude-opus-4-8 | 227 | $53.15 | $60.52 |

Imported keeper history still contains exactly **192,716 calls**. Its estimated
total increased from **$14,220.05819702** to **$24,304.73959667**. **190 calls**
remain unpriced: 173 with missing usage and seventeen unsupported models.
Across the selected thirty-day production view, Opus 5.5 now displays
**$9,017.16** instead of $18.21, with 45 calls unpriced. Fable displays
**$524.10**, with sixteen unpriced. These totals include their previously
priced calls as well as the new historical estimates.

## Verification

All **330 Rust tests**, formatting, and clippy with warnings denied passed.
New tests cover read-only preview, integer cost bounds, evidence preservation,
unchanged existing prices, native exclusion, batch rollback, idempotency, and
concurrent native recording. The estimator forces an ordered primary-key
range, avoiding repeated source-index scans/sorts and keeping writer ownership
bounded to each hundred-record batch.

A consistent private SQLite backup holds **196,564 pre-operation observations**
and was integrity-checked and hashed. A full production-copy trial independently
verified both lifetime costs for all eligible rows, original snapshot digests,
raw evidence, existing prices, and canonical/source accounting entries. A
second application updated zero rows. The live operation repeated these
checks, preserved **196,710 observations' original evidence** and all
**107,174 unaffected rows**, including **106,644 previously priced snapshots**,
and verified the history dashboard against SQL. Existing pricing snapshots
remained byte-for-byte unchanged. The live repeat also updated zero records.

Production continues serving `0.3.2+gui.3697a4b98262`. The separately staged
CLI helper `0.3.2+history.a464acdc0f8a` updated the usage database while native
recording continued. The backend PID, persistent gate, five account identities,
configuration and routing settings were verified unchanged. The operation unit
completed successfully, with zero dropped/rejected usage, writer errors or
unclosed writer sessions. The relevant development service on `thebeast` was
rebuilt, restarted and health-checked.

The production record API exposes the assumption and both lifetime scenarios;
the existing UI renderer displays the explicit assumption and partial label,
while reported lifetime fields still say Unknown. Public HTTPS assets and
management authentication passed verification. Eight-request median dashboard
times were **22.6 ms today**, **98.3 ms for seven days**, and **721.5 ms for
thirty days**; another records page took **21.3 ms**.

The sanitized [production receipt](keeper-history-estimates-production.json)
records the exact helper/source hashes, backup, trial, live evidence, integer
verification, method, costs and performance. Private artifacts are in
`target/usage-history-estimate-20261007/` and the matching production incoming
directory; the private consistent backup is in
`/var/lib/cliproxy-rust-update/history-estimate-20261007/`. This result and receipt
were added after the helper source snapshot.
