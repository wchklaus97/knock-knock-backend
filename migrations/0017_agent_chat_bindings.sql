-- Migration 0017: bind phone voice asks to one concrete Codex chat listener.

CREATE TABLE IF NOT EXISTS agent_chat_bindings (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  chat_id TEXT NOT NULL CHECK (length(trim(chat_id)) BETWEEN 1 AND 128),
  chat_title TEXT NOT NULL CHECK (length(trim(chat_title)) BETWEEN 1 AND 160),
  listener_instance_id TEXT NOT NULL CHECK (length(trim(listener_instance_id)) BETWEEN 8 AND 128),
  status TEXT NOT NULL CHECK (status IN ('active', 'offline', 'expired', 'revoked')),
  last_seen_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  revoked_at TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
  FOREIGN KEY (agent_id) REFERENCES agents(id) ON DELETE CASCADE,
  UNIQUE (agent_id, chat_id)
);

CREATE INDEX IF NOT EXISTS idx_agent_chat_bindings_active
  ON agent_chat_bindings (agent_id, status, expires_at);

ALTER TABLE phone_asks ADD COLUMN binding_id TEXT;
ALTER TABLE phone_asks ADD COLUMN target_chat_id TEXT;
ALTER TABLE phone_asks ADD COLUMN claimed_by_chat_id TEXT;

CREATE INDEX IF NOT EXISTS idx_phone_asks_binding_status
  ON phone_asks (binding_id, status, created_at);
