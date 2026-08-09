-- Additive foundation for CommandEnvelope v1, durable phone sync, user-facing
-- history, retrieval snapshots, and reliable external side effects.
--
-- This migration intentionally does not remove or rename existing columns.
-- Older clients continue using the current session/action routes while the
-- canonical command APIs are introduced.

ALTER TABLE sessions ADD COLUMN version INTEGER NOT NULL DEFAULT 0;
ALTER TABLE sessions ADD COLUMN archived_at TEXT;
ALTER TABLE sessions ADD COLUMN deleted_at TEXT;
ALTER TABLE sessions ADD COLUMN retention_expires_at TEXT;

ALTER TABLE devices ADD COLUMN device_id TEXT;
ALTER TABLE devices ADD COLUMN timezone TEXT;
ALTER TABLE devices ADD COLUMN last_sync_cursor TEXT;

ALTER TABLE pushes ADD COLUMN read_at TEXT;
ALTER TABLE pushes ADD COLUMN dismissed_at TEXT;

CREATE TABLE IF NOT EXISTS commands (
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
  state TEXT NOT NULL CHECK (state IN ('pending', 'validated', 'awaiting_confirmation', 'queued', 'running', 'succeeded', 'failed', 'expired', 'cancelled', 'unknown')),
  command_hash TEXT NOT NULL,
  result_json TEXT,
  error_code TEXT,
  expires_at TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users(id),
  FOREIGN KEY (device_id) REFERENCES devices(id),
  FOREIGN KEY (session_id) REFERENCES sessions(id),
  UNIQUE (user_id, idempotency_key)
);

CREATE TABLE IF NOT EXISTS confirmation_tokens (
  id TEXT PRIMARY KEY,
  command_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  token_hash TEXT NOT NULL UNIQUE,
  command_hash TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  used_at TEXT,
  created_at TEXT NOT NULL,
  FOREIGN KEY (command_id) REFERENCES commands(id),
  FOREIGN KEY (user_id) REFERENCES users(id)
);

CREATE TABLE IF NOT EXISTS session_messages (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  role TEXT NOT NULL CHECK (role IN ('user', 'agent', 'system', 'tool')),
  content TEXT NOT NULL,
  metadata_json TEXT NOT NULL DEFAULT '{}',
  command_id TEXT,
  sequence INTEGER NOT NULL,
  retention_expires_at TEXT,
  created_at TEXT NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users(id),
  FOREIGN KEY (session_id) REFERENCES sessions(id),
  FOREIGN KEY (command_id) REFERENCES commands(id),
  UNIQUE (session_id, sequence)
);

CREATE TABLE IF NOT EXISTS retrieval_items (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  message_id TEXT,
  title TEXT NOT NULL,
  url TEXT NOT NULL,
  snippet TEXT,
  score REAL,
  content_hash TEXT NOT NULL,
  r2_key TEXT,
  retention_expires_at TEXT,
  created_at TEXT NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users(id),
  FOREIGN KEY (session_id) REFERENCES sessions(id),
  FOREIGN KEY (message_id) REFERENCES session_messages(id),
  UNIQUE (session_id, content_hash)
);

-- AUTOINCREMENT provides a durable, monotonic per-database cursor. The API
-- exposes it as an opaque string and always filters by user_id.
CREATE TABLE IF NOT EXISTS phone_changes (
  cursor INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id TEXT NOT NULL,
  entity_type TEXT NOT NULL CHECK (entity_type IN ('session', 'message', 'command', 'push', 'retrieval')),
  entity_id TEXT NOT NULL,
  session_id TEXT,
  version INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users(id)
);

CREATE TABLE IF NOT EXISTS outbox_events (
  id TEXT PRIMARY KEY,
  user_id TEXT,
  topic TEXT NOT NULL,
  aggregate_id TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'succeeded', 'failed', 'retrying', 'unknown')),
  attempts INTEGER NOT NULL DEFAULT 0,
  next_attempt_at TEXT,
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users(id),
  UNIQUE (topic, idempotency_key)
);

CREATE TABLE IF NOT EXISTS action_attempts (
  id TEXT PRIMARY KEY,
  user_id TEXT,
  command_id TEXT,
  action_id TEXT,
  provider TEXT NOT NULL,
  provider_idempotency_key TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('queued', 'running', 'succeeded', 'failed', 'retrying', 'unknown')),
  request_hash TEXT NOT NULL,
  response_json TEXT,
  attempts INTEGER NOT NULL DEFAULT 0,
  next_attempt_at TEXT,
  last_error TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users(id),
  FOREIGN KEY (command_id) REFERENCES commands(id),
  FOREIGN KEY (action_id) REFERENCES actions(id),
  UNIQUE (provider, provider_idempotency_key)
);

CREATE TABLE IF NOT EXISTS sync_tombstones (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  entity_type TEXT NOT NULL,
  entity_id TEXT NOT NULL,
  deleted_at TEXT NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users(id),
  UNIQUE (user_id, entity_type, entity_id)
);

CREATE INDEX IF NOT EXISTS idx_sessions_user_cursor
  ON sessions(user_id, updated_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_sessions_user_retention
  ON sessions(user_id, retention_expires_at);
CREATE INDEX IF NOT EXISTS idx_commands_user_state
  ON commands(user_id, state, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_commands_session
  ON commands(session_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_confirmation_tokens_command
  ON confirmation_tokens(command_id, user_id, expires_at);
CREATE INDEX IF NOT EXISTS idx_messages_user_session
  ON session_messages(user_id, session_id, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS idx_retrieval_user_session
  ON retrieval_items(user_id, session_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_phone_changes_user_cursor
  ON phone_changes(user_id, cursor);
CREATE INDEX IF NOT EXISTS idx_outbox_state_attempt
  ON outbox_events(state, next_attempt_at, created_at);
CREATE INDEX IF NOT EXISTS idx_action_attempts_command
  ON action_attempts(command_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_devices_user_device
  ON devices(user_id, device_id);
CREATE INDEX IF NOT EXISTS idx_pushes_user_read
  ON pushes(user_id, read_at, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tombstones_user_deleted
  ON sync_tombstones(user_id, deleted_at DESC);

