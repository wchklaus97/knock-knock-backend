#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tables="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" "SELECT name FROM sqlite_master WHERE type='table';")"

for table in commands confirmation_tokens session_messages retrieval_items phone_changes outbox_events action_attempts sync_tombstones phone_operations; do
  grep -qx "${table}" <<<"${tables}"
done

columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" "PRAGMA table_info(commands);")"
grep -q "|model_version|" <<<"${columns}"
grep -q "|version|" <<<"${columns}"

push_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" "PRAGMA table_info(pushes);")"
grep -q "|version|" <<<"${push_columns}"
grep -q "|updated_at|" <<<"${push_columns}"

changes="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" "INSERT INTO users (id, email, password_hash, created_at) VALUES ('u', 'u@example.com', 'x', 't');" "INSERT INTO agents (id, user_id, label, api_key_hash, created_at) VALUES ('a', 'u', 'agent', 'hash', 't');" "INSERT INTO sessions (id, agent_id, user_id, skill_id, state, expires_at, created_at, updated_at) VALUES ('s', 'a', 'u', 'skill', 'open', '2099-01-01', 't', 't');" "INSERT INTO pushes (id, user_id, title, body, payload_json, created_at, updated_at) VALUES ('p', 'u', 'title', 'body', '{}', 't', 't');" "INSERT INTO session_messages (id, user_id, session_id, role, content, sequence, created_at) VALUES ('m', 'u', 's', 'agent', 'x', 1, 't');" "SELECT count(*) FROM phone_changes;")"
test "${changes}" -ge 2

phone_change_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" "PRAGMA table_info(phone_changes);")"
grep -q "|deleted_at|" <<<"${phone_change_columns}"

operation_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" "PRAGMA table_info(phone_operations);")"
grep -q "|request_hash|" <<<"${operation_columns}"
grep -q "|session_id|" <<<"${operation_columns}"

echo "architecture migration smoke passed: foundation tables, deletion-aware changes, retrieval triggers, and idempotency metadata apply to a fresh SQLite database"
