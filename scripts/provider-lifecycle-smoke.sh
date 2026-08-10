#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" health >/dev/null

BASE_URL="${BASE_URL:-http://127.0.0.1:8787}"
BASE_URL="${BASE_URL%/}"
PASSWORD="${SMOKE_PASSWORD:-password123}"
EMAIL="${SMOKE_EMAIL:-provider-lifecycle-$(date +%s)-$$@local.test}"
WAIT_SECONDS="${PROVIDER_RECONCILE_WAIT_SECONDS:-6}"
PROVIDER_LOG="${PROVIDER_LOG:-}"
PROVIDER_STRICT_RESOURCE_IDENTITY="${PROVIDER_STRICT_RESOURCE_IDENTITY:-false}"

http_json() {
  local body_file error_file
  body_file="$(mktemp)"
  error_file="$(mktemp)"
  if ! curl --fail-with-body --silent --show-error --max-time "${HTTP_TIMEOUT_SECONDS:-10}" \
    -H 'content-type: application/json' "$@" >"${body_file}" 2>"${error_file}"; then
    echo "provider lifecycle request failed: nonzero HTTP or transport response" >&2
    "${ROOT_DIR}/scripts/ci-log-sanitize.sh" "${error_file}" "${body_file}" >&2 || true
    rm -f "${body_file}" "${error_file}"
    return 1
  fi
  if ! jq -e 'type == "object"' "${body_file}" >/dev/null; then
    echo "provider lifecycle request failed: response was not a JSON object" >&2
    rm -f "${body_file}" "${error_file}"
    return 1
  fi
  sed -n '1,$p' "${body_file}"
  rm -f "${body_file}" "${error_file}"
}

json() {
  local response
  response="$(http_json "$@")" || return 1
  if ! jq -e 'type == "object" and (.error? == null)' <<<"${response}" >/dev/null; then
    echo "provider lifecycle request failed: successful response contained an error object" >&2
    return 1
  fi
  printf '%s\n' "${response}"
}

get_json() {
  http_json "$@"
}

count_provider_requests() {
  local path="$1"
  local log_file="$2"
  test -f "${log_file}"
  grep -Ec "\\[provider-mock\\] POST ${path}" "${log_file}" || true
}

assert_error_response() {
  local body="$1"
  local expected_code="$2"
  jq -e --arg expected_code "${expected_code}" '
    (.error.code == $expected_code) and
    (.error.message | type == "string" and length > 0) and
    (.error.retryable | type == "boolean") and
    (.error.request_id | type == "string" and length > 0)
  ' <<<"${body}" >/dev/null
}

assert_structured_command_error() {
  local body="$1"
  jq -e '
    (.state == "unknown") and
    (.error.code | type == "string" and length > 0) and
    (.error.message | type == "string" and length > 0)
  ' <<<"${body}" >/dev/null
}

auth="$(json -X POST "${BASE_URL}/v1/auth/register" \
  -d "$(jq -nc --arg email "${EMAIL}" --arg password "${PASSWORD}" \
    '{email:$email,password:$password}')")"
jq -e '(.token | type == "string" and length > 0) and (.user_id | type == "string" and length > 0)' \
  <<<"${auth}" >/dev/null
token="$(jq -r '.token' <<<"${auth}")"
user_auth=(-H "authorization: Bearer ${token}")
health="$(get_json "${BASE_URL}/health")"
jq -e '
  (.ok == true) and
  (.api == "rust") and
  (.runtime == "cloudflare-worker") and
  (.action_provider_mode == "external") and
  (.action_provider_ready == true)
' <<<"${health}" >/dev/null

create_reminder() {
  local command_id="$1"
  local idempotency_key="$2"
  json "${user_auth[@]}" -X POST "${BASE_URL}/v1/phone/commands" \
    -d "$(jq -nc --arg id "${command_id}" --arg idem "${idempotency_key}" \
      '{schema_version:1,command_id:$id,intent:"create_reminder",args:{title:"Provider lifecycle smoke",due_at:"2099-01-01T09:00:00Z"},risk_level:"low",needs_confirmation:false,idempotency_key:$idem,confidence:0.99,locale:"en-US",timezone:"UTC"}')"
}

create_message() {
  local command_id="$1"
  local idempotency_key="$2"
  json "${user_auth[@]}" -X POST "${BASE_URL}/v1/phone/commands" \
    -d "$(jq -nc --arg id "${command_id}" --arg idem "${idempotency_key}" \
      '{schema_version:1,command_id:$id,intent:"send_message",args:{recipient:"+85255550123",body:"Provider lifecycle message smoke"},risk_level:"high",needs_confirmation:true,idempotency_key:$idem,confidence:0.99,locale:"en-US",timezone:"UTC"}')"
}

success_id="cmd-provider-success-$(date +%s%N)"
success_key="idem-provider-success-$(date +%s%N)"
success="$(create_reminder "${success_id}" "${success_key}")"
test "$(jq -r '.state' <<<"${success}")" = "queued"
curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
success_result="$(get_json "${user_auth[@]}" "${BASE_URL}/v1/phone/commands/${success_id}")"
test "$(jq -r '.state' <<<"${success_result}")" = "succeeded"
test "$(jq -r '.result.provider' <<<"${success_result}")" = "external.reminder"
test "$(jq -r '.result.provider_id' <<<"${success_result}")" != "null"
if [[ -n "${PROVIDER_LOG}" ]]; then
  success_delivery_count="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
  test "${success_delivery_count}" = "1"
fi
undo="$(json "${user_auth[@]}" -X POST \
  "${BASE_URL}/v1/phone/commands/${success_id}/undo")"
test "$(jq -r '.undo_result.status' <<<"${undo}")" = "cancelled"
test "$(jq -r '.undo_result.provider' <<<"${undo}")" = "external.reminder"

duplicate_success="$(create_reminder "${success_id}" "${success_key}")"
test "$(jq -r '.state' <<<"${duplicate_success}")" = "succeeded"
test "$(jq -r '.result.provider_id' <<<"${duplicate_success}")" = "$(jq -r '.result.provider_id' <<<"${success_result}")"
if [[ -n "${PROVIDER_LOG}" ]]; then
  duplicate_success_delivery_count="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
  test "${duplicate_success_delivery_count}" = "1"
fi

cancel_reconcile_id="cmd-cancel-reconcile-$(date +%s%N)"
cancel_reconcile_key="idem-cancel-reconcile-$(date +%s%N)"
cancel_reconcile="$(create_reminder "${cancel_reconcile_id}" "${cancel_reconcile_key}")"
test "$(jq -r '.state' <<<"${cancel_reconcile}")" = "queued"
curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
cancel_response="$(curl --silent --show-error --max-time "${HTTP_TIMEOUT_SECONDS:-10}" \
  -H 'content-type: application/json' "${user_auth[@]}" \
  -X POST "${BASE_URL}/v1/phone/commands/${cancel_reconcile_id}/undo" \
  -w $'\n%{http_code}')" || {
    echo "provider lifecycle failed: cancellation request had a transport failure" >&2
    exit 1
  }
cancel_status="${cancel_response##*$'\n'}"
cancel_body="${cancel_response%$'\n'*}"
test "${cancel_status}" = "503"
assert_error_response "${cancel_body}" provider_cancel_pending
curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
cancel_final="$(get_json "${user_auth[@]}" "${BASE_URL}/v1/phone/commands/${cancel_reconcile_id}")"
test "$(jq -r '.result.undo.status' <<<"${cancel_final}")" = "cancelled"

reconcile_id="cmd-status-reconcile-$(date +%s%N)"
reconcile_key="idem-status-reconcile-$(date +%s%N)"
if [[ -n "${PROVIDER_LOG}" ]]; then
  reconcile_delivery_before="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
fi
reconcile="$(create_reminder "${reconcile_id}" "${reconcile_key}")"
test "$(jq -r '.state' <<<"${reconcile}")" = "queued"
curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
first="$(get_json "${user_auth[@]}" "${BASE_URL}/v1/phone/commands/${reconcile_id}")"
assert_structured_command_error "${first}"
sleep "${WAIT_SECONDS}"
curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
final="$(get_json "${user_auth[@]}" "${BASE_URL}/v1/phone/commands/${reconcile_id}")"
test "$(jq -r '.state' <<<"${final}")" = "succeeded"
test "$(jq -r '.result.provider_id' <<<"${final}")" != "null"
if [[ -n "${PROVIDER_LOG}" ]]; then
  reconcile_delivery_after="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
  test "${reconcile_delivery_after}" = "$((reconcile_delivery_before + 1))"
fi

message_id="cmd-message-reconcile-$(date +%s%N)"
message_key="idem-message-reconcile-$(date +%s%N)"
message="$(create_message "${message_id}" "${message_key}")"
test "$(jq -r '.state' <<<"${message}")" = "awaiting_confirmation"
confirmation_token="$(jq -r '.confirmation_token' <<<"${message}")"
test -n "${confirmation_token}" && test "${confirmation_token}" != "null"
json "${user_auth[@]}" -X POST \
  "${BASE_URL}/v1/phone/commands/${message_id}/confirm" \
  -d "$(jq -nc --arg token "${confirmation_token}" '{confirmation_token:$token}')" >/dev/null
curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
message_first="$(get_json "${user_auth[@]}" "${BASE_URL}/v1/phone/commands/${message_id}")"
test "$(jq -r '.state' <<<"${message_first}")" = "unknown"
test "$(jq -r '.error.code' <<<"${message_first}")" = "provider_pending"
sleep "${WAIT_SECONDS}"
curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
message_final="$(get_json "${user_auth[@]}" "${BASE_URL}/v1/phone/commands/${message_id}")"
test "$(jq -r '.state' <<<"${message_final}")" = "succeeded"
test "$(jq -r '.result.delivery_state' <<<"${message_final}")" = "sent"
test "$(jq -r '.result.external_delivery' <<<"${message_final}")" = "sent"
test "$(jq -r '.result.provider_id' <<<"${message_final}")" != "null"
if [[ -n "${PROVIDER_LOG}" ]]; then
  message_delivery_count="$(count_provider_requests '/messages/deliver' "${PROVIDER_LOG}")"
  test "${message_delivery_count}" = "1"
fi

duplicate_message="$(create_message "${message_id}" "${message_key}")"
test "$(jq -r '.state' <<<"${duplicate_message}")" = "succeeded"
test "$(jq -r '.result.provider_id' <<<"${duplicate_message}")" = "$(jq -r '.result.provider_id' <<<"${message_final}")"
if [[ -n "${PROVIDER_LOG}" ]]; then
  duplicate_message_delivery_count="$(count_provider_requests '/messages/deliver' "${PROVIDER_LOG}")"
  test "${duplicate_message_delivery_count}" = "1"
fi

if [[ "${PROVIDER_STRICT_RESOURCE_IDENTITY}" == "true" ]]; then
  cancel_missing_id="cmd-cancel-missing-id-$(date +%s%N)"
  cancel_missing_key="idem-cancel-missing-id-$(date +%s%N)"
  cancel_missing="$(create_reminder "${cancel_missing_id}" "${cancel_missing_key}")"
  test "$(jq -r '.state' <<<"${cancel_missing}")" = "queued"
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  cancel_missing_response="$(curl --silent --show-error --max-time "${HTTP_TIMEOUT_SECONDS:-10}" \
    -H 'content-type: application/json' "${user_auth[@]}" \
    -X POST "${BASE_URL}/v1/phone/commands/${cancel_missing_id}/undo" \
    -w $'\n%{http_code}')" || {
      echo "provider lifecycle failed: missing-ID cancellation request had a transport failure" >&2
      exit 1
    }
  cancel_missing_status="${cancel_missing_response##*$'\n'}"
  cancel_missing_body="${cancel_missing_response%$'\n'*}"
  test "${cancel_missing_status}" = "503"
  assert_error_response "${cancel_missing_body}" provider_cancel_mismatch

  cancel_mismatch_id="cmd-cancel-mismatch-$(date +%s%N)"
  cancel_mismatch_key="idem-cancel-mismatch-$(date +%s%N)"
  cancel_mismatch="$(create_reminder "${cancel_mismatch_id}" "${cancel_mismatch_key}")"
  test "$(jq -r '.state' <<<"${cancel_mismatch}")" = "queued"
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  cancel_mismatch_response="$(curl --silent --show-error --max-time "${HTTP_TIMEOUT_SECONDS:-10}" \
    -H 'content-type: application/json' "${user_auth[@]}" \
    -X POST "${BASE_URL}/v1/phone/commands/${cancel_mismatch_id}/undo" \
    -w $'\n%{http_code}')" || {
      echo "provider lifecycle failed: mismatched cancellation request had a transport failure" >&2
      exit 1
    }
  cancel_mismatch_status="${cancel_mismatch_response##*$'\n'}"
  cancel_mismatch_body="${cancel_mismatch_response%$'\n'*}"
  test "${cancel_mismatch_status}" = "503"
  assert_error_response "${cancel_mismatch_body}" provider_cancel_mismatch

  message_missing_id="cmd-message-missing-id-$(date +%s%N)"
  message_missing_key="idem-message-missing-id-$(date +%s%N)"
  message_missing="$(create_message "${message_missing_id}" "${message_missing_key}")"
  test "$(jq -r '.state' <<<"${message_missing}")" = "awaiting_confirmation"
  message_missing_token="$(jq -r '.confirmation_token' <<<"${message_missing}")"
  json "${user_auth[@]}" -X POST \
    "${BASE_URL}/v1/phone/commands/${message_missing_id}/confirm" \
    -d "$(jq -nc --arg token "${message_missing_token}" '{confirmation_token:$token}')" >/dev/null
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  message_missing_final="$(get_json "${user_auth[@]}" "${BASE_URL}/v1/phone/commands/${message_missing_id}")"
  test "$(jq -r '.state' <<<"${message_missing_final}")" = "unknown"
  test "$(jq -r '.error.code' <<<"${message_missing_final}")" = "provider_missing_id"
else
  echo "provider lifecycle smoke: strict provider-resource identity checks are disabled for this backend base"
fi

if [[ "${PROVIDER_STRICT_RESOURCE_IDENTITY}" == "true" ]]; then
  printf '%s\n' 'provider lifecycle smoke passed: provider IDs, duplicate reminder/message idempotency, cancellation safety, scheduled cancellation recovery, reminder status reconciliation, asynchronous message delivery, structured failures, and strict resource-identity fail-closed behavior'
else
  printf '%s\n' 'provider lifecycle smoke passed: provider IDs, duplicate reminder/message idempotency, cancellation safety, scheduled cancellation recovery, reminder status reconciliation, asynchronous message delivery, and structured failures (strict resource-identity checks opt-in)'
fi
