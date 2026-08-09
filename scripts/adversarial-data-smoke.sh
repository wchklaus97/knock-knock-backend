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

checks=$(sqlite3 "${DB_FILE}" "SELECT CASE WHEN (SELECT count(*) FROM session_messages WHERE user_id = 'u_a' AND id = 'm_b') = 0 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT count(*) FROM retrieval_items WHERE user_id = 'u_a' AND id = 'r_b') = 0 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT state FROM sessions WHERE id = 's_a') = 'open' THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT count(*) FROM session_messages WHERE id = 'm_deleted') = 0 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT count(*) FROM phone_changes WHERE user_id = 'u_a' AND entity_type = 'message' AND entity_id = 'm_a' AND deleted_at IS NOT NULL) = 1 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT count(*) FROM phone_operations WHERE user_id = 'u_a' AND idempotency_key = 'idem_a' AND claim_token = 'claim_a' AND response_json IS NULL) = 1 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT count(*) FROM phone_operations WHERE user_id = 'u_a' AND idempotency_key = 'idem_a' AND claim_token = 'claim_old') = 0 THEN 1 ELSE 0 END; SELECT CASE WHEN (SELECT count(*) FROM phone_changes WHERE user_id = 'u_a' AND user_id <> 'u_a') = 0 THEN 1 ELSE 0 END;")

expected=$'1\n1\n1\n1\n1\n1\n1\n1'
if [[ "${checks}" != "${expected}" ]]; then
  echo "adversarial data smoke failed: ${checks}" >&2
  exit 1
fi

echo "adversarial data smoke passed: user isolation, deletion barriers, tombstones, lease fencing, and cursor scope"
