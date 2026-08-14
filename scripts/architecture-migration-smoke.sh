#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# The historical checks below intentionally keep their explicit migration
# list. Inject the later migrations immediately before each final SQL assertion
# so a fresh-D1 smoke verifies through 0015 rather than silently stopping at
# the earlier schema.
sqlite3() {
  local database="$1"
  shift
  local last="${!#}"
  local count=$#
  local args=("${@:1:$((count - 1))}" ".read ${ROOT_DIR}/migrations/0012_reminder_delivery_state.sql" ".read ${ROOT_DIR}/migrations/0013_retrieval_retention_status.sql" ".read ${ROOT_DIR}/migrations/0014_command_safety.sql" ".read ${ROOT_DIR}/migrations/0015_structured_memory.sql" "$last")
  command sqlite3 "$database" "${args[@]}"
}

tables="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "SELECT name FROM sqlite_master WHERE type='table';")"

for table in commands confirmation_tokens session_messages retrieval_items memory_items phone_changes outbox_events action_attempts sync_tombstones phone_operations rate_limit_buckets reminders drafts outbound_messages; do
  grep -qx "${table}" <<<"${tables}"
done

columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "PRAGMA table_info(commands);")"
grep -q "|model_version|" <<<"${columns}"
grep -q "|version|" <<<"${columns}"

retrieval_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "PRAGMA table_info(retrieval_items);")"
grep -q "|r2_delete_status|" <<<"${retrieval_columns}"
grep -q "|r2_deleted_at|" <<<"${retrieval_columns}"
grep -q "|expired_at|" <<<"${retrieval_columns}"

outbox_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "PRAGMA table_info(outbox_events);")"
grep -q "|lease_token|" <<<"${outbox_columns}"
grep -q "|lease_expires_at|" <<<"${outbox_columns}"

command_sql="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'commands';")"
grep -q "'retryable'" <<<"${command_sql}"

foreign_keys="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "PRAGMA foreign_keys;")"
test "${foreign_keys}" = "1"

session_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "PRAGMA table_info(sessions);")"
grep -q "|available_action_descriptors_json|" <<<"${session_columns}"

action_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "PRAGMA table_info(actions);")"
grep -q "|descriptor_json|" <<<"${action_columns}"

push_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "PRAGMA table_info(pushes);")"
grep -q "|version|" <<<"${push_columns}"
grep -q "|updated_at|" <<<"${push_columns}"
grep -q "|dedupe_key|" <<<"${push_columns}"

reminder_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "PRAGMA table_info(reminders);")"
grep -q "|notification_state|" <<<"${reminder_columns}"
grep -q "|notification_attempts|" <<<"${reminder_columns}"
grep -q "|notified_at|" <<<"${reminder_columns}"
grep -q "|provider|" <<<"${reminder_columns}"
grep -q "|provider_reminder_id|" <<<"${reminder_columns}"

operation_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "PRAGMA table_info(phone_operations);")"
grep -q "|claim_token|" <<<"${operation_columns}"

pairing_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "PRAGMA table_info(pairing_codes);")"
grep -q "|claim_token|" <<<"${pairing_columns}"

changes="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "INSERT INTO users (id, email, password_hash, created_at) VALUES ('u', 'u@example.com', 'x', 't');" "INSERT INTO agents (id, user_id, label, api_key_hash, created_at) VALUES ('a', 'u', 'agent', 'hash', 't');" "INSERT INTO sessions (id, agent_id, user_id, skill_id, state, expires_at, created_at, updated_at) VALUES ('s', 'a', 'u', 'skill', 'open', '2099-01-01', 't', 't');" "INSERT INTO pushes (id, user_id, title, body, payload_json, created_at, updated_at) VALUES ('p', 'u', 'title', 'body', '{}', 't', 't');" "SELECT count(*) FROM phone_changes;")"
test "${changes}" -ge 2

memory_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "PRAGMA table_info(memory_items);")"
for column in id user_id schema_version kind subject predicate value_json display_text locale source_type source_session_id source_message_id user_confirmed confidence idempotency_key request_hash version retention_expires_at created_at updated_at deleted_at; do
  grep -q "|${column}|" <<<"${memory_columns}"
done

memory_sql="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'memory_items';")"
grep -q "source_type IN ('explicit_user', 'trusted_system')" <<<"${memory_sql}"
grep -q "UNIQUE (user_id, idempotency_key)" <<<"${memory_sql}"

phone_changes_sql="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'phone_changes';")"
grep -q "'memory'" <<<"${phone_changes_sql}"

entity_version_index_count="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'idx_phone_changes_entity_version';")"
test "${entity_version_index_count}" = "0"

# Exercise 0015 against a populated pre-migration fixture. This proves the
# rebuild preserves every explicit cursor and the AUTOINCREMENT high-water
# mark, leaves historical trigger bodies attached to the replacement table,
# and allocates only newer cursors after migration.
fixture_db="$(mktemp "${TMPDIR:-/tmp}/knock-knock-memory-migration.XXXXXX")"
cleanup() {
  rm -f "${fixture_db}"
}
trap cleanup EXIT

pre_memory_migrations=(
  "${ROOT_DIR}/migrations/0001_initial.sql"
  "${ROOT_DIR}/migrations/0002_supabase_auth.sql"
  "${ROOT_DIR}/migrations/0003_architecture_foundation.sql"
  "${ROOT_DIR}/migrations/0004_command_versions.sql"
  "${ROOT_DIR}/migrations/0005_phone_change_triggers.sql"
  "${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql"
  "${ROOT_DIR}/migrations/0007_rate_limits.sql"
  "${ROOT_DIR}/migrations/0008_history_consistency.sql"
  "${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql"
  "${ROOT_DIR}/migrations/0010_vertical_action_effects.sql"
  "${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql"
  "${ROOT_DIR}/migrations/0012_reminder_delivery_state.sql"
  "${ROOT_DIR}/migrations/0013_retrieval_retention_status.sql"
  "${ROOT_DIR}/migrations/0014_command_safety.sql"
)
pre_memory_commands=()
for migration in "${pre_memory_migrations[@]}"; do
  pre_memory_commands+=(".read ${migration}")
done
command sqlite3 "${fixture_db}" "${pre_memory_commands[@]}"

command sqlite3 "${fixture_db}" \
  "INSERT INTO users (id, email, password_hash, created_at) VALUES ('fixture_user', 'fixture@example.com', 'x', '2026-08-14T00:00:00.000Z');" \
  "INSERT INTO agents (id, user_id, label, api_key_hash, created_at) VALUES ('fixture_agent', 'fixture_user', 'agent', 'hash', '2026-08-14T00:00:00.000Z');" \
  "INSERT INTO sessions (id, agent_id, user_id, skill_id, state, expires_at, created_at, updated_at) VALUES ('fixture_session', 'fixture_agent', 'fixture_user', 'skill', 'open', '2099-01-01T00:00:00.000Z', '2026-08-14T00:00:00.000Z', '2026-08-14T00:00:00.000Z');" \
  "INSERT INTO pushes (id, user_id, title, body, payload_json, created_at, updated_at) VALUES ('fixture_push', 'fixture_user', 'title', 'body', '{}', '2026-08-14T00:00:00.000Z', '2026-08-14T00:00:00.000Z');" \
  "INSERT INTO phone_changes (cursor, user_id, entity_type, entity_id, session_id, version, created_at, deleted_at) VALUES (17, 'fixture_user', 'command', 'legacy_command', 'fixture_session', 7, '2026-08-14T00:00:00.000Z', NULL);" \
  "UPDATE sqlite_sequence SET seq = 40 WHERE name = 'phone_changes';"

before_count="$(command sqlite3 "${fixture_db}" "SELECT COUNT(*) FROM phone_changes;")"
before_cursors="$(command sqlite3 "${fixture_db}" "SELECT group_concat(cursor, ',') FROM (SELECT cursor FROM phone_changes ORDER BY cursor);")"
before_max="$(command sqlite3 "${fixture_db}" "SELECT MAX(cursor) FROM phone_changes;")"
before_sequence="$(command sqlite3 "${fixture_db}" "SELECT seq FROM sqlite_sequence WHERE name = 'phone_changes';")"

command sqlite3 "${fixture_db}" ".read ${ROOT_DIR}/migrations/0015_structured_memory.sql"

after_count="$(command sqlite3 "${fixture_db}" "SELECT COUNT(*) FROM phone_changes;")"
after_cursors="$(command sqlite3 "${fixture_db}" "SELECT group_concat(cursor, ',') FROM (SELECT cursor FROM phone_changes ORDER BY cursor);")"
after_max="$(command sqlite3 "${fixture_db}" "SELECT MAX(cursor) FROM phone_changes;")"
after_sequence="$(command sqlite3 "${fixture_db}" "SELECT seq FROM sqlite_sequence WHERE name = 'phone_changes';")"
test "${after_count}" = "${before_count}"
test "${after_cursors}" = "${before_cursors}"
test "${after_max}" = "${before_max}"
test "${after_sequence}" = "${before_sequence}"

legacy_trigger_sql="$(command sqlite3 "${fixture_db}" "SELECT group_concat(sql, ' ') FROM sqlite_master WHERE type = 'trigger' AND name IN ('sessions_phone_change_insert', 'sessions_phone_change_update', 'pushes_phone_change_insert', 'pushes_phone_change_update');")"
grep -q "phone_changes" <<<"${legacy_trigger_sql}"
if grep -q "phone_changes_next\|phone_changes_legacy" <<<"${legacy_trigger_sql}"; then
  echo "legacy trigger was rewritten to a migration-only table" >&2
  exit 1
fi

command sqlite3 "${fixture_db}" \
  "UPDATE sessions SET title = 'after migration', updated_at = '2026-08-14T00:00:01.000Z' WHERE id = 'fixture_session';" \
  "UPDATE pushes SET title = 'after migration', updated_at = '2026-08-14T00:00:01.000Z' WHERE id = 'fixture_push';"
test "$(command sqlite3 "${fixture_db}" "SELECT COUNT(*) FROM phone_changes WHERE entity_type IN ('session', 'push') AND created_at = '2026-08-14T00:00:01.000Z';")" = "2"

command sqlite3 "${fixture_db}" \
  "INSERT INTO memory_items (id, user_id, schema_version, kind, subject, predicate, value_json, display_text, locale, source_type, user_confirmed, confidence, idempotency_key, request_hash, version, created_at, updated_at) VALUES ('fixture_memory', 'fixture_user', 1, 'fact', 'user', 'timezone', '{\"name\":\"Asia/Hong_Kong\"}', 'The user timezone is Asia/Hong_Kong.', 'en-HK', 'explicit_user', 1, 1.0, 'fixture-memory-key', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 1, '2026-08-14T00:00:02.000Z', '2026-08-14T00:00:02.000Z');" \
  "UPDATE memory_items SET display_text = 'The user timezone remains Asia/Hong_Kong.', updated_at = '2026-08-14T00:00:03.000Z' WHERE id = 'fixture_memory';" \
  "UPDATE memory_items SET deleted_at = '2026-08-14T00:00:04.000Z', updated_at = '2026-08-14T00:00:04.000Z' WHERE id = 'fixture_memory';"

test "$(command sqlite3 "${fixture_db}" "SELECT group_concat(version, ',') FROM (SELECT version FROM phone_changes WHERE entity_type = 'memory' AND entity_id = 'fixture_memory' ORDER BY cursor);")" = "1,2,3"
test "$(command sqlite3 "${fixture_db}" "SELECT COUNT(*) FROM sync_tombstones WHERE user_id = 'fixture_user' AND entity_type = 'memory' AND entity_id = 'fixture_memory';")" = "1"
next_cursor="$(command sqlite3 "${fixture_db}" "SELECT MAX(cursor) FROM phone_changes;")"
test "${next_cursor}" -gt "${before_sequence}"
test "${next_cursor}" -gt "${before_max}"

# Account-row deletion reaches memory_items through its explicit CASCADE. The
# fixture removes sync/audit dependents first, matching the existing teardown
# ordering, and deliberately does not delete the memory row itself.
command sqlite3 "${fixture_db}" \
  "PRAGMA foreign_keys = ON;" \
  "INSERT INTO users (id, email, password_hash, created_at) VALUES ('erase_user', 'erase@example.com', 'x', '2026-08-14T00:00:00.000Z');" \
  "INSERT INTO memory_items (id, user_id, schema_version, kind, subject, predicate, value_json, display_text, locale, source_type, user_confirmed, confidence, idempotency_key, request_hash, version, created_at, updated_at) VALUES ('erase_memory', 'erase_user', 1, 'fact', 'user', 'timezone', '\"UTC\"', 'The user timezone is UTC.', 'en', 'explicit_user', 1, 1.0, 'erase-memory-key', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 1, '2026-08-14T00:00:00.000Z', '2026-08-14T00:00:00.000Z');" \
  "DELETE FROM phone_changes WHERE user_id = 'erase_user';" \
  "DELETE FROM sync_tombstones WHERE user_id = 'erase_user';" \
  "DELETE FROM users WHERE id = 'erase_user';"
test "$(command sqlite3 "${fixture_db}" "SELECT COUNT(*) FROM memory_items WHERE user_id = 'erase_user';")" = "0"

echo "architecture migration smoke passed: migrations through 0015, structured-memory triggers/tombstones, exact phone cursor preservation, legacy trigger continuity, user-delete cascade, leases, and foreign keys apply to SQLite"
