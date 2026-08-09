-- Durable records for the three Phase 4/5 vertical actions.
--
-- These tables are intentionally separate from commands and audit_logs:
-- commands describe intent/lifecycle, these tables describe the materialized
-- effect, and audit_logs remains the security trail. Every row is keyed by the
-- originating command so an outbox retry cannot create a duplicate effect.

CREATE TABLE IF NOT EXISTS reminders (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  command_id TEXT NOT NULL,
  session_id TEXT,
  title TEXT NOT NULL,
  due_at TEXT NOT NULL,
  timezone TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('scheduled', 'cancelled', 'completed')),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (user_id, command_id),
  FOREIGN KEY (user_id) REFERENCES users(id),
  FOREIGN KEY (command_id) REFERENCES commands(id),
  FOREIGN KEY (session_id) REFERENCES sessions(id)
);

CREATE TABLE IF NOT EXISTS drafts (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  command_id TEXT NOT NULL,
  session_id TEXT,
  title TEXT,
  recipient TEXT,
  body TEXT NOT NULL,
  status TEXT NOT NULL CHECK (status IN ('draft', 'cancelled', 'sent')),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (user_id, command_id),
  FOREIGN KEY (user_id) REFERENCES users(id),
  FOREIGN KEY (command_id) REFERENCES commands(id),
  FOREIGN KEY (session_id) REFERENCES sessions(id)
);

CREATE TABLE IF NOT EXISTS outbound_messages (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  command_id TEXT NOT NULL,
  session_id TEXT,
  recipient TEXT NOT NULL,
  body TEXT NOT NULL,
  provider TEXT NOT NULL,
  delivery_state TEXT NOT NULL CHECK (delivery_state IN ('queued', 'sent', 'failed', 'cancelled')),
  provider_message_id TEXT,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE (user_id, command_id),
  FOREIGN KEY (user_id) REFERENCES users(id),
  FOREIGN KEY (command_id) REFERENCES commands(id),
  FOREIGN KEY (session_id) REFERENCES sessions(id)
);

CREATE INDEX IF NOT EXISTS idx_reminders_due
  ON reminders(user_id, status, due_at);
CREATE INDEX IF NOT EXISTS idx_drafts_user_status
  ON drafts(user_id, status, updated_at DESC);
CREATE INDEX IF NOT EXISTS idx_outbound_messages_user_state
  ON outbound_messages(user_id, delivery_state, updated_at DESC);
