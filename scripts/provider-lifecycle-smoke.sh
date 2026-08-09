#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${BASE_URL:-http://127.0.0.1:8787}"
BASE_URL="${BASE_URL%/}"
PASSWORD="${SMOKE_PASSWORD:-password123}"
EMAIL="${SMOKE_EMAIL:-provider-lifecycle-$(date +%s)-$$@local.test}"
WAIT_SECONDS="${PROVIDER_RECONCILE_WAIT_SECONDS:-6}"

json() {
  curl --fail-with-body --silent --show-error \
    -H 'content-type: application/json' "$@"
}

auth="$(json -X POST "${BASE_URL}/v1/auth/register" \
  -d "$(jq -nc --arg email "${EMAIL}" --arg password "${PASSWORD}" \
    '{email:$email,password:$password}')")"
token="$(jq -r '.token' <<<"${auth}")"
user_auth=(-H "authorization: Bearer ${token}")
health="$(curl --fail-with-body --silent --show-error "${BASE_URL}/health")"
test "$(jq -r '.action_provider_mode' <<<"${health}")" = "external"
test "$(jq -r '.action_provider_ready' <<<"${health}")" = "true"

create_reminder() {
  local command_id="$1"
  local idempotency_key="$2"
  json "${user_auth[@]}" -X POST "${BASE_URL}/v1/phone/commands" \
    -d "$(jq -nc --arg id "${command_id}" --arg idem "${idempotency_key}" \
      '{schema_version:1,command_id:$id,intent:"create_reminder",args:{title:"Provider lifecycle smoke",due_at:"2099-01-01T09:00:00Z"},risk_level:"low",needs_confirmation:false,idempotency_key:$idem,confidence:0.99,locale:"en-US",timezone:"UTC"}')"
}

success_id="cmd-provider-success-$(date +%s%N)"
success_key="idem-provider-success-$(date +%s%N)"
success="$(create_reminder "${success_id}" "${success_key}")"
test "$(jq -r '.state' <<<"${success}")" = "queued"
curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
success_result="$(curl --fail-with-body --silent --show-error "${user_auth[@]}" \
  "${BASE_URL}/v1/phone/commands/${success_id}")"
test "$(jq -r '.state' <<<"${success_result}")" = "succeeded"
test "$(jq -r '.result.provider' <<<"${success_result}")" = "external.reminder"
undo="$(json "${user_auth[@]}" -X POST \
  "${BASE_URL}/v1/phone/commands/${success_id}/undo")"
test "$(jq -r '.undo_result.status' <<<"${undo}")" = "cancelled"
test "$(jq -r '.undo_result.provider' <<<"${undo}")" = "external.reminder"

reconcile_id="cmd-status-reconcile-$(date +%s%N)"
reconcile_key="idem-status-reconcile-$(date +%s%N)"
reconcile="$(create_reminder "${reconcile_id}" "${reconcile_key}")"
test "$(jq -r '.state' <<<"${reconcile}")" = "queued"
curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
first="$(curl --fail-with-body --silent --show-error "${user_auth[@]}" \
  "${BASE_URL}/v1/phone/commands/${reconcile_id}")"
test "$(jq -r '.state' <<<"${first}")" = "unknown"
sleep "${WAIT_SECONDS}"
curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
final="$(curl --fail-with-body --silent --show-error "${user_auth[@]}" \
  "${BASE_URL}/v1/phone/commands/${reconcile_id}")"
test "$(jq -r '.state' <<<"${final}")" = "succeeded"
test "$(jq -r '.result.provider_id' <<<"${final}")" != "null"

printf '%s\n' 'provider lifecycle smoke passed: delivery, provider cancellation, timeout, status reconciliation, and idempotent completion'
