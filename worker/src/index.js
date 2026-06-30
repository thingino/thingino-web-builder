// Thingino web-builder — Cloudflare Worker broker (proof of concept).
//
// Ports the core of the Rust/VPS broker onto Cloudflare's free tier:
//   * fetch handler  = the HTTP API (build / status / cancel / stats / defconfigs)
//   * scheduled (cron, every 1 min) = dispatch queued, correlate runs, reap
//   * D1             = the SQLite state (builds / events / settings)
//   * Worker Secret  = GITHUB_TOKEN
//
// The GitHub Actions build (build.yml) and the rolling-release download are
// unchanged. The frontend (GitHub Pages / Cloudflare Pages) calls this over CORS.
//
// Not yet ported here (straightforward follow-ups): admin panel + TOTP 2FA
// (Web Crypto HMAC-SHA1) and GitHub App auth (Web Crypto RS256 JWT).

const WINDOW = 3600;
const DAY = 86400;

const nowSec = () => Math.floor(Date.now() / 1000);
const uuid = () => crypto.randomUUID();
const validUid = (s) => typeof s === "string" && /^[a-zA-Z0-9-]{8,64}$/.test(s);
const validBuildId = (s) => typeof s === "string" && /^[a-f0-9-]{8,40}$/.test(s);

function limits(env) {
  const n = (k, d) => parseInt(env[k] || "", 10) || d;
  return {
    userHourly: n("PER_USER_HOURLY_LIMIT", 2),
    ipHourly: n("PER_IP_HOURLY_LIMIT", 3),
    globalHourly: n("GLOBAL_HOURLY_LIMIT", 20),
    maxConcurrent: n("MAX_CONCURRENT_BUILDS", 6),
    maxQueue: n("MAX_QUEUE", 50),
    retention: n("RETENTION_SECS", 1800),
    failedRetention: n("FAILED_RETENTION_SECS", 3600),
    buildTimeout: n("BUILD_TIMEOUT_SECS", 5400),
  };
}

const assetUrl = (env, id) =>
  `https://github.com/${env.GITHUB_REPO}/releases/download/${env.ROLLING_TAG || "web-builds"}/${id}.bin`;

// ---- CORS + JSON ----------------------------------------------------------
function cors(env) {
  return {
    "Access-Control-Allow-Origin": env.ALLOW_ORIGIN || "*",
    "Access-Control-Allow-Methods": "GET,POST,OPTIONS",
    "Access-Control-Allow-Headers": "content-type,x-builder-uid,authorization",
    "Vary": "Origin",
  };
}
const json = (obj, status, env) =>
  new Response(JSON.stringify(obj), {
    status: status || 200,
    headers: { "content-type": "application/json", ...cors(env) },
  });

// Bucket the client IP: full v4, /64 for v6 (a user usually owns a whole /64).
function ipBucket(ip) {
  if (!ip) return "v4:0.0.0.0";
  if (ip.includes(":")) {
    const [head, tail = ""] = ip.split("::");
    const h = head ? head.split(":").filter(Boolean) : [];
    const t = tail ? tail.split(":").filter(Boolean) : [];
    const fill = Math.max(0, 8 - h.length - t.length);
    const groups = [...h, ...Array(fill).fill("0"), ...t];
    const px = groups.slice(0, 4).map((g) => (parseInt(g || "0", 16) || 0).toString(16).padStart(4, "0")).join(":");
    return `v6:${px}::/64`;
  }
  return `v4:${ip}`;
}
function resolveUid(request) {
  const h = request.headers.get("x-builder-uid");
  return validUid(h) ? h : uuid();
}

// ---- D1 helpers -----------------------------------------------------------
const countQ = async (env, sql, ...args) =>
  ((await env.DB.prepare(sql).bind(...args).first()) || { c: 0 }).c;
const getSetting = async (env, key) => {
  const r = await env.DB.prepare("SELECT value FROM settings WHERE key=?").bind(key).first();
  return r ? r.value : null;
};
const setSetting = (env, key, value) =>
  env.DB.prepare("INSERT INTO settings(key,value) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
    .bind(key, value).run();
const logEvent = (env, kind, build_id, uid, ip, detail, ipFull) =>
  env.DB.prepare("INSERT INTO events(ts,kind,build_id,uid,ip_bucket,ip_full,detail) VALUES(?,?,?,?,?,?,?)")
    .bind(nowSec(), kind, build_id || null, uid || null, ip || null, ipFull || null, detail || "").run();

// ---- GitHub ---------------------------------------------------------------
function ghHeaders(env, auth) {
  const h = {
    Accept: "application/vnd.github+json",
    "X-GitHub-Api-Version": "2022-11-28",
    "User-Agent": "thingino-web-builder-worker",
  };
  if (auth && env.GITHUB_TOKEN) h.Authorization = `Bearer ${env.GITHUB_TOKEN}`;
  return h;
}
const ghFetch = (env, url, opts = {}) =>
  fetch(url, { ...opts, headers: { ...ghHeaders(env, true), ...(opts.headers || {}) } });

// thingino pinned commit + defconfig list, cached in D1 settings (~5 min).
async function resolveThingino(env) {
  const ts = parseInt((await getSetting(env, "thingino_ts")) || "0", 10);
  let commit = await getSetting(env, "thingino_commit");
  let listJson = await getSetting(env, "defconfigs");
  if (commit && listJson && nowSec() - ts < 300) return { commit, list: JSON.parse(listJson) };

  const repo = env.THINGINO_REPO || "themactep/thingino-firmware";
  const ref = env.THINGINO_REF || "master";
  try {
    const cr = await fetch(`https://api.github.com/repos/${repo}/commits/${ref}`, { headers: ghHeaders(env, false) });
    if (cr.ok) {
      const newCommit = (await cr.json()).sha;
      if (newCommit && newCommit !== commit) {
        const list = await fetchDefconfigs(env, repo, newCommit);
        if (list.length) {
          listJson = JSON.stringify(list);
          await setSetting(env, "defconfigs", listJson);
        }
        commit = newCommit;
        await setSetting(env, "thingino_commit", commit);
      }
      await setSetting(env, "thingino_ts", String(nowSec()));
    }
  } catch (_) { /* keep last-good */ }
  return { commit: commit || null, list: listJson ? JSON.parse(listJson) : [] };
}
async function fetchDir(env, repo, commit, subdir) {
  const r = await fetch(`https://api.github.com/repos/${repo}/contents/configs/${subdir}?ref=${commit}`, {
    headers: ghHeaders(env, false),
  });
  if (!r.ok) return [];
  const arr = await r.json();
  return Array.isArray(arr)
    ? arr.filter((e) => e.type === "dir" && /^[a-z0-9_+]+$/.test(e.name)).map((e) => e.name)
    : [];
}
async function fetchDefconfigs(env, repo, commit) {
  const a = await fetchDir(env, repo, commit, "cameras");
  const b = await fetchDir(env, repo, commit, "cameras-exp");
  return [...new Set([...a, ...b])].sort();
}

// ---- API handlers ---------------------------------------------------------
async function handleDefconfigs(env) {
  const { list } = await resolveThingino(env);
  return json(list, 200, env);
}
async function handleStats(request, env) {
  const uid = resolveUid(request);
  const { commit } = await resolveThingino(env);
  const cfg = limits(env);
  return json({
    running: await countQ(env, "SELECT count(*) c FROM builds WHERE state='running'"),
    queued: await countQ(env, "SELECT count(*) c FROM builds WHERE state='queued'"),
    max_concurrent: cfg.maxConcurrent,
    builds_enabled: (await getSetting(env, "builds_enabled")) !== "0",
    commit,
    version: env.VERSION || "v0.1.0",
    uid,
  }, 200, env);
}
async function handleBuild(request, env) {
  let body;
  try { body = await request.json(); } catch { return json({ error: "bad request" }, 400, env); }
  const defconfig = (body.defconfig || "").trim();
  const { commit, list } = await resolveThingino(env);
  if (!list.includes(defconfig)) return json({ error: "unknown defconfig" }, 400, env);

  const uid = resolveUid(request);
  const rawIp = request.headers.get("CF-Connecting-IP") || "";
  const ip = ipBucket(rawIp);
  const ts = nowSec(), cfg = limits(env);
  // Hourly window, but never count builds from before an admin "reset limits".
  const resetTs = parseInt((await getSetting(env, "limits_reset_ts")) || "0", 10);
  const cutoff = Math.max(ts - WINDOW, resetTs);

  if ((await getSetting(env, "builds_enabled")) === "0")
    return json({ error: "builds are temporarily disabled" }, 503, env);

  // Dedup: identical (defconfig, commit) in flight or done within retention.
  if (commit) {
    const e = await env.DB.prepare(
      `SELECT id,state,cancel_requested FROM builds
       WHERE defconfig=? AND commit_sha=?
         AND (state IN ('queued','running') OR (state='done' AND finished_ts > ?))
         AND NOT (state='running' AND cancel_requested=1)
       ORDER BY created_ts DESC LIMIT 1`
    ).bind(defconfig, commit, ts - cfg.retention).first();
    if (e) {
      await logEvent(env, "dedup", e.id, uid, ip, `reused ${e.state} for ${defconfig}`, rawIp);
      const st = e.state === "running" && e.cancel_requested ? "cancelling" : e.state;
      return json({
        build_id: e.id, defconfig, state: st, deduped: true,
        download_url: st === "done" ? assetUrl(env, e.id) : null,
        status_url: `/api/status/${e.id}`, commit,
      }, 200, env);
    }
  }

  if ((await countQ(env, "SELECT count(*) c FROM builds WHERE state='queued'")) >= cfg.maxQueue)
    return json({ error: "the build queue is full, try again shortly" }, 503, env);

  const notCancelledUndispatched = "NOT (state='cancelled' AND dispatched_ts IS NULL)";
  if ((await countQ(env, `SELECT count(*) c FROM builds WHERE created_ts > ? AND ${notCancelledUndispatched}`, cutoff)) >= cfg.globalHourly) {
    await logEvent(env, "rate_limited", null, uid, ip, "global hourly limit", rawIp);
    return json({ error: `the builder is at its hourly limit (${cfg.globalHourly}/hr) — try again later` }, 429, env);
  }
  if ((await countQ(env, `SELECT count(*) c FROM builds WHERE uid=? AND created_ts > ? AND ${notCancelledUndispatched}`, uid, cutoff)) >= cfg.userHourly) {
    await logEvent(env, "rate_limited", null, uid, ip, "per-user hourly limit", rawIp);
    return json({ error: `you've reached ${cfg.userHourly} builds this hour — try again later` }, 429, env);
  }
  if ((await countQ(env, `SELECT count(*) c FROM builds WHERE ip_bucket=? AND created_ts > ? AND ${notCancelledUndispatched}`, ip, cutoff)) >= cfg.ipHourly) {
    await logEvent(env, "rate_limited", null, uid, ip, "per-ip hourly limit", rawIp);
    return json({ error: "too many builds from your network this hour — try again later" }, 429, env);
  }

  const id = uuid();
  await env.DB.prepare("INSERT INTO builds(id,uid,ip_bucket,ip_full,defconfig,state,created_ts,commit_sha) VALUES(?,?,?,?,?,'queued',?,?)")
    .bind(id, uid, ip, rawIp, defconfig, ts, commit).run();
  await logEvent(env, "queued", id, uid, ip, defconfig, rawIp);

  // Inline dispatch: if a slot is free, fire the build NOW rather than waiting for
  // the next cron tick. The cron is only a fallback/reconciler for the rest.
  let state = "queued", position = 0;
  if ((await countQ(env, "SELECT count(*) c FROM builds WHERE state='running'")) < cfg.maxConcurrent) {
    try {
      await dispatchBuild(env, id, defconfig, commit);
      await env.DB.prepare("UPDATE builds SET state='running', dispatched_ts=? WHERE id=?").bind(nowSec(), id).run();
      await logEvent(env, "dispatched", id, uid, ip, defconfig);
      state = "running";
    } catch (_) { /* stays queued; the cron retries */ }
  }
  if (state === "queued") position = await countQ(env, "SELECT count(*) c FROM builds WHERE state='queued'");
  return json({ build_id: id, defconfig, state, position, status_url: `/api/status/${id}`, download_url: assetUrl(env, id), commit }, 202, env);
}
async function handleStatus(id, env) {
  if (!validBuildId(id)) return json({ error: "bad build_id" }, 400, env);
  const b = await env.DB.prepare(
    "SELECT defconfig,state,created_ts,dispatched_ts,finished_ts,cancel_requested FROM builds WHERE id=?"
  ).bind(id).first();
  if (!b) return json({ error: "unknown build" }, 404, env);
  const ts = nowSec();
  const state = b.state === "running" && b.cancel_requested ? "cancelling" : b.state;
  let position = 0;
  if (state === "queued")
    position = await countQ(env, "SELECT count(*) c FROM builds WHERE state='queued' AND created_ts <= ?", b.created_ts);
  let elapsed = 0;
  if (state === "running" || state === "cancelling") elapsed = b.dispatched_ts ? ts - b.dispatched_ts : 0;
  else if (state === "queued") elapsed = ts - b.created_ts;
  else if (b.finished_ts && b.dispatched_ts) elapsed = b.finished_ts - b.dispatched_ts;
  const ready = state === "done";
  return json({ build_id: id, defconfig: b.defconfig, state, ready, position, elapsed_secs: elapsed, download_url: ready ? assetUrl(env, id) : null }, 200, env);
}
// Shared cancel: queued → cancelled; running → cancel_requested + stop the GitHub
// run inline if we can find it (the cron retries otherwise). Returns the new state.
async function doCancel(env, b, id, uid) {
  if (b.state === "queued") {
    await env.DB.prepare("UPDATE builds SET state='cancelled', finished_ts=? WHERE id=?").bind(nowSec(), id).run();
    await logEvent(env, "cancelled", id, uid, null, "cancelled while queued");
    return "cancelled";
  }
  if (b.state === "running") {
    await env.DB.prepare("UPDATE builds SET cancel_requested=1 WHERE id=?").bind(id).run();
    let note = "cancel queued (run not yet listed)";
    try {
      const runs = await fetchRuns(env);
      const m = runs.find((r) => (b.run_id && r.run_id === b.run_id) || r.name.includes(id));
      if (m) {
        await cancelRun(env, m.run_id);
        await env.DB.prepare("UPDATE builds SET run_id=? WHERE id=?").bind(m.run_id, id).run();
        note = "cancel sent to run";
      }
    } catch (_) { /* cron will retry */ }
    await logEvent(env, "cancel_requested", id, uid, null, note);
    return "cancelling";
  }
  return "already finished";
}
async function handleCancel(id, request, env) {
  if (!validBuildId(id)) return json({ error: "bad build_id" }, 400, env);
  const uid = resolveUid(request);
  const b = await env.DB.prepare("SELECT uid,state,run_id FROM builds WHERE id=?").bind(id).first();
  if (!b) return json({ error: "unknown build" }, 404, env);
  if (b.uid !== uid) return json({ error: "not your build" }, 403, env);
  return json({ state: await doCancel(env, b, id, uid) }, 200, env);
}
async function handleAdminCancel(id, request, env) {
  if (!(await sessionOk(request, env))) return json({ error: "admin auth required" }, 401, env);
  if (!validBuildId(id)) return json({ error: "bad build_id" }, 400, env);
  const b = await env.DB.prepare("SELECT uid,state,run_id FROM builds WHERE id=?").bind(id).first();
  if (!b) return json({ error: "unknown build" }, 404, env);
  await logEvent(env, "admin_cancel", id, b.uid, null, `admin cancelled (was ${b.state})`);
  return json({ state: await doCancel(env, b, id, b.uid) }, 200, env);
}
// Admin: remove a finished build's artifact + Actions run early (the reaper's job, on demand).
async function handleAdminExpire(id, request, env) {
  if (!(await sessionOk(request, env))) return json({ error: "admin auth required" }, 401, env);
  if (!validBuildId(id)) return json({ error: "bad build_id" }, 400, env);
  const b = await env.DB.prepare("SELECT uid,state,run_id FROM builds WHERE id=?").bind(id).first();
  if (!b) return json({ error: "unknown build" }, 404, env);
  if (!["done", "failed", "cancelled"].includes(b.state)) return json({ error: "build is not finished" }, 400, env);
  const assetOk = b.state === "done" ? await deleteReleaseAssets(env, id) : true;
  const runOk = b.run_id ? await deleteRun(env, b.run_id) : true;
  if (!(assetOk && runOk)) return json({ error: "GitHub cleanup failed; the cron will retry" }, 502, env);
  await env.DB.prepare("UPDATE builds SET state='expired' WHERE id=?").bind(id).run();
  await logEvent(env, "expired", id, b.uid, null, "admin removed early");
  return json({ ok: true, state: "expired" }, 200, env);
}

// ---- scheduler (cron) -----------------------------------------------------
async function dispatchBuild(env, id, defconfig, commit) {
  const cp = { build_id: id, defconfig };
  if (commit) cp.commit = commit;
  const r = await ghFetch(env, `https://api.github.com/repos/${env.GITHUB_REPO}/dispatches`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ event_type: "web-build", client_payload: cp }),
  });
  if (!r.ok) throw new Error(`dispatch ${r.status}`);
}
async function fetchRuns(env) {
  const r = await ghFetch(env, `https://api.github.com/repos/${env.GITHUB_REPO}/actions/runs?per_page=50&event=repository_dispatch`);
  if (!r.ok) return [];
  return ((await r.json()).workflow_runs || []).map((x) => ({
    run_id: x.id, name: x.name || x.display_title || "", status: x.status || "", conclusion: x.conclusion || null,
  }));
}
const cancelRun = (env, runId) =>
  ghFetch(env, `https://api.github.com/repos/${env.GITHUB_REPO}/actions/runs/${runId}/cancel`, { method: "POST" }).catch(() => {});
async function deleteRun(env, runId) {
  try {
    const r = await ghFetch(env, `https://api.github.com/repos/${env.GITHUB_REPO}/actions/runs/${runId}`, { method: "DELETE" });
    return r.ok || r.status === 404;
  } catch { return false; }
}
async function deleteReleaseAssets(env, id) {
  try {
    const r = await ghFetch(env, `https://api.github.com/repos/${env.GITHUB_REPO}/releases/tags/${env.ROLLING_TAG || "web-builds"}`);
    if (r.status === 404) return true;
    if (!r.ok) return false;
    const v = await r.json();
    const targets = [`${id}.bin`, `${id}.bin.sha256sum`];
    let ok = true;
    for (const a of v.assets || []) {
      if (targets.includes(a.name)) {
        const d = await ghFetch(env, `https://api.github.com/repos/${env.GITHUB_REPO}/releases/assets/${a.id}`, { method: "DELETE" });
        if (!(d.ok || d.status === 404)) ok = false;
      }
    }
    return ok;
  } catch { return false; }
}

async function schedulerStep(env) {
  const ts = nowSec(), cfg = limits(env);
  await resolveThingino(env);

  const running = ((await env.DB.prepare("SELECT id,run_id,dispatched_ts,cancel_requested FROM builds WHERE state='running'").all()).results) || [];
  const slots = Math.max(0, cfg.maxConcurrent - running.length);
  const queued = slots > 0
    ? ((await env.DB.prepare("SELECT id,defconfig,commit_sha FROM builds WHERE state='queued' ORDER BY created_ts ASC LIMIT ?").bind(slots).all()).results) || []
    : [];

  const runs = running.length ? await fetchRuns(env) : [];

  for (const b of running) {
    const m = runs.find((r) => (b.run_id && r.run_id === b.run_id) || r.name.includes(b.id));
    if (b.cancel_requested) {
      if (m && m.status === "completed") {
        await deleteRun(env, m.run_id);
        await env.DB.prepare("UPDATE builds SET state='cancelled', finished_ts=?, run_id=NULL WHERE id=?").bind(ts, b.id).run();
        await logEvent(env, "cancelled", b.id, null, null, "run stopped + deleted");
      } else if (m) {
        await cancelRun(env, m.run_id);
      } else if (b.dispatched_ts && ts - b.dispatched_ts > 180) {
        // Give up only after a grace window — otherwise we'd orphan a run that
        // simply hasn't appeared in the runs list yet.
        await env.DB.prepare("UPDATE builds SET state='cancelled', finished_ts=? WHERE id=?").bind(ts, b.id).run();
        await logEvent(env, "cancelled", b.id, null, null, "cancelled (run not found after grace)");
      }
      // else: stay 'cancelling' and retry next tick
      continue;
    }
    if (m) {
      if (!b.run_id) await env.DB.prepare("UPDATE builds SET run_id=? WHERE id=?").bind(m.run_id, b.id).run();
      if (m.status === "completed") {
        const st = m.conclusion === "success" ? "done" : m.conclusion === "cancelled" ? "cancelled" : "failed";
        await env.DB.prepare("UPDATE builds SET state=?, finished_ts=? WHERE id=?").bind(st, ts, b.id).run();
        await logEvent(env, st, b.id, null, null, `run ${m.run_id} ${m.conclusion || "?"}`);
      }
    } else if (ts - (b.dispatched_ts || ts) > cfg.buildTimeout) {
      await env.DB.prepare("UPDATE builds SET state='failed', finished_ts=? WHERE id=?").bind(ts, b.id).run();
      await logEvent(env, "failed", b.id, null, null, "timed out / run not found");
    }
  }

  for (const q of queued) {
    const still = await env.DB.prepare("SELECT 1 FROM builds WHERE id=? AND state='queued'").bind(q.id).first();
    if (!still) continue;
    try {
      await dispatchBuild(env, q.id, q.defconfig, q.commit_sha);
      await env.DB.prepare("UPDATE builds SET state='running', dispatched_ts=? WHERE id=?").bind(nowSec(), q.id).run();
      await logEvent(env, "dispatched", q.id, null, null, q.defconfig);
    } catch (_) {
      await env.DB.prepare("UPDATE builds SET attempts=attempts+1 WHERE id=?").bind(q.id).run();
      const at = ((await env.DB.prepare("SELECT attempts FROM builds WHERE id=?").bind(q.id).first()) || { attempts: 0 }).attempts;
      if (at >= 3) {
        await env.DB.prepare("UPDATE builds SET state='failed', finished_ts=? WHERE id=?").bind(nowSec(), q.id).run();
        await logEvent(env, "failed", q.id, null, null, "dispatch failed 3x");
      }
    }
  }

  const reap = ((await env.DB.prepare("SELECT id,state,run_id,finished_ts FROM builds WHERE state IN ('done','failed','cancelled') AND finished_ts IS NOT NULL").all()).results) || [];
  for (const b of reap) {
    const age = ts - b.finished_ts;
    const expired = b.state === "done" ? age > cfg.retention : age > cfg.failedRetention;
    if (!expired) continue;
    const assetOk = b.state === "done" ? await deleteReleaseAssets(env, b.id) : true;
    const runOk = b.run_id ? await deleteRun(env, b.run_id) : true;
    if (assetOk && runOk) {
      await env.DB.prepare("UPDATE builds SET state='expired' WHERE id=?").bind(b.id).run();
      await logEvent(env, "expired", b.id, null, null, `reaped ${b.state}`);
    }
  }

  await env.DB.prepare("DELETE FROM builds WHERE state='expired' AND finished_ts < ?").bind(ts - 7 * DAY).run();
  await env.DB.prepare("DELETE FROM events WHERE ts < ?").bind(ts - 30 * DAY).run();
}

// ---- admin (TOTP 2FA + sessions in D1) ------------------------------------
function ctEq(a, b) {
  if (a.length !== b.length) return false;
  let d = 0;
  for (let i = 0; i < a.length; i++) d |= a.charCodeAt(i) ^ b.charCodeAt(i);
  return d === 0;
}
function base32Decode(s) {
  const A = "ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
  let bits = 0, val = 0;
  const out = [];
  for (const ch of s.trim().toUpperCase()) {
    if (ch === "=" || ch === " ") continue;
    const i = A.indexOf(ch);
    if (i < 0) return null;
    val = (val << 5) | i;
    bits += 5;
    if (bits >= 8) { bits -= 8; out.push((val >> bits) & 0xff); }
  }
  return new Uint8Array(out);
}
async function hotp(secret, counter) {
  const key = await crypto.subtle.importKey("raw", secret, { name: "HMAC", hash: "SHA-1" }, false, ["sign"]);
  const buf = new ArrayBuffer(8);
  const dv = new DataView(buf);
  dv.setUint32(0, Math.floor(counter / 2 ** 32));
  dv.setUint32(4, counter >>> 0);
  const mac = new Uint8Array(await crypto.subtle.sign("HMAC", key, buf));
  const off = mac[19] & 0x0f;
  const bin = ((mac[off] & 0x7f) << 24) | (mac[off + 1] << 16) | (mac[off + 2] << 8) | mac[off + 3];
  return bin % 1000000;
}
async function totpCheck(secretB32, code) {
  if (!/^[0-9]{6}$/.test(code)) return false;
  const secret = base32Decode(secretB32);
  if (!secret) return false;
  const want = parseInt(code, 10);
  const step = Math.floor(Date.now() / 1000 / 30);
  for (const c of [step - 1, step, step + 1]) if ((await hotp(secret, c)) === want) return true;
  return false;
}
const bearer = (request) => {
  const a = request.headers.get("authorization") || "";
  return a.startsWith("Bearer ") ? a.slice(7) : "";
};
async function sessionOk(request, env) {
  const tok = bearer(request);
  if (!tok) return false;
  const t = nowSec();
  await env.DB.prepare("DELETE FROM sessions WHERE expires <= ?").bind(t).run();
  const r = await env.DB.prepare("SELECT expires FROM sessions WHERE token=?").bind(tok).first();
  return !!(r && r.expires > t);
}

async function handleAdminLogin(request, env) {
  let body;
  try { body = await request.json(); } catch { return json({ error: "bad request" }, 400, env); }
  const rawIp = request.headers.get("CF-Connecting-IP") || "";
  const ip = ipBucket(rawIp);
  const fails = await countQ(env, "SELECT count(*) c FROM events WHERE kind='admin_login_fail' AND ip_bucket=? AND ts > ?", ip, nowSec() - 900);
  if (fails >= 10) {
    await logEvent(env, "admin_login_throttled", null, null, ip, "too many failed logins", rawIp);
    return json({ error: "too many attempts — try again later" }, 429, env);
  }
  if (!env.ADMIN_TOKEN) return json({ error: "admin is disabled" }, 503, env);
  if (!env.ADMIN_TOTP_SECRET) return json({ error: "admin 2FA is not configured" }, 503, env);
  const ok = ctEq(String(body.token || ""), env.ADMIN_TOKEN) && (await totpCheck(env.ADMIN_TOTP_SECRET, String(body.totp || "").trim()));
  if (!ok) {
    await logEvent(env, "admin_login_fail", null, null, ip, "bad token or 2FA", rawIp);
    return json({ error: "invalid credentials" }, 401, env);
  }
  const session = uuid(), ttl = 8 * 3600;
  await env.DB.prepare("INSERT INTO sessions(token,expires) VALUES(?,?)").bind(session, nowSec() + ttl).run();
  await logEvent(env, "admin_login_ok", null, null, ip, "session created", rawIp);
  return json({ session, expires_in: ttl, totp: true }, 200, env);
}
async function handleAdminLogout(request, env) {
  const tok = bearer(request);
  if (tok) await env.DB.prepare("DELETE FROM sessions WHERE token=?").bind(tok).run();
  return json({ ok: true }, 200, env);
}
async function handleAdminStats(request, env) {
  if (!(await sessionOk(request, env))) return json({ error: "admin auth required" }, 401, env);
  const cfg = limits(env);
  const counts = {};
  for (const s of ["queued", "running", "done", "failed", "cancelled", "expired"])
    counts[s] = await countQ(env, "SELECT count(*) c FROM builds WHERE state=?", s);
  const avg = await env.DB.prepare("SELECT avg(finished_ts - dispatched_ts) a FROM builds WHERE state='done' AND finished_ts IS NOT NULL AND dispatched_ts IS NOT NULL").first();
  const builds = ((await env.DB.prepare("SELECT id,defconfig,state,created_ts,dispatched_ts,finished_ts,run_id,cancel_requested,uid,ip_bucket,ip_full FROM builds ORDER BY created_ts DESC LIMIT 25").all()).results || []).map((b) => ({
    build_id: b.id, defconfig: b.defconfig,
    state: b.state === "running" && b.cancel_requested ? "cancelling" : b.state,
    created_ts: b.created_ts, dispatched_ts: b.dispatched_ts, finished_ts: b.finished_ts, run_id: b.run_id, uid: b.uid,
    ip: b.ip_full || b.ip_bucket, ip_bucket: b.ip_bucket,
  }));
  const events = ((await env.DB.prepare("SELECT ts,kind,build_id,detail,uid,ip_bucket,ip_full FROM events ORDER BY id DESC LIMIT 60").all()).results || []).map((e) => ({
    ts: e.ts, kind: e.kind, build_id: e.build_id, detail: e.detail, uid: e.uid,
    ip: e.ip_full || e.ip_bucket, ip_bucket: e.ip_bucket,
  }));
  return json({
    builds_enabled: (await getSetting(env, "builds_enabled")) !== "0",
    counts,
    last24h: await countQ(env, "SELECT count(*) c FROM builds WHERE created_ts > ?", nowSec() - DAY),
    avg_build_secs: avg && avg.a ? Math.round(avg.a) : null,
    max_concurrent: cfg.maxConcurrent, retention_secs: cfg.retention,
    recent_builds: builds, recent_events: events,
    version: env.VERSION || "v0.1.0", latest_version: null, update_available: false,
  }, 200, env);
}
async function handleAdminToggle(request, env) {
  if (!(await sessionOk(request, env))) return json({ error: "admin auth required" }, 401, env);
  let body;
  try { body = await request.json(); } catch { return json({ error: "bad request" }, 400, env); }
  await setSetting(env, "builds_enabled", body.enabled ? "1" : "0");
  await logEvent(env, "admin_toggle", null, null, null, `builds_enabled=${!!body.enabled}`);
  return json({ builds_enabled: !!body.enabled }, 200, env);
}
async function handleAdminClearLogs(request, env) {
  if (!(await sessionOk(request, env))) return json({ error: "admin auth required" }, 401, env);
  const r = await env.DB.prepare("DELETE FROM events").run();
  const n = r.meta?.changes ?? 0;
  await logEvent(env, "admin_clear_logs", null, null, null, `cleared ${n} events`);
  return json({ ok: true, cleared: n }, 200, env);
}
async function handleAdminResetLimits(request, env) {
  if (!(await sessionOk(request, env))) return json({ error: "admin auth required" }, 401, env);
  // Mark "now" so the rate-limit queries ignore every build created before this.
  await setSetting(env, "limits_reset_ts", String(nowSec()));
  await logEvent(env, "admin_reset_limits", null, null, null, "hourly limits reset");
  return json({ ok: true }, 200, env);
}

// ---- entrypoints ----------------------------------------------------------
export default {
  async fetch(request, env, _ctx) {
    if (request.method === "OPTIONS") return new Response(null, { status: 204, headers: cors(env) });
    const p = new URL(request.url).pathname;
    try {
      if (p === "/api/health") return new Response("ok", { headers: cors(env) });
      if (p === "/api/defconfigs" && request.method === "GET") return await handleDefconfigs(env);
      if (p === "/api/stats" && request.method === "GET") return await handleStats(request, env);
      if (p === "/api/build" && request.method === "POST") return await handleBuild(request, env);
      let m;
      if ((m = p.match(/^\/api\/status\/(.+)$/)) && request.method === "GET") return await handleStatus(m[1], env);
      if ((m = p.match(/^\/api\/cancel\/(.+)$/)) && request.method === "POST") return await handleCancel(m[1], request, env);
      if ((m = p.match(/^\/api\/admin\/cancel\/(.+)$/)) && request.method === "POST") return await handleAdminCancel(m[1], request, env);
      if ((m = p.match(/^\/api\/admin\/expire\/(.+)$/)) && request.method === "POST") return await handleAdminExpire(m[1], request, env);
      if (p === "/api/admin/login" && request.method === "POST") return await handleAdminLogin(request, env);
      if (p === "/api/admin/stats" && request.method === "GET") return await handleAdminStats(request, env);
      if (p === "/api/admin/toggle" && request.method === "POST") return await handleAdminToggle(request, env);
      if (p === "/api/admin/clear-logs" && request.method === "POST") return await handleAdminClearLogs(request, env);
      if (p === "/api/admin/reset-limits" && request.method === "POST") return await handleAdminResetLimits(request, env);
      if (p === "/api/admin/logout" && request.method === "POST") return await handleAdminLogout(request, env);
      return json({ error: "not found" }, 404, env);
    } catch (e) {
      return json({ error: "internal error" }, 500, env);
    }
  },

  async scheduled(_event, env, ctx) {
    ctx.waitUntil(schedulerStep(env));
  },
};
