#!/usr/bin/env bash
# Pull the published image from ghcr.io and install/start the Podman Quadlet units (rootful).
# The image is built by CI (release-image workflow). Override with IMAGE / IMAGE_TAG env.
set -euo pipefail
cd "$(dirname "$0")"
DIR="$(pwd)"

[ -f .env ] || { echo "run ./setup.sh first, then edit .env (DOMAIN, GITHUB_REPO, GITHUB_TOKEN)"; exit 1; }
[ "$(id -u)" -eq 0 ] || { echo "run as root: sudo ./deploy.sh   (rootful Quadlet binds 80/443)"; exit 1; }
command -v podman >/dev/null 2>&1 || { echo "podman is not installed"; exit 1; }
mkdir -p certs state

IMAGE="${IMAGE:-ghcr.io/thingino/thingino-web-builder}"
TAG="${IMAGE_TAG:-latest}"
echo "==> pulling ${IMAGE}:${TAG}"
podman pull "${IMAGE}:${TAG}"

echo "==> installing Quadlet units to /etc/containers/systemd"
install -d /etc/containers/systemd
for f in deploy/quadlet/*.container; do
  sed -e "s|@DIR@|$DIR|g" -e "s|@IMAGE@|${IMAGE}:${TAG}|g" "$f" > "/etc/containers/systemd/$(basename "$f")"
done

echo "==> installing host update units to /etc/systemd/system"
for f in deploy/systemd/*.path deploy/systemd/*.service; do
  sed "s|@DIR@|$DIR|g" "$f" > "/etc/systemd/system/$(basename "$f")"
done

echo "==> (re)starting services"
systemctl daemon-reload
systemctl restart thingino-broker.service
systemctl restart thingino-caddy.service
systemctl enable --now thingino-broker-update.path

DOMAIN="$(sed -n 's/^DOMAIN=//p' .env)"
echo
echo "done -> https://${DOMAIN}    (admin: https://${DOMAIN}/admin.html)"
echo "logs -> journalctl -u thingino-broker -u thingino-caddy -f"
