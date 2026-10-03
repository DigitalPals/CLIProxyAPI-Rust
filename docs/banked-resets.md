# Banked subscription resets

## Turning it on

Banked resets are off by default, because they rely on unofficial subscription endpoints. Turn them on with **Banked resets** under Config, Connections, or in `config.yaml`:

```yaml
banked-resets: true
```

While off, nothing contacts these endpoints and no journal is created. While on, each Claude and ChatGPT subscription is checked every 30 minutes, and again whenever you open its reset panel or press **Refresh**.

## Using a reset

Overview and Accounts show a compact badge beside the account name for native Codex and Claude OAuth subscriptions that have resets. Accounts with none, or whose reset status can't be read, show no badge; unresolved operations keep a review badge so recovery remains accessible. Click the badge to open a modal with grant expiry, scopes, and current eligibility. Purchased monetary credits are separate.

In the modal, select **Refresh** to fetch current provider usage, including after a reset made outside this proxy. Select **Use 1 reset**, review the account and grant, and confirm. Claude defaults to the provider's recommended usable grant, then the earliest expiry; you can choose another usable grant. Codex selects its grant on the provider side. Applying a reset is always manual. API keys, custom endpoints, and disabled accounts cannot redeem.

Codex manual reset availability follows the upstream management center: an available, unexpired credit permits confirmation even when the usage endpoint reports zero currently applicable credits. That counter does not block manual redemption. The provider decides whether to accept the confirmed request. Claude eligibility continues to follow each grant's provider rules.

The server rechecks identity, eligibility, and the confirmation before dispatch. A confirmation expires after two minutes. Changes in inventory or a reset submitted from another tab invalidate older confirmations. Confirmed success is saved independently of the subsequent usage refresh: a refresh failure does not change a successful redemption into a failure. Quota windows affected by a confirmed reset are invalidated; unrelated model quotas, disabled state, authentication cooldowns, and overload cooldowns are preserved.

## Interrupted requests

A timeout, connection failure, or unknown provider response can mean that a reset was spent. The saved request blocks new spending for that subscription identity until resolved, including after a restart or from a duplicate credential file.

For Claude, **Retry request** manually reuses the saved request and grant IDs within ten minutes of the original submission. The proxy never retries a spending POST automatically or follows its redirects. A refusal on a retry leaves the original uncertain outcome unresolved. Codex has no documented retry guarantee in the upstream management implementation, so ambiguous Codex requests require reconciliation instead.

Use **Check outcome** only after checking the provider account. **Reset was used** or **No reset was used** records the operator's finding without sending another spending request. Refreshing inventory alone does not prove what an interrupted request did, and never releases this safeguard. Another account in the same Claude organization cannot retry or reconcile an operation belonging to a different account.

## Storage and deployment

The journal lives in `<startup-auth-dir>/.banked-resets/`. It contains request IDs, provider identity bindings, selected grant/scope, timestamps, and outcomes; no OAuth tokens. Filenames hash the Codex account or Claude organization identity. Unix directories/files use `0700`/`0600`. Atomic replacement and file synchronization persist the pending operation **before** dispatch. A filesystem lock serializes operations across tabs, duplicate files, and processes sharing the same auth directory. An unreadable journal fails closed.

Keep this directory in auth-volume backups and through binary rollbacks. Do not delete or restore an old journal while requests may be unresolved. Separate installations without a shared auth directory, direct provider clients, and other management tools cannot participate in this locking. The journal retains settled IDs to prevent stale replay; after 10,000 operations or a 16 MiB journal, an operator must review storage rather than having history silently discarded.

## Management API

All routes use the existing management authentication and remote-access policy.

While the feature is off, the two `banked-resets` routes answer 404 and `quota/refresh` only refreshes usage.

- `GET /api/accounts/{id}/banked-resets`: fetch inventory and current saved operation; returns a short-lived `quote` when redemption is available.
- `POST /api/accounts/{id}/quota/refresh`: refresh authoritative usage and reset inventory without consuming a reset. Usage can refresh even if banked-reset metadata is unavailable.
- `POST /api/accounts/{id}/banked-resets`: body `{ "action": "redeem", "request_id": "<quote>", "grant_id": "<Claude grant, or empty for Codex>", "confirmed": true }`.
- Recovery uses the same POST with `action: "retry"`, `"resolve-used"`, or `"resolve-unused"` and the saved operation's `request_id`. The stored grant is always reused for a retry.

The read result contains `checked_at`, `inventory` (available/applicable counts, eligibility, grant details/reasons), `error`, `quote`, `operation`, and `retryable`. A successful HTTP response may carry an unavailable inventory or a refused/unknown operation: inspect the fields. Failed preconditions return 409; unknown accounts return 404. Closing a browser connection does not cancel a dispatched operation.

## Provider contracts

Implementation follows [CLIProxyAPI's management center](https://github.com/router-for-me/Cli-Proxy-API-Management-Center/tree/752e0ee772220ce49aae1221a3f39f23236590d7), particularly `src/features/quota/providers/codex/data.ts`, `src/utils/quota/resetCredits.ts`, `src/services/api/claudeResetGrants.ts`, and `src/features/quota/providers/claude/resetGrantOperations.ts`.

Codex uses `/backend-api/wham/usage`, `/backend-api/wham/rate-limit-reset-credits`, and a spending POST to `/backend-api/wham/rate-limit-reset-credits/consume` with `redeem_request_id`. Claude reads `/api/oauth/usage?cedar_ember=1&skip_spend=1`, verifies account/organization through `/api/oauth/profile`, and posts `program: "cedar_ember"`, `grant_id`, and `request_id` to `/api/organizations/{organization}/reset_rate_limits`. Fixed HTTPS origins and server-side OAuth credentials are used with each account's configured proxy.

These subscription backend endpoints are observed upstream contracts, not stable public API guarantees. Missing or changed metadata disables redemption and preserves local operation history. Tests use local mock providers; no real grant is consumed during development verification.
