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
    github_token: String,
    github_repo: String,
    rolling_tag: String,
    per_user_hourly: i64,
    per_ip_hourly: i64,
    max_concurrent: i64,
    max_queue: i64,
    retention_secs: i64,
    failed_retention_secs: i64,
    build_timeout_secs: i64,
    ip_header: Option<String>,
    admin_token: Option<String>,
}

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    http: reqwest::Client,
    cfg: Arc<Config>,
    defconfigs: Arc<HashSet<String>>,
    defconfigs_list: Arc<Vec<String>>,
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
fn admin_ok(headers: &HeaderMap, cfg: &Config) -> bool {
    let Some(token) = &cfg.admin_token else {
        return false;
    };
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(|p| constant_time_eq(p.as_bytes(), token.as_bytes()))
        .unwrap_or(false)
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

    let cfg = Config {
        github_token: std::env::var("GITHUB_TOKEN").map_err(|_| anyhow::anyhow!("GITHUB_TOKEN required"))?,
        github_repo: std::env::var("GITHUB_REPO").map_err(|_| anyhow::anyhow!("GITHUB_REPO required (owner/repo)"))?,
        rolling_tag: env_or("ROLLING_TAG", "web-builds"),
        per_user_hourly: env_i64("PER_USER_HOURLY_LIMIT", 2),
        per_ip_hourly: env_i64("PER_IP_HOURLY_LIMIT", 3),
        max_concurrent: env_i64("MAX_CONCURRENT_BUILDS", 6),
        max_queue: env_i64("MAX_QUEUE", 50),
        retention_secs: env_i64("RETENTION_SECS", 1800),
        failed_retention_secs: env_i64("FAILED_RETENTION_SECS", 3600),
        build_timeout_secs: env_i64("BUILD_TIMEOUT_SECS", 5400),
        ip_header: std::env::var("IP_HEADER").ok().filter(|s| !s.is_empty()),
        admin_token: std::env::var("ADMIN_TOKEN").ok().filter(|s| !s.is_empty()),
    };
    let bind_addr = env_or("BIND_ADDR", "[::]:8080");
    let static_dir = env_or("STATIC_DIR", "web");
    let db_path = env_or("DB_PATH", "broker.db");
    let defconfigs_path = env_or("DEFCONFIGS_PATH", "defconfigs.json");

    let raw = std::fs::read_to_string(&defconfigs_path)
        .map_err(|e| anyhow::anyhow!("reading {defconfigs_path}: {e}"))?;
    let list: Vec<String> = serde_json::from_str(&raw)?;
    let defconfigs: HashSet<String> = list.iter().cloned().collect();
    tracing::info!("loaded {} defconfigs", list.len());

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
            finished_ts INTEGER
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

    let http = reqwest::Client::builder()
        .user_agent("thingino-web-builder-broker")
        .build()?;

    let state = AppState {
        db: Arc::new(Mutex::new(conn)),
        http,
        cfg: Arc::new(cfg),
        defconfigs: Arc::new(defconfigs),
        defconfigs_list: Arc::new(list),
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
    Json(st.defconfigs_list.as_ref().clone()).into_response()
}

async fn get_stats(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let uid = resolve_uid(&headers);
    let now_ts = now();
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
    if !st.defconfigs.contains(&defconfig) {
        return json_err(StatusCode::BAD_REQUEST, "unknown defconfig");
    }
    let uid = resolve_uid(&headers);
    let ip = ip_bucket(client_ip(&headers, peer, &st.cfg.ip_header));
    let now_ts = now();
    let cutoff = now_ts - WINDOW_SECS;
    let build_id = Uuid::new_v4().to_string();

    let position: i64 = {
        let conn = st.db.lock().unwrap();
        if !builds_enabled(&conn) {
            return json_uid(StatusCode::SERVICE_UNAVAILABLE, &uid, json!({"error": "builds are temporarily disabled"}));
        }
        let queued_now: i64 = conn.query_row("SELECT count(*) FROM builds WHERE state='queued'", [], |r| r.get(0)).unwrap_or(0);
        if queued_now >= st.cfg.max_queue {
            return json_uid(StatusCode::SERVICE_UNAVAILABLE, &uid, json!({"error": "the build queue is full, try again shortly"}));
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
            "INSERT INTO builds(id, uid, ip_bucket, defconfig, state, created_ts) VALUES (?1,?2,?3,?4,'queued',?5)",
            rusqlite::params![build_id, uid, ip, defconfig, now_ts],
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
            "SELECT defconfig, state, created_ts, dispatched_ts, finished_ts FROM builds WHERE id=?1",
            [&build_id],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, Option<i64>>(3)?, r.get::<_, Option<i64>>(4)?)),
        )
        .optional().ok().flatten();
    let Some((defconfig, state, created_ts, dispatched_ts, finished_ts)) = row else {
        return json_err(StatusCode::NOT_FOUND, "unknown build");
    };
    let position: i64 = if state == "queued" {
        conn.query_row("SELECT count(*) FROM builds WHERE state='queued' AND created_ts <= ?1", [created_ts], |r| r.get(0)).unwrap_or(1)
    } else {
        0
    };
    drop(conn);
    let elapsed = match state.as_str() {
        "running" => dispatched_ts.map(|d| now_ts - d).unwrap_or(0),
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

async fn admin_stats(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !admin_ok(&headers, &st.cfg) {
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
    if !admin_ok(&headers, &st.cfg) {
        return json_err(StatusCode::UNAUTHORIZED, "admin auth required");
    }
    let conn = st.db.lock().unwrap();
    set_setting(&conn, "builds_enabled", if body.enabled { "1" } else { "0" });
    log_event(&conn, "admin_toggle", None, None, None, &format!("builds_enabled={}", body.enabled));
    drop(conn);
    Json(json!({ "builds_enabled": body.enabled })).into_response()
}

// ---- query helpers --------------------------------------------------------

fn latest_user_build(conn: &Connection, cfg: &Config, uid: &str, now_ts: i64) -> Option<serde_json::Value> {
    let (id, defconfig, state, created_ts, dispatched_ts, finished_ts) = conn
        .query_row(
            "SELECT id, defconfig, state, created_ts, dispatched_ts, finished_ts FROM builds WHERE uid=?1 ORDER BY created_ts DESC LIMIT 1",
            [uid],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, i64>(3)?, r.get::<_, Option<i64>>(4)?, r.get::<_, Option<i64>>(5)?)),
        )
        .optional().ok().flatten()?;
    let position: i64 = if state == "queued" {
        conn.query_row("SELECT count(*) FROM builds WHERE state='queued' AND created_ts <= ?1", [created_ts], |r| r.get(0)).unwrap_or(1)
    } else {
        0
    };
    let elapsed = match state.as_str() {
        "running" => dispatched_ts.map(|d| now_ts - d).unwrap_or(0),
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
        "SELECT id, defconfig, state, created_ts, dispatched_ts, finished_ts, run_id FROM builds ORDER BY created_ts DESC LIMIT ?1",
    ) else {
        return vec![];
    };
    let it = stmt.query_map([limit], |r| {
        Ok(json!({
            "build_id": r.get::<_, String>(0)?,
            "defconfig": r.get::<_, String>(1)?,
            "state": r.get::<_, String>(2)?,
            "created_ts": r.get::<_, i64>(3)?,
            "dispatched_ts": r.get::<_, Option<i64>>(4)?,
            "finished_ts": r.get::<_, Option<i64>>(5)?,
            "run_id": r.get::<_, Option<i64>>(6)?,
        }))
    });
    match it {
        Ok(rows) => rows.filter_map(|x| x.ok()).collect(),
        Err(_) => vec![],
    }
}

fn query_recent_events(conn: &Connection, limit: i64) -> Vec<serde_json::Value> {
    let Ok(mut stmt) = conn.prepare("SELECT ts, kind, build_id, detail FROM events ORDER BY id DESC LIMIT ?1") else {
        return vec![];
    };
    let it = stmt.query_map([limit], |r| {
        Ok(json!({
            "ts": r.get::<_, i64>(0)?,
            "kind": r.get::<_, String>(1)?,
            "build_id": r.get::<_, Option<String>>(2)?,
            "detail": r.get::<_, String>(3)?,
        }))
    });
    match it {
        Ok(rows) => rows.filter_map(|x| x.ok()).collect(),
        Err(_) => vec![],
    }
}

// ---- scheduler ------------------------------------------------------------

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

    // 1) Snapshot running builds + the next queued builds to dispatch.
    let (running, to_dispatch): (Vec<(String, Option<i64>, i64, bool)>, Vec<(String, String)>) = {
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
        let to_dispatch: Vec<(String, String)> = if slots > 0 {
            let mut q = conn.prepare("SELECT id, defconfig FROM builds WHERE state='queued' ORDER BY created_ts ASC LIMIT ?1")?;
            let rows = q
                .query_map([slots], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
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
            if let Some(r) = matched {
                let _ = cancel_run(st, r.run_id).await;
            }
            let conn = st.db.lock().unwrap();
            conn.execute("UPDATE builds SET state='cancelled', finished_ts=?2 WHERE id=?1", rusqlite::params![id, now_ts]).ok();
            log_event(&conn, "cancelled", Some(id), None, None, "cancelled while running");
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
    for (id, defconfig) in &to_dispatch {
        let still_queued: bool = {
            let conn = st.db.lock().unwrap();
            conn.query_row("SELECT 1 FROM builds WHERE id=?1 AND state='queued'", [id], |_| Ok(())).optional().ok().flatten().is_some()
        };
        if !still_queued {
            continue;
        }
        match dispatch_build(st, id, defconfig).await {
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
        .bearer_auth(&st.cfg.github_token)
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

async fn dispatch_build(st: &AppState, build_id: &str, defconfig: &str) -> anyhow::Result<()> {
    let url = format!("https://api.github.com/repos/{}/dispatches", st.cfg.github_repo);
    let payload = json!({ "event_type": "web-build", "client_payload": { "build_id": build_id, "defconfig": defconfig } });
    let resp = st
        .http
        .post(&url)
        .bearer_auth(&st.cfg.github_token)
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
        .bearer_auth(&st.cfg.github_token)
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
        .bearer_auth(&st.cfg.github_token)
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
        .bearer_auth(&st.cfg.github_token)
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
                        .bearer_auth(&st.cfg.github_token)
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
