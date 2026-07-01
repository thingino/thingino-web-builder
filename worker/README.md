# Cloudflare Worker broker (no-VPS, free tier)

A drop-in alternative to the VPS Rust broker (`../broker`), same GitHub Actions
build pipeline and rolling-release downloads, but the control plane runs as a
**Cloudflare Worker** ($0): state in **D1** (SQLite), background reconciliation on a
**1-minute Cron Trigger**, and the UI on **GitHub Pages**.

```
GitHub Pages (UI) ──fetch (CORS)──▶ Worker (API) ──▶ D1 + Cron ──▶ GitHub Actions
```

Builds **dispatch inline** (sub-second, the moment you submit) and **cancel inline**
too; the cron only reconciles run status and runs the retention reaper.

## What it does

Server-issued identity; **user-selectable thingino branch** (`master`/`ciao`/`stable`,
allow-listed, resolved + cached per branch); per-user / per-IP (/64, via
`CF-Connecting-IP`) / global hourly limits; FIFO queue + concurrency cap;
`(defconfig, commit)` dedup;
`repository_dispatch`; run correlation; cancel; retention reaper + DB pruning;
audit events; and a full **admin panel** (named admins + master break-glass, all
2FA-enforced with **single-use** codes; sensitive actions (kill switch, live limit
editing, clear logs/builds, reset limits) are **privilege-gated** per admin, while
per-build cancel/remove + live stats stay open), sessions in D1. See
**[Admin](#admin)** below.

**GitHub auth** is dual-mode: if a **GitHub App** is configured (`GITHUB_APP_ID` +
`GITHUB_APP_INSTALLATION_ID` vars + `GITHUB_APP_PRIVATE_KEY` secret), the Worker
mints short-lived installation tokens (Web Crypto RS256 JWT → installation token,
cached ~1 h in D1) and builds are attributed to the **App/bot**, not a personal
PAT. Otherwise it falls back to a static `GITHUB_TOKEN` PAT.

## Branch selection

The builder page carries a **Settings** gear that picks which thingino branch to build
from: **master** (the default), **ciao**, or **stable**. The choice travels through the
API as a `ref`: `GET /api/defconfigs?ref=<branch>`, `GET /api/stats?ref=<branch>`, and
the `POST /api/build` body `{defconfig, ref}`. Both the Worker and the Rust broker
validate `ref` against a fixed allow-list (`master` / `ciao` / `stable`); anything else
quietly falls back to `master`, so a caller can never coax an arbitrary-ref fetch out of
GitHub. Each branch resolves its own thingino commit and camera (defconfig) list, cached
per branch, and the cron scheduler keeps all three warm. The camera list, the
`<branch>@<hash>` commit badge (it links to that commit on GitHub), and the dispatched
build all follow the selection. `build.yml` needs no change: it already checks out the
commit it is handed.

## Deploy

### One-time setup

```bash
npm i -g wrangler && wrangler login        # or export CLOUDFLARE_API_TOKEN

# 1. Create D1, paste the printed database_id into wrangler.toml
wrangler d1 create thingino-builder

# 2. Apply the schema (builds / events / settings / sessions / admins)
wrangler d1 execute thingino-builder --remote --file schema.sql

# 3. Secrets (Cloudflare-side; persist across deploys, never in the repo)
#    GitHub auth, pick one:
#    (a) GitHub App (runs show as the bot): set GITHUB_APP_ID + GITHUB_APP_INSTALLATION_ID
#        as [vars] in wrangler.toml, then the private key as a secret. Convert the
#        downloaded PKCS#1 key first:  openssl pkcs8 -topk8 -nocrypt -in app.pem -out app-pkcs8.pem
wrangler secret put GITHUB_APP_PRIVATE_KEY < app-pkcs8.pem
#    (b) or a PAT (runs show as you):
wrangler secret put GITHUB_TOKEN           # PAT: Contents R/W + Actions R/W
wrangler secret put ADMIN_TOKEN            # master break-glass password
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

CI stamps the build via `--var VERSION:"v0.5.0-<sha>"`, shown in the footer and
`/api/stats`.

## UI on GitHub Pages

`.github/workflows/pages.yml` publishes `../web` to GitHub Pages on every push to
`web/`. It writes `web/config.js` with the Worker URL (`window.API_BASE`) so the
Pages site calls the Worker cross-origin. CORS is `*`; identity is header-based
(`X-Builder-Uid` + a localStorage mirror), so no cookies are needed cross-origin.
Assets use relative paths, so it works both at the project URL
(`<org>.github.io/<repo>/`) and at a custom domain. The page also ships a strict
**Content-Security-Policy** (`script-src 'self'`, `worker-src 'self'`, etc.) as an XSS
backstop, with all
scripts external (`app.js` / `admin.js`) since Pages can't supply CSP nonces.

The UI (builder and admin) is **localized into 8 languages**: English, Spanish, French,
German, Simplified Chinese, Portuguese, Russian, and Japanese. It auto-detects from
`navigator.language` (with a fallback chain down to English), offers a language picker in
the header, and remembers a manual override in `localStorage`. The runtime is CSP-safe
(no eval, no external fetch), with one small dictionary file per language.

GitHub Pages serves everything with `cache-control: max-age=600` and ignores the
`no-store` `<meta>` tags, so a redeploy could otherwise sit stale for ~10 minutes. A
**network-first service worker** (`sw.js`, registered by `sw-register.js`) closes that
gap, so a normal reload picks up the new build; this is why the CSP carries
`worker-src 'self'`.

Two smaller touches: an opt-in **balloon-help** mode (a `?` toggle in the header plus a
"show help hints" switch in Settings float hover tips over the controls, ported from
thingino-dfu but CSP-clean, all `addEventListener`, no inline handlers), and a static
English **privacy policy** (`privacy.html`) linked as "privacy" in the footer (its link
label localized in all 8 languages). The policy states that the IP is logged only for
rate-limiting and abuse, describes the anonymous builder ID and the public build images
(GitHub release assets, ~30 minutes then deleted), names the processors (GitHub Pages,
Cloudflare Workers, GitHub Actions) and the brief retention, and confirms no cookies,
accounts, or trackers.

**Custom domain** (the `webflash.thingino.com` model, no Cloudflare DNS required):
add a `CNAME` DNS record at your registrar (`web-builder.thingino.com → <org>.github.io`)
and uncomment the `CNAME` line in `pages.yml`.

## Admin

`<site>/admin.html`. Two ways in:

- **Named admins**: username + password + their **own** TOTP (all enforced). The
  password is PBKDF2-SHA256 (salted, in D1); 2FA is mandatory.
- **Master token** (`ADMIN_TOKEN` + `ADMIN_TOTP_SECRET`, the Worker secrets): a
  **break-glass** login. It's not in D1, so it always works even if the admins
  table is wiped or you lock yourself out, and it's the only login that can manage
  users. Click *"Use master token instead"* on the sign-in screen.

The admin session token lives in **`localStorage`** (not `sessionStorage`), so switching
apps on a phone won't drop the login; it stays bounded by the 8-hour server session and
the **Sign out** button.

**Open to any admin**: cancel any build, remove a finished build's artifact + run
early, live stats / recent builds + events.

**Privilege-gated**: the **kill switch** (enable/disable builds), edit **limits**
(live, no redeploy, usage shown), **clear logs**, **clear finished builds**, **reset
limits**: a named admin can run each only if granted that privilege (new admins
start with **none**); the master always can, and grants them via checkboxes in the
Users card.

**User management** (master only): invite a username → you get a one-time link
(60 min). The new admin opens it, scans the **QR** into their authenticator, sets
their own password, and is enrolled (the master never sees their password). If the
one-time link is lost before enrollment, click the **"invited"** state in the Users
card to re-show it (copy button + expiry) instead of deleting and re-inviting. The
master also grants each admin's **privileges** via the Users-card checkboxes. Admins
can be listed and removed (removal also kills their sessions). Sessions carry the
identity, so the audit log records **who** did each action.

(The "Update" button is hidden on the Worker: there's nothing to self-update; a
deploy is just `git push`.)

## Local smoke test

```bash
wrangler dev          # needs Node 22+; uses a local D1
curl -s localhost:8787/api/health        # -> ok
curl -s localhost:8787/api/defconfigs | head
```

## Trade-offs vs the VPS

- **Starting and cancelling a build are instant**: the click hits GitHub inline.
  The 1-min cron only does passive sync: *noticing* a build has **finished** (→ mark
  it done, show the download) and dispatching a **queued** build once a slot frees.
  Each of those can lag ≤1 min, invisible against a 20–40 min build. (A Durable
  Object alarm could do it faster if ever needed.)
- The build-count caps are enforced by a single atomic `INSERT … SELECT WHERE
  count < cap`, so a concurrent burst can't slip past them; a per-IP **request**
  flood (in front of the D1 work) is capped by a **Durable Object** limiter
  (strongly consistent, free-tier: the Workers Rate Limiting binding is a verified
  no-op on free, so it's not used).
- No container to self-update: "update" is `git push` (CI runs `wrangler deploy`).
