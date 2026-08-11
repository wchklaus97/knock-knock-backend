#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOCAL_CONFIG="$ROOT/wrangler.toml"
LOCAL_EXAMPLE="$ROOT/wrangler.toml.example"
PRODUCTION_EXAMPLE="$ROOT/wrangler.production.toml.example"
STAGING_EXAMPLE="$ROOT/wrangler.staging.toml.example"

for file in "$LOCAL_CONFIG" "$LOCAL_EXAMPLE" "$PRODUCTION_EXAMPLE" "$STAGING_EXAMPLE"; do
  test -f "$file"
done

grep -q '^NODE_ENV = "development"$' "$LOCAL_CONFIG"
grep -q '^PUSH_MODE = "dev"$' "$LOCAL_CONFIG"
grep -q '^ACTION_PROVIDER_MODE = "internal"$' "$LOCAL_CONFIG"
grep -q '^ACTION_REMINDER_ENABLED = "true"$' "$LOCAL_CONFIG"
grep -q '^ACTION_MESSAGE_ENABLED = "true"$' "$LOCAL_CONFIG"
grep -q '^NODE_ENV = "development"$' "$LOCAL_EXAMPLE"
grep -q '^NODE_ENV = "production"$' "$PRODUCTION_EXAMPLE"
grep -q '^AUTH_PROVIDER = "supabase"$' "$PRODUCTION_EXAMPLE"
grep -q '^SUPABASE_URL = "REPLACE_WITH_SUPABASE_PROJECT_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^PUSH_MODE = "both"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_PROVIDER_MODE = "external"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_REMINDER_ENABLED = "false"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_MESSAGE_ENABLED = "false"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_REMINDER_URL = "REPLACE_WITH_REMINDER_PROVIDER_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_MESSAGE_URL = "REPLACE_WITH_MESSAGE_PROVIDER_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_REMINDER_CANCEL_URL = "REPLACE_WITH_REMINDER_CANCEL_PROVIDER_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_REMINDER_STATUS_URL = "REPLACE_WITH_REMINDER_STATUS_PROVIDER_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_MESSAGE_STATUS_URL = "REPLACE_WITH_MESSAGE_STATUS_PROVIDER_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^CORS_ORIGIN = "REPLACE_WITH_ALLOWED_ORIGIN"$' "$PRODUCTION_EXAMPLE"
grep -q '^SERVICE_VERSION = "REPLACE_WITH_RELEASE_VERSION"$' "$PRODUCTION_EXAMPLE"
grep -q '^binding = "R2"$' "$PRODUCTION_EXAMPLE"
grep -q '^bucket_name = "REPLACE_WITH_R2_BUCKET_NAME"$' "$PRODUCTION_EXAMPLE"
grep -q '^VOICE_MODEL_ENABLED = "true"$' "$PRODUCTION_EXAMPLE"
grep -q '^VOICE_MODEL_URL = "REPLACE_WITH_SIGNED_MODEL_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^VOICE_MODEL_R2_KEY = "REPLACE_WITH_SIGNED_MODEL_R2_KEY"$' "$PRODUCTION_EXAMPLE"
grep -q '^VOICE_MODEL_MANIFEST_JSON = "REPLACE_WITH_SIGNED_MODEL_MANIFEST_JSON"$' "$PRODUCTION_EXAMPLE"
grep -q '^NODE_ENV = "staging"$' "$STAGING_EXAMPLE"
grep -q '^AUTH_PROVIDER = "supabase"$' "$STAGING_EXAMPLE"
grep -q '^PUSH_MODE = "both"$' "$STAGING_EXAMPLE"
grep -q '^APNS_PRODUCTION = "false"$' "$STAGING_EXAMPLE"
grep -q '^ACTION_PROVIDER_MODE = "disabled"$' "$STAGING_EXAMPLE"
grep -q '^ACTION_REMINDER_ENABLED = "false"$' "$STAGING_EXAMPLE"
grep -q '^ACTION_MESSAGE_ENABLED = "false"$' "$STAGING_EXAMPLE"
grep -q '^CORS_ORIGIN = "REPLACE_WITH_STAGING_ALLOWED_ORIGIN"$' "$STAGING_EXAMPLE"
grep -q '^SERVICE_VERSION = "REPLACE_WITH_STAGING_RELEASE_VERSION"$' "$STAGING_EXAMPLE"
grep -q '^binding = "R2"$' "$STAGING_EXAMPLE"
grep -q '^bucket_name = "REPLACE_WITH_STAGING_R2_BUCKET_NAME"$' "$STAGING_EXAMPLE"

grep -q 'BACKUP_BUCKET' "$ROOT/.github/workflows/production-backup.yml"
grep -q 'BACKUP_PASSPHRASE' "$ROOT/.github/workflows/production-backup.yml"
grep -q 'gpg' "$ROOT/.github/workflows/production-backup.yml"
if grep -q 'actions/upload-artifact' "$ROOT/.github/workflows/production-backup.yml"; then
  echo "production backups must not be retained as plaintext CI artifacts" >&2
  exit 1
fi

if grep -q '^CORS_ORIGIN = "\*"$' "$PRODUCTION_EXAMPLE"; then
  echo "production config must not allow wildcard CORS" >&2
  exit 1
fi

grep -q 'pub fn runtime_configuration' "$ROOT/src/auth.rs"
grep -q 'runtime_configuration(&env)' "$ROOT/src/lib.rs"
grep -q 'must be configured for production APNs' "$ROOT/src/auth.rs"
grep -q 'PUSH_MODE must be apns or both in production' "$ROOT/src/auth.rs"
grep -q 'PUSH_MODE must be dev, apns, or both in staging' "$ROOT/src/auth.rs"
grep -q 'APNS_PRODUCTION must be false in staging' "$ROOT/src/auth.rs"
grep -q 'SUPABASE_PUBLISHABLE_KEY must be configured' "$ROOT/src/auth.rs"

echo "production config smoke passed: local defaults are explicit and production is fail-closed"
