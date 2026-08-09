-- Phase 3 hardening: deletion-aware sync, request-bound compatibility
-- idempotency, retrieval invalidation, and metadata visibility.

ALTER TABLE phone_changes ADD COLUMN deleted_at TEXT;

ALTER TABLE phone_operations ADD COLUMN request_hash TEXT;
ALTER TABLE phone_operations ADD COLUMN session_id TEXT;
ALTER TABLE phone_operations ADD COLUMN action_id TEXT;

CREATE INDEX IF NOT EXISTS idx_phone_operations_request
  ON phone_operations(user_id, idempotency_key, operation, request_hash);

-- Recreate the session triggers so a soft delete is represented in the
-- durable phone_changes stream. Existing clients ignore the additive field;
-- new clients remove the local entity when deleted_at is present.
DROP TRIGGER IF EXISTS sessions_phone_change_insert;
DROP TRIGGER IF EXISTS sessions_phone_change_update;

CREATE TRIGGER sessions_phone_change_insert
AFTER INSERT ON sessions
BEGIN
  UPDATE sessions SET version = 1 WHERE id = NEW.id AND version = 0;
  INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, deleted_at, created_at)
  SELECT NEW.user_id, 'session', NEW.id, NEW.id, version, NEW.deleted_at, NEW.updated_at
  FROM sessions WHERE id = NEW.id;
END;

CREATE TRIGGER sessions_phone_change_update
AFTER UPDATE ON sessions
WHEN NEW.version = OLD.version
BEGIN
  UPDATE sessions SET version = OLD.version + 1 WHERE id = NEW.id;
  INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, deleted_at, created_at)
  VALUES (NEW.user_id, 'session', NEW.id, NEW.id, OLD.version + 1, NEW.deleted_at, NEW.updated_at);
END;

CREATE TRIGGER retrieval_phone_change_insert
AFTER INSERT ON retrieval_items
BEGIN
  INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at)
  VALUES (NEW.user_id, 'retrieval', NEW.id, NEW.session_id, 1, NEW.created_at);
END;

CREATE TRIGGER message_phone_change_insert
AFTER INSERT ON session_messages
BEGIN
  INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at)
  VALUES (NEW.user_id, 'message', NEW.id, NEW.session_id, 1, NEW.created_at);
END;

CREATE TRIGGER retrieval_phone_change_delete
AFTER DELETE ON retrieval_items
BEGIN
  INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, deleted_at, created_at)
  VALUES (OLD.user_id, 'retrieval', OLD.id, OLD.session_id, 2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
END;

CREATE TRIGGER message_phone_change_delete
AFTER DELETE ON session_messages
BEGIN
  INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, deleted_at, created_at)
  VALUES (OLD.user_id, 'message', OLD.id, OLD.session_id, 2, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));
END;
