#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" health >/dev/null

BASE_URL="${BASE_URL:-http://127.0.0.1:8787}"
BASE_URL="${BASE_URL%/}"
AUTH_MODE="${SMOKE_AUTH_MODE:-register}"
PASSWORD="${SMOKE_PASSWORD:-password123}"
EMAIL="${SMOKE_EMAIL:-disabled-send-$(date +%s)-$$@local.test}"

case "${AUTH_MODE}" in
  register) ;;
  login)
    : "${SMOKE_EMAIL:?SMOKE_EMAIL is required when SMOKE_AUTH_MODE=login}"
    : "${SMOKE_PASSWORD:?SMOKE_PASSWORD is required when SMOKE_AUTH_MODE=login}"
    EMAIL="${SMOKE_EMAIL}"
    PASSWORD="${SMOKE_PASSWORD}"
    ;;
  *)
    echo "SMOKE_AUTH_MODE must be register or login" >&2
    exit 64
    ;;
esac

json() {
  curl --fail-with-body --silent --show-error \
    -H 'content-type: application/json' "$@"
}

get() {
  curl --fail-with-body --silent --show-error "$@"
}

health="$(get "$BASE_URL/health")"
jq -e '
  (.ok == true) and
  (.action_provider_ready == false) and
  (.action_message_enabled == false)
' <<<"$health" >/dev/null

auth_endpoint="register"
if [[ "${AUTH_MODE}" == "login" ]]; then
  auth_endpoint="login"
fi
auth="$(json -X POST "$BASE_URL/v1/auth/${auth_endpoint}" \
  -d "$(jq -nc --arg email "$EMAIL" --arg password "$PASSWORD" '{email:$email,password:$password}')")"
token="$(jq -r '.token' <<<"$auth")"
test -n "$token" && test "$token" != "null"
user_auth=(-H "authorization: Bearer $token")

command_key="disabled-send-$(date +%s%N)"
command_id="cmd-disabled-send-$(date +%s%N)"
created="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/commands" \
  -d "$(jq -nc --arg id "$command_id" --arg key "$command_key" \
    '{schema_version:1,command_id:$id,intent:"send_message",args:{recipient:"disabled-recipient",body:"should not send"},risk_level:"low",needs_confirmation:false,idempotency_key:$key,confidence:0.99,locale:"en-HK",timezone:"Asia/Hong_Kong"}')")"
test "$(jq -r '.state' <<<"$created")" = "awaiting_confirmation"
confirmation_token="$(jq -r '.confirmation_token' <<<"$created")"
test -n "$confirmation_token" && test "$confirmation_token" != "null"

confirm="$(json "${user_auth[@]}" \
  -X POST "$BASE_URL/v1/phone/commands/$command_id/confirm" \
  -d "$(jq -nc --arg token "$confirmation_token" '{confirmation_token:$token}')")"
jq -e '
  (.state == "failed") and
  (.state != "queued") and
  (.error.code == "action_disabled") and
  (.error.retryable == false) and
  (.presentation.terminal == true)
' <<<"$confirm" >/dev/null

detail="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/commands/$command_id")"
jq -e --arg command_id "$command_id" '
  (.command_id == $command_id) and
  (.state == "failed") and
  (.state != "queued") and
  (.error.code == "action_disabled") and
  (.presentation.terminal == true)
' <<<"$detail" >/dev/null

health_after="$(get "$BASE_URL/health")"
jq -e '
  (.action_provider_ready == false) and
  (.action_message_enabled == false)
' <<<"$health_after" >/dev/null

printf '%s\n' 'disabled send_message smoke passed: confirm and GET are failed/action_disabled; sending stays off'
