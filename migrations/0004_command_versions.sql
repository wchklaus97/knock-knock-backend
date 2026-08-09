-- Complete the command/sync foundation without rewriting the additive 0003
-- migration. This keeps expand -> migrate -> contract safe for deployed D1s.

ALTER TABLE commands ADD COLUMN model_version TEXT;
ALTER TABLE commands ADD COLUMN version INTEGER NOT NULL DEFAULT 0;

CREATE UNIQUE INDEX IF NOT EXISTS idx_phone_changes_entity_version
  ON phone_changes(user_id, entity_type, entity_id, version);
