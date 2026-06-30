#!/usr/bin/env bash
# One-time setup: scaffold .env and generate admin credentials (token + TOTP).
# Safe to re-run — it keeps any values already set in .env.
set -euo pipefail
cd "$(dirname "$0")"

[ -f .env ] || { cp .env.example .env; echo "created .env from .env.example"; }

set_if_empty() { # key value
  local key="$1" val="$2" cur
  cur="$(sed -n "s/^${key}=//p" .env)"
  if [ -z "$cur" ]; then
    sed -i "s|^${key}=.*|${key}=${val}|" .env
    printf '%s' "$val"
  else
    printf '%s' "$cur"
  fi
}

ADMIN_TOKEN="$(set_if_empty ADMIN_TOKEN "$(head -c 24 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9' | head -c 32)")"
TOTP="$(set_if_empty ADMIN_TOTP_SECRET "$(head -c 20 /dev/urandom | base32 | tr -d '=')")"
URI="otpauth://totp/thingino-builder:admin?secret=${TOTP}&issuer=thingino-builder&algorithm=SHA1&digits=6&period=30"

echo
echo "  admin token : ${ADMIN_TOKEN}"
echo "  TOTP secret : ${TOTP}"
echo
echo "  Enroll this in Google Authenticator:"
if command -v qrencode >/dev/null 2>&1; then qrencode -t ANSIUTF8 "$URI"; else echo "    (install qrencode for a QR, or enter the secret manually)"; echo "    $URI"; fi
echo
echo "Next:"
echo "  1. edit .env  -> DOMAIN, GITHUB_REPO, GITHUB_TOKEN"
echo "  2. docker compose up -d --build"
