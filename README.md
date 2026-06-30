# thingino-web-builder

A public, rate-limited **web firmware builder** for
[thingino](https://github.com/themactep/thingino-firmware). Pick a camera
defconfig in the browser, submit, and GitHub Actions builds it for free — no
build compute on our side.

## Architecture

```
browser ──POST /api/build──▶ Rust broker (cheap VPS) ──repository_dispatch──▶ GitHub Actions
   ▲                          • holds the GitHub token (never shipped)            • clones thingino@master
   │                          • global + per-IP hourly caps (SQLite)              • BOARD=<x> make fast
   └──────── polls GitHub's public releases API ◀── publishes <build_id>.bin ─────┘  on rolling `web-builds` tag
```

- **CI** — `.github/workflows/build.yml`. On `repository_dispatch` (event
  `web-build`, payload `{build_id, defconfig}`) it mirrors the proven
  `firmware-x86_64.yaml` recipe (debian:forky, apt deps, ccache + dl-cache),
  builds one board, and uploads `<build_id>.bin` to the `web-builds`
  pre-release for anonymous CDN download.
- **broker/** *(todo)* — small Rust service on a VPS: validates the defconfig,
  mints a `build_id`, enforces rate limits, fires the dispatch, serves the page.
- **web/** *(todo)* — static UI: defconfig dropdown → submit → poll the release
  asset → download link.

Status: **proof-of-concept** — building a single existing defconfig end-to-end.
