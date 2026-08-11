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
PROVIDER_PERSIST_TO="${PROVIDER_PERSIST_TO:-}"
PROVIDER_ENV_FILE="${PROVIDER_ENV_FILE:-}"

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
    (.state == "unknown" or .state == "retryable") and
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
user_id="$(jq -r '.user_id' <<<"${auth}")"
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
  create_reminder_at "${command_id}" "${idempotency_key}" "2099-01-01T09:00:00Z"
}

create_reminder_at() {
  local command_id="$1"
  local idempotency_key="$2"
  local due_at="$3"
  json "${user_auth[@]}" -X POST "${BASE_URL}/v1/phone/commands" \
    -d "$(jq -nc --arg id "${command_id}" --arg idem "${idempotency_key}" --arg due_at "${due_at}" \
      '{schema_version:1,command_id:$id,intent:"create_reminder",args:{title:"Provider lifecycle smoke",due_at:$due_at},risk_level:"low",needs_confirmation:false,idempotency_key:$idem,confidence:0.99,locale:"en-US",timezone:"UTC"}')"
}

create_reminder_at_in_session() {
  local command_id="$1"
  local idempotency_key="$2"
  local due_at="$3"
  local session_id="$4"
  json "${user_auth[@]}" -X POST "${BASE_URL}/v1/phone/commands" \
    -d "$(jq -nc --arg id "${command_id}" --arg idem "${idempotency_key}" \
      --arg due_at "${due_at}" --arg session_id "${session_id}" \
      '{schema_version:1,command_id:$id,intent:"create_reminder",args:{title:"Provider lifecycle smoke",due_at:$due_at},risk_level:"low",needs_confirmation:false,idempotency_key:$idem,confidence:0.99,locale:"en-US",timezone:"UTC",session_id:$session_id}')"
}

future_rfc3339() {
  local seconds="$1"
  python3 - "${seconds}" <<'PY'
from datetime import datetime, timedelta, timezone
import sys

value = datetime.now(timezone.utc) + timedelta(seconds=int(sys.argv[1]))
print(value.isoformat(timespec="milliseconds").replace("+00:00", "Z"))
PY
}

d1_execute_fixture() {
  local sql="$1"
  test -n "${PROVIDER_PERSIST_TO}"
  test -n "${PROVIDER_ENV_FILE}"
  wrangler d1 execute DB --local \
    --persist-to "${PROVIDER_PERSIST_TO}" \
    --config "${ROOT_DIR}/wrangler.toml" \
    --env-file "${PROVIDER_ENV_FILE}" \
    --command "${sql}" >/dev/null
}

expire_command_ttl_fixture() {
  local command_id="$1"
  [[ "${command_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  d1_execute_fixture \
    "UPDATE commands SET expires_at = '2000-01-01T00:00:00.000Z' WHERE id = '${command_id}'"
}

requeue_succeeded_command_fixture() {
  local command_id="$1"
  [[ "${command_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  d1_execute_fixture \
    "UPDATE commands SET state = 'retryable', expires_at = '2000-01-01T00:00:00.000Z', version = version + 1 WHERE id = '${command_id}' AND state = 'succeeded'; UPDATE outbox_events SET state = 'retrying', next_attempt_at = NULL, last_error = 'ttl_recovery_fixture', lease_token = NULL, lease_expires_at = NULL WHERE aggregate_id = '${command_id}' AND state = 'succeeded'"
}

create_session_fixture() {
  local session_id="$1"
  local agent_id="agt-${session_id}"
  local skill_id="skill-${session_id}"
  [[ "${session_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  [[ "${user_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  d1_execute_fixture \
    "INSERT INTO agents (id, user_id, label, api_key_hash, created_at) VALUES ('${agent_id}', '${user_id}', 'Provider lifecycle agent', 'hash-${agent_id}', strftime('%Y-%m-%dT%H:%M:%fZ','now')); INSERT INTO skills (skill_id, template, facts_schema_json, actions_json, ttl_json, created_at) VALUES ('${skill_id}', 'Provider lifecycle skill', '{}', '[]', '{}', strftime('%Y-%m-%dT%H:%M:%fZ','now')); INSERT INTO sessions (id, agent_id, user_id, skill_id, state, facts_json, created_at, updated_at) VALUES ('${session_id}', '${agent_id}', '${user_id}', '${skill_id}', 'active', '{}', strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))"
}

delete_session_fixture() {
  local session_id="$1"
  [[ "${session_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  d1_execute_fixture \
    "UPDATE sessions SET deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ','now'), updated_at = strftime('%Y-%m-%dT%H:%M:%fZ','now') WHERE id = '${session_id}' AND user_id = '${user_id}'"
}

insert_zero_attempt_permit_fixture() {
  local command_id="$1"
  [[ "${command_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  d1_execute_fixture \
    "INSERT INTO action_attempts (id, user_id, command_id, action_id, provider, provider_idempotency_key, state, request_hash, response_json, attempts, next_attempt_at, last_error, created_at, updated_at) SELECT 'attempt-${command_id}', user_id, id, NULL, 'action.reminder', 'permit-${command_id}', 'running', command_hash, NULL, 0, NULL, 'execution_permit', strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now') FROM commands WHERE id = '${command_id}' AND user_id = '${user_id}'"
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

if [[ -n "${PROVIDER_PERSIST_TO}" && -n "${PROVIDER_ENV_FILE}" ]]; then
  ttl_success_id="cmd-ttl-success-reuse-$(date +%s%N)"
  ttl_success_key="idem-ttl-success-reuse-$(date +%s%N)"
  ttl_success="$(create_reminder "${ttl_success_id}" "${ttl_success_key}")"
  test "$(jq -r '.state' <<<"${ttl_success}")" = "queued"
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  ttl_success_first="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${ttl_success_id}")"
  test "$(jq -r '.state' <<<"${ttl_success_first}")" = "succeeded"
  if [[ -n "${PROVIDER_LOG}" ]]; then
    ttl_success_delivery_before="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
    ttl_success_status_before="$(count_provider_requests '/reminders/status' "${PROVIDER_LOG}")"
  fi
  requeue_succeeded_command_fixture "${ttl_success_id}"
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  ttl_success_final="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${ttl_success_id}")"
  test "$(jq -r '.state' <<<"${ttl_success_final}")" = "succeeded"
  test "$(jq -r '.result.provider_id' <<<"${ttl_success_final}")" = \
    "$(jq -r '.result.provider_id' <<<"${ttl_success_first}")"
  if [[ -n "${PROVIDER_LOG}" ]]; then
    ttl_success_delivery_after="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
    ttl_success_status_after="$(count_provider_requests '/reminders/status' "${PROVIDER_LOG}")"
    test "${ttl_success_delivery_after}" = "${ttl_success_delivery_before}"
    test "${ttl_success_status_after}" = "${ttl_success_status_before}"
  fi

  ttl_fresh_id="cmd-ttl-fresh-expire-$(date +%s%N)"
  ttl_fresh_key="idem-ttl-fresh-expire-$(date +%s%N)"
  if [[ -n "${PROVIDER_LOG}" ]]; then
    ttl_fresh_delivery_before="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
  fi
  ttl_fresh="$(create_reminder "${ttl_fresh_id}" "${ttl_fresh_key}")"
  test "$(jq -r '.state' <<<"${ttl_fresh}")" = "queued"
  expire_command_ttl_fixture "${ttl_fresh_id}"
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  ttl_fresh_final="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${ttl_fresh_id}")"
  test "$(jq -r '.state' <<<"${ttl_fresh_final}")" = "expired"
  if [[ -n "${PROVIDER_LOG}" ]]; then
    ttl_fresh_delivery_after="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
    test "${ttl_fresh_delivery_after}" = "${ttl_fresh_delivery_before}"
  fi

  # Once attempts>=1 records that an effect may have started, deleting the
  # parent session must not convert the command to cancelled. The next claim
  # reconciles the provider resource and truthfully settles succeeded.
  deleted_reconcile_session="ses-deleted-reconcile-$(date +%s%N)"
  deleted_reconcile_id="cmd-status-reconcile-deleted-session-$(date +%s%N)"
  deleted_reconcile_key="idem-deleted-reconcile-$(date +%s%N)"
  create_session_fixture "${deleted_reconcile_session}"
  if [[ -n "${PROVIDER_LOG}" ]]; then
    deleted_reconcile_delivery_before="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
    deleted_reconcile_status_before="$(count_provider_requests '/reminders/status' "${PROVIDER_LOG}")"
  fi
  deleted_reconcile="$(create_reminder_at_in_session \
    "${deleted_reconcile_id}" "${deleted_reconcile_key}" \
    "2099-01-01T09:00:00Z" "${deleted_reconcile_session}")"
  test "$(jq -r '.state' <<<"${deleted_reconcile}")" = "queued"
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  deleted_reconcile_first="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${deleted_reconcile_id}")"
  assert_structured_command_error "${deleted_reconcile_first}"
  delete_session_fixture "${deleted_reconcile_session}"
  sleep "${WAIT_SECONDS}"
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  deleted_reconcile_final="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${deleted_reconcile_id}")"
  test "$(jq -r '.state' <<<"${deleted_reconcile_final}")" = "succeeded"
  test "$(jq -r '.result.provider_id' <<<"${deleted_reconcile_final}")" != "null"
  if [[ -n "${PROVIDER_LOG}" ]]; then
    deleted_reconcile_delivery_after="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
    deleted_reconcile_status_after="$(count_provider_requests '/reminders/status' "${PROVIDER_LOG}")"
    test "${deleted_reconcile_delivery_after}" = "$((deleted_reconcile_delivery_before + 1))"
    test "${deleted_reconcile_status_after}" = "$((deleted_reconcile_status_before + 1))"
  fi

  # An attempts=0 row is only a permit: no effect began. Session deletion may
  # safely cancel it, and the provider must not receive a delivery.
  deleted_permit_session="ses-deleted-permit-$(date +%s%N)"
  deleted_permit_id="cmd-deleted-permit-$(date +%s%N)"
  deleted_permit_key="idem-deleted-permit-$(date +%s%N)"
  create_session_fixture "${deleted_permit_session}"
  deleted_permit="$(create_reminder_at_in_session \
    "${deleted_permit_id}" "${deleted_permit_key}" \
    "2099-01-01T09:00:00Z" "${deleted_permit_session}")"
  test "$(jq -r '.state' <<<"${deleted_permit}")" = "queued"
  insert_zero_attempt_permit_fixture "${deleted_permit_id}"
  delete_session_fixture "${deleted_permit_session}"
  if [[ -n "${PROVIDER_LOG}" ]]; then
    deleted_permit_delivery_before="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
  fi
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  deleted_permit_final="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${deleted_permit_id}")"
  test "$(jq -r '.state' <<<"${deleted_permit_final}")" = "cancelled"
  if [[ -n "${PROVIDER_LOG}" ]]; then
    deleted_permit_delivery_after="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
    test "${deleted_permit_delivery_after}" = "${deleted_permit_delivery_before}"
  fi
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

# The first delivery reaches the provider but intentionally loses its response.
# By the time status reconciliation reports success, due_at has elapsed. The
# Worker must materialize that known provider success, not send again or mark
# the command failed because the original deadline is now in the past.
elapsed_reconcile_id="cmd-status-reconcile-expired-due-$(date +%s%N)"
elapsed_reconcile_key="idem-status-reconcile-expired-due-$(date +%s%N)"
elapsed_reconcile_due_at="$(future_rfc3339 8)"
if [[ -n "${PROVIDER_LOG}" ]]; then
  elapsed_delivery_before="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
  elapsed_status_before="$(count_provider_requests '/reminders/status' "${PROVIDER_LOG}")"
fi
elapsed_reconcile="$(create_reminder_at \
  "${elapsed_reconcile_id}" "${elapsed_reconcile_key}" "${elapsed_reconcile_due_at}")"
test "$(jq -r '.state' <<<"${elapsed_reconcile}")" = "queued"
curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
elapsed_first="$(get_json "${user_auth[@]}" \
  "${BASE_URL}/v1/phone/commands/${elapsed_reconcile_id}")"
assert_structured_command_error "${elapsed_first}"
elapsed_cancel_response="$(curl --silent --show-error --max-time "${HTTP_TIMEOUT_SECONDS:-10}" \
  -H 'content-type: application/json' "${user_auth[@]}" \
  -X POST "${BASE_URL}/v1/phone/commands/${elapsed_reconcile_id}/cancel" \
  -w $'\n%{http_code}')" || {
    echo "provider lifecycle failed: recovery cancellation check had a transport failure" >&2
    exit 1
  }
elapsed_cancel_status="${elapsed_cancel_response##*$'\n'}"
elapsed_cancel_body="${elapsed_cancel_response%$'\n'*}"
test "${elapsed_cancel_status}" = "409"
assert_error_response "${elapsed_cancel_body}" command_effect_in_progress
if [[ -n "${PROVIDER_PERSIST_TO}" && -n "${PROVIDER_ENV_FILE}" ]]; then
  expire_command_ttl_fixture "${elapsed_reconcile_id}"
fi
sleep 10
curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
elapsed_final="$(get_json "${user_auth[@]}" \
  "${BASE_URL}/v1/phone/commands/${elapsed_reconcile_id}")"
test "$(jq -r '.state' <<<"${elapsed_final}")" = "succeeded"
test "$(jq -r '.result.provider_id' <<<"${elapsed_final}")" != "null"
if [[ -n "${PROVIDER_LOG}" ]]; then
  elapsed_delivery_after="$(count_provider_requests '/reminders/deliver' "${PROVIDER_LOG}")"
  elapsed_status_after="$(count_provider_requests '/reminders/status' "${PROVIDER_LOG}")"
  test "${elapsed_delivery_after}" = "$((elapsed_delivery_before + 1))"
  test "${elapsed_status_after}" = "$((elapsed_status_before + 1))"
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
jq -e '(.state == "unknown" or .state == "retryable")' <<<"${message_first}" >/dev/null
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
jq -e '(.state == "unknown" or .state == "retryable")' <<<"${message_missing_final}" >/dev/null
  test "$(jq -r '.error.code' <<<"${message_missing_final}")" = "provider_missing_id"
else
  echo "provider lifecycle smoke: strict provider-resource identity checks are disabled for this backend base"
fi

if [[ "${PROVIDER_STRICT_RESOURCE_IDENTITY}" == "true" ]]; then
  printf '%s\n' 'provider lifecycle smoke passed: provider IDs, duplicate idempotency, cancellation safety, deleted-session reconciliation, zero-attempt cancellation, elapsed-deadline and expired-TTL recovery, asynchronous delivery, structured failures, and strict resource-identity fail-closed behavior'
else
  printf '%s\n' 'provider lifecycle smoke passed: provider IDs, duplicate idempotency, cancellation safety, deleted-session reconciliation, zero-attempt cancellation, elapsed-deadline and expired-TTL recovery, asynchronous delivery, and structured failures (strict resource-identity checks opt-in)'
fi
