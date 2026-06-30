#!/usr/bin/env bash
# Manage admin credentials for the Podman/Quadlet deployment. They live in .env and
# are read by the broker at startup, so a change means recreating the broker
# container (which also clears all in-memory admin sessions).
#
#   ./creds.sh show           print the current admin token + TOTP secret (+ QR)
#   ./creds.sh rotate-token   generate a new admin token, apply it, print it
#   ./creds.sh rotate-totp    generate a new TOTP secret, apply it, print a QR
set -euo pipefail
cd "$(dirname "$0")"
[ -f .env ] || { echo "no .env here — run ./setup.sh first"; exit 1; }

get()     { sed -n "s/^$1=//p" .env; }
set_kv()  { sed -i "s|^$1=.*|$1=$2|" .env; }
uri()     { echo "otpauth://totp/thingino-builder:admin?secret=$1&issuer=thingino-builder&algorithm=SHA1&digits=6&period=30"; }
qr()      { command -v qrencode >/dev/null 2>&1 && qrencode -t ANSIUTF8 "$1" || echo "  $1"; }
apply()   {
  if systemctl cat thingino-broker.service >/dev/null 2>&1 && [ "$(id -u)" -eq 0 ]; then
    systemctl restart thingino-broker.service \
      && echo "broker restarted — change applied, existing admin sessions invalidated."
  else
    echo "note: restart the broker to apply  ->  sudo systemctl restart thingino-broker"
  fi
}

case "${1:-show}" in
  show)
    echo "admin token : $(get ADMIN_TOKEN)"
    echo "TOTP secret : $(get ADMIN_TOTP_SECRET)"
    echo "enroll:"; qr "$(uri "$(get ADMIN_TOTP_SECRET)")"
    ;;
  rotate-token)
    NEW=$(head -c 24 /dev/urandom | base64 | tr -dc 'a-zA-Z0-9' | head -c 32)
    set_kv ADMIN_TOKEN "$NEW"; echo "new admin token: $NEW"; apply
    ;;
  rotate-totp)
    NEW=$(head -c 20 /dev/urandom | base32 | tr -d '=')
    set_kv ADMIN_TOTP_SECRET "$NEW"; echo "new TOTP secret: $NEW"
    echo "re-enroll in Google Authenticator:"; qr "$(uri "$NEW")"; apply
    ;;
  *) echo "usage: $0 {show|rotate-token|rotate-totp}"; exit 1 ;;
esac
