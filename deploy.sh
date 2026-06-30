#!/usr/bin/env bash
# Build the image and install/start the Podman Quadlet units (rootful).
# Re-run after `git pull` to update — it rebuilds and restarts.
set -euo pipefail
cd "$(dirname "$0")"
DIR="$(pwd)"

[ -f .env ] || { echo "run ./setup.sh first, then edit .env (DOMAIN, GITHUB_REPO, GITHUB_TOKEN)"; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "run as root: sudo ./deploy.sh   (rootful Quadlet binds 80/443)"; exit 1; }
command -v podman >/dev/null 2>&1 || { echo "podman is not installed"; exit 1; }
mkdir -p certs

SHA="$(git rev-parse --short HEAD 2>/dev/null || echo dev)"
echo "==> building image (BUILD_SHA=$SHA)"
podman build --build-arg BUILD_SHA="$SHA" -t localhost/thingino-web-builder:latest .

echo "==> installing Quadlet units to /etc/containers/systemd"
install -d /etc/containers/systemd
for f in deploy/quadlet/*.container; do
  sed "s|@DIR@|$DIR|g" "$f" > "/etc/containers/systemd/$(basename "$f")"
done

echo "==> (re)starting services"
systemctl daemon-reload
systemctl restart thingino-broker.service
systemctl restart thingino-caddy.service

DOMAIN="$(sed -n 's/^DOMAIN=//p' .env)"
echo
echo "done -> https://${DOMAIN}    (admin: https://${DOMAIN}/admin.html)"
echo "logs -> journalctl -u thingino-broker -u thingino-caddy -f"
