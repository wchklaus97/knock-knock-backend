-- Migration 0016: user-initiated phone asks for a paired Home agent.
-- The phone never calls POST /v1/sessions; this table plus an internal
-- session create is the intake. last_seen_at marks MCP/host polling.

ALTER TABLE agents ADD COLUMN last_seen_at TEXT;

CREATE TABLE IF NOT EXISTS phone_asks (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  agent_id TEXT NOT NULL,
  transcript TEXT NOT NULL CHECK (length(trim(transcript)) BETWEEN 1 AND 2000),
  locale TEXT CHECK (locale IS NULL OR length(locale) BETWEEN 2 AND 35),
  idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 8 AND 128),
  session_id TEXT,
  status TEXT NOT NULL CHECK (status IN ('queued', 'claimed', 'expired')),
  claimed_at TEXT,
  expires_at TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
  FOREIGN KEY (agent_id) REFERENCES agents(id) ON DELETE CASCADE,
  UNIQUE (user_id, agent_id, idempotency_key)
);

CREATE INDEX IF NOT EXISTS idx_phone_asks_agent_status
  ON phone_asks (agent_id, status, created_at);
CREATE INDEX IF NOT EXISTS idx_phone_asks_user_created
  ON phone_asks (user_id, created_at);
