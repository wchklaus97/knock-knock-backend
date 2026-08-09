-- Per-principal, per-category request counters. Values are hashed bucket keys;
-- no bearer token or raw IP is persisted.

CREATE TABLE IF NOT EXISTS rate_limit_buckets (
  bucket_key TEXT PRIMARY KEY,
  window_started_at TEXT NOT NULL,
  request_count INTEGER NOT NULL DEFAULT 0,
  expires_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_rate_limit_buckets_expiry
  ON rate_limit_buckets(expires_at);
