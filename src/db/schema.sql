CREATE TABLE IF NOT EXISTS accounts (
  id                  INTEGER PRIMARY KEY,
  provider            TEXT    NOT NULL,
  provider_account_id TEXT    NOT NULL,
  trusted             INTEGER NOT NULL DEFAULT 0,
  auth_mode           TEXT    NOT NULL DEFAULT 'oauth',
  email               TEXT,
  label               TEXT,
  plan_type           TEXT,
  access_token        TEXT    NOT NULL,
  refresh_token       TEXT    NOT NULL,
  http_referer        TEXT,
  access_expires_at   INTEGER,
  last_refresh_at     INTEGER,
  status              TEXT    NOT NULL DEFAULT 'active',
  cooldown_until      INTEGER,
  disabled_reason     TEXT,
  created_at          INTEGER NOT NULL DEFAULT (unixepoch()),
  updated_at          INTEGER NOT NULL DEFAULT (unixepoch()),
  UNIQUE (provider, provider_account_id)
);

CREATE TABLE IF NOT EXISTS api_tokens (
  id             INTEGER PRIMARY KEY,
  user           TEXT    NOT NULL,
  token_hash     BLOB    NOT NULL UNIQUE,
  token_prefix   TEXT    NOT NULL,
  request_limit  INTEGER,
  token_limit    INTEGER,
  five_hour_limit REAL,
  weekly_limit   REAL,
  window_seconds INTEGER NOT NULL DEFAULT 3600,
  slowdown_ms    INTEGER NOT NULL DEFAULT 0,
  prefer_trusted INTEGER NOT NULL DEFAULT 0,
  allowed_providers TEXT NOT NULL DEFAULT '',
  created_at     INTEGER NOT NULL DEFAULT (unixepoch()),
  revoked_at     INTEGER
);

CREATE TABLE IF NOT EXISTS api_meter (
  id            INTEGER PRIMARY KEY,
  token_id      INTEGER NOT NULL REFERENCES api_tokens(id),
  ts_ms         INTEGER NOT NULL,
  input_tokens  INTEGER NOT NULL DEFAULT 0,
  output_tokens INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_api_meter_token_ts ON api_meter(token_id, ts_ms);

CREATE TABLE IF NOT EXISTS usage_log (
  id                INTEGER PRIMARY KEY,
  ts                INTEGER NOT NULL DEFAULT (unixepoch()),
  token_id          INTEGER REFERENCES api_tokens(id),
  user              TEXT    NOT NULL,
  account_id        INTEGER REFERENCES accounts(id),
  provider          TEXT    NOT NULL DEFAULT '',
  dialect           TEXT    NOT NULL,
  requested_model   TEXT    NOT NULL,
  upstream_model    TEXT    NOT NULL,
  effort            TEXT    NOT NULL DEFAULT '',
  service_tier      TEXT    NOT NULL DEFAULT '',
  input_tokens      INTEGER NOT NULL DEFAULT 0,
  output_tokens     INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens INTEGER NOT NULL DEFAULT 0,
  cache_write_tokens INTEGER NOT NULL DEFAULT 0,
  reasoning_tokens  INTEGER NOT NULL DEFAULT 0,
  cost_usd          REAL    NOT NULL DEFAULT 0,
  list_cost_usd     REAL    NOT NULL DEFAULT 0,
  status            INTEGER NOT NULL,
  error_kind        TEXT,
  duration_ms       INTEGER,
  session_key       TEXT    NOT NULL DEFAULT '',
  turn_index        INTEGER NOT NULL DEFAULT 0,
  tools_declared    INTEGER NOT NULL DEFAULT 0,
  tools_called      TEXT    NOT NULL DEFAULT '',
  thinking_budget   INTEGER NOT NULL DEFAULT 0,
  image_count       INTEGER NOT NULL DEFAULT 0,
  request_bytes     INTEGER NOT NULL DEFAULT 0,
  response_bytes    INTEGER NOT NULL DEFAULT 0,
  ttft_ms           INTEGER,
  stop_reason       TEXT    NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS idx_usage_ts         ON usage_log(ts);
CREATE INDEX IF NOT EXISTS idx_usage_user_ts    ON usage_log(user, ts);
CREATE INDEX IF NOT EXISTS idx_usage_account_ts ON usage_log(account_id, ts);

-- Provider snapshots are an audit trail, including valid but stale readings.
CREATE TABLE IF NOT EXISTS quota_observations (
  id INTEGER PRIMARY KEY,
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  window_seconds INTEGER NOT NULL CHECK (window_seconds IN (18000, 604800)),
  resets_at INTEGER NOT NULL,
  used_percent REAL NOT NULL CHECK (used_percent BETWEEN 0 AND 100),
  observed_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_quota_observations_epoch
  ON quota_observations(account_id, window_seconds, resets_at, observed_at);

-- Reset epochs and estimates survive restarts until explicit account removal.
CREATE TABLE IF NOT EXISTS quota_epochs (
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  window_seconds INTEGER NOT NULL,
  resets_at INTEGER NOT NULL,
  observed_percent REAL NOT NULL,
  high_water_percent REAL NOT NULL,
  baseline_percent REAL NOT NULL,
  unattributed_percent REAL NOT NULL,
  observed_at INTEGER NOT NULL,
  usage_cursor INTEGER NOT NULL,
  PRIMARY KEY (account_id, window_seconds, resets_at)
);
CREATE TABLE IF NOT EXISTS user_quota_estimates (
  user TEXT NOT NULL,
  account_id INTEGER NOT NULL,
  window_seconds INTEGER NOT NULL,
  resets_at INTEGER NOT NULL,
  used_percent REAL NOT NULL,
  PRIMARY KEY (user, account_id, window_seconds, resets_at),
  FOREIGN KEY (account_id, window_seconds, resets_at)
    REFERENCES quota_epochs(account_id, window_seconds, resets_at) ON DELETE CASCADE
);
CREATE TABLE IF NOT EXISTS user_quota_budgets (
  user TEXT NOT NULL CHECK (length(trim(user)) > 0),
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  window_seconds INTEGER NOT NULL CHECK (window_seconds IN (18000, 604800)),
  budget_percent REAL NOT NULL CHECK (budget_percent BETWEEN 0 AND 100),
  PRIMARY KEY (user, account_id, window_seconds)
);

-- Fleet-wide user quota budgets aggregate all OpenAI accounts equally.
CREATE TABLE IF NOT EXISTS user_fleet_quota_budgets (
  user TEXT NOT NULL CHECK (length(trim(user)) > 0),
  window_seconds INTEGER NOT NULL CHECK (window_seconds IN (18000, 604800)),
  budget_percent REAL NOT NULL CHECK (budget_percent BETWEEN 0 AND 100),
  PRIMARY KEY (user, window_seconds)
);

-- Lifetime spend budgets have no automatic reset.
CREATE TABLE IF NOT EXISTS user_spend_budgets (
  user       TEXT NOT NULL CHECK (length(trim(user)) > 0),
  account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
  budget_usd REAL NOT NULL CHECK (budget_usd >= 0),
  PRIMARY KEY (user, account_id)
);
