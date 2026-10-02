-- Migration 0018: fence listener ownership with one monotonic lease per agent.
-- The 0017 binding rows remain intact for phone ask/session compatibility.

ALTER TABLE agent_chat_bindings ADD COLUMN lease_id TEXT;
ALTER TABLE agent_chat_bindings ADD COLUMN generation INTEGER NOT NULL DEFAULT 0;

CREATE INDEX IF NOT EXISTS idx_agent_chat_bindings_generation
  ON agent_chat_bindings (agent_id, generation);

CREATE TABLE IF NOT EXISTS agent_listener_leases (
  agent_id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  binding_id TEXT NOT NULL,
  chat_id TEXT NOT NULL CHECK (length(trim(chat_id)) BETWEEN 1 AND 128),
  chat_title TEXT NOT NULL CHECK (length(trim(chat_title)) BETWEEN 1 AND 160),
  listener_instance_id TEXT NOT NULL CHECK (length(trim(listener_instance_id)) BETWEEN 8 AND 128),
  lease_id TEXT NOT NULL UNIQUE CHECK (length(trim(lease_id)) BETWEEN 8 AND 128),
  generation INTEGER NOT NULL CHECK (generation > 0),
  acquired_at TEXT NOT NULL,
  last_seen_at TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
  FOREIGN KEY (agent_id) REFERENCES agents(id) ON DELETE CASCADE,
  FOREIGN KEY (binding_id) REFERENCES agent_chat_bindings(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_agent_listener_leases_expiry
  ON agent_listener_leases (expires_at);

-- Preserve one still-live 0017 owner per agent. Ties resolve deterministically,
-- and all later acquisitions advance this row's generation instead of replacing it.
INSERT OR IGNORE INTO agent_listener_leases (
  agent_id, user_id, binding_id, chat_id, chat_title, listener_instance_id,
  lease_id, generation, acquired_at, last_seen_at, expires_at, updated_at
)
SELECT
  b.agent_id,
  b.user_id,
  b.id,
  b.chat_id,
  b.chat_title,
  b.listener_instance_id,
  'lease_legacy_' || lower(hex(randomblob(16))),
  1,
  b.created_at,
  b.last_seen_at,
  b.expires_at,
  b.updated_at
FROM agent_chat_bindings b
WHERE b.status = 'active'
  AND b.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
  AND NOT EXISTS (
    SELECT 1
    FROM agent_chat_bindings newer
    WHERE newer.agent_id = b.agent_id
      AND newer.status = 'active'
      AND newer.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
      AND (
        newer.updated_at > b.updated_at
        OR (newer.updated_at = b.updated_at AND newer.id > b.id)
      )
  );

UPDATE agent_chat_bindings
SET
  lease_id = (
    SELECT lease_id FROM agent_listener_leases l WHERE l.binding_id = agent_chat_bindings.id
  ),
  generation = (
    SELECT generation FROM agent_listener_leases l WHERE l.binding_id = agent_chat_bindings.id
  )
WHERE EXISTS (
  SELECT 1 FROM agent_listener_leases l WHERE l.binding_id = agent_chat_bindings.id
);
