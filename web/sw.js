/*
 * Minimal service worker: keep the page shell + fixed-name scripts fresh on a normal
 * reload, so a redeploy is picked up without a hard-reload. GitHub Pages serves
 * everything with cache-control: max-age=600, which otherwise sticks for ~10 minutes
 * and makes the no-store <meta> tags ineffective.
 *
 *  - Page navigations (index.html / admin.html): network-first (cache: 'reload'), so
 *    new HTML lands at once.
 *  - Same-origin .js / .css (app.js, admin.js, config.js, i18n*.js, vendor/*): these are
 *    fixed-name (not content-hashed), so a normal reload would keep a stale copy.
 *    Revalidate them (cache: 'no-cache' => a conditional GET: a cheap 304 when
 *    unchanged, a fresh fetch when changed).
 *
 * Cross-origin requests (the Worker API on *.workers.dev) are left untouched — they
 * pass straight through. The worker caches nothing itself, so it can't get stuck on a
 * stale page. To retire it, deploy a sw.js whose fetch handler is empty.
 */
self.addEventListener("install", () => self.skipWaiting());
self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));
self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.mode === "navigate") {
    event.respondWith(fetch(req, { cache: "reload" }).catch(() => fetch(req)));
    return;
  }
  const url = new URL(req.url);
  if (url.origin === self.location.origin && /\.(?:js|css)$/.test(url.pathname)) {
    event.respondWith(fetch(req, { cache: "no-cache" }).catch(() => fetch(req)));
  }
});
