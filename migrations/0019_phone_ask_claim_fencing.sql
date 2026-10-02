-- Migration 0019: stable phone turn identity and fenced Ask claims.
-- Existing rows intentionally keep a NULL client_turn_id. Existing claimed
-- rows also keep NULL claim fields so they drain explicitly instead of being
-- interpreted as expired and silently reassigned.

ALTER TABLE phone_asks ADD COLUMN client_turn_id TEXT
  CHECK (client_turn_id IS NULL OR length(trim(client_turn_id)) BETWEEN 8 AND 128);
ALTER TABLE phone_asks ADD COLUMN conversation_id TEXT;
ALTER TABLE phone_asks ADD COLUMN lease_id TEXT
  CHECK (lease_id IS NULL OR length(trim(lease_id)) BETWEEN 8 AND 128);
ALTER TABLE phone_asks ADD COLUMN listener_generation INTEGER
  CHECK (listener_generation IS NULL OR listener_generation > 0);
ALTER TABLE phone_asks ADD COLUMN claim_token TEXT;
ALTER TABLE phone_asks ADD COLUMN claim_deadline TEXT;
ALTER TABLE phone_asks ADD COLUMN claim_generation INTEGER
  CHECK (claim_generation IS NULL OR claim_generation > 0);
ALTER TABLE phone_asks ADD COLUMN answered_at TEXT;
ALTER TABLE phone_asks ADD COLUMN reply_event_id TEXT;
ALTER TABLE phone_asks ADD COLUMN attempt_count INTEGER NOT NULL DEFAULT 0
  CHECK (attempt_count >= 0);

-- target_chat_id is the resolved Codex conversation chosen at intake. This
-- backfill records that immutable decision without consulting the active lease.
UPDATE phone_asks
SET conversation_id = target_chat_id
WHERE conversation_id IS NULL AND target_chat_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_phone_asks_client_turn
  ON phone_asks (user_id, agent_id, client_turn_id)
  WHERE client_turn_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_phone_asks_reply_event
  ON phone_asks (reply_event_id)
  WHERE reply_event_id IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_phone_asks_claim_recovery
  ON phone_asks (binding_id, target_chat_id, status, claim_deadline, created_at);
