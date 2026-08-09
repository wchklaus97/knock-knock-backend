#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOCAL_CONFIG="$ROOT/wrangler.toml"
LOCAL_EXAMPLE="$ROOT/wrangler.toml.example"
PRODUCTION_EXAMPLE="$ROOT/wrangler.production.toml.example"

for file in "$LOCAL_CONFIG" "$LOCAL_EXAMPLE" "$PRODUCTION_EXAMPLE"; do
  test -f "$file"
done

grep -q '^NODE_ENV = "development"$' "$LOCAL_CONFIG"
grep -q '^PUSH_MODE = "dev"$' "$LOCAL_CONFIG"
grep -q '^NODE_ENV = "development"$' "$LOCAL_EXAMPLE"
grep -q '^NODE_ENV = "production"$' "$PRODUCTION_EXAMPLE"
grep -q '^AUTH_PROVIDER = "supabase"$' "$PRODUCTION_EXAMPLE"
grep -q '^SUPABASE_URL = "REPLACE_WITH_SUPABASE_PROJECT_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^PUSH_MODE = "both"$' "$PRODUCTION_EXAMPLE"
grep -q '^CORS_ORIGIN = "REPLACE_WITH_ALLOWED_ORIGIN"$' "$PRODUCTION_EXAMPLE"
grep -q '^SERVICE_VERSION = "REPLACE_WITH_RELEASE_VERSION"$' "$PRODUCTION_EXAMPLE"

if grep -q '^CORS_ORIGIN = "\*"$' "$PRODUCTION_EXAMPLE"; then
  echo "production config must not allow wildcard CORS" >&2
  exit 1
fi

grep -q 'pub fn runtime_configuration' "$ROOT/src/auth.rs"
grep -q 'runtime_configuration(&env)' "$ROOT/src/lib.rs"
grep -q 'must be configured for production APNs' "$ROOT/src/auth.rs"
grep -q 'PUSH_MODE must be apns or both in production' "$ROOT/src/auth.rs"
grep -q 'SUPABASE_PUBLISHABLE_KEY must be configured' "$ROOT/src/auth.rs"

echo "production config smoke passed: local defaults are explicit and production is fail-closed"
