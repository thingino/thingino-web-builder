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
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

// parking_lot mutexes don't poison: a panic while holding the lock releases it
// cleanly instead of wedging every later `.lock()` (which would kill the scheduler).
use parking_lot::Mutex;

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::json;
use tower_http::services::ServeDir;
use uuid::Uuid;

const WINDOW_SECS: i64 = 3600;
const DAY_SECS: i64 = 86400;
const THINGINO_CACHE_SECS: i64 = 300; // trust a resolved commit/list this long
const SESSION_TTL_SECS: i64 = 8 * 3600;
const TOKEN_REFRESH_SECS: i64 = 3300; // re-mint the App token before its ~1h expiry
const RELEASE_CACHE_SECS: i64 = 600;
const EVENT_TTL_SECS: i64 = 30 * DAY_SECS; // prune audit events older than this
const EXPIRED_BUILD_TTL_SECS: i64 = 7 * DAY_SECS; // drop long-expired build rows
const LOGIN_FAIL_WINDOW_SECS: i64 = 900; // admin-login throttle window
const LOGIN_FAIL_MAX: i64 = 10; // ...and max failures within it per IP

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
    update_marker: Option<String>,
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
    sessions: Arc<Mutex<std::collections::HashMap<String, (String, i64)>>>,
    installation_token: Arc<Mutex<(Option<String>, i64)>>,
    latest_release: Arc<Mutex<(Option<String>, i64)>>,
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

/// Apply the standard GitHub REST headers to a request builder.
fn gh(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    req.header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
}

/// Builder version: Cargo package version + git short-sha (baked via BUILD_SHA at build time).
fn version_string() -> String {
    match option_env!("BUILD_SHA") {
        // A release tag (e.g. v1.2.0) IS the version.
        Some(t) if t.starts_with('v') && t.as_bytes().get(1).is_some_and(|b| b.is_ascii_digit()) => t.to_string(),
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
    // HttpOnly: JS never reads it (the uid is mirrored from the JSON body to
    // localStorage). Secure: production is HTTPS; on plain-HTTP dev the localStorage
    // mirror still carries the uid via X-Builder-Uid.
    let cookie = format!("uid={uid}; Path=/; Max-Age=31536000; SameSite=Lax; Secure; HttpOnly");
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

#[allow(clippy::too_many_arguments)]
fn log_event(
    conn: &Connection,
    kind: &str,
    build_id: Option<&str>,
    uid: Option<&str>,
    ip: Option<&str>,
    detail: &str,
    ip_full: Option<&str>,
) {
    let _ = conn.execute(
        "INSERT INTO events(ts, kind, build_id, uid, ip_bucket, ip_full, detail) VALUES (?1,?2,?3,?4,?5,?6,?7)",
        rusqlite::params![now(), kind, build_id, uid, ip, ip_full, detail],
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

/// Admin requests authenticate with a session token minted by /api/admin/login.
/// Returns the session's admin identity ("master" for the break-glass token, else
/// a username), or None when the token is missing/expired/unknown.
fn session_admin(headers: &HeaderMap, st: &AppState) -> Option<String> {
    let tok = bearer(headers)?;
    let now_ts = now();
    let mut s = st.sessions.lock();
    s.retain(|_, (_, exp)| *exp > now_ts);
    s.get(tok).filter(|(_, exp)| *exp > now_ts).map(|(id, _)| id.clone())
}
fn session_ok(headers: &HeaderMap, st: &AppState) -> bool {
    session_admin(headers, st).is_some()
}
/// Gate a master-only endpoint: returns `Some(error response)` for any non-master
/// identity (including unauthenticated), matching the Worker's `!== "master"` check;
/// `None` means the caller is the master and may proceed.
fn require_master(headers: &HeaderMap, st: &AppState) -> Option<Response> {
    match session_admin(headers, st) {
        Some(id) if id == "master" => None,
        _ => Some(json_err(StatusCode::FORBIDDEN, "master token required")),
    }
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

// ---- account crypto (passwords, TOTP secrets, invite tokens) --------------

/// RFC4648 base32 encode, no padding (matches the Worker's base32Encode), used to
/// render a freshly-generated TOTP secret for the authenticator app.
fn base32_encode(bytes: &[u8]) -> String {
    const ALPHA: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut bits = 0u32;
    let mut val = 0u32;
    let mut out = String::new();
    for &b in bytes {
        val = (val << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHA[((val >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHA[((val << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn rand_bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    getrandom::getrandom(&mut v).expect("getrandom");
    v
}
/// Invite token: 24 random bytes, hex-encoded (matches the Worker's randToken).
fn rand_token() -> String {
    rand_bytes(24).iter().map(|b| format!("{b:02x}")).collect()
}
/// A fresh base32 TOTP secret from 20 random bytes (matches the Worker's newTotpSecret).
fn new_totp_secret() -> String {
    base32_encode(&rand_bytes(20))
}

const PBKDF2_ITERS: u32 = 100_000;
fn pbkdf2_sha256(password: &[u8], salt: &[u8], iters: u32) -> [u8; 32] {
    use pbkdf2::pbkdf2_hmac;
    use sha2::Sha256;
    let mut out = [0u8; 32];
    pbkdf2_hmac::<Sha256>(password, salt, iters, &mut out);
    out
}
/// Stored as "iters.saltB64.hashB64" (PBKDF2-HMAC-SHA256, standard base64 with
/// padding) so the work factor can change without breaking old hashes — byte-for-byte
/// the same format the Worker writes (btoa = standard base64).
fn hash_password(password: &str) -> String {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let salt = rand_bytes(16);
    let hash = pbkdf2_sha256(password.as_bytes(), &salt, PBKDF2_ITERS);
    format!("{}.{}.{}", PBKDF2_ITERS, STANDARD.encode(&salt), STANDARD.encode(hash))
}
fn verify_password(password: &str, stored: &str) -> bool {
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    let parts: Vec<&str> = stored.split('.').collect();
    if parts.len() != 3 || parts[0].is_empty() || parts[1].is_empty() || parts[2].is_empty() {
        return false;
    }
    let Ok(iters) = parts[0].parse::<u32>() else { return false; };
    let Ok(salt) = STANDARD.decode(parts[1]) else { return false; };
    let recomputed = STANDARD.encode(pbkdf2_sha256(password.as_bytes(), &salt, iters));
    // Compare the recomputed base64 string to the stored one, like the Worker's ctEq.
    constant_time_eq(recomputed.as_bytes(), parts[2].as_bytes())
}

/// `^[a-z0-9_.-]{3,32}$` — the admin username charset (validated after lowercasing).
fn valid_username(s: &str) -> bool {
    let n = s.len();
    (3..=32).contains(&n)
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '.' || c == '-')
}

/// encodeURIComponent-equivalent (escapes everything outside the JS unreserved set),
/// so the otpauth URL is byte-identical to the Worker's.
fn encode_uri_component(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*' | b'\'' | b'(' | b')' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
/// otpauth:// URL for the invitee's authenticator (matches the Worker's inviteOtpauth).
fn invite_otpauth(username: &str, secret: &str) -> String {
    let issuer = "thingino web-builder";
    format!(
        "otpauth://totp/{}:{}?secret={}&issuer={}&algorithm=SHA1&digits=6&period=30",
        encode_uri_component(issuer),
        encode_uri_component(username),
        secret,
        encode_uri_component(issuer),
    )
}

// ---- runtime limits (env defaults + D1/SQLite override) -------------------

/// Effective rate-limit knobs: the env/Config defaults, with the admin-set `limits`
/// JSON setting layered on top (matches the Worker's `limits(env)`).
#[derive(Clone, Copy)]
struct Limits {
    user_hourly: i64,
    ip_hourly: i64,
    global_hourly: i64,
    max_concurrent: i64,
    max_queue: i64,
    retention: i64,
    failed_retention: i64,
    build_timeout: i64,
}

fn effective_limits(conn: &Connection, cfg: &Config) -> Limits {
    let mut l = Limits {
        user_hourly: cfg.per_user_hourly,
        ip_hourly: cfg.per_ip_hourly,
        global_hourly: cfg.global_hourly,
        max_concurrent: cfg.max_concurrent,
        max_queue: cfg.max_queue,
        retention: cfg.retention_secs,
        failed_retention: cfg.failed_retention_secs,
        build_timeout: cfg.build_timeout_secs,
    };
    if let Some(ov) = get_setting(conn, "limits") {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&ov) {
            let g = |key: &str| v.get(key).and_then(serde_json::Value::as_i64);
            if let Some(n) = g("userHourly") { l.user_hourly = n; }
            if let Some(n) = g("ipHourly") { l.ip_hourly = n; }
            if let Some(n) = g("globalHourly") { l.global_hourly = n; }
            if let Some(n) = g("maxConcurrent") { l.max_concurrent = n; }
            if let Some(n) = g("maxQueue") { l.max_queue = n; }
            if let Some(n) = g("retention") { l.retention = n; }
            if let Some(n) = g("failedRetention") { l.failed_retention = n; }
            if let Some(n) = g("buildTimeout") { l.build_timeout = n; }
        }
    }
    l
}

/// JS `parseInt(x, 10)`-equivalent over a JSON value (number truncated toward zero,
/// or a leading-integer parse of a string), used to validate the `limits` POST body.
fn js_parse_int(v: &serde_json::Value) -> Option<i64> {
    if let Some(n) = v.as_i64() {
        return Some(n);
    }
    if let Some(f) = v.as_f64() {
        return if f.is_finite() { Some(f.trunc() as i64) } else { None };
    }
    if let Some(s) = v.as_str() {
        let t = s.trim_start();
        let bytes = t.as_bytes();
        let mut i = 0;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        let digits_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == digits_start {
            return None;
        }
        return t[..i].parse::<i64>().ok();
    }
    None
}

// ---- main -----------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // Singleton guard — refuse to start if another broker already holds the lock.
    let lock_path = env_or("LOCK_PATH", "broker.lock");
    let lock_file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
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
        update_marker: std::env::var("UPDATE_MARKER_PATH").ok().filter(|s| !s.is_empty()),
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
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS builds(
            id TEXT PRIMARY KEY,
            uid TEXT NOT NULL,
            ip_bucket TEXT NOT NULL,
            ip_full TEXT,
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
            ip_full TEXT,
            detail TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);
        CREATE TABLE IF NOT EXISTS settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS admins(
            username TEXT PRIMARY KEY,
            pw_hash TEXT,
            totp_secret TEXT NOT NULL,
            invite_token TEXT,
            invite_expires INTEGER,
            disabled INTEGER NOT NULL DEFAULT 0,
            created_ts INTEGER NOT NULL,
            created_by TEXT,
            last_login INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_admins_invite ON admins(invite_token);",
    )?;
    // Idempotent migrations for older DBs (swallow the "duplicate column" error on re-run).
    let _ = conn.execute("ALTER TABLE builds ADD COLUMN commit_sha TEXT", []);
    let _ = conn.execute("ALTER TABLE builds ADD COLUMN ip_full TEXT", []);
    let _ = conn.execute("ALTER TABLE events ADD COLUMN ip_full TEXT", []);

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
        latest_release: Arc::new(Mutex::new((None, 0))),
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
        .route("/api/admin/cancel/{build_id}", post(admin_cancel))
        .route("/api/admin/expire/{build_id}", post(admin_expire))
        .route("/api/admin/login", post(admin_login))
        .route("/api/admin/stats", get(admin_stats))
        .route("/api/admin/toggle", post(admin_toggle))
        .route("/api/admin/clear-logs", post(admin_clear_logs))
        .route("/api/admin/reset-limits", post(admin_reset_limits))
        .route("/api/admin/limits", post(admin_limits))
        .route("/api/admin/update", post(admin_update))
        .route("/api/admin/users", post(admin_invite).get(admin_list_users))
        .route("/api/admin/users/{username}", delete(admin_delete_user))
        .route("/api/admin/users/{username}/disable", post(admin_disable_user))
        .route("/api/admin/invite/{token}", get(admin_get_invite))
        .route("/api/admin/accept-invite", post(admin_accept_invite))
        .route("/api/admin/logout", post(admin_logout))
        .fallback_service(ServeDir::new(&static_dir).append_index_html_on_directories(true))
        .with_state(state);

    let addr: SocketAddr = bind_addr.parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("broker listening on http://{addr}");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolve on SIGINT or SIGTERM so `systemctl stop/restart` drains cleanly.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    tracing::info!("shutdown signal received");
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
    let conn = st.db.lock();
    let max_concurrent = effective_limits(&conn, &st.cfg).max_concurrent;
    let running: i64 = conn.query_row("SELECT count(*) FROM builds WHERE state='running'", [], |r| r.get(0)).unwrap_or(0);
    let queued: i64 = conn.query_row("SELECT count(*) FROM builds WHERE state='queued'", [], |r| r.get(0)).unwrap_or(0);
    let avg: Option<f64> = conn
        .query_row(
            "SELECT avg(finished_ts - dispatched_ts) FROM builds WHERE state='done' AND finished_ts IS NOT NULL AND dispatched_ts IS NOT NULL AND finished_ts > ?1",
            [now_ts - DAY_SECS],
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
            "max_concurrent": max_concurrent,
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
    let client = client_ip(&headers, peer, &st.cfg.ip_header);
    let ip_full = client.to_string();
    let ip = ip_bucket(client);
    let now_ts = now();
    let build_id = Uuid::new_v4().to_string();
    let commit = thingino.commit.clone();

    let position: i64 = {
        let conn = st.db.lock();
        if !builds_enabled(&conn) {
            return json_uid(StatusCode::SERVICE_UNAVAILABLE, &uid, json!({"error": "builds are temporarily disabled"}));
        }
        let lim = effective_limits(&conn, &st.cfg);
        // Hourly window, but never count builds created before an admin "reset limits".
        let reset_ts = get_setting(&conn, "limits_reset_ts").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
        let cutoff = (now_ts - WINDOW_SECS).max(reset_ts);
        // Dedup: same (defconfig, commit) already built (within retention) or in flight → reuse it.
        if let Some(c) = commit.as_deref() {
            if let Some((eid, estate, edl)) = find_existing(&conn, &defconfig, c, now_ts - lim.retention, &st.cfg) {
                log_event(&conn, "dedup", Some(&eid), Some(&uid), Some(&ip), &format!("reused {estate} for {defconfig}"), Some(&ip_full));
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
        if queued_now >= lim.max_queue {
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
        if global_n >= lim.global_hourly {
            log_event(&conn, "rate_limited", None, Some(&uid), Some(&ip), "global hourly limit", Some(&ip_full));
            return json_uid(StatusCode::TOO_MANY_REQUESTS, &uid, json!({"error": format!("the builder is at its hourly limit ({}/hr) — try again later", lim.global_hourly)}));
        }
        // Per-user cap. NB: uid is client-supplied, so this is a soft/UX limit — the
        // per-IP and global caps are the real enforcement. Builds count toward a limit
        // unless they were cancelled before ever dispatching.
        let user_n: i64 = conn
            .query_row(
                "SELECT count(*) FROM builds WHERE uid=?1 AND created_ts > ?2 AND NOT (state='cancelled' AND dispatched_ts IS NULL)",
                rusqlite::params![uid, cutoff],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if user_n >= lim.user_hourly {
            log_event(&conn, "rate_limited", None, Some(&uid), Some(&ip), "per-user hourly limit", Some(&ip_full));
            return json_uid(StatusCode::TOO_MANY_REQUESTS, &uid, json!({"error": format!("you've reached {} builds this hour — try again later", lim.user_hourly)}));
        }
        let ip_n: i64 = conn
            .query_row(
                "SELECT count(*) FROM builds WHERE ip_bucket=?1 AND created_ts > ?2 AND NOT (state='cancelled' AND dispatched_ts IS NULL)",
                rusqlite::params![ip, cutoff],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if ip_n >= lim.ip_hourly {
            log_event(&conn, "rate_limited", None, Some(&uid), Some(&ip), "per-ip hourly limit", Some(&ip_full));
            return json_uid(StatusCode::TOO_MANY_REQUESTS, &uid, json!({"error": "too many builds from your network this hour — try again later"}));
        }
        if let Err(e) = conn.execute(
            "INSERT INTO builds(id, uid, ip_bucket, ip_full, defconfig, state, created_ts, commit_sha) VALUES (?1,?2,?3,?4,?5,'queued',?6,?7)",
            rusqlite::params![build_id, uid, ip, ip_full, defconfig, now_ts, commit],
        ) {
            tracing::error!("insert failed: {e}");
            return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
        }
        log_event(&conn, "queued", Some(&build_id), Some(&uid), Some(&ip), &defconfig, Some(&ip_full));
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
    let conn = st.db.lock();
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

/// Shared cancel logic: queued → cancelled; running → set cancel_requested and try to
/// stop the GitHub run inline (the scheduler retries otherwise). Returns the new state.
/// Mirrors the Worker's doCancel.
async fn do_cancel(st: &AppState, id: &str, state: &str, run_id: Option<i64>, uid: Option<&str>) -> String {
    let now_ts = now();
    if state == "queued" {
        let conn = st.db.lock();
        conn.execute("UPDATE builds SET state='cancelled', finished_ts=?2 WHERE id=?1", rusqlite::params![id, now_ts]).ok();
        log_event(&conn, "cancelled", Some(id), uid, None, "cancelled while queued", None);
        return "cancelled".to_string();
    }
    if state == "running" {
        {
            let conn = st.db.lock();
            conn.execute("UPDATE builds SET cancel_requested=1 WHERE id=?1", [id]).ok();
        }
        // Try to stop the run now; if the runs list doesn't show it yet, the scheduler retries.
        let mut note = "cancel queued (run not yet listed)";
        if let Ok(runs) = fetch_runs(st).await {
            if let Some(m) = runs.iter().find(|r| run_id.map(|rid| rid == r.run_id).unwrap_or(false) || r.name.contains(id)) {
                let _ = cancel_run(st, m.run_id).await;
                let conn = st.db.lock();
                conn.execute("UPDATE builds SET run_id=?2 WHERE id=?1", rusqlite::params![id, m.run_id]).ok();
                note = "cancel sent to run";
            }
        }
        let conn = st.db.lock();
        log_event(&conn, "cancel_requested", Some(id), uid, None, note, None);
        return "cancelling".to_string();
    }
    "already finished".to_string()
}

async fn post_cancel(State(st): State<AppState>, headers: HeaderMap, Path(build_id): Path<String>) -> Response {
    if !valid_build_id(&build_id) {
        return json_err(StatusCode::BAD_REQUEST, "bad build_id");
    }
    let uid = resolve_uid(&headers);
    let row = {
        let conn = st.db.lock();
        conn.query_row("SELECT uid, state, run_id FROM builds WHERE id=?1", [&build_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?))
        })
        .optional().ok().flatten()
    };
    let Some((owner, state, run_id)) = row else {
        return json_err(StatusCode::NOT_FOUND, "unknown build");
    };
    if owner != uid {
        return json_err(StatusCode::FORBIDDEN, "not your build");
    }
    let new_state = do_cancel(&st, &build_id, &state, run_id, Some(&uid)).await;
    json_uid(StatusCode::OK, &uid, json!({ "state": new_state }))
}

// ---- admin ----------------------------------------------------------------

#[derive(Deserialize)]
struct LoginReq {
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: String,
    #[serde(default)]
    token: String,
    #[serde(default)]
    totp: String,
}

/// Exchange credentials for a session token. Two paths, matching the Worker: a named
/// admin (username + password (PBKDF2) + that user's own TOTP), or master break-glass
/// (the ADMIN_TOKEN + the master TOTP secret). On success the session carries the
/// identity ("master" or the username).
async fn admin_login(
    State(st): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginReq>,
) -> Response {
    let client = client_ip(&headers, peer, &st.cfg.ip_header);
    let ip_full = client.to_string();
    let ip = ip_bucket(client);
    // Throttle brute force: too many recent failures from this IP bucket → reject early.
    {
        let conn = st.db.lock();
        let fails: i64 = conn
            .query_row(
                "SELECT count(*) FROM events WHERE kind='admin_login_fail' AND ip_bucket=?1 AND ts > ?2",
                rusqlite::params![ip, now() - LOGIN_FAIL_WINDOW_SECS],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if fails >= LOGIN_FAIL_MAX {
            log_event(&conn, "admin_login_throttled", None, None, Some(&ip), "too many failed logins", Some(&ip_full));
            return json_err(StatusCode::TOO_MANY_REQUESTS, "too many attempts — try again later");
        }
    }
    let totp = body.totp.trim();
    let username = body.username.as_deref().filter(|s| !s.is_empty());
    let identity: Option<String> = if let Some(uname) = username {
        // Named admin: username + password + their own TOTP (all enforced).
        let u = uname.to_lowercase();
        let row = {
            let conn = st.db.lock();
            conn.query_row(
                "SELECT pw_hash, totp_secret, disabled FROM admins WHERE username=?1",
                [&u],
                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)),
            )
            .optional().ok().flatten()
        };
        match row {
            Some((Some(pw_hash), totp_secret, disabled))
                if disabled == 0 && verify_password(&body.password, &pw_hash) && totp_check(&totp_secret, totp) =>
            {
                let conn = st.db.lock();
                conn.execute("UPDATE admins SET last_login=?2 WHERE username=?1", rusqlite::params![u, now()]).ok();
                Some(u)
            }
            _ => None,
        }
    } else if let (Some(admin_token), Some(secret)) = (st.cfg.admin_token.as_deref(), st.cfg.admin_totp_secret.as_deref()) {
        // Master break-glass: token + master TOTP (env secrets, independent of the DB).
        if constant_time_eq(body.token.as_bytes(), admin_token.as_bytes()) && totp_check(secret, totp) {
            Some("master".to_string())
        } else {
            None
        }
    } else {
        None
    };

    let Some(identity) = identity else {
        let conn = st.db.lock();
        let detail = match username {
            Some(u) => format!("bad login ({})", u.to_lowercase()),
            None => "bad token or 2FA".to_string(),
        };
        log_event(&conn, "admin_login_fail", None, None, Some(&ip), &detail, Some(&ip_full));
        return json_err(StatusCode::UNAUTHORIZED, "invalid credentials");
    };

    let session = Uuid::new_v4().to_string();
    st.sessions.lock().insert(session.clone(), (identity.clone(), now() + SESSION_TTL_SECS));
    {
        let conn = st.db.lock();
        log_event(&conn, "admin_login_ok", None, None, Some(&ip), &format!("session created ({identity})"), Some(&ip_full));
    }
    let master = identity == "master";
    Json(json!({ "session": session, "expires_in": SESSION_TTL_SECS, "admin": identity, "master": master })).into_response()
}

async fn admin_stats(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(me) = session_admin(&headers, &st) else {
        return json_err(StatusCode::UNAUTHORIZED, "admin auth required");
    };
    let master = me == "master";
    let current = version_string();
    let latest = check_latest_release(&st).await;
    let update_available = latest.as_ref().is_some_and(|l| l != &current);
    let now_ts = now();
    let conn = st.db.lock();
    let lim = effective_limits(&conn, &st.cfg);
    let mut counts = serde_json::Map::new();
    let mut running_n = 0i64;
    let mut queued_n = 0i64;
    for s in ["queued", "running", "done", "failed", "cancelled", "expired"] {
        let n: i64 = conn.query_row("SELECT count(*) FROM builds WHERE state=?1", [s], |r| r.get(0)).unwrap_or(0);
        if s == "running" { running_n = n; }
        if s == "queued" { queued_n = n; }
        counts.insert(s.to_string(), json!(n));
    }
    let last24: i64 = conn.query_row("SELECT count(*) FROM builds WHERE created_ts > ?1", [now_ts - DAY_SECS], |r| r.get(0)).unwrap_or(0);
    let avg: Option<f64> = conn
        .query_row("SELECT avg(finished_ts - dispatched_ts) FROM builds WHERE state='done' AND finished_ts IS NOT NULL AND dispatched_ts IS NOT NULL", [], |r| r.get(0))
        .optional().ok().flatten();
    // Builds counted against the global hourly cap in the current (post-reset) window.
    let reset_ts = get_setting(&conn, "limits_reset_ts").and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
    let usage_cutoff = (now_ts - WINDOW_SECS).max(reset_ts);
    let usage_global: i64 = conn
        .query_row(
            "SELECT count(*) FROM builds WHERE created_ts > ?1 AND NOT (state='cancelled' AND dispatched_ts IS NULL)",
            [usage_cutoff],
            |r| r.get(0),
        )
        .unwrap_or(0);
    let recent_builds = query_recent_builds(&conn, 25);
    let recent_events = query_recent_events(&conn, 60);
    let enabled = builds_enabled(&conn);
    drop(conn);
    Json(json!({
        "builds_enabled": enabled,
        "counts": counts,
        "last24h": last24,
        "avg_build_secs": avg.map(|v| v.round() as i64),
        "max_concurrent": lim.max_concurrent,
        "retention_secs": lim.retention,
        "recent_builds": recent_builds,
        "recent_events": recent_events,
        "version": current,
        "latest_version": latest,
        "update_available": update_available,
        "limits": {
            "userHourly": lim.user_hourly,
            "ipHourly": lim.ip_hourly,
            "globalHourly": lim.global_hourly,
            "maxConcurrent": lim.max_concurrent,
            "maxQueue": lim.max_queue,
            "retention": lim.retention,
        },
        "usage": {
            "globalHourly": usage_global,
            "maxConcurrent": running_n,
            "maxQueue": queued_n,
        },
        "me": me,
        "master": master,
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
    let conn = st.db.lock();
    set_setting(&conn, "builds_enabled", if body.enabled { "1" } else { "0" });
    log_event(&conn, "admin_toggle", None, None, None, &format!("builds_enabled={}", body.enabled), None);
    drop(conn);
    Json(json!({ "builds_enabled": body.enabled })).into_response()
}

/// UI-triggered self-update: drop a marker file that a host systemd path-unit watches,
/// which runs `podman auto-update` (pull newer image + restart). The broker gets no
/// host/socket access — it only touches a file on a shared volume.
async fn admin_update(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !session_ok(&headers, &st) {
        return json_err(StatusCode::UNAUTHORIZED, "admin auth required");
    }
    let Some(path) = st.cfg.update_marker.as_deref() else {
        return json_err(StatusCode::NOT_IMPLEMENTED, "self-update is not configured on this deployment");
    };
    match std::fs::write(path, "update\n") {
        Ok(_) => {
            let conn = st.db.lock();
            log_event(&conn, "admin_update", None, None, None, "self-update requested", None);
            drop(conn);
            Json(json!({ "ok": true, "status": "update requested — the broker will restart on the new image shortly" })).into_response()
        }
        Err(e) => json_err(StatusCode::INTERNAL_SERVER_ERROR, &format!("could not write update marker: {e}")),
    }
}

/// Revoke the current admin session server-side (not just client-side).
async fn admin_logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(tok) = bearer(&headers) {
        st.sessions.lock().remove(tok);
    }
    Json(json!({ "ok": true })).into_response()
}

/// Admin: cancel any build (session-gated), attributed to the build's owner.
async fn admin_cancel(State(st): State<AppState>, headers: HeaderMap, Path(build_id): Path<String>) -> Response {
    if !session_ok(&headers, &st) {
        return json_err(StatusCode::UNAUTHORIZED, "admin auth required");
    }
    if !valid_build_id(&build_id) {
        return json_err(StatusCode::BAD_REQUEST, "bad build_id");
    }
    let row = {
        let conn = st.db.lock();
        conn.query_row("SELECT uid, state, run_id FROM builds WHERE id=?1", [&build_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?))
        })
        .optional().ok().flatten()
    };
    let Some((owner, state, run_id)) = row else {
        return json_err(StatusCode::NOT_FOUND, "unknown build");
    };
    {
        let conn = st.db.lock();
        log_event(&conn, "admin_cancel", Some(&build_id), Some(&owner), None, &format!("admin cancelled (was {state})"), None);
    }
    let new_state = do_cancel(&st, &build_id, &state, run_id, Some(&owner)).await;
    Json(json!({ "state": new_state })).into_response()
}

/// Admin: remove a finished build's artifact + Actions run early (the reaper's job, on demand).
async fn admin_expire(State(st): State<AppState>, headers: HeaderMap, Path(build_id): Path<String>) -> Response {
    if !session_ok(&headers, &st) {
        return json_err(StatusCode::UNAUTHORIZED, "admin auth required");
    }
    if !valid_build_id(&build_id) {
        return json_err(StatusCode::BAD_REQUEST, "bad build_id");
    }
    let row = {
        let conn = st.db.lock();
        conn.query_row("SELECT uid, state, run_id FROM builds WHERE id=?1", [&build_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?))
        })
        .optional().ok().flatten()
    };
    let Some((owner, state, run_id)) = row else {
        return json_err(StatusCode::NOT_FOUND, "unknown build");
    };
    if !matches!(state.as_str(), "done" | "failed" | "cancelled") {
        return json_err(StatusCode::BAD_REQUEST, "build is not finished");
    }
    let asset_ok = if state == "done" { delete_release_assets(&st, &build_id).await.is_ok() } else { true };
    let run_ok = match run_id {
        Some(rid) => delete_run(&st, rid).await.is_ok(),
        None => true,
    };
    if !(asset_ok && run_ok) {
        return json_err(StatusCode::BAD_GATEWAY, "GitHub cleanup failed; the cron will retry");
    }
    {
        let conn = st.db.lock();
        conn.execute("UPDATE builds SET state='expired' WHERE id=?1", [&build_id]).ok();
        log_event(&conn, "expired", Some(&build_id), Some(&owner), None, "admin removed early", None);
    }
    Json(json!({ "ok": true, "state": "expired" })).into_response()
}

/// Admin: wipe the audit log.
async fn admin_clear_logs(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !session_ok(&headers, &st) {
        return json_err(StatusCode::UNAUTHORIZED, "admin auth required");
    }
    let conn = st.db.lock();
    let cleared = conn.execute("DELETE FROM events", []).unwrap_or(0);
    log_event(&conn, "admin_clear_logs", None, None, None, &format!("cleared {cleared} events"), None);
    drop(conn);
    Json(json!({ "ok": true, "cleared": cleared })).into_response()
}

/// Admin: reset the hourly rate-limit window (mark "now"; queries ignore earlier builds).
async fn admin_reset_limits(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if !session_ok(&headers, &st) {
        return json_err(StatusCode::UNAUTHORIZED, "admin auth required");
    }
    let conn = st.db.lock();
    set_setting(&conn, "limits_reset_ts", &now().to_string());
    log_event(&conn, "admin_reset_limits", None, None, None, "hourly limits reset", None);
    drop(conn);
    Json(json!({ "ok": true })).into_response()
}

/// Admin: set runtime limit overrides (stored as the `limits` setting, layered over env).
async fn admin_limits(State(st): State<AppState>, headers: HeaderMap, raw: Bytes) -> Response {
    if !session_ok(&headers, &st) {
        return json_err(StatusCode::UNAUTHORIZED, "admin auth required");
    }
    let body: serde_json::Value = serde_json::from_slice(&raw).unwrap_or_else(|_| json!({}));
    let conn = st.db.lock();
    let cur = effective_limits(&conn, &st.cfg);
    // Each key: a positive int ≤ 100000 wins, else keep the current value.
    let pick = |key: &str, cur_val: i64| -> i64 {
        match js_parse_int(&body[key]) {
            Some(v) if v > 0 && v <= 100_000 => v,
            _ => cur_val,
        }
    };
    let next = json!({
        "userHourly": pick("userHourly", cur.user_hourly),
        "ipHourly": pick("ipHourly", cur.ip_hourly),
        "globalHourly": pick("globalHourly", cur.global_hourly),
        "maxConcurrent": pick("maxConcurrent", cur.max_concurrent),
        "maxQueue": pick("maxQueue", cur.max_queue),
        "retention": pick("retention", cur.retention),
    });
    let next_str = next.to_string();
    set_setting(&conn, "limits", &next_str);
    log_event(&conn, "admin_limits", None, None, None, &next_str, None);
    drop(conn);
    Json(json!({ "ok": true, "limits": next })).into_response()
}

// --- Admin user management (master only) + invite self-enrollment ----------

#[derive(Deserialize)]
struct InviteReq {
    #[serde(default)]
    username: String,
}

/// Master only: invite a new named admin (generates a one-time invite token + TOTP secret).
async fn admin_invite(State(st): State<AppState>, headers: HeaderMap, Json(body): Json<InviteReq>) -> Response {
    if let Some(e) = require_master(&headers, &st) {
        return e;
    }
    let u = body.username.to_lowercase().trim().to_string();
    if !valid_username(&u) {
        return json_err(StatusCode::BAD_REQUEST, "username must be 3-32 chars: a-z 0-9 . _ -");
    }
    let token = rand_token();
    let secret = new_totp_secret();
    let exp = now() + 3600; // invite valid 60 minutes
    let conn = st.db.lock();
    let exists = conn
        .query_row("SELECT 1 FROM admins WHERE username=?1", [&u], |_| Ok(()))
        .optional().ok().flatten().is_some();
    if exists {
        return json_err(StatusCode::CONFLICT, "that username already exists");
    }
    if conn
        .execute(
            "INSERT INTO admins(username, totp_secret, invite_token, invite_expires, created_ts, created_by) VALUES (?1,?2,?3,?4,?5,?6)",
            rusqlite::params![u, secret, token, exp, now(), "master"],
        )
        .is_err()
    {
        return json_err(StatusCode::INTERNAL_SERVER_ERROR, "internal error");
    }
    log_event(&conn, "admin_user_invited", None, None, None, &format!("invited {u}"), None);
    drop(conn);
    Json(json!({ "ok": true, "username": u, "invite_token": token, "expires_in": 3600 })).into_response()
}

/// Master only: list admin users with their state (active/invited/invite-expired/disabled).
async fn admin_list_users(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(e) = require_master(&headers, &st) {
        return e;
    }
    let conn = st.db.lock();
    let users = query_admin_users(&conn);
    drop(conn);
    Json(json!({ "users": users })).into_response()
}

/// Master only: delete a named admin + kill its live sessions.
async fn admin_delete_user(State(st): State<AppState>, headers: HeaderMap, Path(username): Path<String>) -> Response {
    if let Some(e) = require_master(&headers, &st) {
        return e;
    }
    let u = username.to_lowercase();
    let deleted = {
        let conn = st.db.lock();
        let n = conn.execute("DELETE FROM admins WHERE username=?1", [&u]).unwrap_or(0);
        log_event(&conn, "admin_user_deleted", None, None, None, &format!("deleted {u}"), None);
        n
    };
    st.sessions.lock().retain(|_, (id, _)| id != &u);
    Json(json!({ "ok": true, "deleted": deleted })).into_response()
}

#[derive(Deserialize)]
struct DisableReq {
    #[serde(default)]
    disabled: bool,
}

/// Master only: disable/enable a named admin (disabling also kills its sessions).
/// Not used by the current frontend; present for full Worker parity. Tolerates an
/// empty/invalid body (defaults to enabled), like the Worker's `catch { body = {} }`.
async fn admin_disable_user(State(st): State<AppState>, headers: HeaderMap, Path(username): Path<String>, raw: Bytes) -> Response {
    if let Some(e) = require_master(&headers, &st) {
        return e;
    }
    let disabled = serde_json::from_slice::<DisableReq>(&raw).map(|b| b.disabled).unwrap_or(false);
    let u = username.to_lowercase();
    {
        let conn = st.db.lock();
        conn.execute("UPDATE admins SET disabled=?2 WHERE username=?1", rusqlite::params![u, i64::from(disabled)]).ok();
        log_event(&conn, "admin_user_disabled", None, None, None, &format!("{} {}", if disabled { "disabled" } else { "enabled" }, u), None);
    }
    if disabled {
        st.sessions.lock().retain(|_, (id, _)| id != &u);
    }
    Json(json!({ "ok": true })).into_response()
}

/// Invite enrollment (no session — the invitee isn't an admin yet): fetch the TOTP secret.
async fn admin_get_invite(State(st): State<AppState>, Path(token): Path<String>) -> Response {
    let row = {
        let conn = st.db.lock();
        conn.query_row(
            "SELECT username, totp_secret, invite_expires, pw_hash FROM admins WHERE invite_token=?1",
            [&token],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<String>>(3)?)),
        )
        .optional().ok().flatten()
    };
    let Some((username, secret, invite_expires, pw_hash)) = row else {
        return json_err(StatusCode::NOT_FOUND, "invalid or already-used invite");
    };
    if pw_hash.is_some() {
        return json_err(StatusCode::NOT_FOUND, "invalid or already-used invite");
    }
    if invite_expires.unwrap_or(0) <= now() {
        return json_err(StatusCode::GONE, "this invite has expired");
    }
    let otpauth = invite_otpauth(&username, &secret);
    Json(json!({ "username": username, "secret": secret, "otpauth": otpauth })).into_response()
}

#[derive(Deserialize)]
struct AcceptReq {
    #[serde(default)]
    token: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    totp: String,
}

/// Invite enrollment: set the password (after verifying the TOTP) and clear the invite.
async fn admin_accept_invite(State(st): State<AppState>, Json(body): Json<AcceptReq>) -> Response {
    let row = {
        let conn = st.db.lock();
        conn.query_row(
            "SELECT username, totp_secret, invite_expires, pw_hash FROM admins WHERE invite_token=?1",
            [&body.token],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, Option<String>>(3)?)),
        )
        .optional().ok().flatten()
    };
    let Some((username, totp_secret, invite_expires, pw_hash)) = row else {
        return json_err(StatusCode::NOT_FOUND, "invalid or already-used invite");
    };
    if pw_hash.is_some() {
        return json_err(StatusCode::NOT_FOUND, "invalid or already-used invite");
    }
    if invite_expires.unwrap_or(0) <= now() {
        return json_err(StatusCode::GONE, "this invite has expired");
    }
    if body.password.chars().count() < 10 {
        return json_err(StatusCode::BAD_REQUEST, "password must be at least 10 characters");
    }
    if !totp_check(&totp_secret, body.totp.trim()) {
        return json_err(StatusCode::UNAUTHORIZED, "that 2FA code doesn't match — re-scan and try the next code");
    }
    let hash = hash_password(&body.password);
    let conn = st.db.lock();
    conn.execute(
        "UPDATE admins SET pw_hash=?2, invite_token=NULL, invite_expires=NULL WHERE username=?1",
        rusqlite::params![username, hash],
    ).ok();
    log_event(&conn, "admin_user_enrolled", None, None, None, &format!("{username} enrolled"), None);
    drop(conn);
    Json(json!({ "ok": true, "username": username })).into_response()
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
        "SELECT id, defconfig, state, created_ts, dispatched_ts, finished_ts, run_id, cancel_requested, uid, ip_bucket, ip_full FROM builds ORDER BY created_ts DESC LIMIT ?1",
    ) else {
        return vec![];
    };
    let it = stmt.query_map([limit], |r| {
        let real_state: String = r.get(2)?;
        let cancel_req: i64 = r.get(7)?;
        let state = if real_state == "running" && cancel_req != 0 { "cancelling".to_string() } else { real_state };
        let ip_bucket: String = r.get(9)?;
        let ip_full: Option<String> = r.get(10)?;
        // Full client IP when stored, falling back to the bucket (older rows have no ip_full).
        let ip = ip_full.filter(|s| !s.is_empty()).unwrap_or_else(|| ip_bucket.clone());
        Ok(json!({
            "build_id": r.get::<_, String>(0)?,
            "defconfig": r.get::<_, String>(1)?,
            "state": state,
            "created_ts": r.get::<_, i64>(3)?,
            "dispatched_ts": r.get::<_, Option<i64>>(4)?,
            "finished_ts": r.get::<_, Option<i64>>(5)?,
            "run_id": r.get::<_, Option<i64>>(6)?,
            "uid": r.get::<_, String>(8)?,
            "ip": ip,
            "ip_bucket": ip_bucket,
        }))
    });
    match it {
        Ok(rows) => rows.filter_map(|x| x.ok()).collect(),
        Err(_) => vec![],
    }
}

fn query_recent_events(conn: &Connection, limit: i64) -> Vec<serde_json::Value> {
    let Ok(mut stmt) = conn.prepare("SELECT ts, kind, build_id, detail, uid, ip_bucket, ip_full FROM events ORDER BY id DESC LIMIT ?1") else {
        return vec![];
    };
    let it = stmt.query_map([limit], |r| {
        let ip_bucket: Option<String> = r.get(5)?;
        let ip_full: Option<String> = r.get(6)?;
        let ip = ip_full.filter(|s| !s.is_empty()).or_else(|| ip_bucket.clone());
        Ok(json!({
            "ts": r.get::<_, i64>(0)?,
            "kind": r.get::<_, String>(1)?,
            "build_id": r.get::<_, Option<String>>(2)?,
            "detail": r.get::<_, String>(3)?,
            "uid": r.get::<_, Option<String>>(4)?,
            "ip": ip,
            "ip_bucket": ip_bucket,
        }))
    });
    match it {
        Ok(rows) => rows.filter_map(|x| x.ok()).collect(),
        Err(_) => vec![],
    }
}

fn query_admin_users(conn: &Connection) -> Vec<serde_json::Value> {
    let Ok(mut stmt) = conn.prepare(
        "SELECT username, pw_hash, invite_expires, disabled, created_ts, last_login FROM admins ORDER BY created_ts DESC",
    ) else {
        return vec![];
    };
    let now_ts = now();
    let it = stmt.query_map([], |r| {
        let username: String = r.get(0)?;
        let pw_hash: Option<String> = r.get(1)?;
        let invite_expires: Option<i64> = r.get(2)?;
        let disabled: i64 = r.get(3)?;
        let created_ts: i64 = r.get(4)?;
        let last_login: Option<i64> = r.get(5)?;
        let state = if disabled != 0 {
            "disabled"
        } else if pw_hash.is_some() {
            "active"
        } else if invite_expires.unwrap_or(0) > now_ts {
            "invited"
        } else {
            "invite-expired"
        };
        Ok(json!({
            "username": username,
            "state": state,
            "created_ts": created_ts,
            "last_login": last_login,
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
    match gh(st.http.get(&url)).send().await {
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
    let v: serde_json::Value = gh(st.http.get(&url)).send().await.ok()?.json().await.ok()?;
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
        let t = st.thingino.lock();
        if t.commit.is_some() && now() - t.fetched_at < THINGINO_CACHE_SECS {
            return t.clone();
        }
    }
    let commit = fetch_commit(st).await;
    let need_list = {
        let t = st.thingino.lock();
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
    let mut t = st.thingino.lock();
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
        // Isolate each step in its own task: a panic surfaces as a logged JoinError
        // instead of permanently killing the scheduler for the rest of the process.
        let st2 = st.clone();
        match tokio::spawn(async move { scheduler_step(&st2).await }).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!("scheduler step error: {e}"),
            Err(e) => tracing::error!("scheduler step panicked: {e}"),
        }
    }
}

struct RunRow {
    run_id: i64,
    name: String,
    status: String,
    conclusion: Option<String>,
}

/// (id, run_id, dispatched_ts, cancel_requested)
type RunningBuild = (String, Option<i64>, i64, bool);
/// (id, defconfig, commit_sha)
type QueuedBuild = (String, String, Option<String>);

async fn scheduler_step(st: &AppState) -> anyhow::Result<()> {
    let now_ts = now();
    let _ = resolve_thingino(st).await; // keep commit + defconfig list warm (picks up new boards)
    let lim = { let conn = st.db.lock(); effective_limits(&conn, &st.cfg) };

    // 1) Snapshot running builds + the next queued builds to dispatch.
    let (running, to_dispatch): (Vec<RunningBuild>, Vec<QueuedBuild>) = {
        let conn = st.db.lock();
        let running: Vec<RunningBuild> = {
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
        let slots = (lim.max_concurrent - running.len() as i64).max(0);
        let to_dispatch: Vec<QueuedBuild> = if slots > 0 {
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
                    let conn = st.db.lock();
                    conn.execute("UPDATE builds SET state='cancelled', finished_ts=?2, run_id=NULL WHERE id=?1", rusqlite::params![id, now_ts]).ok();
                    log_event(&conn, "cancelled", Some(id), None, None, "run stopped + deleted", None);
                }
                Some(r) => {
                    let _ = cancel_run(st, r.run_id).await; // run still active — (re)request cancellation
                }
                None => {
                    let conn = st.db.lock();
                    conn.execute("UPDATE builds SET state='cancelled', finished_ts=?2 WHERE id=?1", rusqlite::params![id, now_ts]).ok();
                    log_event(&conn, "cancelled", Some(id), None, None, "cancelled (run not found)", None);
                }
            }
            continue;
        }

        match matched {
            Some(r) => {
                if run_id_opt.is_none() {
                    let conn = st.db.lock();
                    conn.execute("UPDATE builds SET run_id=?2 WHERE id=?1", rusqlite::params![id, r.run_id]).ok();
                }
                if r.status == "completed" {
                    let new_state = match r.conclusion.as_deref() {
                        Some("success") => "done",
                        Some("cancelled") => "cancelled",
                        _ => "failed",
                    };
                    let conn = st.db.lock();
                    conn.execute("UPDATE builds SET state=?2, finished_ts=?3 WHERE id=?1", rusqlite::params![id, new_state, now_ts]).ok();
                    log_event(&conn, new_state, Some(id), None, None, &format!("run {} {}", r.run_id, r.conclusion.as_deref().unwrap_or("?")), None);
                }
            }
            None => {
                if now_ts - dispatched_ts > lim.build_timeout {
                    let conn = st.db.lock();
                    conn.execute("UPDATE builds SET state='failed', finished_ts=?2 WHERE id=?1", rusqlite::params![id, now_ts]).ok();
                    log_event(&conn, "failed", Some(id), None, None, "timed out / run not found", None);
                }
            }
        }
    }

    // 4) Dispatch from the queue into free slots.
    for (id, defconfig, commit) in &to_dispatch {
        let still_queued: bool = {
            let conn = st.db.lock();
            conn.query_row("SELECT 1 FROM builds WHERE id=?1 AND state='queued'", [id], |_| Ok(())).optional().ok().flatten().is_some()
        };
        if !still_queued {
            continue;
        }
        match dispatch_build(st, id, defconfig, commit.as_deref()).await {
            Ok(()) => {
                let conn = st.db.lock();
                conn.execute("UPDATE builds SET state='running', dispatched_ts=?2 WHERE id=?1", rusqlite::params![id, now()]).ok();
                log_event(&conn, "dispatched", Some(id), None, None, defconfig, None);
            }
            Err(e) => {
                let conn = st.db.lock();
                conn.execute("UPDATE builds SET attempts=attempts+1 WHERE id=?1", [id]).ok();
                let attempts: i64 = conn.query_row("SELECT attempts FROM builds WHERE id=?1", [id], |r| r.get(0)).unwrap_or(0);
                if attempts >= 3 {
                    conn.execute("UPDATE builds SET state='failed', finished_ts=?2 WHERE id=?1", rusqlite::params![id, now()]).ok();
                    log_event(&conn, "failed", Some(id), None, None, "dispatch failed 3x", None);
                }
                tracing::warn!("dispatch failed for {id} (attempt {attempts}): {e}");
            }
        }
    }

    // 5) Reap finished builds past their retention window.
    let reap: Vec<(String, String, Option<i64>, i64)> = {
        let conn = st.db.lock();
        let mut stmt = conn.prepare("SELECT id, state, run_id, finished_ts FROM builds WHERE state IN ('done','failed','cancelled') AND finished_ts IS NOT NULL")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?, r.get::<_, i64>(3)?)))?
            .filter_map(|x| x.ok())
            .collect::<Vec<_>>();
        rows
    };
    for (id, state, run_id, finished_ts) in reap {
        let age = now_ts - finished_ts;
        let expired = if state == "done" { age > lim.retention } else { age > lim.failed_retention };
        if !expired {
            continue;
        }
        // Only mark expired once GitHub cleanup actually succeeded, so a transient
        // failure is retried next tick instead of orphaning the asset/run.
        let asset_ok = if state == "done" { delete_release_assets(st, &id).await.is_ok() } else { true };
        let run_ok = match run_id {
            Some(rid) => delete_run(st, rid).await.is_ok(),
            None => true,
        };
        if asset_ok && run_ok {
            let conn = st.db.lock();
            conn.execute("UPDATE builds SET state='expired' WHERE id=?1", [&id]).ok();
            log_event(&conn, "expired", Some(&id), None, None, &format!("reaped {state}: asset+run removed"), None);
        } else {
            tracing::warn!("reap of {id} incomplete (asset_ok={asset_ok} run_ok={run_ok}); will retry");
        }
    }

    // 6) Prune to keep the DB bounded: long-expired build rows + old audit events.
    {
        let conn = st.db.lock();
        let _ = conn.execute("DELETE FROM builds WHERE state='expired' AND finished_ts < ?1", [now_ts - EXPIRED_BUILD_TTL_SECS]);
        let _ = conn.execute("DELETE FROM events WHERE ts < ?1", [now_ts - EVENT_TTL_SECS]);
    }

    Ok(())
}

async fn fetch_runs(st: &AppState) -> anyhow::Result<Vec<RunRow>> {
    let url = format!(
        "https://api.github.com/repos/{}/actions/runs?per_page=50&event=repository_dispatch",
        st.cfg.github_repo
    );
    let v: serde_json::Value = gh(st.http.get(&url).bearer_auth(github_token(st).await)).send().await?.json().await?;
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
            let c = st.installation_token.lock();
            if let Some(t) = &c.0 {
                if now() < c.1 {
                    return t.clone();
                }
            }
        }
        match mint_installation_token(st).await {
            Ok((tok, exp)) => {
                *st.installation_token.lock() = (Some(tok.clone()), exp);
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
    let resp = gh(st.http.post(&url).bearer_auth(&jwt)).send().await?;
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("installation token {code}: {body}");
    }
    let v: serde_json::Value = resp.json().await?;
    let token = v["token"].as_str().ok_or_else(|| anyhow::anyhow!("no token in response"))?.to_string();
    Ok((token, nowt + TOKEN_REFRESH_SECS)) // ~1h tokens; refresh before expiry
}

/// Latest published release tag (e.g. "v1.2.0"), cached ~10 min; None if no release yet.
async fn check_latest_release(st: &AppState) -> Option<String> {
    {
        let c = st.latest_release.lock();
        if now() - c.1 < RELEASE_CACHE_SECS {
            return c.0.clone();
        }
    }
    let url = format!("https://api.github.com/repos/{}/releases/latest", st.cfg.github_repo);
    let tag = match gh(st.http.get(&url).bearer_auth(github_token(st).await)).send().await {
        Ok(r) if r.status().is_success() => {
            r.json::<serde_json::Value>().await.ok().and_then(|v| v["tag_name"].as_str().map(str::to_string))
        }
        _ => None,
    };
    *st.latest_release.lock() = (tag.clone(), now());
    tag
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
    let resp = gh(st.http.post(&url).bearer_auth(github_token(st).await)).json(&payload).send().await?;
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
    let _ = gh(st.http.post(&url).bearer_auth(github_token(st).await)).send().await;
    Ok(())
}

async fn delete_run(st: &AppState, run_id: i64) -> anyhow::Result<()> {
    let url = format!("https://api.github.com/repos/{}/actions/runs/{}", st.cfg.github_repo, run_id);
    let resp = gh(st.http.delete(&url).bearer_auth(github_token(st).await)).send().await?;
    // 204 on success; 404 = already gone — both are fine.
    if resp.status().is_success() || resp.status() == reqwest::StatusCode::NOT_FOUND {
        Ok(())
    } else {
        anyhow::bail!("delete run {run_id}: {}", resp.status())
    }
}

async fn delete_release_assets(st: &AppState, build_id: &str) -> anyhow::Result<()> {
    let url = format!("https://api.github.com/repos/{}/releases/tags/{}", st.cfg.github_repo, st.cfg.rolling_tag);
    let resp = gh(st.http.get(&url).bearer_auth(github_token(st).await)).send().await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(()); // no rolling release yet → nothing to delete
    }
    let v: serde_json::Value = resp.error_for_status()?.json().await?;
    let targets = [format!("{build_id}.bin"), format!("{build_id}.bin.sha256sum")];
    let mut all_ok = true;
    if let Some(assets) = v["assets"].as_array() {
        for a in assets {
            if let (Some(name), Some(aid)) = (a["name"].as_str(), a["id"].as_i64()) {
                if targets.iter().any(|t| t == name) {
                    let durl = format!("https://api.github.com/repos/{}/releases/assets/{}", st.cfg.github_repo, aid);
                    let ok = match gh(st.http.delete(&durl).bearer_auth(github_token(st).await)).send().await {
                        Ok(r) => r.status().is_success() || r.status() == reqwest::StatusCode::NOT_FOUND,
                        Err(_) => false,
                    };
                    all_ok &= ok;
                }
            }
        }
    }
    if all_ok {
        Ok(())
    } else {
        anyhow::bail!("some asset deletes failed for {build_id}")
    }
}
