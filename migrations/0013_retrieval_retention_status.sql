-- Keep an inert retrieval tombstone after retention expiry. All columns are
-- additive; legacy writers continue to create active snapshots.
ALTER TABLE retrieval_items ADD COLUMN r2_delete_status TEXT NOT NULL DEFAULT 'active';
ALTER TABLE retrieval_items ADD COLUMN r2_deleted_at TEXT;
ALTER TABLE retrieval_items ADD COLUMN expired_at TEXT;

CREATE INDEX IF NOT EXISTS idx_retrieval_retention_sweep
  ON retrieval_items(retention_expires_at, r2_delete_status, created_at, id);

CREATE INDEX IF NOT EXISTS idx_retrieval_active_user
  ON retrieval_items(user_id, session_id, r2_delete_status, created_at DESC, id DESC);
