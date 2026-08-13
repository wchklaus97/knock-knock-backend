#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" health >/dev/null

BASE_URL="${BASE_URL:-https://knock-knock-backend-production.wch-klaus.workers.dev}"
BASE_URL="${BASE_URL%/}"
: "${SMOKE_EMAIL:?Set SMOKE_EMAIL to the Supabase UAT account email}"
: "${SMOKE_PASSWORD:?Set SMOKE_PASSWORD to the Supabase UAT account password}"

response_file="$(mktemp)"
trap 'rm -f -- "$response_file"' EXIT

json() {
  local attempt curl_status http_status

  for attempt in 1 2 3 4; do
    : >"$response_file"
    if http_status="$(curl --silent --show-error \
      --connect-timeout 10 --max-time 30 \
      --output "$response_file" --write-out '%{http_code}' \
      -H 'content-type: application/json' "$@")"; then
      if [[ "$http_status" =~ ^2[0-9][0-9]$ ]]; then
        cat "$response_file"
        return 0
      fi
      if [[ "$http_status" =~ ^5[0-9][0-9]$ ]] && (( attempt < 4 )); then
        echo "Supabase-backed request returned transient HTTP ${http_status} (attempt ${attempt}/4); retrying" >&2
        sleep $((attempt * 2))
        continue
      fi
      echo "Supabase-backed request failed with non-retryable HTTP ${http_status}" >&2
      return 22
    else
      curl_status=$?
      case "$curl_status" in
        5|6|7|16|18|28|35|52|55|56|92)
          if (( attempt < 4 )); then
            echo "Supabase-backed request had a transient network failure (attempt ${attempt}/4); retrying" >&2
            sleep $((attempt * 2))
            continue
          fi
          echo 'Supabase-backed request exhausted network retries' >&2
          return "$curl_status"
          ;;
        *)
          echo "Supabase-backed request failed with non-retryable curl status ${curl_status}" >&2
          return "$curl_status"
          ;;
      esac
    fi
  done

  return 1
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
