# Deploying the Thingino web-builder

Runs as two containers: the **broker** (Rust control plane + static UI) and
**Caddy** (TLS termination + reverse proxy). The actual firmware builds run on
GitHub Actions — the VPS only orchestrates, so a tiny box is plenty.

## Prerequisites

- A VPS with **Docker** + **Docker Compose**.
- A **domain** with an A/AAAA record pointing at the VPS, and ports **80 + 443** open.
- A **GitHub token** for the builder repo (see below).

## Three-step deploy

```bash
git clone https://github.com/gtxaspec/thingino-web-builder.git
cd thingino-web-builder

./setup.sh                       # generates ADMIN_TOKEN + ADMIN_TOTP_SECRET, prints a QR
#   -> scan the QR into Google Authenticator
#   -> edit .env: set DOMAIN, GITHUB_REPO, GITHUB_TOKEN

docker compose up -d --build
```

That's it — open `https://<DOMAIN>`. Admin panel at `https://<DOMAIN>/admin.html`
(admin token + 6-digit code). Caddy fetches a Let's Encrypt cert automatically on
first start.

## The GitHub token

Least-privilege **fine-grained PAT**, scoped to **only the builder repo**
(`gtxaspec/thingino-web-builder`):

| Permission | Access | Why |
|---|---|---|
| Contents | Read and write | `repository_dispatch`, create release, upload/delete assets |
| Actions  | Read and write | list runs, cancel run, delete run (log cleanup) |
| Metadata | Read-only | mandatory |

(Resolving thingino's `master` commit is an unauthenticated public read, so the
token needs no access to `themactep/thingino-firmware`.) A **classic PAT** with
`repo` + `workflow` scopes works too, with broader reach.

## TLS

- **Default — automatic.** Just set `DOMAIN`; Caddy provisions and renews a free
  Let's Encrypt cert. Nothing to provide.
- **Bring your own cert** (wildcard, Cloudflare origin cert, internal CA): put
  `cert.pem` + `key.pem` in `./certs`, then uncomment the `tls` line in `Caddyfile`
  and `docker compose up -d`.

## Operating it

- **Update:** `git pull && docker compose up -d --build`
- **Logs:** `docker compose logs -f broker` / `... caddy`
- **Toggle builds / view stats:** the admin panel (kill switch + live metrics).
- **Data:** builds/events/settings live in the `broker-data` volume (SQLite) and
  survive restarts/redeploys; certs live in `caddy-data`.
- **Tuning:** the rate limits, concurrency cap, and retention window are env vars
  in `.env` (see `.env.example`); change and `docker compose up -d`.

## Admin credentials

`ADMIN_TOKEN` and `ADMIN_TOTP_SECRET` are env vars in `.env`, read at broker
startup. Manage them with `./creds.sh`:

```bash
./creds.sh show           # print the current token + TOTP secret (+ QR)
./creds.sh rotate-token   # new admin token, recreate broker, print it
./creds.sh rotate-totp    # new 2FA secret, recreate broker, print a QR to re-enroll
```

By hand: edit `.env`, then `docker compose up -d --force-recreate broker`.
Recreating the broker applies the change and clears all logged-in admin sessions.

## IPv6 & real client IPs

Caddy runs on the **host network**, so it binds the host's IPv4 **and IPv6**
directly and the broker sees the **true client IP** — Docker's bridge would
publish IPv4 only and NAT the source away. IPv6 visitors reach the service and
the per-IP limit buckets them by **/64**, as long as the VPS host itself has a
public IPv6 address and ports 80/443 are free.

Prefer a fully isolated bridge instead of host networking? Put Caddy back on the
bridge with published `80:80`/`443:443`, point `reverse_proxy` at `broker:8080`,
and enable IPv6 on the Docker daemon — `/etc/docker/daemon.json`:
`{"ipv6": true, "ip6tables": true, "userland-proxy": false, "fixed-cidr-v6": "fd00:d0::/48"}`
then restart Docker. `userland-proxy: false` is what preserves the real source IP.

## No-Docker alternative

Prefer bare metal? Build `cargo build --release` in `broker/`, drop the binary +
`web/` + `defconfigs.json` under `/opt/thingino-broker`, install the provided
`thingino-build-broker.service` (systemd), and front it with Caddy or nginx for
TLS. The binary is a singleton (flock) and reads the same env vars.
