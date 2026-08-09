#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tables="$(sqlite3 :memory: ".read ${ROOT_DIR}/migrations/0001_initial.sql" ".read ${ROOT_DIR}/migrations/0002_supabase_auth.sql" ".read ${ROOT_DIR}/migrations/0003_architecture_foundation.sql" "SELECT name FROM sqlite_master WHERE type='table';")"

for table in commands confirmation_tokens session_messages retrieval_items phone_changes outbox_events action_attempts sync_tombstones; do
  grep -qx "${table}" <<<"${tables}"
done

echo "architecture migration smoke passed: foundation tables apply to a fresh SQLite database"
