# Native imports and standalone collectors

Local history import is opt-in. `usage status` reports configured roots and the current CLI's `CODEX_HOME`/`CLAUDE_CONFIG_DIR` candidates (defaulting to `~/.codex`/`~/.claude`) without reading those histories or provider configuration. An enabled root is explicit consent to read histories below that directory and retain the configured root path for management. Transcript text, tool payloads, cwd/project metadata, filenames, instructions, provider credentials and account files are never retained as observations or sent by collectors. Checkpoints use hashed file identities and bounded usage-only parser state.

Examples (the database must match the service's usage database):

```sh
fusebox usage status --database /srv/fusebox/usage.sqlite3
fusebox usage enable --database /srv/fusebox/usage.sqlite3 --source codex --root "$HOME/.codex/sessions"
fusebox usage enable --database /srv/fusebox/usage.sqlite3 --source claude_code --root "$HOME/.claude/projects"
fusebox usage scan --database /srv/fusebox/usage.sqlite3
fusebox usage disable --database /srv/fusebox/usage.sqlite3 --source codex --root "$HOME/.codex/sessions"
fusebox usage backfill --database /srv/fusebox/usage.sqlite3 --source claude_code
```

The CLI opens existing databases with their saved retention and pricing policy; it does not replace a service's settings with a CLI default. The service poller scans enabled roots every 30 seconds. `backfill` resets checkpoints only for enabled selected roots and explicitly bypasses the purge watermark inside those import transactions. The watermark is restored before each commit, so unrelated automatic imports remain blocked. Configured retention still applies: increasing the retention horizon is necessary to retain older reimported observations beyond a later retention pass. Disabling a root stops future scans and preserves existing observations.

The importer accepts Codex rollout JSONL and Zstd `.jsonl.zst`, plus Claude Code project and nested subagent JSONL. Plain JSONL wins over a compressed sibling. No symlink entries are followed. Unix traversal skips mounted subtrees whose device differs from the consented root; Linux rejects roots on recognized NFS/CIFS/9p/network FUSE mounts. Arbitrary FUSE mount types and Windows mapped-drive classification are not conclusively identified; use local native histories, not network roots. Scans are bounded to 4,096 files/directories, 32 MiB of newly consumed text per root per pass, 8 MiB per file/pass, 256 MiB per logical file, 512 KiB per row, and 2,000 observations per file/pass. Bounds or invalid rows appear in counters and sanitized status errors. A trailing partial line is retried from its starting offset. Prefix fingerprints detect truncation and replacement; observations and checkpoints commit together. Compressed resumes replay the decoded prefix up to the saved offset, bounded by the logical-file limit.

Codex per-response `token_usage_record` entries are preferred. Legacy `token_count` snapshots contribute only cumulative differences, with repeated snapshots ignored and reset/estimated counters reported as incomplete evidence. When exact native evidence supersedes an imported legacy counter, the counter's audit row is retained while its expense entry is suppressed, including when the rows arrive in separate scans. Ambiguous legacy fork histories are skipped as unsupported; current per-response fork records remain supported. Native response IDs deduplicate copied/reverted/archived histories. Explicit unmatched or evicted turn IDs leave the model unknown; the bounded context map never guesses a later model. `session_meta.model_provider` is retained: custom or unknown providers are never upgraded to OpenAI just because a model name resembles an OpenAI model.

Claude imports use assistant `message.id` and separate `requestId` when present, falling back to row UUID for weaker identity. Repeated message snapshots coalesce; later revisions retain audited provenance and the store chooses consistent richer usage. Sidechain/subagent usage is imported, since it can be billable. Parent summaries without assistant usage are skipped. Native account/auth identity is never inferred from current sign-ins. Both formats preserve missing counters as unknown. The documented Codex absent cache-write field defaults to zero for older records; missing cache-read or reasoning fields remain unknown. Claude ordinary input excludes cache reads/writes; Codex inclusive input is split into disjoint categories. Reasoning is already included in output. Claude TTL write subsets are not added to aggregate writes again.

## Enroll and run a collector

Create a collector using the management API or Usage page, protected by existing management authentication. Enrollment/rotation returns a `fbxc_` credential once. The server stores only its SHA-256 hash. Each enrollment has a stable UUID; rotating retains that identity and immediately invalidates the old credential. Revocation also invalidates it. Collector credentials are rejected by management and inference routes, even when anonymous loopback access is otherwise enabled.

Install the returned raw credential on the source machine via a protected file or stdin, never an argument or environment variable:

```sh
chmod 600 /secure/collector-credential
fusebox collector enroll \
  --state-dir "$HOME/.local/state/fusebox-collector" \
  --destination https://fusebox.example.com/api/usage-ingest \
  --credential-file /secure/collector-credential \
  --codex-root "$HOME/.codex/sessions" \
  --claude-root "$HOME/.claude/projects"
fusebox collector run --state-dir "$HOME/.local/state/fusebox-collector" --once
fusebox collector status --state-dir "$HOME/.local/state/fusebox-collector"
fusebox collector sync --state-dir "$HOME/.local/state/fusebox-collector"
fusebox collector run --state-dir "$HOME/.local/state/fusebox-collector"
```

Without `--credential-file`, `enroll` reads the raw credential from stdin. The destination must be exactly an `/api/usage-ingest` endpoint, without userinfo, query keys or fragments. HTTPS uses normal certificate verification and redirects are disabled; plaintext is accepted only for a literal loopback address or `localhost`. `run` and `sync` execute before proxy configuration/App initialization and never start a provider pool or proxy. `sync` delivers queued metadata; `run` additionally scans consented roots. Enrollment with no roots reads no histories. Root selection is additive on reenrollment; existing consented roots remain enabled until disabled through `usage disable --database <state-dir>/outbox.sqlite3 ...`.

The local state UUID, destination and credential are atomically written to `collector.json` with Unix mode 0600 in a 0700 state directory. SQLite DB/WAL/SHM files are 0600. Server enrollment identity is bound by the credential, not this local UUID or a client assertion. Reenrolling an existing state directory preserves its local UUID and pending outbox. Install a rotated credential using `enroll` again and restart a running collector. Creating a fresh server enrollment creates a new server identity; restoring the state directory and using rotation preserves identity across restarts. Keep the state directory private and persistent. CLI/status output never includes the credential.

The outbox is durable SQLite with FULL synchronization, bounded to 10,000 queued metadata/supersession records and 16 MiB of serialized metadata. If full, scanning pauses before its checkpoint advances, so source histories are retryable after delivery frees capacity. Its file is a bounded metadata database plus parser/checkpoint state; SQLite file allocation can retain freed pages. It is not a strict 16 MiB total on-disk file limit. Offline failures retain the outbox, use exponential backoff capped at five minutes, and expose a sanitized error and pending count. Send batches contain at most 200 observations and 512 KiB. Metadata-only counter supersessions are limited to 200 SHA-256 counter identities per batch and can suppress only the authenticated collector's own prior counters. The server caps authenticated requests to 120 per collector per minute.

The server stamps ingestion time and collector origin and discards claimed account/auth/client/logical-request/attempt/status attribution. It rejects proxy-source claims, unsupported fields, invalid categories, timestamp/numeric bounds and mismatched Claude provider assertions. Native custom Codex provider identifiers remain custom. It rechecks the credential inside the commit so rotation/revocation cannot race a successful ingest. A successful acknowledgement is sent only after the observations, supersessions and coverage state commit. Contact time is separate from committed sync time. Retries are idempotent. The local acknowledgement deletes only the exact sent payload, so concurrent scanning or another command cannot delete a newer unsent revision.

## Provenance and limits

These imports are local evidence and API list-price equivalents, not provider invoices, subscription charges, balances or quota percentages. Source-specific totals can overlap proxy and other imported histories; conservative reconciliation uses durable native identity, never token/time similarity. Unknown provider/model/tier/cache TTL/pricing period remains explicit. Pricing verification date and effective date are different facts.

Native implementation evidence was checked on 2026-10-07: official OpenAI [configuration](https://developers.openai.com/codex/config-advanced), [API pricing](https://developers.openai.com/api/docs/pricing), [prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching), and public Codex commit `5a3140176e668a2f72f3c098490eb7f7052d9d85` (`codex-rs/protocol/src/protocol.rs`, `codex-rs/history/src/rollout_payload.rs`, `codex-rs/codex-api/src/sse/responses.rs`); official Anthropic [SDK sessions](https://platform.claude.com/docs/en/agent-sdk/sessions), [prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching), [API pricing](https://platform.claude.com/docs/en/about-claude/pricing), and Python Agent SDK commit `23bb0157f51f83c21a7c47fbed1e5741ad581082` (`_internal/sessions.py`). Vendor main may include features absent from older installed releases. There is no established stable universal complete native Claude billing schema; unknown rows remain visible as skipped/failed rather than guessed.

Synthetic fixtures under `tests/fixtures/usage` deliberately include privacy markers in content/cwd fields. Tests cover opt-in behavior, counters/revisions, modern supersession across checkpoints, partial writes, copies/rotation/Zstd, unknown/custom provider and model attribution, forks, explicit reimport/purge, symlinks, credential rotation/revocation, source/identity spoofing, strict payload bounds, two independent collectors through a real local HTTP server, offline restart, a full outbox, and blocked network acknowledgements racing newer revisions.
