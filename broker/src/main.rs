//! Thingino web-builder broker — control plane + scheduler.
//!
//! It NEVER builds. It:
//!   * serves the static frontend (same-origin, no CORS),
//!   * issues a per-user id (cookie + `X-Builder-Uid` mirror) and rate-limits
//!     per user and per IP bucket (IPv4 /32, IPv6 /64),
//!   * holds a global concurrency cap (default 6) with a FIFO queue,
//!   * dispatches `repository_dispatch` (event `web-build`) and tracks each
//!     build's lifecycle in SQLite (survives restarts),
//!   * correlates each build to its Actions run to detect completion/failure
//!     and to cancel it,
//!   * reaps finished builds after a retention window — deleting the release
//!     asset AND the Actions run (build-log history),
//!   * writes an audit event for everything,
//!   * exposes an admin API (stats + global kill switch) behind ADMIN_TOKEN.
//!
//! Env: GITHUB_TOKEN, GITHUB_REPO (required); BIND_ADDR (default [::]:8080),
//! STATIC_DIR, DB_PATH, DEFCONFIGS_PATH, ROLLING_TAG, PER_USER_HOURLY_LIMIT,
//! PER_IP_HOURLY_LIMIT, MAX_CONCURRENT_BUILDS, MAX_QUEUE, RETENTION_SECS,
//! FAILED_RETENTION_SECS, BUILD_TIMEOUT_SECS, IP_HEADER, ADMIN_TOKEN.

use std::{
    collections::HashSet,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    extract::{ConnectInfo, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::json;
use tower_http::services::ServeDir;
use uuid::Uuid;

const WINDOW_SECS: i64 = 3600;

struct Config {
    github_token: Option<String>,
    github_app_id: Option<String>,
    github_app_installation_id: Option<String>,
    github_app_key: Option<Vec<u8>>,
    app_mode: bool,
    github_repo: String,
    thingino_repo: String,
    thingino_ref: String,
    rolling_tag: String,
    per_user_hourly: i64,
    per_ip_hourly: i64,
    global_hourly: i64,
    max_concurrent: i64,
    max_queue: i64,
    retention_secs: i64,
    failed_retention_secs: i64,
    build_timeout_secs: i64,
    ip_header: Option<String>,
    admin_token: Option<String>,
    admin_totp_secret: Option<String>,
}

/// Thingino's pinned commit + the buildable defconfig list AT that commit. Seeded
/// from the baked defconfigs.json, then refreshed live from GitHub so new boards
/// appear without a redeploy. List and commit always move together.
#[derive(Clone)]
struct Thingino {
    commit: Option<String>,
    list: Arc<Vec<String>>,
    set: Arc<HashSet<String>>,
    list_commit: Option<String>,
    fetched_at: i64,
}

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    http: reqwest::Client,
    cfg: Arc<Config>,
    thingino: Arc<Mutex<Thingino>>,
    sessions: Arc<Mutex<std::collections::HashMap<String, i64>>>,
    installation_token: Arc<Mutex<(Option<String>, i64)>>,
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.into())
}
fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}
fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Builder version: Cargo package version + git short-sha (baked via BUILD_SHA at build time).
fn version_string() -> String {
    match option_env!("BUILD_SHA") {
        Some(sha) if !sha.is_empty() && sha != "dev" => format!("v{} ({sha})", env!("CARGO_PKG_VERSION")),
        _ => format!("v{}", env!("CARGO_PKG_VERSION")),
    }
}

fn valid_build_id(s: &str) -> bool {
    let n = s.len();
    (8..=40).contains(&n) && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}
fn valid_uid(s: &str) -> bool {
    let n = s.len();
    (8..=64).contains(&n) && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Bucket the client IP for rate limiting: full address for IPv4, /64 prefix for
/// IPv6 (a user usually owns a whole /64, so limiting /128 is pointless).
fn ip_bucket(ip: IpAddr) -> String {
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        other => other,
    };
    match ip {
        IpAddr::V4(a) => format!("v4:{a}"),
        IpAddr::V6(a) => {
            let o = a.octets();
            format!(
                "v6:{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}:{:02x}{:02x}::/64",
                o[0], o[1], o[2], o[3], o[4], o[5], o[6], o[7]
            )
        }
    }
}

fn client_ip(headers: &HeaderMap, peer: SocketAddr, ip_header: &Option<String>) -> IpAddr {
    if let Some(h) = ip_header {
        if let Some(v) = headers.get(h.as_str()).and_then(|v| v.to_str().ok()) {
            if let Some(first) = v.split(',').next() {
                if let Ok(ip) = first.trim().parse::<IpAddr>() {
                    return ip;
                }
            }
        }
    }
    peer.ip()
}

fn parse_cookie_uid(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';')
        .filter_map(|p| p.trim().strip_prefix("uid="))
        .find(|v| valid_uid(v))
        .map(|s| s.to_string())
}

/// Resolve the user id: prefer the explicit header (survives a cookie clear via
/// the localStorage mirror), then the cookie, else mint a fresh one.
fn resolve_uid(headers: &HeaderMap) -> String {
    if let Some(h) = headers.get("x-builder-uid").and_then(|v| v.to_str().ok()) {
        if valid_uid(h) {
            return h.to_string();
        }
    }
    parse_cookie_uid(headers).unwrap_or_else(|| Uuid::new_v4().to_string())
}

fn asset_url(cfg: &Config, build_id: &str) -> String {
    format!(
        "https://github.com/{}/releases/download/{}/{}.bin",
        cfg.github_repo, cfg.rolling_tag, build_id
    )
}

// ---- response helpers -----------------------------------------------------

fn json_err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}
fn set_uid_cookie(resp: &mut Response, uid: &str) {
    let cookie = format!("uid={uid}; Path=/; Max-Age=31536000; SameSite=Lax");
    if let Ok(hv) = header::HeaderValue::from_str(&cookie) {
        resp.headers_mut().append(header::SET_COOKIE, hv);
    }
}
fn json_uid(code: StatusCode, uid: &str, body: serde_json::Value) -> Response {
    let mut resp = (code, Json(body)).into_response();
    set_uid_cookie(&mut resp, uid);
    resp
}

// ---- settings + audit -----------------------------------------------------

fn get_setting(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get(0))
        .optional()
        .ok()
        .flatten()
}
fn set_setting(conn: &Connection, key: &str, value: &str) {
    let _ = conn.execute(
        "INSERT INTO settings(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        rusqlite::params![key, value],
    );
}
fn builds_enabled(conn: &Connection) -> bool {
    get_setting(conn, "builds_enabled").map(|v| v != "0").unwrap_or(true)
}

fn log_event(
    conn: &Connection,
    kind: &str,
    build_id: Option<&str>,
    uid: Option<&str>,
    ip: Option<&str>,
    detail: &str,
) {
    let _ = conn.execute(
        "INSERT INTO events(ts, kind, build_id, uid, ip_bucket, detail) VALUES (?1,?2,?3,?4,?5,?6)",
        rusqlite::params![now(), kind, build_id, uid, ip, detail],
    );
    tracing::info!("event {kind}: {detail}");
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        d |= x ^ y;
    }
    d == 0
}
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|s| s.strip_prefix("Bearer "))
}

/// Admin requests authenticate with a session token minted by /api/admin/login
/// (which requires the admin token + a valid TOTP code when 2FA is configured).
fn session_ok(headers: &HeaderMap, st: &AppState) -> bool {
    let Some(tok) = bearer(headers) else { return false; };
    let now_ts = now();
    let mut s = st.sessions.lock().unwrap();
    s.retain(|_, exp| *exp > now_ts);
    s.get(tok).map(|&exp| exp > now_ts).unwrap_or(false)
}

// ---- TOTP (RFC 6238, Google Authenticator compatible) --------------------

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut buf = 0u64;
    let mut bits = 0u32;
    let mut out = Vec::new();
    for c in s.trim().to_ascii_uppercase().bytes() {
        if c == b'=' || c == b' ' {
            continue;
        }
        let v = ALPHA.iter().position(|&x| x == c)? as u64;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

fn hotp(secret: &[u8], counter: u64) -> u32 {
    use hmac::{Hmac, Mac};
    use sha1::Sha1;
    let mut mac = Hmac::<Sha1>::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(&counter.to_be_bytes());
    let h = mac.finalize().into_bytes();
    let off = (h[19] & 0x0f) as usize;
    let bin = ((h[off] as u32 & 0x7f) << 24)
        | ((h[off + 1] as u32) << 16)
        | ((h[off + 2] as u32) << 8)
        | (h[off + 3] as u32);
    bin % 1_000_000
}

/// Validate a 6-digit code against the base32 secret, accepting the current step ±1.
fn totp_check(secret_b32: &str, code: &str) -> bool {
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Some(secret) = base32_decode(secret_b32) else { return false; };
    let Ok(want) = code.parse::<u32>() else { return false; };
    let step = (now() as u64) / 30;
    [step.wrapping_sub(1), step, step + 1].iter().any(|&c| hotp(&secret, c) == want)
}

// ---- main -----------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

    // Singleton guard — refuse to start if another broker already holds the lock.
    let lock_path = env_or("LOCK_PATH", "broker.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(&lock_path)
        .map_err(|e| anyhow::anyhow!("opening lock {lock_path}: {e}"))?;
    if unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&lock_file), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!("another broker already holds {lock_path} — refusing to start a second instance");
    }
    std::mem::forget(lock_file); // hold the advisory lock for the whole process lifetime
    let _ = std::fs::write(env_or("PID_PATH", "broker.pid"), std::process::id().to_string());
    tracing::info!("singleton lock {lock_path} acquired, pid {}", std::process::id());

    // GitHub auth: a static token, OR a GitHub App (App ID + installation + private key).
    let github_token = std::env::var("GITHUB_TOKEN").ok().filter(|s| !s.is_empty());
    let github_app_id = std::env::var("GITHUB_APP_ID").ok().filter(|s| !s.is_empty());
    let github_app_installation_id = std::env::var("GITHUB_APP_INSTALLATION_ID").ok().filter(|s| !s.is_empty());
    let github_app_key = match std::env::var("GITHUB_APP_KEY_PATH").ok().filter(|s| !s.is_empty()) {
        Some(p) => Some(std::fs::read(&p).map_err(|e| anyhow::anyhow!("reading GITHUB_APP_KEY_PATH {p}: {e}"))?),
        None => None,
    };
    let app_mode = github_app_id.is_some() && github_app_installation_id.is_some() && github_app_key.is_some();
    if !app_mode && github_token.is_none() {
        anyhow::bail!("set GITHUB_TOKEN, or GITHUB_APP_ID + GITHUB_APP_INSTALLATION_ID + GITHUB_APP_KEY_PATH");
    }
    tracing::info!("github auth: {}", if app_mode { "GitHub App" } else { "static token" });

    let cfg = Config {
        github_token,
        github_app_id,
        github_app_installation_id,
        github_app_key,
        app_mode,
        github_repo: std::env::var("GITHUB_REPO").map_err(|_| anyhow::anyhow!("GITHUB_REPO required (owner/repo)"))?,
        thingino_repo: env_or("THINGINO_REPO", "themactep/thingino-firmware"),
        thingino_ref: env_or("THINGINO_REF", "master"),
        rolling_tag: env_or("ROLLING_TAG", "web-builds"),
        per_user_hourly: env_i64("PER_USER_HOURLY_LIMIT", 2),
        per_ip_hourly: env_i64("PER_IP_HOURLY_LIMIT", 3),
        global_hourly: env_i64("GLOBAL_HOURLY_LIMIT", 20),
        max_concurrent: env_i64("MAX_CONCURRENT_BUILDS", 6),
        max_queue: env_i64("MAX_QUEUE", 50),
        retention_secs: env_i64("RETENTION_SECS", 1800),
        failed_retention_secs: env_i64("FAILED_RETENTION_SECS", 3600),
        build_timeout_secs: env_i64("BUILD_TIMEOUT_SECS", 5400),
        ip_header: std::env::var("IP_HEADER").ok().filter(|s| !s.is_empty()),
        admin_token: std::env::var("ADMIN_TOKEN").ok().filter(|s| !s.is_empty()),
        admin_totp_secret: std::env::var("ADMIN_TOTP_SECRET").ok().filter(|s| !s.is_empty()),
    };
    let bind_addr = env_or("BIND_ADDR", "[::]:8080");
    let static_dir = env_or("STATIC_DIR", "web");
    let db_path = env_or("DB_PATH", "broker.db");
    let defconfigs_path = env_or("DEFCONFIGS_PATH", "defconfigs.json");

    let raw = std::fs::read_to_string(&defconfigs_path)
        .map_err(|e| anyhow::anyhow!("reading {defconfigs_path}: {e}"))?;
    let mut fallback_list: Vec<String> = serde_json::from_str(&raw)?;
    fallback_list.sort();
    let fallback_set: HashSet<String> = fallback_list.iter().cloned().collect();
    tracing::info!("loaded {} fallback defconfigs", fallback_list.len());

    let conn = Connection::open(&db_path)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS builds(
            id TEXT PRIMARY KEY,
            uid TEXT NOT NULL,
            ip_bucket TEXT NOT NULL,
            defconfig TEXT NOT NULL,
            state TEXT NOT NULL,
            run_id INTEGER,
            attempts INTEGER NOT NULL DEFAULT 0,
            cancel_requested INTEGER NOT NULL DEFAULT 0,
            created_ts INTEGER NOT NULL,
            dispatched_ts INTEGER,
            finished_ts INTEGER,
            commit_sha TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_state ON builds(state);
        CREATE INDEX IF NOT EXISTS idx_uid_created ON builds(uid, created_ts);
        CREATE INDEX IF NOT EXISTS idx_ip_created ON builds(ip_bucket, created_ts);
        CREATE TABLE IF NOT EXISTS events(
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            ts INTEGER NOT NULL,
            kind TEXT NOT NULL,
            build_id TEXT,
            uid TEXT,
            ip_bucket TEXT,
            detail TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);
        CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )?;
    let _ = conn.execute("ALTER TABLE builds ADD COLUMN commit_sha TEXT", []); // migrate older DBs

    let http = reqwest::Client::builder()
        .user_agent("thingino-web-builder-broker")
        .build()?;

    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
        http,
        cfg: Arc::new(cfg),
        thingino: Arc::new(Mutex::new(Thingino {
            commit: None,
            list: Arc::new(fallback_list),
            set: Arc::new(fallback_set),
            list_commit: None,
            fetched_at: 0,
        })),
        sessions: Arc::new(Mutex::new(std::collections::HashMap::new())),
        installation_token: Arc::new(Mutex::new((None, 0))),
    };

    {
        let st = state.clone();
        tokio::spawn(async move { scheduler_loop(st).await });
    }

    let app = Router::new()
        .route("/api/health", get(|| async { "ok" }))
        .route("/api/defconfigs", get(get_defconfigs))
        .route("/api/stats", get(get_stats))
        .route("/api/build", post(post_build))
        .route("/api/status/{build_id}", get(get_status))
        .route("/api/cancel/{build_id}", post(post_cancel))
        .route("/api/admin/login", post(admin_login))
        .route("/api/admin/stats", get(admin_stats))
        .route("/api/admin/toggle", post(admin_toggle))
        .fallback_service(ServeDir::new(&static_dir).append_index_html_on_directories(true))
        .with_state(state);

    let addr: SocketAddr = bind_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("broker listening on http://{addr}");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

// ---- public handlers ------------------------------------------------------

async fn get_defconfigs(State(st): State<AppState>) -> Response {
    let t = resolve_thingino(&st).await;
    Json(t.list.as_ref().clone()).into_response()
}

async fn get_stats(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let uid = resolve_uid(&headers);
    let now_ts = now();
    let commit = current_commit(&st).await;
    let conn = st.db.lock().unwrap();
    let running: i64 = conn.query_row("SELECT count(*) FROM builds WHERE state='running'", [], |r| r.get(0)).unwrap_or(0);
    let queued: i64 = conn.query_row("SELECT count(*) FROM builds WHERE state='queued'", [], |r| r.get(0)).unwrap_or(0);
    let avg: Option<f64> = conn
        .query_row(
            "SELECT avg(finished_ts - dispatched_ts) FROM builds WHERE state='done' AND finished_ts IS NOT NULL AND dispatched_ts IS NOT NULL AND finished_ts > ?1",
            [now_ts - 86400],
            |r| r.get(0),
        )
        .optional().ok().flatten();
    let you = latest_user_build(&conn, &st.cfg, &uid, now_ts);
    let enabled = builds_enabled(&conn);
    drop(conn);
    json_uid(
        StatusCode::OK,
        &uid,
        json!({
            "running": running,
            "queued": queued,
            "max_concurrent": st.cfg.max_concurrent,
            "avg_build_secs": avg.map(|v| v.round() as i64),
            "builds_enabled": enabled,
            "commit": commit,
            "version": version_string(),
            "you": you,
            "uid": uid,
        }),
    )
}

#[derive(Deserialize)]
struct BuildReq {
    defconfig: String,
}

async fn post_build(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<BuildReq>,
) -> Response {
    let defconfig = req.defconfig.trim().to_string();
    let thingino = resolve_thingino(&st).await;
    if !thingino.set.contains(&defconfig) {
        return json_err(StatusCode::BAD_REQUEST, "unknown defconfig");
    }
    let uid = resolve_uid(&headers);
    let ip = ip_bucket(client_ip(&headers, peer, &st.cfg.ip_header));
    let now_ts = now();
    let cutoff = now_ts - WINDOW_SECS;
    let build_id = Uuid::new_v4().to_string();
    let commit = thingino.commit.clone();

    let position: i64 = {
        let conn = st.db.lock().unwrap();
        if !builds_enabled(&conn) {
            return json_uid(StatusCode::SERVICE_UNAVAILABLE, &uid, json!({"error": "builds are temporarily disabled"}));
        }
        // Dedup: same (defconfig, commit) already built (within retention) or in flight → reuse it.
        if let Some(c) = commit.as_deref() {
            if let Some((eid, estate, edl)) = find_existing(&conn, &defconfig, c, now_ts - st.cfg.retention_secs, &st.cfg) {
                log_event(&conn, "dedup", Some(&eid), Some(&uid), Some(&ip), &format!("reused {estate} for {defconfig}"));
                return json_uid(StatusCode::OK, &uid, json!({
                    "build_id": eid,
                    "defconfig": defconfig,
                    "state": estate,
                    "deduped": true,
                    "download_url": edl,
                    "status_url": format!("/api/status/{eid}"),
                    "commit": c,
                }));
            }
        }
        let queued_now: i64 = conn.query_row("SELECT count(*) FROM builds WHERE state='queued'", [], |r| r.get(0)).unwrap_or(0);
        if queued_now >= st.cfg.max_queue {
            return json_uid(StatusCode::SERVICE_UNAVAILABLE, &uid, json!({"error": "the build queue is full, try again shortly"}));
        }
        // Global hourly cap across everyone (counts builds that actually got going).
        let global_n: i64 = conn
            .query_row(
                "SELECT count(*) FROM builds WHERE created_ts > ?1 AND NOT (state='cancelled' AND dispatched_ts IS NULL)",
                [cutoff],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if global_n >= st.cfg.global_hourly {
            log_event(&conn, "rate_limited", None, Some(&uid), Some(&ip), "global hourly limit");
            return json_uid(StatusCode::TOO_MANY_REQUESTS, &uid, json!({"error": format!("the builder is at its hourly limit ({}/hr) — try again later", st.cfg.global_hourly)}));
        }
        // Builds count toward a limit unless they were cancelled before ever dispatching.
        let user_n: i64 = conn
            .query_row(
                "SELECT count(*) FROM builds WHERE uid=?1 AND created_ts > ?2 AND NOT (state='cancelled' AND dispatched_ts IS NULL)",
                rusqlite::params![uid, cutoff],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if user_n >= st.cfg.per_user_hourly {
            log_event(&conn, "rate_limited", None, Some(&uid), Some(&ip), "per-user hourly limit");
            return json_uid(StatusCode::TOO_MANY_REQUESTS, &uid, json!({"error": format!("you've reached {} builds this hour — try again later", st.cfg.per_user_hourly)}));
        }
        let ip_n: i64 = conn
            .query_row(
                "SELECT count(*) FROM builds WHERE ip_bucket=?1 AND created_ts > ?2 AND NOT (state='cancelled' AND dispatched_ts IS NULL)",
                rusqlite::params![ip, cutoff],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if ip_n >= st.cfg.per_ip_hourly {
            log_event(&conn, "rate_limited", None, Some(&uid), Some(&ip), "per-ip hourly limit");
            return json_uid(StatusCode::TOO_MANY_REQUESTS, &uid, json!({"error": "too many builds from your network this hour — try again later"}));
        }
        if let Err(e) = conn.execute(
            "INSERT INTO builds(id, uid, ip_bucket, defconfig, state, created_ts, commit_sha) VALUES (?1,?2,?3,?4,'queued',?5,?6)",
            rusqlite::params![build_id, uid, ip, defconfig, now_ts, commit],
        ) {
            tracing::error!("insert failed: {e}");
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
        log_event(&conn, "queued", Some(&build_id), Some(&uid), Some(&ip), &defconfig);
        conn.query_row("SELECT count(*) FROM builds WHERE state='queued'", [], |r| r.get(0)).unwrap_or(1)
    };

    json_uid(
        StatusCode::ACCEPTED,
        &uid,
        json!({
            "build_id": build_id,
            "defconfig": defconfig,
            "state": "queued",
            "position": position,
            "status_url": format!("/api/status/{build_id}"),
            "download_url": asset_url(&st.cfg, &build_id),
            "commit": commit,
        }),
    )
}

async fn get_status(State(st): State<AppState>, Path(build_id): Path<String>) -> Response {
    if !valid_build_id(&build_id) {
        return json_err(StatusCode::BAD_REQUEST, "bad build_id");
    }
    let now_ts = now();
    let conn = st.db.lock().unwrap();
    let row = conn
        .query_row(
            "SELECT defconfig, state, created_ts, dispatched_ts, finished_ts, cancel_requested FROM builds WHERE id=?1",
            [&build_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?, r.get::<_, i64>(5)?)),
        )
        .optional().ok().flatten();
    let Some((defconfig, real_state, created_ts, dispatched_ts, finished_ts, cancel_req)) = row else {
        return json_err(StatusCode::NOT_FOUND, "unknown build");
    };
    // A running build with a pending cancel surfaces as "cancelling" — persisted via
    // cancel_requested, so it survives reloads until the run actually stops.
    let state = if real_state == "running" && cancel_req != 0 { "cancelling".to_string() } else { real_state };
    let position: i64 = if state == "queued" {
        conn.query_row("SELECT count(*) FROM builds WHERE state='queued' AND created_ts <= ?1", [created_ts], |r| r.get(0)).unwrap_or(1)
    } else {
        0
    };
    drop(conn);
    let elapsed = match state.as_str() {
        "running" | "cancelling" => dispatched_ts.map(|d| now_ts - d).unwrap_or(0),
        "queued" => now_ts - created_ts,
        _ => match (finished_ts, dispatched_ts) {
            (Some(f), Some(d)) => f - d,
            _ => 0,
        },
    };
    let ready = state == "done";
    Json(json!({
        "build_id": build_id,
        "defconfig": defconfig,
        "state": state,
        "ready": ready,
        "position": position,
        "elapsed_secs": elapsed,
        "download_url": if ready { Some(asset_url(&st.cfg, &build_id)) } else { None },
    }))
    .into_response()
}

async fn post_cancel(State(st): State<AppState>, headers: HeaderMap, Path(build_id): Path<String>) -> Response {
    if !valid_build_id(&build_id) {
        return json_err(StatusCode::BAD_REQUEST, "bad build_id");
    }
    let uid = resolve_uid(&headers);
    let now_ts = now();
    let conn = st.db.lock().unwrap();
    let row = conn
        .query_row("SELECT uid, state FROM builds WHERE id=?1", [&build_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .optional().ok().flatten();
    let resp = match row {
        None => json_err(StatusCode::NOT_FOUND, "unknown build"),
        Some((owner, _)) if owner != uid => json_err(StatusCode::FORBIDDEN, "not your build"),
        Some((_, state)) if state == "queued" => {
            conn.execute("UPDATE builds SET state='cancelled', finished_ts=?2 WHERE id=?1", rusqlite::params![build_id, now_ts]).ok();
            log_event(&conn, "cancelled", Some(&build_id), Some(&uid), None, "cancelled while queued");
            json_uid(StatusCode::OK, &uid, json!({"state": "cancelled"}))
        }
        Some((_, state)) if state == "running" => {
            conn.execute("UPDATE builds SET cancel_requested=1 WHERE id=?1", [&build_id]).ok();
            log_event(&conn, "cancel_requested", Some(&build_id), Some(&uid), None, "cancel requested while running");
            json_uid(StatusCode::OK, &uid, json!({"state": "cancelling"}))
        }
        Some(_) => json_uid(StatusCode::OK, &uid, json!({"state": "already finished"})),
    };
    resp
}

// ---- admin ----------------------------------------------------------------

#[derive(Deserialize)]
struct LoginReq {
    token: String,
    #[serde(default)]
    totp: String,
}

/// Exchange the admin token (+ TOTP code when 2FA is configured) for a session token.
async fn admin_login(State(st): State<AppState>, Json(body): Json<LoginReq>) -> Response {
    let Some(admin_token) = st.cfg.admin_token.as_deref() else {
        return json_err(StatusCode::SERVICE_UNAVAILABLE, "admin is disabled");
    };
    if !constant_time_eq(body.token.as_bytes(), admin_token.as_bytes()) {
        return json_err(StatusCode::UNAUTHORIZED, "invalid credentials");
    }
    // 2FA is mandatory: no TOTP secret configured → admin is unavailable.
    let Some(secret) = st.cfg.admin_totp_secret.as_deref() else {
        return json_err(StatusCode::SERVICE_UNAVAILABLE, "admin 2FA is not configured");
    };
    if !totp_check(secret, body.totp.trim()) {
        return json_err(StatusCode::UNAUTHORIZED, "invalid or missing 2FA code");
    }
    let session = Uuid::new_v4().to_string();
    let ttl = 8 * 3600;
    st.sessions.lock().unwrap().insert(session.clone(), now() + ttl);
    Json(json!({ "session": session, "expires_in": ttl, "totp": st.cfg.admin_totp_secret.is_some() })).into_response()
}

async fn admin_stats(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !session_ok(&headers, &st) {
        return json_err(StatusCode::UNAUTHORIZED, "admin auth required");
    }
    let conn = st.db.lock().unwrap();
    let mut counts = serde_json::Map::new();
    for s in ["queued", "running", "done", "failed", "cancelled", "expired"] {
        let n: i64 = conn.query_row("SELECT count(*) FROM builds WHERE state=?1", [s], |r| r.get(0)).unwrap_or(0);
        counts.insert(s.to_string(), json!(n));
    }
    let last24: i64 = conn.query_row("SELECT count(*) FROM builds WHERE created_ts > ?1", [now() - 86400], |r| r.get(0)).unwrap_or(0);
    let avg: Option<f64> = conn
        .query_row("SELECT avg(finished_ts - dispatched_ts) FROM builds WHERE state='done' AND finished_ts IS NOT NULL AND dispatched_ts IS NOT NULL", [], |r| r.get(0))
        .optional().ok().flatten();
    let recent_builds = query_recent_builds(&conn, 25);
    let recent_events = query_recent_events(&conn, 60);
    let enabled = builds_enabled(&conn);
    drop(conn);
    Json(json!({
        "builds_enabled": enabled,
        "counts": counts,
        "last24h": last24,
        "avg_build_secs": avg.map(|v| v.round() as i64),
        "max_concurrent": st.cfg.max_concurrent,
        "retention_secs": st.cfg.retention_secs,
        "recent_builds": recent_builds,
        "recent_events": recent_events,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct ToggleReq {
    enabled: bool,
}

async fn admin_toggle(State(st): State<AppState>, headers: HeaderMap, Json(body): Json<ToggleReq>) -> Response {
    if !session_ok(&headers, &st) {
        return json_err(StatusCode::UNAUTHORIZED, "admin auth required");
    }
    let conn = st.db.lock().unwrap();
    set_setting(&conn, "builds_enabled", if body.enabled { "1" } else { "0" });
    log_event(&conn, "admin_toggle", None, None, None, &format!("builds_enabled={}", body.enabled));
    drop(conn);
    Json(json!({ "builds_enabled": body.enabled })).into_response()
}

// ---- query helpers --------------------------------------------------------

/// Find an existing build for this exact (defconfig, commit) that's worth reusing:
/// in flight (queued/running, not being cancelled) or done within the retention window.
fn find_existing(conn: &Connection, defconfig: &str, commit: &str, done_cutoff: i64, cfg: &Config) -> Option<(String, String, Option<String>)> {
    conn.query_row(
        "SELECT id, state, cancel_requested FROM builds
         WHERE defconfig=?1 AND commit_sha=?2
           AND (state IN ('queued','running') OR (state='done' AND finished_ts > ?3))
           AND NOT (state='running' AND cancel_requested=1)
         ORDER BY created_ts DESC LIMIT 1",
        rusqlite::params![defconfig, commit, done_cutoff],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)),
    )
    .optional().ok().flatten()
    .map(|(id, state, cancel_req)| {
        let dstate = if state == "running" && cancel_req != 0 { "cancelling".to_string() } else { state };
        let dl = if dstate == "done" { Some(asset_url(cfg, &id)) } else { None };
        (id, dstate, dl)
    })
}

fn latest_user_build(conn: &Connection, cfg: &Config, uid: &str, now_ts: i64) -> Option<serde_json::Value> {
    let (id, defconfig, real_state, created_ts, dispatched_ts, finished_ts, cancel_req) = conn
        .query_row(
            "SELECT id, defconfig, state, created_ts, dispatched_ts, finished_ts, cancel_requested FROM builds WHERE uid=?1 ORDER BY created_ts DESC LIMIT 1",
            [uid],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<i64>>(4)?, r.get::<_, Option<i64>>(5)?, r.get::<_, i64>(6)?)),
        )
        .optional().ok().flatten()?;
    let state = if real_state == "running" && cancel_req != 0 { "cancelling".to_string() } else { real_state };
    let position: i64 = if state == "queued" {
        conn.query_row("SELECT count(*) FROM builds WHERE state='queued' AND created_ts <= ?1", [created_ts], |r| r.get(0)).unwrap_or(1)
    } else {
        0
    };
    let elapsed = match state.as_str() {
        "running" | "cancelling" => dispatched_ts.map(|d| now_ts - d).unwrap_or(0),
        "queued" => now_ts - created_ts,
        _ => match (finished_ts, dispatched_ts) {
            (Some(f), Some(d)) => f - d,
            _ => 0,
        },
    };
    Some(json!({
        "build_id": id,
        "defconfig": defconfig,
        "state": state,
        "position": position,
        "elapsed_secs": elapsed,
        "download_url": if state == "done" { Some(asset_url(cfg, &id)) } else { None },
    }))
}

fn query_recent_builds(conn: &Connection, limit: i64) -> Vec<serde_json::Value> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT id, defconfig, state, created_ts, dispatched_ts, finished_ts, run_id, cancel_requested, uid, ip_bucket FROM builds ORDER BY created_ts DESC LIMIT ?1",
    ) else {
        return vec![];
    };
    let it = stmt.query_map([limit], |r| {
        let real_state: String = r.get(2)?;
        let cancel_req: i64 = r.get(7)?;
        let state = if real_state == "running" && cancel_req != 0 { "cancelling".to_string() } else { real_state };
        Ok(json!({
            "build_id": r.get::<_, String>(0)?,
            "defconfig": r.get::<_, String>(1)?,
            "state": state,
            "created_ts": r.get::<_, i64>(3)?,
            "dispatched_ts": r.get::<_, Option<i64>>(4)?,
            "finished_ts": r.get::<_, Option<i64>>(5)?,
            "run_id": r.get::<_, Option<i64>>(6)?,
            "uid": r.get::<_, String>(8)?,
            "ip": r.get::<_, String>(9)?,
        }))
    });
    match it {
        Ok(rows) => rows.filter_map(|x| x.ok()).collect(),
        Err(_) => vec![],
    }
}

fn query_recent_events(conn: &Connection, limit: i64) -> Vec<serde_json::Value> {
    let Ok(mut stmt) = conn.prepare("SELECT ts, kind, build_id, detail, uid, ip_bucket FROM events ORDER BY id DESC LIMIT ?1") else {
        return vec![];
    };
    let it = stmt.query_map([limit], |r| {
        Ok(json!({
            "ts": r.get::<_, i64>(0)?,
            "kind": r.get::<_, String>(1)?,
            "build_id": r.get::<_, Option<String>>(2)?,
            "detail": r.get::<_, String>(3)?,
            "uid": r.get::<_, Option<String>>(4)?,
            "ip": r.get::<_, Option<String>>(5)?,
        }))
    });
    match it {
        Ok(rows) => rows.filter_map(|x| x.ok()).collect(),
        Err(_) => vec![],
    }
}

// ---- scheduler ------------------------------------------------------------

// Public reads (no auth) so a builder-repo-scoped token needs no thingino access.

async fn fetch_commit(st: &AppState) -> Option<String> {
    let url = format!("https://api.github.com/repos/{}/commits/{}", st.cfg.thingino_repo, st.cfg.thingino_ref);
    match st
        .http
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
    {
        Ok(r) => r.json::<serde_json::Value>().await.ok().and_then(|v| v["sha"].as_str().map(|s| s.to_string())),
        Err(_) => None,
    }
}

fn valid_board(n: &str) -> bool {
    // board tokens are lowercase/digit/underscore, plus '+' for the eth+<wifi> combos.
    !n.is_empty() && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '+')
}

/// Dir names (board folders) under configs/<subdir> at a commit.
async fn fetch_camera_dir(st: &AppState, commit: &str, subdir: &str) -> Option<Vec<String>> {
    let url = format!("https://api.github.com/repos/{}/contents/configs/{}?ref={}", st.cfg.thingino_repo, subdir, commit);
    let v: serde_json::Value = st
        .http
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let list: Vec<String> = v
        .as_array()?
        .iter()
        .filter(|e| e["type"].as_str() == Some("dir"))
        .filter_map(|e| e["name"].as_str())
        .filter(|n| valid_board(n))
        .map(|s| s.to_string())
        .collect();
    Some(list)
}

/// Buildable boards = configs/cameras (stable) + configs/cameras-exp (experimental).
/// The workflow detects which group a board is in and passes GROUP= accordingly.
async fn fetch_defconfigs(st: &AppState, commit: &str) -> Option<Vec<String>> {
    let mut names = fetch_camera_dir(st, commit, "cameras").await?;
    if let Some(exp) = fetch_camera_dir(st, commit, "cameras-exp").await {
        names.extend(exp);
    }
    names.sort();
    names.dedup();
    if names.is_empty() {
        None
    } else {
        Some(names)
    }
}

/// Resolve thingino's pinned commit + the defconfig list at that commit, cached
/// ~2 min. The list is re-fetched only when the commit moves; on any failure the
/// last-good (seeded from defconfigs.json) cache is kept.
async fn resolve_thingino(st: &AppState) -> Thingino {
    {
        let t = st.thingino.lock().unwrap();
        if t.commit.is_some() && now() - t.fetched_at < 120 {
            return t.clone();
        }
    }
    let commit = fetch_commit(st).await;
    let need_list = {
        let t = st.thingino.lock().unwrap();
        match (&commit, &t.list_commit) {
            (Some(c), Some(lc)) => c != lc,
            (Some(_), None) => true,
            (None, _) => false,
        }
    };
    let fetched = match (&commit, need_list) {
        (Some(c), true) => fetch_defconfigs(st, c).await.map(|l| (l, c.clone())),
        _ => None,
    };
    let mut t = st.thingino.lock().unwrap();
    if let Some(c) = commit {
        t.commit = Some(c);
        t.fetched_at = now();
    }
    if let Some((list, lc)) = fetched {
        t.set = Arc::new(list.iter().cloned().collect());
        t.list = Arc::new(list);
        t.list_commit = Some(lc);
        tracing::info!("defconfigs refreshed: {} boards @ {}", t.list.len(), t.list_commit.as_deref().unwrap_or("?"));
    }
    t.clone()
}

/// The thingino commit builds will use (pinned).
async fn current_commit(st: &AppState) -> Option<String> {
    resolve_thingino(st).await.commit
}

async fn scheduler_loop(st: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(10));
    loop {
        tick.tick().await;
        if let Err(e) = scheduler_step(&st).await {
            tracing::warn!("scheduler step error: {e}");
        }
    }
}

struct RunRow {
    run_id: i64,
    name: String,
    status: String,
    conclusion: Option<String>,
}

async fn scheduler_step(st: &AppState) -> anyhow::Result<()> {
    let now_ts = now();
    let _ = resolve_thingino(st).await; // keep commit + defconfig list warm (picks up new boards)

    // 1) Snapshot running builds + the next queued builds to dispatch.
    let (running, to_dispatch): (Vec<(String, Option<i64>, i64, bool)>, Vec<(String, String, Option<String>)>) = {
        let conn = st.db.lock().unwrap();
        let running: Vec<(String, Option<i64>, i64, bool)> = {
            let mut stmt = conn.prepare("SELECT id, run_id, dispatched_ts, cancel_requested FROM builds WHERE state='running'")?;
            let rows = stmt
                .query_map([], |r| {
                    let id: String = r.get(0)?;
                    let run_id: Option<i64> = r.get(1)?;
                    let disp: Option<i64> = r.get(2)?;
                    let can: i64 = r.get(3)?;
                    Ok((id, run_id, disp.unwrap_or(now_ts), can != 0))
                })?
                .filter_map(|x| x.ok())
                .collect();
            rows
        };
        let slots = (st.cfg.max_concurrent - running.len() as i64).max(0);
        let to_dispatch: Vec<(String, String, Option<String>)> = if slots > 0 {
            let mut q = conn.prepare("SELECT id, defconfig, commit_sha FROM builds WHERE state='queued' ORDER BY created_ts ASC LIMIT ?1")?;
            let rows = q
                .query_map([slots], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)))?
                .filter_map(|x| x.ok())
                .collect::<Vec<_>>();
            rows
        } else {
            vec![]
        };
        (running, to_dispatch)
    };

    // 2) One Actions runs listing per tick covers correlation + status.
    let runs = if running.is_empty() { vec![] } else { fetch_runs(st).await.unwrap_or_default() };

    // 3) Reconcile each running build.
    for (id, run_id_opt, dispatched_ts, cancel_req) in &running {
        let matched = runs
            .iter()
            .find(|r| run_id_opt.map(|rid| rid == r.run_id).unwrap_or(false) || r.name.contains(id.as_str()));

        if *cancel_req {
            // Stay in 'cancelling' until the run actually stops; only then finalize.
            match matched {
                Some(r) if r.status == "completed" => {
                    let _ = delete_run(st, r.run_id).await; // wipe the cancelled run + its logs now, not at retention
                    let conn = st.db.lock().unwrap();
                    conn.execute("UPDATE builds SET state='cancelled', finished_ts=?2, run_id=NULL WHERE id=?1", rusqlite::params![id, now_ts]).ok();
                    log_event(&conn, "cancelled", Some(id), None, None, "run stopped + deleted");
                }
                Some(r) => {
                    let _ = cancel_run(st, r.run_id).await; // run still active — (re)request cancellation
                }
                None => {
                    let conn = st.db.lock().unwrap();
                    conn.execute("UPDATE builds SET state='cancelled', finished_ts=?2 WHERE id=?1", rusqlite::params![id, now_ts]).ok();
                    log_event(&conn, "cancelled", Some(id), None, None, "cancelled (run not found)");
                }
            }
            continue;
        }

        match matched {
            Some(r) => {
                if run_id_opt.is_none() {
                    let conn = st.db.lock().unwrap();
                    conn.execute("UPDATE builds SET run_id=?2 WHERE id=?1", rusqlite::params![id, r.run_id]).ok();
                }
                if r.status == "completed" {
                    let new_state = match r.conclusion.as_deref() {
                        Some("success") => "done",
                        Some("cancelled") => "cancelled",
                        _ => "failed",
                    };
                    let conn = st.db.lock().unwrap();
                    conn.execute("UPDATE builds SET state=?2, finished_ts=?3 WHERE id=?1", rusqlite::params![id, new_state, now_ts]).ok();
                    log_event(&conn, new_state, Some(id), None, None, &format!("run {} {}", r.run_id, r.conclusion.as_deref().unwrap_or("?")));
                }
            }
            None => {
                if now_ts - dispatched_ts > st.cfg.build_timeout_secs {
                    let conn = st.db.lock().unwrap();
                    conn.execute("UPDATE builds SET state='failed', finished_ts=?2 WHERE id=?1", rusqlite::params![id, now_ts]).ok();
                    log_event(&conn, "failed", Some(id), None, None, "timed out / run not found");
                }
            }
        }
    }

    // 4) Dispatch from the queue into free slots.
    for (id, defconfig, commit) in &to_dispatch {
        let still_queued: bool = {
            let conn = st.db.lock().unwrap();
            conn.query_row("SELECT 1 FROM builds WHERE id=?1 AND state='queued'", [id], |_| Ok(())).optional().ok().flatten().is_some()
        };
        if !still_queued {
            continue;
        }
        match dispatch_build(st, id, defconfig, commit.as_deref()).await {
            Ok(()) => {
                let conn = st.db.lock().unwrap();
                conn.execute("UPDATE builds SET state='running', dispatched_ts=?2 WHERE id=?1", rusqlite::params![id, now()]).ok();
                log_event(&conn, "dispatched", Some(id), None, None, defconfig);
            }
            Err(e) => {
                let conn = st.db.lock().unwrap();
                conn.execute("UPDATE builds SET attempts=attempts+1 WHERE id=?1", [id]).ok();
                let attempts: i64 = conn.query_row("SELECT attempts FROM builds WHERE id=?1", [id], |r| r.get(0)).unwrap_or(0);
                if attempts >= 3 {
                    conn.execute("UPDATE builds SET state='failed', finished_ts=?2 WHERE id=?1", rusqlite::params![id, now()]).ok();
                    log_event(&conn, "failed", Some(id), None, None, "dispatch failed 3x");
                }
                tracing::warn!("dispatch failed for {id} (attempt {attempts}): {e}");
            }
        }
    }

    // 5) Reap finished builds past their retention window.
    let reap: Vec<(String, String, Option<i64>, i64)> = {
        let conn = st.db.lock().unwrap();
        let mut stmt = conn.prepare("SELECT id, state, run_id, finished_ts FROM builds WHERE state IN ('done','failed','cancelled') AND finished_ts IS NOT NULL")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, i64>(3)?)))?
            .filter_map(|x| x.ok())
            .collect::<Vec<_>>();
        rows
    };
    for (id, state, run_id, finished_ts) in reap {
        let age = now_ts - finished_ts;
        let expired = if state == "done" { age > st.cfg.retention_secs } else { age > st.cfg.failed_retention_secs };
        if !expired {
            continue;
        }
        if state == "done" {
            let _ = delete_release_assets(st, &id).await;
        }
        if let Some(rid) = run_id {
            let _ = delete_run(st, rid).await;
        }
        let conn = st.db.lock().unwrap();
        conn.execute("UPDATE builds SET state='expired' WHERE id=?1", [&id]).ok();
        log_event(&conn, "expired", Some(&id), None, None, &format!("reaped {state}: asset+run removed"));
    }

    Ok(())
}

async fn fetch_runs(st: &AppState) -> anyhow::Result<Vec<RunRow>> {
    let url = format!(
        "https://api.github.com/repos/{}/actions/runs?per_page=50&event=repository_dispatch",
        st.cfg.github_repo
    );
    let v: serde_json::Value = st
        .http
        .get(&url)
        .bearer_auth(github_token(st).await)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?
        .json()
        .await?;
    let mut out = vec![];
    if let Some(arr) = v["workflow_runs"].as_array() {
        for r in arr {
            out.push(RunRow {
                run_id: r["id"].as_i64().unwrap_or(0),
                name: r["name"].as_str().or_else(|| r["display_title"].as_str()).unwrap_or("").to_string(),
                status: r["status"].as_str().unwrap_or("").to_string(),
                conclusion: r["conclusion"].as_str().map(|s| s.to_string()),
            });
        }
    }
    Ok(out)
}

/// Current GitHub token: a minted + cached App installation token (App mode),
/// else the static GITHUB_TOKEN.
async fn github_token(st: &AppState) -> String {
    if st.cfg.app_mode {
        {
            let c = st.installation_token.lock().unwrap();
            if let Some(t) = &c.0 {
                if now() < c.1 {
                    return t.clone();
                }
            }
        }
        match mint_installation_token(st).await {
            Ok((tok, exp)) => {
                *st.installation_token.lock().unwrap() = (Some(tok.clone()), exp);
                return tok;
            }
            Err(e) => tracing::error!("GitHub App token mint failed: {e}"),
        }
    }
    st.cfg.github_token.clone().unwrap_or_default()
}

/// Mint a ~1h GitHub App installation token (RS256 JWT → installation access token).
async fn mint_installation_token(st: &AppState) -> anyhow::Result<(String, i64)> {
    use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
    let app_id = st.cfg.github_app_id.as_deref().ok_or_else(|| anyhow::anyhow!("no app id"))?;
    let inst = st.cfg.github_app_installation_id.as_deref().ok_or_else(|| anyhow::anyhow!("no installation id"))?;
    let pem = st.cfg.github_app_key.as_deref().ok_or_else(|| anyhow::anyhow!("no app key"))?;

    #[derive(serde::Serialize)]
    struct Claims {
        iat: i64,
        exp: i64,
        iss: String,
    }
    let nowt = now();
    let claims = Claims { iat: nowt - 60, exp: nowt + 540, iss: app_id.to_string() };
    let jwt = encode(&Header::new(Algorithm::RS256), &claims, &EncodingKey::from_rsa_pem(pem)?)?;

    let url = format!("https://api.github.com/app/installations/{inst}/access_tokens");
    let resp = st
        .http
        .post(&url)
        .bearer_auth(&jwt)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?;
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("installation token {code}: {body}");
    }
    let v: serde_json::Value = resp.json().await?;
    let token = v["token"].as_str().ok_or_else(|| anyhow::anyhow!("no token in response"))?.to_string();
    Ok((token, nowt + 3300)) // ~1h tokens; refresh at 55 min
}

async fn dispatch_build(st: &AppState, build_id: &str, defconfig: &str, commit: Option<&str>) -> anyhow::Result<()> {
    let url = format!("https://api.github.com/repos/{}/dispatches", st.cfg.github_repo);
    let mut cp = serde_json::Map::new();
    cp.insert("build_id".into(), json!(build_id));
    cp.insert("defconfig".into(), json!(defconfig));
    if let Some(c) = commit {
        cp.insert("commit".into(), json!(c));
    }
    let payload = json!({ "event_type": "web-build", "client_payload": cp });
    let resp = st
        .http
        .post(&url)
        .bearer_auth(github_token(st).await)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&payload)
        .send()
        .await?;
    if resp.status().is_success() {
        Ok(())
    } else {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("dispatch {code}: {body}")
    }
}

async fn cancel_run(st: &AppState, run_id: i64) -> anyhow::Result<()> {
    let url = format!("https://api.github.com/repos/{}/actions/runs/{}/cancel", st.cfg.github_repo, run_id);
    let _ = st
        .http
        .post(&url)
        .bearer_auth(github_token(st).await)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await;
    Ok(())
}

async fn delete_run(st: &AppState, run_id: i64) -> anyhow::Result<()> {
    let url = format!("https://api.github.com/repos/{}/actions/runs/{}", st.cfg.github_repo, run_id);
    let _ = st
        .http
        .delete(&url)
        .bearer_auth(github_token(st).await)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await;
    Ok(())
}

async fn delete_release_assets(st: &AppState, build_id: &str) -> anyhow::Result<()> {
    let url = format!("https://api.github.com/repos/{}/releases/tags/{}", st.cfg.github_repo, st.cfg.rolling_tag);
    let v: serde_json::Value = st
        .http
        .get(&url)
        .bearer_auth(github_token(st).await)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?
        .json()
        .await?;
    let targets = [format!("{build_id}.bin"), format!("{build_id}.bin.sha256sum")];
    if let Some(assets) = v["assets"].as_array() {
        for a in assets {
            if let (Some(name), Some(aid)) = (a["name"].as_str(), a["id"].as_i64()) {
                if targets.iter().any(|t| t == name) {
                    let durl = format!("https://api.github.com/repos/{}/releases/assets/{}", st.cfg.github_repo, aid);
                    let _ = st
                        .http
                        .delete(&durl)
                        .bearer_auth(github_token(st).await)
                        .header("Accept", "application/vnd.github+json")
                        .header("X-GitHub-Api-Version", "2022-11-28")
                        .send()
                        .await;
                }
            }
        }
    }
    Ok(())
}
