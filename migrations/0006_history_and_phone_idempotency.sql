-- History/retrieval product APIs and compatibility-operation idempotency.
-- Additive only: existing audit and phone reply/confirm routes remain valid.

CREATE TABLE IF NOT EXISTS phone_operations (
  user_id TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  operation TEXT NOT NULL CHECK (operation IN ('reply', 'confirm')),
  response_json TEXT,
  expires_at TEXT NOT NULL,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  PRIMARY KEY (user_id, idempotency_key),
  FOREIGN KEY (user_id) REFERENCES users(id)
);

CREATE INDEX IF NOT EXISTS idx_phone_operations_expiry
  ON phone_operations(expires_at);

CREATE INDEX IF NOT EXISTS idx_messages_user_session_cursor
  ON session_messages(user_id, session_id, created_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS idx_retrieval_user_session_cursor
  ON retrieval_items(user_id, session_id, created_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS idx_sessions_user_title
  ON sessions(user_id, title, updated_at DESC, id DESC);
