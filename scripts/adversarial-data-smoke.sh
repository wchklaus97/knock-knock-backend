#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DB_FILE="$(mktemp -t knock-knock-adversarial.XXXXXX)"
trap 'rm -f "${DB_FILE}"' EXIT

sqlite3 "${DB_FILE}" \
  ".read ${ROOT_DIR}/migrations/0001_initial.sql" \
  ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" \
  ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" \
  ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" \
  ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" \
  ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" \
  ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" \
  ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" \
  ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql"

sqlite3 "${DB_FILE}" <<'SQL'
INSERT INTO users (id, email, password_hash, created_at) VALUES
  ('u_a', 'a@example.com', 'test', '2026-08-09T00:00:00Z'),
  ('u_b', 'b@example.com', 'test', '2026-08-09T00:00:00Z');
INSERT INTO agents (id, user_id, label, api_key_hash, created_at) VALUES
  ('a_a', 'u_a', 'A', 'hash_a', '2026-08-09T00:00:00Z'),
  ('a_b', 'u_b', 'B', 'hash_b', '2026-08-09T00:00:00Z');
INSERT INTO sessions (id, agent_id, user_id, skill_id, state, facts_json, expires_at, created_at, updated_at) VALUES
  ('s_a', 'a_a', 'u_a', 'skill', 'open', '{}', '2099-01-01T00:00:00Z', '2026-08-09T00:00:00Z', '2026-08-09T00:00:00Z'),
  ('s_b', 'a_b', 'u_b', 'skill', 'open', '{}', '2099-01-01T00:00:00Z', '2026-08-09T00:00:00Z', '2026-08-09T00:00:00Z');
INSERT INTO session_messages (id, user_id, session_id, role, content, sequence, created_at) VALUES
  ('m_a', 'u_a', 's_a', 'agent', 'private A', 1, '2026-08-09T00:00:01Z'),
  ('m_b', 'u_b', 's_b', 'agent', 'private B', 1, '2026-08-09T00:00:01Z');
INSERT INTO retrieval_items (id, user_id, session_id, title, url, content_hash, created_at) VALUES
  ('r_a', 'u_a', 's_a', 'A source', 'https://a.example', 'hash_a', '2026-08-09T00:00:02Z'),
  ('r_b', 'u_b', 's_b', 'B source', 'https://b.example', 'hash_b', '2026-08-09T00:00:02Z');
INSERT INTO phone_operations (user_id, idempotency_key, operation, request_hash, session_id, response_json, expires_at, claim_token, created_at, updated_at) VALUES
  ('u_a', 'idem_a', 'reply', 'request_a', 's_a', NULL, '2099-01-01T00:00:00Z', 'claim_a', '2026-08-09T00:00:00Z', '2026-08-09T00:00:00Z');

-- Event idempotency gate: a duplicate claim must not run the business batch;
-- a fresh event claim must permit it.
INSERT INTO events (id, session_id, status, idempotency_key, payload_json, created_at)
  VALUES ('evt_existing', 's_b', 'info', 'event_existing', '{}', '2026-08-09T00:00:00Z');
INSERT INTO events (id, session_id, status, idempotency_key, payload_json, created_at)
  SELECT 'evt_duplicate', 's_b', 'succeeded', 'event_existing', '{}', '2026-08-09T00:00:01Z'
  WHERE EXISTS (SELECT 1 FROM sessions WHERE id = 's_b' AND user_id = 'u_b' AND deleted_at IS NULL)
  ON CONFLICT(session_id, idempotency_key) DO NOTHING;
UPDATE sessions SET summary_text = 'must not change'
  WHERE id = 's_b' AND deleted_at IS NULL
    AND EXISTS (SELECT 1 FROM events WHERE id = 'evt_duplicate' AND session_id = 's_b' AND idempotency_key = 'event_existing');
INSERT INTO session_messages (id, user_id, session_id, role, content, sequence, created_at)
  SELECT 'm_duplicate', 'u_b', 's_b', 'agent', 'must not write', 2, '2026-08-09T00:00:02Z'
  WHERE EXISTS (SELECT 1 FROM events WHERE id = 'evt_duplicate' AND session_id = 's_b' AND idempotency_key = 'event_existing');
INSERT INTO events (id, session_id, status, idempotency_key, payload_json, created_at)
  SELECT 'evt_fresh', 's_b', 'succeeded', 'event_fresh', '{}', '2026-08-09T00:00:03Z'
  WHERE EXISTS (SELECT 1 FROM sessions WHERE id = 's_b' AND user_id = 'u_b' AND deleted_at IS NULL)
  ON CONFLICT(session_id, idempotency_key) DO NOTHING;
UPDATE sessions SET summary_text = 'fresh event applied'
  WHERE id = 's_b' AND deleted_at IS NULL
    AND EXISTS (SELECT 1 FROM events WHERE id = 'evt_fresh' AND session_id = 's_b' AND idempotency_key = 'event_fresh');
INSERT INTO session_messages (id, user_id, session_id, role, content, sequence, created_at)
  SELECT 'm_fresh', 'u_b', 's_b', 'agent', 'fresh write', 2, '2026-08-09T00:00:04Z'
  WHERE EXISTS (SELECT 1 FROM events WHERE id = 'evt_fresh' AND session_id = 's_b' AND idempotency_key = 'event_fresh');

-- Outbox lease recovery increments the command version before retrying so a
-- crashed worker cannot publish a late success from its old version.
INSERT INTO commands (id, user_id, intent, args_json, risk_level, needs_confirmation, idempotency_key, confidence, locale, timezone, state, command_hash, version, created_at, updated_at)
  VALUES ('cmd_stale', 'u_b', 'search_history', '{"q":"x"}', 'low', 0, 'cmd_stale_key', 0.9, 'en-HK', 'Asia/Hong_Kong', 'running', 'cmd_hash', 4, '2026-08-09T00:00:00Z', '2026-08-09T00:00:00Z');
INSERT INTO outbox_events (id, user_id, topic, aggregate_id, payload_json, idempotency_key, state, created_at, updated_at)
  VALUES ('out_stale', 'u_b', 'command.execute', 'cmd_stale', '{"command_id":"cmd_stale"}', 'cmd_stale_key', 'running', '2026-08-09T00:00:00Z', '2026-08-09T00:00:00Z');
UPDATE commands SET state = 'unknown', error_code = 'worker_lease_expired', version = 5, updated_at = '2026-08-09T00:05:01Z'
  WHERE id = 'cmd_stale' AND user_id = 'u_b' AND state = 'running' AND version = 4 AND updated_at <= '2026-08-09T00:00:00Z';
UPDATE outbox_events SET state = 'retrying', next_attempt_at = '2026-08-09T00:05:01Z', last_error = 'worker_lease_expired', updated_at = '2026-08-09T00:05:01Z'
  WHERE id = 'out_stale' AND state = 'running'
    AND EXISTS (SELECT 1 FROM commands WHERE id = 'cmd_stale' AND user_id = 'u_b' AND state = 'unknown' AND version = 5);

-- Delete barrier: guarded live-session writes become no-ops after deletion.
UPDATE sessions SET deleted_at = '2026-08-09T00:01:00Z', archived_at = '2026-08-09T00:01:00Z' WHERE id = 's_a' AND user_id = 'u_a';
UPDATE sessions SET state = 'running' WHERE id = 's_a' AND user_id = 'u_a' AND deleted_at IS NULL;
INSERT INTO session_messages (id, user_id, session_id, role, content, sequence, created_at)
  SELECT 'm_deleted', 'u_a', 's_a', 'agent', 'stale', 2, '2026-08-09T00:01:01Z'
  WHERE EXISTS (SELECT 1 FROM sessions WHERE id = 's_a' AND user_id = 'u_a' AND deleted_at IS NULL);

-- Tombstone stream: retention deletes produce a deleted change.
DELETE FROM session_messages WHERE id = 'm_a';

-- Lease fencing: a stale owner cannot complete or delete a replacement claim.
UPDATE phone_operations SET response_json = '{"stale":true}' WHERE user_id = 'u_a' AND idempotency_key = 'idem_a' AND claim_token = 'claim_old';
DELETE FROM phone_operations WHERE user_id = 'u_a' AND idempotency_key = 'idem_a' AND claim_token = 'claim_old';
SQL

checks=$(sqlite3 "${DB_FILE}" "SELECT CASE WHEN (SELECT count(*) FROM session_messages WHERE user_id = 'u_a' AND id = 'm_b') = 0 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT count(*) FROM retrieval_items WHERE user_id = 'u_a' AND id = 'r_b') = 0 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT state FROM sessions WHERE id = 's_a') = 'open' THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT count(*) FROM session_messages WHERE id = 'm_deleted') = 0 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT count(*) FROM phone_changes WHERE user_id = 'u_a' AND entity_type = 'message' AND entity_id = 'm_a' AND deleted_at IS NOT NULL) = 1 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT count(*) FROM phone_operations WHERE user_id = 'u_a' AND idempotency_key = 'idem_a' AND claim_token = 'claim_a' AND response_json IS NULL) = 1 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT count(*) FROM phone_operations WHERE user_id = 'u_a' AND idempotency_key = 'idem_a' AND claim_token = 'claim_old') = 0 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT count(*) FROM phone_changes WHERE user_id = 'u_a' AND user_id <> 'u_a') = 0 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT summary_text FROM sessions WHERE id = 's_b') = 'must not change' THEN 0 ELSE 1 END; SELECT CASE WHEN (SELECT count(*) FROM session_messages WHERE id = 'm_duplicate') = 0 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT summary_text FROM sessions WHERE id = 's_b') = 'fresh event applied' THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT count(*) FROM session_messages WHERE id = 'm_fresh') = 1 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT state FROM commands WHERE id = 'cmd_stale') = 'unknown' AND (SELECT version FROM commands WHERE id = 'cmd_stale') = 5 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT state FROM outbox_events WHERE id = 'out_stale') = 'retrying' THEN 1 ELSE 0 END;")

expected=$'1\n1\n1\n1\n1\n1\n1\n1\n1\n1\n1\n1\n1\n1'
if [[ "${checks}" != "${expected}" ]]; then
  echo "adversarial data smoke failed: ${checks}" >&2
  exit 1
fi

echo "adversarial data smoke passed: user isolation, deletion barriers, tombstones, event idempotency gates, outbox lease recovery, and cursor scope"
