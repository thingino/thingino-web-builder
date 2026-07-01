# thingino-web-builder

A public, rate-limited **web firmware builder** for
[thingino](https://github.com/themactep/thingino-firmware). Pick a camera
defconfig in the browser, submit, and it builds on **GitHub Actions**, no build
compute on the server. A small broker orchestrates (run it as a Rust service on a
VPS **or** as a Cloudflare Worker with no server at all) and the heavy lifting is
done by the CI.

## How it works

```
browser ──POST /api/build──▶ Rust broker ──repository_dispatch──▶ GitHub Actions
  ▲  pick a defconfig          rate-limit · queue · dedup ·         build thingino@<commit>
  │  poll status               pin commit · hold the token          upload <build_id>.bin
  └───────────── download ◀── rolling `web-builds` pre-release ◀─────┘   (anonymous CDN)
```

- The broker **never builds**: it validates the request, enforces limits, mints a
  `build_id`, pins the chosen branch's current commit, and fires a `repository_dispatch`.
- The workflow checks out that commit, runs `make fast`, and publishes
  `<build_id>.bin` to a rolling pre-release for anonymous download.
- Finished images are downloadable for **30 minutes**, then a reaper deletes the
  release asset **and** the Actions run (logs included).

## Features

- **Defconfig picker** over every thingino camera profile (`cameras` +
  `cameras-exp`), fetched live at the pinned commit; shows the exact commit built. A
  **Settings** panel switches the thingino **branch** (`master` / `ciao` / `stable`),
  and the camera list plus the `<branch>@<hash>` commit badge track the choice.
- **Dedup**: an identical `(defconfig, commit)` that's in flight or built within
  the window is reused, not rebuilt.
- **Limits**: per-user **2/hr**, per-IP **3/hr** (IPv6 bucketed by /64), global
  **20/hr**, and **6** concurrent with a FIFO queue.
- **Live status**: queue position, build progress, cancel (persisted
  "cancelling" state until the run stops), 30-minute download window.
- **Admin panel** (`/admin.html`): live stats, recent builds/events with
  requester uid + full **IP** (click → /64 bucket), a global **kill switch**,
  **live limit editing** (with usage), per-build **cancel / remove**, **clear logs
  / finished builds**, **named admin accounts** (invite-link self-enrollment,
  PBKDF2 passwords, per-user TOTP) with a **master break-glass** token, and an
  audit log of *who* did what, all behind **single-use 2FA**. Sensitive actions
  (kill switch, limit editing, clearing logs/builds, reset) are **privilege-gated**
  per admin (new admins start with **none**, the master grants each) and the
  Pages UI ships a strict **CSP** (`script-src 'self'`) as an XSS backstop. Both
  deploy paths carry the full set; the **VPS** broker additionally offers one-click
  container **self-update** (the Worker deploys via `git push`).
- **GitHub auth** via static token **or a GitHub App** (both paths), the broker
  mints its own short-lived installation tokens, so builds are attributed to the
  app/bot and nothing long-lived sits on the box.
- **Audit log**, **IPv6** end to end, **singleton** broker (flock + pidfile),
  self-hosted frontend assets (no CDN), an **8-language UI** (auto-detected, CSP-safe),
  and a **network-first service worker** so a redeploy lands on a normal reload.

## Layout

| Path | What |
|---|---|
| `broker/` | Rust control plane + scheduler (axum + SQLite), the VPS path |
| `worker/` | **no-VPS** alternative: the same control plane as a Cloudflare Worker + D1 |
| `web/` | static UI (Bootstrap; self-hosted assets in `web/vendor/`), shared by both |
| `.github/workflows/build.yml` | the CI build worker (`repository_dispatch`) |
| `.github/workflows/release.yml` | builds + publishes the broker image to `ghcr.io` |
| `.github/workflows/deploy-worker.yml`, `pages.yml` | deploy the Worker + UI (no-VPS path) |
| `Containerfile`, `deploy/`, `deploy.sh`, `setup.sh`, `creds.sh` | Podman/Quadlet deploy (VPS path) |
| `DEPLOY.md`, `worker/README.md` | deployment guides: **VPS** · **no-VPS** |

## Deploy

Two ways to run it: **same build pipeline, same UI, and the same full admin feature
set** on both (named accounts, live limit editing, per-build cancel/remove, …). Pick
by where you want the control plane to live:

**A. No server ($0):** Cloudflare Worker (API + D1 + cron) + GitHub Pages (UI),
both free tier. Guide → **[worker/README.md](worker/README.md)**.

**B. VPS:** the Rust broker on Podman + Quadlet (systemd); TLS via Caddy (auto
Let's Encrypt or BYO certs). Guide → **[DEPLOY.md](DEPLOY.md)** (short version):

```bash
sudo git clone https://github.com/thingino/thingino-web-builder.git /opt/thingino-web-builder
cd /opt/thingino-web-builder
sudo ./setup.sh          # generate admin token + TOTP (prints a QR)
# edit .env: DOMAIN, GITHUB_REPO, and a GITHUB_TOKEN (or a GitHub App)
sudo ./deploy.sh         # pull the ghcr image, install Quadlet units, start
```

The broker image is built by CI and published to
`ghcr.io/thingino/thingino-web-builder`, so the box just pulls it, no toolchain
needed. The admin panel offers a one-click **self-update** when a newer image ships.

## Local dev

```bash
cd broker && cargo build
GITHUB_TOKEN=$(gh auth token) GITHUB_REPO=<owner>/<repo> \
  ADMIN_TOKEN=secret ADMIN_TOTP_SECRET=$(head -c 20 /dev/urandom | base32 | tr -d =) \
  DEFCONFIGS_PATH=../defconfigs.json STATIC_DIR=../web \
  ./target/debug/thingino-build-broker      # serves http://[::]:8080
```

`defconfigs.json` is a **baked fallback** allowlist; at runtime the broker fetches
the live list (`configs/cameras` + `configs/cameras-exp`) from GitHub at the pinned
commit, so new boards appear without redeploying or regenerating it.
