-- Migration 0015: additive, user-scoped structured Memory.
--
-- `value_json` is durable typed data, not model prompt text. Only the reviewed
-- `display_text` projection may be consumed by a read-only shadow evaluator.
-- Public phone writes are additionally restricted in Rust to explicit_user;
-- trusted_system is reserved for a future internal server path.

PRAGMA foreign_keys = OFF;

CREATE TABLE memory_items (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  schema_version INTEGER NOT NULL CHECK (schema_version = 1),
  kind TEXT NOT NULL CHECK (kind IN ('fact', 'preference', 'relationship', 'project', 'goal', 'constraint')),
  subject TEXT NOT NULL CHECK (length(trim(subject)) BETWEEN 1 AND 100),
  predicate TEXT NOT NULL CHECK (length(trim(predicate)) BETWEEN 1 AND 100),
  value_json TEXT NOT NULL CHECK (json_valid(value_json) AND length(CAST(value_json AS BLOB)) <= 8192),
  display_text TEXT NOT NULL CHECK (length(trim(display_text)) BETWEEN 1 AND 2000),
  locale TEXT NOT NULL CHECK (length(locale) BETWEEN 2 AND 35),
  source_type TEXT NOT NULL CHECK (source_type IN ('explicit_user', 'trusted_system')),
  source_session_id TEXT,
  source_message_id TEXT,
  user_confirmed INTEGER NOT NULL CHECK (user_confirmed IN (0, 1)),
  confidence REAL NOT NULL CHECK (confidence >= 0.0 AND confidence <= 1.0),
  idempotency_key TEXT NOT NULL CHECK (length(idempotency_key) BETWEEN 8 AND 200),
  request_hash TEXT NOT NULL CHECK (length(request_hash) = 64),
  version INTEGER NOT NULL DEFAULT 1 CHECK (version >= 1),
  retention_expires_at TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  deleted_at TEXT,
  FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
  UNIQUE (user_id, idempotency_key),
  CHECK (source_message_id IS NULL OR source_session_id IS NOT NULL),
  CHECK (source_type = 'trusted_system' OR user_confirmed = 1)
);

CREATE INDEX idx_memory_items_user_page
  ON memory_items(user_id, created_at DESC, id DESC);
CREATE INDEX idx_memory_items_user_retention
  ON memory_items(user_id, retention_expires_at, deleted_at);
CREATE INDEX idx_memory_items_source_session
  ON memory_items(user_id, source_session_id, source_message_id);

-- SQLite cannot ALTER a CHECK constraint. Build the replacement beside the
-- live table, copy explicit cursor values, then drop/rename. Do not rename the
-- original first: SQLite may otherwise rewrite trigger bodies on sessions,
-- pushes, messages, and retrievals to target the legacy table name.
CREATE TABLE phone_changes_0015_snapshot (
  row_count INTEGER NOT NULL,
  minimum_cursor INTEGER,
  maximum_cursor INTEGER,
  sequence_value INTEGER NOT NULL
);

INSERT INTO phone_changes_0015_snapshot (
  row_count, minimum_cursor, maximum_cursor, sequence_value
)
SELECT
  COUNT(*),
  MIN(cursor),
  MAX(cursor),
  MAX(
    COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'phone_changes'), 0),
    COALESCE(MAX(cursor), 0)
  )
FROM phone_changes;

CREATE TABLE phone_changes_next (
  cursor INTEGER PRIMARY KEY AUTOINCREMENT,
  user_id TEXT NOT NULL,
  entity_type TEXT NOT NULL CHECK (entity_type IN ('session', 'message', 'command', 'push', 'retrieval', 'memory')),
  entity_id TEXT NOT NULL,
  session_id TEXT,
  version INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL,
  deleted_at TEXT,
  FOREIGN KEY (user_id) REFERENCES users(id)
);

INSERT INTO phone_changes_next (
  cursor, user_id, entity_type, entity_id, session_id, version, created_at, deleted_at
)
SELECT
  cursor, user_id, entity_type, entity_id, session_id, version, created_at, deleted_at
FROM phone_changes
ORDER BY cursor ASC;

-- Abort the migration before the original table is dropped if the explicit
-- cursor copy did not preserve its cardinality and bounds.
CREATE TABLE phone_changes_0015_copy_guard (
  valid INTEGER NOT NULL CHECK (valid = 1)
);

INSERT INTO phone_changes_0015_copy_guard (valid)
SELECT CASE WHEN
  (SELECT COUNT(*) FROM phone_changes_next) = row_count
  AND ((SELECT MIN(cursor) FROM phone_changes_next) IS minimum_cursor)
  AND ((SELECT MAX(cursor) FROM phone_changes_next) IS maximum_cursor)
THEN 1 ELSE 0 END
FROM phone_changes_0015_snapshot;


-- D1 validates trigger bodies while the replacement table is being renamed.
-- Remove every existing trigger that targets phone_changes before the brief
-- drop/rename window, then recreate the same bodies below. This avoids both a
-- dangling-reference failure and SQLite's legacy-table rename rewrite hazard.
DROP TRIGGER IF EXISTS sessions_phone_change_insert;
DROP TRIGGER IF EXISTS sessions_phone_change_update;
DROP TRIGGER IF EXISTS pushes_phone_change_insert;
DROP TRIGGER IF EXISTS pushes_phone_change_update;
DROP TRIGGER IF EXISTS retrieval_phone_change_insert;
DROP TRIGGER IF EXISTS retrieval_phone_change_delete;
DROP TRIGGER IF EXISTS message_phone_change_insert;
DROP TRIGGER IF EXISTS message_phone_change_delete;

DROP TABLE phone_changes;
ALTER TABLE phone_changes_next RENAME TO phone_changes;

-- Preserve the AUTOINCREMENT high-water mark even when the former highest row
-- had already been removed. This prevents a post-migration cursor from being
-- reused or moving backwards.
DELETE FROM sqlite_sequence
WHERE name IN ('phone_changes', 'phone_changes_next');
INSERT INTO sqlite_sequence (name, seq)
SELECT 'phone_changes', sequence_value
FROM phone_changes_0015_snapshot;

DROP TABLE phone_changes_0015_copy_guard;
DROP TABLE phone_changes_0015_snapshot;

CREATE INDEX idx_phone_changes_user_cursor
  ON phone_changes(user_id, cursor);

-- Do not recreate the historical entity/version uniqueness index here. The
-- cursor is the synchronization identity, and rebuilding a non-essential
-- uniqueness constraint would make this additive migration fail on any
-- production database that already contains duplicate legacy invalidations.


-- Restore the pre-0015 sync behavior byte-for-byte against the replacement
-- table before adding Memory-specific invalidations.
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

CREATE TRIGGER pushes_phone_change_insert
AFTER INSERT ON pushes
BEGIN
  UPDATE pushes SET version = 1 WHERE id = NEW.id AND version = 0;
  INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at)
  SELECT NEW.user_id, 'push', NEW.id, NEW.session_id, version, NEW.created_at
  FROM pushes WHERE id = NEW.id;
END;

CREATE TRIGGER pushes_phone_change_update
AFTER UPDATE ON pushes
WHEN NEW.version = OLD.version
BEGIN
  UPDATE pushes SET version = OLD.version + 1 WHERE id = NEW.id;
  INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at)
  VALUES (NEW.user_id, 'push', NEW.id, NEW.session_id, OLD.version + 1, COALESCE(NEW.updated_at, NEW.created_at));
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

-- Inserts and ordinary updates publish invalidations through the existing
-- durable cursor. Soft deletion additionally creates the durable tombstone
-- used to converge offline devices.
CREATE TRIGGER memory_items_phone_change_insert
AFTER INSERT ON memory_items
BEGIN
  INSERT INTO phone_changes (
    user_id, entity_type, entity_id, session_id, version, deleted_at, created_at
  ) VALUES (
    NEW.user_id, 'memory', NEW.id, NEW.source_session_id, NEW.version,
    NEW.deleted_at, NEW.updated_at
  );
  INSERT OR IGNORE INTO sync_tombstones (
    id, user_id, entity_type, entity_id, deleted_at
  )
  SELECT
    'memory_tombstone_' || NEW.id, NEW.user_id, 'memory', NEW.id, NEW.deleted_at
  WHERE NEW.deleted_at IS NOT NULL;
END;

CREATE TRIGGER memory_items_phone_change_update
AFTER UPDATE ON memory_items
WHEN NEW.version = OLD.version
  AND OLD.deleted_at IS NULL
  AND NEW.deleted_at IS OLD.deleted_at
BEGIN
  UPDATE memory_items
  SET version = OLD.version + 1
  WHERE id = NEW.id;
  INSERT INTO phone_changes (
    user_id, entity_type, entity_id, session_id, version, deleted_at, created_at
  ) VALUES (
    NEW.user_id, 'memory', NEW.id, NEW.source_session_id, OLD.version + 1,
    NEW.deleted_at, NEW.updated_at
  );
END;

CREATE TRIGGER memory_items_phone_change_soft_delete
AFTER UPDATE ON memory_items
WHEN NEW.version = OLD.version
  AND OLD.deleted_at IS NULL
  AND NEW.deleted_at IS NOT NULL
BEGIN
  UPDATE memory_items
  SET version = OLD.version + 1
  WHERE id = NEW.id;
  INSERT INTO phone_changes (
    user_id, entity_type, entity_id, session_id, version, deleted_at, created_at
  ) VALUES (
    NEW.user_id, 'memory', NEW.id, NEW.source_session_id, OLD.version + 1,
    NEW.deleted_at, NEW.updated_at
  );
  INSERT OR IGNORE INTO sync_tombstones (
    id, user_id, entity_type, entity_id, deleted_at
  ) VALUES (
    'memory_tombstone_' || NEW.id, NEW.user_id, 'memory', NEW.id, NEW.deleted_at
  );
END;

PRAGMA foreign_keys = ON;
