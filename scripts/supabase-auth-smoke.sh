#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" health >/dev/null

BASE_URL="${BASE_URL:-https://knock-knock-backend-production.wch-klaus.workers.dev}"
BASE_URL="${BASE_URL%/}"
: "${SMOKE_EMAIL:?Set SMOKE_EMAIL to the Supabase UAT account email}"
: "${SMOKE_PASSWORD:?Set SMOKE_PASSWORD to the Supabase UAT account password}"

json() {
  curl --fail-with-body --silent --show-error \
    -H 'content-type: application/json' "$@"
}

auth="$(json -X POST "$BASE_URL/v1/auth/login" \
  -d "$(jq -nc --arg email "$SMOKE_EMAIL" --arg password "$SMOKE_PASSWORD" \
    '{email:$email,password:$password}')")"
token="$(jq -r '.token' <<<"$auth")"
refresh="$(jq -r '.refresh_token' <<<"$auth")"
test -n "$token" && test "$token" != "null"
test -n "$refresh" && test "$refresh" != "null"

user_auth=(-H "authorization: Bearer $token")
agents="$(json "${user_auth[@]}" "$BASE_URL/v1/agents")"
test "$(jq -r '(.agents | type)' <<<"$agents")" = "array"

rotated="$(json -X POST "$BASE_URL/v1/auth/refresh" \
  -d "$(jq -nc --arg refresh "$refresh" '{refresh_token:$refresh}')")"
rotated_token="$(jq -r '.token' <<<"$rotated")"
rotated_refresh="$(jq -r '.refresh_token' <<<"$rotated")"
test -n "$rotated_token" && test "$rotated_token" != "null"
test -n "$rotated_refresh" && test "$rotated_refresh" != "null"

rotated_auth=(-H "authorization: Bearer $rotated_token")
json "${rotated_auth[@]}" -X POST "$BASE_URL/v1/auth/logout" \
  -d "$(jq -nc --arg refresh "$rotated_refresh" '{refresh_token:$refresh}')" \
  | jq -e '.ok == true' >/dev/null

printf '%s\n' 'supabase auth smoke passed: login/protected-api/refresh/logout'
