-- Populate the durable phone_changes cursor for legacy session and push writes.
-- The version guard prevents the trigger's version bump from recursively
-- creating a second change record.

ALTER TABLE pushes ADD COLUMN version INTEGER NOT NULL DEFAULT 0;
ALTER TABLE pushes ADD COLUMN updated_at TEXT;
UPDATE pushes SET updated_at = created_at WHERE updated_at IS NULL;

CREATE TRIGGER IF NOT EXISTS sessions_phone_change_insert
AFTER INSERT ON sessions
BEGIN
  UPDATE sessions SET version = 1 WHERE id = NEW.id AND version = 0;
  INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at)
  SELECT NEW.user_id, 'session', NEW.id, NEW.id, version, NEW.updated_at
  FROM sessions WHERE id = NEW.id;
END;

CREATE TRIGGER IF NOT EXISTS sessions_phone_change_update
AFTER UPDATE ON sessions
WHEN NEW.version = OLD.version
BEGIN
  UPDATE sessions SET version = OLD.version + 1 WHERE id = NEW.id;
  INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at)
  VALUES (NEW.user_id, 'session', NEW.id, NEW.id, OLD.version + 1, NEW.updated_at);
END;

CREATE TRIGGER IF NOT EXISTS pushes_phone_change_insert
AFTER INSERT ON pushes
BEGIN
  UPDATE pushes SET version = 1 WHERE id = NEW.id AND version = 0;
  INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at)
  SELECT NEW.user_id, 'push', NEW.id, NEW.session_id, version, NEW.created_at
  FROM pushes WHERE id = NEW.id;
END;

CREATE TRIGGER IF NOT EXISTS pushes_phone_change_update
AFTER UPDATE ON pushes
WHEN NEW.version = OLD.version
BEGIN
  UPDATE pushes SET version = OLD.version + 1 WHERE id = NEW.id;
  INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at)
  VALUES (NEW.user_id, 'push', NEW.id, NEW.session_id, OLD.version + 1, COALESCE(NEW.updated_at, NEW.created_at));
END;
