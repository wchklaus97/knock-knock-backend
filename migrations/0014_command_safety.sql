-- Migration 0014: G1 command safety expansion.
--
-- SQLite cannot ALTER an existing CHECK constraint. Recreate only the
-- commands table with the additive `retryable` lifecycle value, copying every
-- existing row before the old table is removed. Child tables continue to
-- reference the same final `commands` table name while foreign-key checks are
-- temporarily disabled for the table swap.
PRAGMA foreign_keys = OFF;

CREATE TABLE commands_g1 (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  device_id TEXT,
  session_id TEXT,
  schema_version INTEGER NOT NULL DEFAULT 1,
  intent TEXT NOT NULL,
  args_json TEXT NOT NULL DEFAULT '{}',
  risk_level TEXT NOT NULL CHECK (risk_level IN ('low', 'medium', 'high', 'destructive')),
  needs_confirmation INTEGER NOT NULL DEFAULT 0,
  idempotency_key TEXT NOT NULL,
  confidence REAL,
  locale TEXT NOT NULL,
  timezone TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending', 'validated', 'awaiting_confirmation', 'queued', 'running', 'succeeded', 'failed', 'expired', 'cancelled', 'retryable', 'unknown')),
  command_hash TEXT NOT NULL,
  result_json TEXT,
  error_code TEXT,
  expires_at TEXT,
  model_version TEXT,
  version INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users(id),
  FOREIGN KEY (device_id) REFERENCES devices(id),
  FOREIGN KEY (session_id) REFERENCES sessions(id),
  UNIQUE (user_id, idempotency_key)
);

INSERT INTO commands_g1 (
  id, user_id, device_id, session_id, schema_version, intent, args_json,
  risk_level, needs_confirmation, idempotency_key, confidence, locale,
  timezone, state, command_hash, result_json, error_code, expires_at,
  model_version, version, created_at, updated_at
)
SELECT
  id, user_id, device_id, session_id, schema_version, intent, args_json,
  risk_level, needs_confirmation, idempotency_key, confidence, locale,
  timezone, state, command_hash, result_json, error_code, expires_at,
  model_version, version, created_at, updated_at
FROM commands;

DROP TABLE commands;
ALTER TABLE commands_g1 RENAME TO commands;

CREATE INDEX IF NOT EXISTS idx_commands_user_state
  ON commands(user_id, state, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_commands_session
  ON commands(session_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_commands_user_updated_id
  ON commands(user_id, updated_at DESC, id DESC);

PRAGMA foreign_keys = ON;

-- Lease ownership is explicit so a stale worker cannot settle a row claimed
-- by a later worker. Existing running rows are recoverable by the legacy
-- updated_at fence until the next scheduled drain; new claims always write
-- both fields.
ALTER TABLE outbox_events ADD COLUMN lease_token TEXT;
ALTER TABLE outbox_events ADD COLUMN lease_expires_at TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_outbox_lease_token
  ON outbox_events(lease_token)
  WHERE lease_token IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_outbox_lease_expiry
  ON outbox_events(state, lease_expires_at, updated_at);

-- There is one live confirmation authority per user/command/hash. The token
-- itself remains write-only (only its SHA-256 digest is stored).
CREATE UNIQUE INDEX IF NOT EXISTS idx_confirmation_active_command_hash
  ON confirmation_tokens(user_id, command_id, command_hash)
  WHERE used_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_confirmation_tokens_expiry
  ON confirmation_tokens(user_id, expires_at, used_at);
