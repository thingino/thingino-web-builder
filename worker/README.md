# Cloudflare Worker broker (no-VPS, free tier)

A drop-in alternative to the VPS Rust broker (`../broker`) — same GitHub Actions
build pipeline and rolling-release downloads, but the control plane runs as a
**Cloudflare Worker** ($0): state in **D1** (SQLite), background reconciliation on a
**1-minute Cron Trigger**, and the UI on **GitHub Pages**.

```
GitHub Pages (UI) ──fetch (CORS)──▶ Worker (API) ──▶ D1 + Cron ──▶ GitHub Actions
```

Builds **dispatch inline** (sub-second, the moment you submit) and **cancel inline**
too; the cron only reconciles run status and runs the retention reaper.

## What it does

Server-issued identity; per-user / per-IP (/64, via `CF-Connecting-IP`) / global
hourly limits; FIFO queue + concurrency cap; `(defconfig, commit)` dedup;
`repository_dispatch`; run correlation; cancel; retention reaper + DB pruning;
audit events; and a full **admin panel** — TOTP 2FA, kill switch, clear logs, reset
limits, live stats — with sessions in D1.

The one piece not ported from the VPS broker is **GitHub App auth**; the Worker uses
a static `GITHUB_TOKEN` secret (App auth would be Web Crypto RS256 JWT).

## Deploy

### One-time setup

```bash
npm i -g wrangler && wrangler login        # or export CLOUDFLARE_API_TOKEN

# 1. Create D1, paste the printed database_id into wrangler.toml
wrangler d1 create thingino-builder

# 2. Apply the schema (builds / events / settings / sessions)
wrangler d1 execute thingino-builder --remote --file schema.sql

# 3. Secrets (Cloudflare-side; persist across deploys, never in the repo)
wrangler secret put GITHUB_TOKEN           # PAT: Contents R/W + Actions R/W
wrangler secret put ADMIN_TOKEN            # admin password
wrangler secret put ADMIN_TOTP_SECRET      # base32 TOTP seed (enroll in an authenticator)

# 4. Deploy → prints the Worker URL
wrangler deploy
```

### CI deploy (git push = deploy)

`.github/workflows/deploy-worker.yml` runs `wrangler deploy` on every push to
`worker/`. It needs two repo **Actions** secrets:

```bash
gh secret set CLOUDFLARE_API_TOKEN     # Workers Scripts:Edit + D1:Edit
gh secret set CLOUDFLARE_ACCOUNT_ID
```

CI stamps the build via `--var VERSION:"v0.1.0-<sha>"`, shown in the footer and
`/api/stats`.

## UI on GitHub Pages

`.github/workflows/pages.yml` publishes `../web` to GitHub Pages on every push to
`web/`. It writes `web/config.js` with the Worker URL (`window.API_BASE`) so the
Pages site calls the Worker cross-origin. CORS is `*`; identity is header-based
(`X-Builder-Uid` + a localStorage mirror), so no cookies are needed cross-origin.
Assets use relative paths, so it works both at the project URL
(`<org>.github.io/<repo>/`) and at a custom domain.

**Custom domain** (the `webflash.thingino.com` model — no Cloudflare DNS required):
add a `CNAME` DNS record at your registrar (`web-builder.thingino.com → <org>.github.io`)
and uncomment the `CNAME` line in `pages.yml`.

## Admin

`<site>/admin.html`. Two ways in:

- **Named admins** — username + password + their **own** TOTP (all enforced). The
  password is PBKDF2-SHA256 (salted, in D1); 2FA is mandatory.
- **Master token** (`ADMIN_TOKEN` + `ADMIN_TOTP_SECRET`, the Worker secrets) — a
  **break-glass** login. It's not in D1, so it always works even if the admins
  table is wiped or you lock yourself out, and it's the only login that can manage
  users. Click *"Use master token instead"* on the sign-in screen.

**Operational actions** (any admin): enable/disable builds, edit **limits** (live,
no redeploy) with usage shown, **clear logs**, **reset limits**, cancel any build,
remove a finished build's artifact + run early, live stats / recent builds + events.

**User management** (master only): invite a username → you get a one-time link
(60 min). The new admin opens it, scans the **QR** into their authenticator, sets
their own password, and is enrolled — the master never sees their password. Admins
can be listed and removed (removal also kills their sessions). Sessions carry the
identity, so the audit log records **who** did each action.

(The "Update" button is hidden on the Worker — there's nothing to self-update; a
deploy is just `git push`.)

## Local smoke test

```bash
wrangler dev          # needs Node 22+; uses a local D1
curl -s localhost:8787/api/health        # -> ok
curl -s localhost:8787/api/defconfigs | head
```

## Trade-offs vs the VPS

- Dispatch and cancel are **inline (instant)**. Run-completion detection rides the
  1-min cron — negligible against a ~30-min build; a Durable Object alarm could poll
  faster if ever needed.
- Rate-limit checks are count-then-insert on D1 (no single-mutex), so a burst can
  exceed a cap by 1–2. A Durable Object would give strict caps.
- No container to self-update — "update" is `git push` (CI runs `wrangler deploy`).
