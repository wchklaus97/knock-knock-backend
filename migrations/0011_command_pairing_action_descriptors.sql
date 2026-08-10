-- Phase 4/5 contract completion: make pairing claims race-safe and retain
-- the backend-resolved action metadata returned to phone clients.
ALTER TABLE pairing_codes ADD COLUMN claim_token TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_pairing_codes_claim_token
  ON pairing_codes(claim_token)
  WHERE claim_token IS NOT NULL;

ALTER TABLE sessions ADD COLUMN available_action_descriptors_json TEXT;
ALTER TABLE actions ADD COLUMN descriptor_json TEXT;

CREATE INDEX IF NOT EXISTS idx_commands_user_updated_id
  ON commands(user_id, updated_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS idx_actions_session_status_created
  ON actions(session_id, status, created_at);
