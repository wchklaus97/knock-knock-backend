#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# The historical checks below intentionally keep their explicit migration
# list. Inject the G1 migrations immediately before each final SQL assertion
# so a fresh-D1 smoke verifies 0013 then 0014 rather than silently stopping at
# the pre-G1 schema.
sqlite3() {
  local database="$1"
  shift
  local last="${!#}"
  local count=$#
  local args=("${@:1:$((count - 1))}" ".read ${ROOT_DIR}/migrations/0012_reminder_delivery_state.sql" ".read ${ROOT_DIR}/migrations/0013_retrieval_retention_status.sql" ".read ${ROOT_DIR}/migrations/0014_command_safety.sql" "$last")
  command sqlite3 "$database" "${args[@]}"
}

tables="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" ".read ${ROOT_DIR}/migrations/0004_command_versions.sql" ".read ${ROOT_DIR}/migrations/0005_phone_change_triggers.sql" ".read ${ROOT_DIR}/migrations/0006_history_and_phone_idempotency.sql" ".read ${ROOT_DIR}/migrations/0007_rate_limits.sql" ".read ${ROOT_DIR}/migrations/0008_history_consistency.sql" ".read ${ROOT_DIR}/migrations/0009_phone_operation_claim_tokens.sql" ".read ${ROOT_DIR}/migrations/0010_vertical_action_effects.sql" ".read ${ROOT_DIR}/migrations/0011_command_pairing_action_descriptors.sql" "SELECT name FROM sqlite_master WHERE type='table';")"

for table in commands confirmation_tokens session_messages retrieval_items phone_changes outbox_events action_attempts sync_tombstones phone_operations rate_limit_buckets reminders drafts outbound_messages; do
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

echo "architecture migration smoke passed: migrations 0013 then 0014, retrieval tombstones, command retryable state, leases, and foreign keys apply to a fresh SQLite database"
