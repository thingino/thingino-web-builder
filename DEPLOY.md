# Deploying the Thingino web-builder (Podman)

Two containers managed by **systemd via Podman Quadlet** — no daemon:

- **broker** — the Rust control plane + static UI (this image).
- **caddy** — TLS termination + reverse proxy, on the **host network** so it binds
  80/443 directly and sees real client IPs (v4 & v6).

The actual firmware builds run on GitHub Actions, so a tiny VPS is plenty.

## Prerequisites

- A VPS with **Podman** (5+ recommended) and **systemd**.
- A **domain** with an A/AAAA record at the VPS; ports **80 + 443** open.
- A **GitHub token** for the builder repo (see below).

## Deploy

```bash
sudo git clone https://github.com/thingino/thingino-web-builder.git /opt/thingino-web-builder
cd /opt/thingino-web-builder

sudo ./setup.sh                  # generates ADMIN_TOKEN + ADMIN_TOTP_SECRET, prints a QR
#   -> scan the QR into Google Authenticator
#   -> edit .env: set DOMAIN, GITHUB_REPO, GITHUB_TOKEN

sudo ./deploy.sh                 # pulls the ghcr image, installs Quadlet units, starts services
```

Open `https://<DOMAIN>` — admin at `/admin.html` (token + 6-digit code). Caddy
fetches a Let's Encrypt cert automatically on first start.

The broker image is **built by CI and published to `ghcr.io/thingino/thingino-web-builder`**;
`deploy.sh` pulls it (no toolchain needed on the box). A **release** is cut by pushing
a `v*` tag — the workflow publishes `:vX.Y.Z` + `:latest` + a GitHub Release. Pin a
specific version with `IMAGE_TAG=v1.2.0 sudo ./deploy.sh` (default `latest`).

> Clone to a stable path like `/opt/thingino-web-builder`: the Quadlet units
> reference this directory for `.env`, the `Caddyfile`, and `./certs`.

## The GitHub token

Least-privilege **fine-grained PAT**, scoped to **only the builder repo**:

| Permission | Access | Why |
|---|---|---|
| Contents | Read and write | `repository_dispatch`, create release, upload/delete assets |
| Actions  | Read and write | list runs, cancel run, delete run (log cleanup) |
| Metadata | Read-only | mandatory |

Resolving thingino's commit is an unauthenticated public read, so the token needs
no access to `themactep/thingino-firmware`. A classic PAT with `repo` + `workflow`
also works.

**GitHub App (preferred long-term).** Instead of a token, install a GitHub App on
the repo (Contents R/W + Actions R/W) and set `GITHUB_APP_ID`,
`GITHUB_APP_INSTALLATION_ID`, and `GITHUB_APP_KEY_PATH` (the App's `.pem`) in `.env`
— the broker mints short-lived installation tokens itself, so nothing long-lived
sits on the box and there's no annual expiry. Mount the key by uncommenting the
`Volume` line in `deploy/quadlet/thingino-broker.container` and using
`GITHUB_APP_KEY_PATH=/app/app-key.pem`.

## TLS

- **Automatic (default):** set `DOMAIN`; Caddy provisions + renews a free Let's
  Encrypt cert. Nothing to provide.
- **Bring your own:** put `cert.pem` + `key.pem` in `./certs`, uncomment the `tls`
  line in `Caddyfile`, then `sudo ./deploy.sh`.

## IPv6 & real client IPs

Caddy runs on the host network, so it binds the host's IPv4 **and IPv6** and the
broker sees the **true client IP** — the per-IP limit buckets IPv6 by /64, IPv4 by
/32. Just make sure the VPS host has a public IPv6 address.

Rootless instead of rootful? It works, but privileged ports need one of:
`sysctl net.ipv4.ip_unprivileged_port_start=80`, or add
`AmbientCapabilities=CAP_NET_BIND_SERVICE` to the Caddy unit. Rootful avoids both.

## Operating it

- **Update:** `sudo podman auto-update` pulls a newer published image and restarts
  (the unit carries `AutoUpdate=registry`); or re-run `sudo ./deploy.sh`. The admin
  panel also surfaces "update available" and an **Update** button that does this.
- **Logs:** `journalctl -u thingino-broker -u thingino-caddy -f`
- **Restart:** `systemctl restart thingino-broker thingino-caddy`
- **Admin creds:** `./creds.sh show | rotate-token | rotate-totp` (edits `.env`,
  restarts the broker, which also clears logged-in admin sessions).
- **Limits / retention / concurrency:** env vars in `.env` (see `.env.example`) —
  per-user **2/hr**, per-IP **3/hr**, global **20/hr**, **6** concurrent, 30-min
  retention. Change and `sudo ./deploy.sh`.
- **Data:** builds/events/settings persist in the `thingino-broker-data` volume
  (SQLite); certs in `thingino-caddy-data`. Both survive restarts/redeploys.

## Bare-binary alternative (no containers)

`cargo build --release` in `broker/`, drop the binary + `web/` + `defconfigs.json`
under `/opt/thingino-broker`, install `thingino-build-broker.service` (systemd),
and front it with Caddy/nginx. The binary is a singleton (flock) and reads the
same env vars.
