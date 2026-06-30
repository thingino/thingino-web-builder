-- D1 schema for the Cloudflare Worker broker (mirrors the Rust/SQLite broker).
-- Apply: wrangler d1 execute thingino-builder --file schema.sql   (add --remote to apply to prod)
CREATE TABLE IF NOT EXISTS builds (
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
CREATE INDEX IF NOT EXISTS idx_builds_created ON builds(created_ts);

CREATE TABLE IF NOT EXISTS events (
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
CREATE INDEX IF NOT EXISTS idx_events_kind_ip_ts ON events(kind, ip_bucket, ts);

CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);

-- Admin sessions (Workers are stateless, so sessions live in D1, not memory).
-- admin = the identity that owns the session ("master" for the break-glass token, else a username).
CREATE TABLE IF NOT EXISTS sessions (token TEXT PRIMARY KEY, admin TEXT, expires INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS idx_sessions_exp ON sessions(expires);

-- Named admin users. The master token (a Worker secret) is separate + always works.
-- pw_hash = "iters.saltB64.hashB64" (PBKDF2-SHA256); null until the invite is accepted.
-- privileges = JSON array of granted privileged-action names (NULL/absent = none).
-- last_totp_step = the 30s TOTP step last accepted (single-use anti-replay; null = none yet).
CREATE TABLE IF NOT EXISTS admins (
  username TEXT PRIMARY KEY,
  pw_hash TEXT,
  totp_secret TEXT NOT NULL,
  invite_token TEXT,
  invite_expires INTEGER,
  disabled INTEGER NOT NULL DEFAULT 0,
  created_ts INTEGER NOT NULL,
  created_by TEXT,
  last_login INTEGER,
  privileges TEXT,
  last_totp_step INTEGER
);
CREATE INDEX IF NOT EXISTS idx_admins_invite ON admins(invite_token);
