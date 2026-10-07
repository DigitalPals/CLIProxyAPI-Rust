'use strict';
// Fusebox service worker, served from /sw.js so its scope is the dashboard at /.
// It caches nothing, so an upgraded binary is never shadowed by an old dashboard. It
// shows a page of its own when the server can't be reached, shows push notifications
// and opens the dashboard when one is clicked. Only the dashboard page itself is
// handled: API calls, proxy traffic and other paths on the same origin pass by.

self.addEventListener('install', () => self.skipWaiting());
self.addEventListener('activate', (event) => event.waitUntil(self.clients.claim()));

self.addEventListener('fetch', (event) => {
  const req = event.request;
  if (req.mode !== 'navigate' || new URL(req.url).pathname !== '/') return;
  event.respondWith(fetch(req).catch(offline));
});

const MARK = '<svg viewBox="0 0 48 56" width="34" height="40" aria-hidden="true"><path fill="#F3B52F" d="M0 0H16V8H8V48H16V56H0ZM48 0H32V8H40V48H32V56H48Z"/><path fill="#f2efe8" d="M16 24H32V32H16Z"/></svg>';

// System fonts on purpose: nothing is cached, so the dashboard's fonts may be out of reach.
function offline() {
  const host = self.location.host.replace(/[<>&"]/g, '');
  const html = `<!doctype html><html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, viewport-fit=cover">
<meta name="color-scheme" content="dark"><meta name="theme-color" content="#0b0b0a"><title>Fusebox</title>
<style>
html,body{height:100%;margin:0;background:#0b0b0a;color:#f2efe8;font:15px/1.5 system-ui,-apple-system,"Segoe UI",sans-serif}
main{min-height:100%;display:flex;flex-direction:column;align-items:center;justify-content:center;gap:14px;padding:24px;text-align:center;box-sizing:border-box}
h1{margin:6px 0 0;font-size:19px;font-weight:600}p{margin:0;max-width:34ch;color:#b4afa4}b{color:#f2efe8;font-weight:500}
button{margin-top:6px;height:44px;padding:0 20px;font:inherit;color:#0b0b0a;background:#f2efe8;border:0;border-radius:6px}
button:focus-visible{outline:2px solid #4a4740;outline-offset:2px}
</style></head><body><main>${MARK}<h1>Can’t reach Fusebox</h1>
<p><b>${host}</b> isn’t answering. This page reloads by itself once it’s back.</p>
<button type="button" onclick="location.reload()">Try again</button></main>
<script>setInterval(function(){fetch('/healthz',{cache:'no-store'}).then(function(r){if(r.ok)location.reload()}).catch(function(){})},5000)</script>
</body></html>`;
  return new Response(html, { status: 503, headers: { 'content-type': 'text/html; charset=utf-8', 'cache-control': 'no-store' } });
}

// The server sends the Declarative Web Push shape; Safari may show it by itself, and
// showing it here replaces that proposed notification rather than adding a second one.
self.addEventListener('push', (event) => {
  let msg = {};
  try { msg = event.data ? event.data.json() : {}; } catch {}
  const n = msg.notification || {};
  const shown = self.registration.showNotification(n.title || 'Fusebox', {
    body: n.body || '',
    tag: n.tag || undefined,
    renotify: !!n.tag,
    lang: n.lang || 'en',
    icon: '/ui/icons/icon-192.png',
    data: { url: n.navigate || self.registration.scope },
  });
  const count = Number(n.app_badge);
  const badge = 'setAppBadge' in self.navigator && n.app_badge != null && Number.isFinite(count)
    ? (count > 0 ? self.navigator.setAppBadge(count) : self.navigator.clearAppBadge()).catch(() => {})
    : Promise.resolve();
  event.waitUntil(Promise.all([shown, badge]));
});

// Focus an open dashboard and send it to the notification's page, or open one.
self.addEventListener('notificationclick', (event) => {
  event.notification.close();
  const url = (event.notification.data && event.notification.data.url) || self.registration.scope;
  event.waitUntil((async () => {
    const windows = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
    const open = windows.find((c) => new URL(c.url).pathname === '/');
    if (open) {
      await open.focus();
      open.postMessage({ type: 'open', url });
      return;
    }
    await self.clients.openWindow(url);
  })());
});
