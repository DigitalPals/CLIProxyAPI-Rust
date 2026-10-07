# Production log follow-up

The October 7 audit found stable proxy routing and a 95.9% cached-input share, but the old `cpa-usage-keeper` stopped ingesting on October 2. It expects Redis and `/v0/management`, which Fusebox does not implement. A valid management credential does not make those interfaces compatible.

Use Fusebox's native **Usage** page and durable usage database for new observations. The legacy database remains a historical archive: do not delete it, and do not import arbitrary old rows as new proxy events. The interval between its last ingestion and native capture is a coverage gap unless independent retained evidence can reconstruct it.

## Retiring the obsolete collector

On the production host, run `python3 scripts/retire-legacy-usage-keeper.py` for a read-only check. The script reads the existing management credential internally, verifies TLS, and requires a healthy native writer with a committed observation in the past five minutes. It does not generate an inference request.

With `--apply`, it first takes a private SQLite backup, including committed WAL contents, and verifies its integrity. It then disables and stops only `cpa-usage-keeper.service`. The original database, configuration, executable and historical data remain. A private JSON receipt records the backup digest, record count, native capture health and service state. The backup directory defaults to `/var/lib/cliproxy-rust-update/legacy-usage-backups`; arguments allow other installations to supply their paths and verified HTTPS endpoint.

To reverse retirement, explicitly run `systemctl enable --now cpa-usage-keeper.service` after supplying a compatible source. Re-enabling against Fusebox's HTTPS port as a Redis server will recreate the error loop. Do not rotate credentials to conceal the protocol mismatch.

## Interpreting request outcomes

Client cancellations are separate from provider/service failures. A disconnected client does not prove an intentional user stop: network errors and downstream timeouts also end requests. Missing provider usage is unknown, not evidence of zero consumption. Preserve available partial usage while keeping its completeness visible.

The request journal carries sanitized diagnostic categories, transport, timing and attempt information so a failure remains investigable after the dashboard's 300-request ring rolls over. Do not enable full request-body debugging to diagnose production failures; prompts and credentials do not belong in the journal. Native usage remains the durable accounting record and exposes dropped observations and unclean shutdowns separately.

The observed context sizes warrant a controlled benchmark, not automatic prompt rewriting. See [context efficiency](context-efficiency.md) for comparing explicit caller-provided compacted requests without changing the production request path.
