#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tables="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" "SELECT name FROM sqlite_master WHERE type='table';")"

for table in commands confirmation_tokens session_messages retrieval_items phone_changes outbox_events action_attempts sync_tombstones phone_operations rate_limit_buckets reminders drafts outbound_messages; do
  grep -qx "${table}" <<<"${tables}"
done

columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" "PRAGMA table_info(commands);")"
grep -q "|model_version|" <<<"${columns}"
grep -q "|version|" <<<"${columns}"

push_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" "PRAGMA table_info(pushes);")"
grep -q "|version|" <<<"${push_columns}"
grep -q "|updated_at|" <<<"${push_columns}"

operation_columns="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" "PRAGMA table_info(phone_operations);")"
grep -q "|claim_token|" <<<"${operation_columns}"

changes="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" "INSERT INTO users (id, email, password_hash, created_at) VALUES ('u', 'u@example.com', 'x', 't');" "INSERT INTO agents (id, user_id, label, api_key_hash, created_at) VALUES ('a', 'u', 'agent', 'hash', 't');" "INSERT INTO sessions (id, agent_id, user_id, skill_id, state, expires_at, created_at, updated_at) VALUES ('s', 'a', 'u', 'skill', 'open', '2099-01-01', 't', 't');" "INSERT INTO pushes (id, user_id, title, body, payload_json, created_at, updated_at) VALUES ('p', 'u', 'title', 'body', '{}', 't', 't');" "SELECT count(*) FROM phone_changes;")"
test "${changes}" -ge 2

echo "architecture migration smoke passed: foundation tables, versions, phone change triggers, and rate limits apply to a fresh SQLite database"
