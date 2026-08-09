#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

grep -q 'secret_value(env, "ACTION_REMINDER_TOKEN")' "$ROOT_DIR/src/providers.rs"
grep -q 'secret_value(env, "ACTION_MESSAGE_TOKEN")' "$ROOT_DIR/src/providers.rs"
grep -q 'x-idempotency-key' "$ROOT_DIR/src/providers.rs"
grep -q 'provider_network_error' "$ROOT_DIR/src/providers.rs"
grep -q 'provider_rejected' "$ROOT_DIR/src/providers.rs"
grep -q 'external.reminder' "$ROOT_DIR/src/action_effects.rs"
grep -q 'external.message' "$ROOT_DIR/src/action_effects.rs"
grep -q "provider = 'local.reminder'" "$ROOT_DIR/src/reminders.rs"
grep -q 'idx_pushes_dedupe_key' "$ROOT_DIR/migrations/0012_reminder_delivery_state.sql"

echo "provider safety smoke passed: secret-only credentials, idempotency, fail-closed errors, and local reminder dedupe are wired"
