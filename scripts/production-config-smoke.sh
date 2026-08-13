#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOCAL_CONFIG="$ROOT/wrangler.toml"
LOCAL_EXAMPLE="$ROOT/wrangler.toml.example"
PRODUCTION_EXAMPLE="$ROOT/wrangler.production.toml.example"
STAGING_EXAMPLE="$ROOT/wrangler.staging.toml.example"
BACKUP_WORKFLOW="$ROOT/.github/workflows/production-backup.yml"

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

grep -q 'BACKUP_BUCKET' "$BACKUP_WORKFLOW"
grep -q 'BACKUP_PASSPHRASE' "$BACKUP_WORKFLOW"
grep -q 'gpg' "$BACKUP_WORKFLOW"
grep -Fq 'name: production-backup' "$BACKUP_WORKFLOW"
grep -Fq 'group: knock-knock-production-backup' "$BACKUP_WORKFLOW"
grep -Fq "[[ -z \"\$BACKUP_BUCKET\" ]]" "$BACKUP_WORKFLOW"
grep -Fq '^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$' "$BACKUP_WORKFLOW"
grep -Fq -- "-e \"s|REPLACE_WITH_R2_BUCKET_NAME|\$BACKUP_BUCKET|g\"" "$BACKUP_WORKFLOW"
grep -Fq "REPLACE_WITH_(D1_DATABASE_ID|ALLOWED_ORIGIN|RELEASE_VERSION|R2_BUCKET_NAME)" "$BACKUP_WORKFLOW"
grep -Fq "\`production-backup\` environment" "$ROOT/docs/PRODUCTION_RELEASE_RUNBOOK.md"
if grep -q 'actions/upload-artifact' "$BACKUP_WORKFLOW"; then
  echo "production backups must not be retained as plaintext CI artifacts" >&2
  exit 1
fi

SMOKE_DIR="$(mktemp -d)"
trap 'rm -rf "$SMOKE_DIR"' EXIT
SMOKE_CONFIG="$SMOKE_DIR/wrangler.production.toml"
sed \
  -e 's|REPLACE_WITH_D1_DATABASE_ID|00000000-0000-0000-0000-000000000000|g' \
  -e 's|REPLACE_WITH_ALLOWED_ORIGIN|https://backup-smoke.invalid|g' \
  -e 's|REPLACE_WITH_RELEASE_VERSION|backup-smoke|g' \
  -e 's|REPLACE_WITH_R2_BUCKET_NAME|knock-knock-backup-smoke|g' \
  "$PRODUCTION_EXAMPLE" > "$SMOKE_CONFIG"
if grep -Eq 'REPLACE_WITH_(D1_DATABASE_ID|ALLOWED_ORIGIN|RELEASE_VERSION|R2_BUCKET_NAME)' "$SMOKE_CONFIG"; then
  echo "materialized backup config still contains a required placeholder" >&2
  exit 1
fi
grep -Fqx 'bucket_name = "knock-knock-backup-smoke"' "$SMOKE_CONFIG"
python3 - "$SMOKE_CONFIG" <<'PY'
import pathlib
import sys
import tomllib

with pathlib.Path(sys.argv[1]).open("rb") as config_file:
    tomllib.load(config_file)
PY
if command -v wrangler >/dev/null 2>&1; then
  env \
    -u CLOUDFLARE_API_TOKEN \
    -u CLOUDFLARE_ACCOUNT_ID \
    -u CLOUDFLARE_API_KEY \
    -u CLOUDFLARE_EMAIL \
    CI=1 \
    WRANGLER_SEND_METRICS=false \
    wrangler d1 export knock-knock \
      --local \
      --config "$SMOKE_CONFIG" \
      --output "$SMOKE_DIR/local-export.sql"
  test -s "$SMOKE_DIR/local-export.sql"
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

grep -Fq "STAGING_RELEASE_VERSION: \${{ github.sha }}" "$ROOT/.github/workflows/staging-deploy.yml"
grep -Fq "STAGING_RELEASE_VERSION: \${{ github.sha }}" "$ROOT/.github/workflows/staging-contract-gate.yml"
if grep -Fq 'vars.KNOCK_KNOCK_STAGING_RELEASE_VERSION' "$ROOT/.github/workflows/staging-deploy.yml" \
  || grep -Fq 'vars.KNOCK_KNOCK_STAGING_RELEASE_VERSION' "$ROOT/.github/workflows/staging-contract-gate.yml"; then
  echo "staging release identity must come from github.sha, not a mutable repository variable" >&2
  exit 1
fi

echo "production config smoke passed: local defaults are explicit and production is fail-closed"
