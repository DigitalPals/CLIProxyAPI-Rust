# Notifications and the installed app

The dashboard can be installed as an app and send push notifications when something trips, so you hear about an expired sign-in or a provider that has run out without keeping a tab open.

## What you need

- **HTTPS**, or the dashboard on `localhost`. Browsers only allow service workers and push in a secure context. On a server, use Fusebox's own `tls` setting or a proxy such as `tailscale serve`, nginx or Caddy; see [HTTPS with Tailscale, nginx or Caddy](../README.md#https-with-tailscale-nginx-or-caddy). A self-signed certificate only works on devices that trust it.
- **A `management-key`** whenever a proxy is in front of Fusebox.
- **On iPhone and iPad** (iOS 16.4 or later), the dashboard added to the Home Screen: in Safari tap **Share**, then **Add to Home Screen**, and open Fusebox from its icon. Safari tabs can't receive push notifications on iOS. The installed app keeps its own storage, so it asks for the management key once.
- **Elsewhere**, any current Chrome, Edge, Firefox or Safari. Installing is optional on Android and desktops, but the app gets its own window and a fault count on its icon.

## Turning them on

Open **Config, Notifications** on the device that should get them and click **Turn on**, then allow notifications when the browser asks. **Send test** checks the whole path. Each device is turned on separately; the section lists the others, and **Remove** stops notifications to one of them.

The events are shared by every device and saved in `config.yaml`:

```yaml
notifications:
  sign-in-expired: true      # an account needs signing in again
  provider-exhausted: true   # every account of a provider is out, and when one is back
  account-used-up: false     # one account used up its 5-hour or weekly limit
  account-errors: false      # account errors, and three or more failed requests in an hour
```

## What is sent, and when

Fusebox checks every 30 seconds, using the rules behind the dashboard's faults button. A fault must still be there at the next check before it is sent, so a blip doesn't notify, and each fault is sent once until it clears. Faults that were already there when notifications were turned on aren't sent. Restarting Fusebox doesn't send anything again.

- **Sign-in expired:** "Claude sign-in expired", with the account. Opens its page.
- **Provider out of capacity:** "Every Claude account is used up" with when the first one is back, or "No Claude account can take requests" when sign-ins have expired too. When a provider has run out, its accounts' own limit notifications are left out. "Claude is back" follows when an account can take requests again.
- **Account limit used up:** "work@example.com: weekly limit used up · back in 3d 7h".
- **Account errors:** the account's error, or "3 failed requests" in the last hour.

Rate limits are never sent; they come and go too quickly. The app's icon shows the number of faults the dashboard shows.

## Reading faults from other tools

Desktop widgets and scripts can read the same faults through the management API, with the management key as a bearer token, like every other `/api` route:

- `GET /api/faults` lists them, errors first.
- `/api/live` sends a `faults` event with the whole list as soon as a socket connects, then again whenever the list changes. The dashboard's `load` events work the same way.

```json
{
  "key": "quota:file:claude-work.json",
  "kind": "quota",
  "level": "warn",
  "provider": "claude",
  "provider_name": "Claude",
  "account_id": "file:claude-work.json",
  "label": "work@example.com",
  "title": "Weekly limit used up",
  "detail": null,
  "until": "2026-10-12T09:00:00+00:00",
  "path": "#/accounts/file%3Aclaude-work.json"
}
```

The same socket's `load` event says what each busy account is doing: `in_flight` requests, `sessions` seen in the last five minutes (the dashboard's live sessions, and what smart-quota routing weighs), and `ongoing_sessions` seen in the last 30 minutes, which still counts a session waiting on its user or a long tool run. Each counts calls in flight, and no window outlasts `session-affinity-idle-seconds`.

`kind` is `signin`, `quota`, `rate_limit`, `error`, `failures` or `provider`. `level` is `err` while requests fail and `warn` while an account sits out for a while. `title` and `detail` never contain a countdown: `until` is when the fault should clear by itself, so a client counts down on its own and a fault only changes when something does. A whole provider (`kind: "provider"`) has no `account_id` or `label`. `path` is where the fault opens in the dashboard. Unlike notifications, the list includes rate limits and doesn't wait for a second check.

## Privacy

Notifications go from Fusebox to your browser's push service (Google for Chrome and Edge, Mozilla for Firefox, Apple for Safari), which delivers them to the device. The message is end-to-end encrypted ([RFC 8291](https://www.rfc-editor.org/rfc/rfc8291)); the push service sees only when a message is sent and how large it is. Nothing is sent unless a device has turned notifications on.

Fusebox keeps its push key (VAPID), the subscribed devices and the faults it has sent in `.web-push.state` in the credentials directory (`auth-dir`), readable only by its owner. Deleting the file signs every device out of notifications; they turn them on again from the dashboard.

## Troubleshooting

- **"Notifications need HTTPS"**: open the dashboard over HTTPS, not `http://<ip>:8317`.
- **"Add Fusebox to your Home Screen first"**: on iOS, push only works in the installed app.
- **"Notifications are blocked for this site"**: allow them in the browser's site settings (on iOS: Settings, Notifications, Fusebox), then reload.
- **The test arrives but faults don't**: check that the event is switched on, and that the fault is still there after 30 seconds. Fusebox logs each notification it sends (`push sent`) and each one a push service refuses.
- **A device stopped getting them**: browsers sometimes replace a subscription; the push service then tells Fusebox the old one is gone and it is removed. Turn notifications on again on that device.
- **A proxy in front, and the dashboard says it is local-only**: set `management-key`; Fusebox no longer treats proxied requests as coming from `localhost`.
