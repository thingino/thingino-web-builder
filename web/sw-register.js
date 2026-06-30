// Register the network-first service worker (sw.js) so a redeploy lands on a normal
// reload — GitHub Pages caches the HTML + fixed-name assets for ~10 min and ignores the
// no-store <meta> tags. In its own file (not inline) to satisfy the strict CSP.
if ("serviceWorker" in navigator) {
  window.addEventListener("load", function () {
    navigator.serviceWorker.register("sw.js").catch(function () {});
  });
}
