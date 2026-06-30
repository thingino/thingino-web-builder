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

CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);

-- Admin sessions (Workers are stateless, so sessions live in D1, not memory).
CREATE TABLE IF NOT EXISTS sessions (token TEXT PRIMARY KEY, expires INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS idx_sessions_exp ON sessions(expires);
