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
PROVIDER_PERSIST_TO="${PROVIDER_PERSIST_TO:-}"
PROVIDER_ENV_FILE="${PROVIDER_ENV_FILE:-}"
PROVIDER_STRICT_RESOURCE_IDENTITY="${PROVIDER_STRICT_RESOURCE_IDENTITY:-auto}"
if [[ "${PROVIDER_STRICT_RESOURCE_IDENTITY}" == "auto" ]]; then
  if [[ -n "${PROVIDER_LOG}" && -n "${PROVIDER_PERSIST_TO}" && -n "${PROVIDER_ENV_FILE}" ]]; then
    PROVIDER_STRICT_RESOURCE_IDENTITY=true
  else
    PROVIDER_STRICT_RESOURCE_IDENTITY=false
  fi
fi

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

assert_atomic_message_identity_sqlite_fence() {
  local sqlite_db sqlite_output expected
  if ! grep -Fq \
    'AND (? IS NULL OR provider_message_id IS NULL OR provider_message_id = ?)' \
    "${ROOT_DIR}/src/action_effects.rs"; then
    echo "provider lifecycle failed: message identity update is missing its compare-and-set fence" >&2
    return 1
  fi

  sqlite_db="$(mktemp "${TMPDIR:-/tmp}/knock-knock-message-identity.XXXXXX")"
  if ! sqlite_output="$(sqlite3 -batch -noheader "${sqlite_db}" <<'SQL'
CREATE TABLE outbound_messages (
  user_id TEXT NOT NULL,
  command_id TEXT NOT NULL,
  delivery_state TEXT NOT NULL,
  provider_message_id TEXT,
  updated_at TEXT NOT NULL,
  UNIQUE (user_id, command_id)
);
INSERT INTO outbound_messages
  (user_id, command_id, delivery_state, provider_message_id, updated_at)
VALUES ('user-1', 'failed-status', 'queued', 'provider-a', 'before');
UPDATE outbound_messages
SET delivery_state = 'failed',
    provider_message_id = COALESCE(provider_message_id, 'provider-b'),
    updated_at = 'mismatch'
WHERE user_id = 'user-1'
  AND command_id = 'failed-status'
  AND ('provider-b' IS NULL OR provider_message_id IS NULL OR provider_message_id = 'provider-b');
SELECT changes(), delivery_state, provider_message_id
FROM outbound_messages WHERE command_id = 'failed-status';
UPDATE outbound_messages
SET delivery_state = 'failed',
    provider_message_id = COALESCE(provider_message_id, 'provider-a'),
    updated_at = 'matching'
WHERE user_id = 'user-1'
  AND command_id = 'failed-status'
  AND ('provider-a' IS NULL OR provider_message_id IS NULL OR provider_message_id = 'provider-a');
SELECT changes(), delivery_state, provider_message_id
FROM outbound_messages WHERE command_id = 'failed-status';
INSERT INTO outbound_messages
  (user_id, command_id, delivery_state, provider_message_id, updated_at)
VALUES ('user-1', 'overlap', 'queued', NULL, 'before');
UPDATE outbound_messages
SET delivery_state = 'sent',
    provider_message_id = COALESCE(provider_message_id, 'provider-a'),
    updated_at = 'winner'
WHERE user_id = 'user-1'
  AND command_id = 'overlap'
  AND ('provider-a' IS NULL OR provider_message_id IS NULL OR provider_message_id = 'provider-a');
SELECT changes(), delivery_state, provider_message_id
FROM outbound_messages WHERE command_id = 'overlap';
UPDATE outbound_messages
SET delivery_state = 'failed',
    provider_message_id = COALESCE(provider_message_id, 'provider-b'),
    updated_at = 'loser'
WHERE user_id = 'user-1'
  AND command_id = 'overlap'
  AND ('provider-b' IS NULL OR provider_message_id IS NULL OR provider_message_id = 'provider-b');
SELECT changes(), delivery_state, provider_message_id
FROM outbound_messages WHERE command_id = 'overlap';
SQL
)"; then
    rm -f "${sqlite_db}"
    echo "provider lifecycle failed: message identity SQLite fence did not execute" >&2
    return 1
  fi
  rm -f "${sqlite_db}"

  expected=$'0|queued|provider-a\n1|failed|provider-a\n1|sent|provider-a\n0|sent|provider-a'
  if [[ "${sqlite_output}" != "${expected}" ]]; then
    echo "provider lifecycle failed: message identity SQLite fence changed canonical state" >&2
    return 1
  fi
}

assert_atomic_message_identity_sqlite_fence

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

d1_query_fixture() {
  local sql="$1"
  test -n "${PROVIDER_PERSIST_TO}"
  test -n "${PROVIDER_ENV_FILE}"
  wrangler d1 execute DB --local \
    --persist-to "${PROVIDER_PERSIST_TO}" \
    --config "${ROOT_DIR}/wrangler.toml" \
    --env-file "${PROVIDER_ENV_FILE}" \
    --command "${sql}" \
    --json
}

replace_message_provider_id_fixture() {
  local command_id="$1"
  local provider_message_id="$2"
  [[ "${command_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  [[ "${provider_message_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  d1_execute_fixture \
    "UPDATE outbound_messages SET provider_message_id = '${provider_message_id}' WHERE user_id = '${user_id}' AND command_id = '${command_id}'"
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
  local fixture_user_id="${2:-${user_id}}"
  local agent_id="agt-${session_id}"
  local skill_id="skill-${session_id}"
  [[ "${session_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  [[ "${fixture_user_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  d1_execute_fixture \
    "INSERT INTO agents (id, user_id, label, api_key_hash, created_at) VALUES ('${agent_id}', '${fixture_user_id}', 'Provider lifecycle agent', 'hash-${agent_id}', strftime('%Y-%m-%dT%H:%M:%fZ','now')); INSERT INTO skills (skill_id, template, facts_schema_json, actions_json, ttl_json, created_at) VALUES ('${skill_id}', 'Provider lifecycle skill', '{}', '[]', '{}', strftime('%Y-%m-%dT%H:%M:%fZ','now')); INSERT INTO sessions (id, agent_id, user_id, skill_id, state, facts_json, created_at, updated_at) VALUES ('${session_id}', '${agent_id}', '${fixture_user_id}', '${skill_id}', 'active', '{}', strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now'))"
}

insert_history_message_fixture() {
  local session_id="$1"
  local fixture_user_id="$2"
  local message_id="$3"
  local content="$4"
  [[ "${session_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  [[ "${fixture_user_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  [[ "${message_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  [[ "${content}" =~ ^[A-Za-z0-9._\ -]+$ ]]
  d1_execute_fixture \
    "INSERT INTO session_messages (id, user_id, session_id, role, content, metadata_json, sequence, created_at) VALUES ('${message_id}', '${fixture_user_id}', '${session_id}', 'user', '${content}', '{}', 1, strftime('%Y-%m-%dT%H:%M:%fZ','now'))"
}

assert_single_draft_undo_fixture() {
  local command_id="$1"
  local effect_id="$2"
  local undo_version="$3"
  local assertion
  [[ "${command_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  [[ "${effect_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  [[ "${undo_version}" =~ ^[0-9]+$ ]]
  assertion="$(d1_query_fixture \
    "SELECT CASE WHEN (SELECT COUNT(*) FROM drafts WHERE user_id = '${user_id}' AND command_id = '${command_id}') = 1 AND (SELECT COUNT(*) FROM drafts WHERE user_id = '${user_id}' AND command_id = '${command_id}' AND id = '${effect_id}' AND status = 'cancelled') = 1 AND (SELECT COUNT(*) FROM audit_logs WHERE user_id = '${user_id}' AND action = 'command.undo' AND json_extract(metadata_json, '$.command_id') = '${command_id}') = 1 AND (SELECT COUNT(*) FROM phone_changes WHERE user_id = '${user_id}' AND entity_type = 'command' AND entity_id = '${command_id}' AND version = ${undo_version}) = 1 THEN 1 ELSE 0 END AS ok")"
  jq -e '
    (type == "array") and
    (length == 1) and
    (.[0].success == true) and
    (.[0].results == [{"ok": 1}])
  ' <<<"${assertion}" >/dev/null
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

insert_succeeded_cancel_fixture() {
  local command_id="$1"
  local response_provider_id="$2"
  [[ "${command_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  [[ "${response_provider_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  d1_execute_fixture \
    "INSERT INTO action_attempts (id, user_id, command_id, action_id, provider, provider_idempotency_key, state, request_hash, response_json, attempts, next_attempt_at, last_error, created_at, updated_at) SELECT 'attempt-cancel-${command_id}', user_id, id, NULL, 'external.reminder.cancel', 'cancel-fixture-${command_id}', 'succeeded', command_hash, '{\"provider_id\":\"${response_provider_id}\",\"state\":\"succeeded\"}', 1, NULL, NULL, strftime('%Y-%m-%dT%H:%M:%fZ','now'), strftime('%Y-%m-%dT%H:%M:%fZ','now') FROM commands WHERE id = '${command_id}' AND user_id = '${user_id}'"
}

create_message() {
  local command_id="$1"
  local idempotency_key="$2"
  json "${user_auth[@]}" -X POST "${BASE_URL}/v1/phone/commands" \
    -d "$(jq -nc --arg id "${command_id}" --arg idem "${idempotency_key}" \
      '{schema_version:1,command_id:$id,intent:"send_message",args:{recipient:"+85255550123",body:"Provider lifecycle message smoke"},risk_level:"high",needs_confirmation:true,idempotency_key:$idem,confidence:0.99,locale:"en-US",timezone:"UTC"}')"
}

search_history() {
  local command_id="$1"
  local idempotency_key="$2"
  local query="$3"
  json "${user_auth[@]}" -X POST "${BASE_URL}/v1/phone/commands" \
    -d "$(jq -nc --arg id "${command_id}" --arg idem "${idempotency_key}" --arg query "${query}" \
      '{schema_version:1,command_id:$id,intent:"search_history",args:{q:$query},risk_level:"low",needs_confirmation:false,idempotency_key:$idem,confidence:0.99,locale:"en-US",timezone:"UTC"}')"
}

create_draft() {
  local command_id="$1"
  local idempotency_key="$2"
  local title="$3"
  local recipient="$4"
  local body="$5"
  json "${user_auth[@]}" -X POST "${BASE_URL}/v1/phone/commands" \
    -d "$(jq -nc --arg id "${command_id}" --arg idem "${idempotency_key}" \
      --arg title "${title}" --arg recipient "${recipient}" --arg body "${body}" \
      '{schema_version:1,command_id:$id,intent:"create_draft",args:{title:$title,recipient:$recipient,body:$body},risk_level:"low",needs_confirmation:false,idempotency_key:$idem,confidence:0.99,locale:"en-US",timezone:"UTC"}')"
}

if [[ -n "${PROVIDER_PERSIST_TO}" && -n "${PROVIDER_ENV_FILE}" ]]; then
  scoped_history_marker="provider-history-scope-$(date +%s%N)"
  owner_history_session="ses-${scoped_history_marker}-owner"
  other_history_session="ses-${scoped_history_marker}-other"
  owner_history_message="msg-${scoped_history_marker}-owner"
  other_history_message="msg-${scoped_history_marker}-other"
  owner_history_content="${scoped_history_marker} owner fixture"
  other_history_content="${scoped_history_marker} cross user fixture"
  other_auth="$(json -X POST "${BASE_URL}/v1/auth/register" \
    -d "$(jq -nc \
      --arg email "provider-history-other-$(date +%s%N)-$$@local.test" \
      --arg password "${PASSWORD}" '{email:$email,password:$password}')")"
  other_user_id="$(jq -r '.user_id' <<<"${other_auth}")"
  [[ "${other_user_id}" =~ ^[A-Za-z0-9._-]+$ ]]
  create_session_fixture "${owner_history_session}" "${user_id}"
  create_session_fixture "${other_history_session}" "${other_user_id}"
  insert_history_message_fixture \
    "${owner_history_session}" "${user_id}" \
    "${owner_history_message}" "${owner_history_content}"
  insert_history_message_fixture \
    "${other_history_session}" "${other_user_id}" \
    "${other_history_message}" "${other_history_content}"

  history_command_id="cmd-${scoped_history_marker}"
  history_command_key="idem-${scoped_history_marker}"
  history_queued="$(search_history \
    "${history_command_id}" "${history_command_key}" "${scoped_history_marker}")"
  test "$(jq -r '.state' <<<"${history_queued}")" = "queued"
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  history_result="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${history_command_id}")"
  jq -e \
    --arg command_id "${history_command_id}" \
    --arg query "${scoped_history_marker}" \
    --arg owner_session "${owner_history_session}" \
    --arg owner_message "${owner_history_message}" \
    --arg owner_content "${owner_history_content}" \
    --arg other_session "${other_history_session}" \
    --arg other_message "${other_history_message}" \
    --arg other_content "${other_history_content}" '
      (.command_id == $command_id) and
      (.state == "succeeded") and
      (.result.kind == "history_search") and
      (.result.data.query == $query) and
      (.result.data.messages == .result.data.items) and
      (.result.data.messages | length == 1) and
      (.result.data.messages[0].session_id == $owner_session) and
      (.result.data.messages[0].message_id == $owner_message) and
      (.result.data.messages[0].content == $owner_content) and
      ([.result.data.messages[] | select(
        .session_id == $other_session or
        .message_id == $other_message or
        .content == $other_content
      )] | length == 0)
    ' <<<"${history_result}" >/dev/null

  draft_marker="provider-draft-route-$(date +%s%N)"
  draft_command_id="cmd-${draft_marker}"
  draft_command_key="idem-${draft_marker}"
  draft_title="${draft_marker} title"
  draft_recipient="fixture recipient"
  draft_body="${draft_marker} body"
  draft_queued="$(create_draft \
    "${draft_command_id}" "${draft_command_key}" \
    "${draft_title}" "${draft_recipient}" "${draft_body}")"
  test "$(jq -r '.state' <<<"${draft_queued}")" = "queued"
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  draft_result="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${draft_command_id}")"
  jq -e \
    --arg command_id "${draft_command_id}" \
    --arg title "${draft_title}" \
    --arg recipient "${draft_recipient}" '
      (.command_id == $command_id) and
      (.state == "succeeded") and
      (.result.kind == "draft") and
      (.result.status == "draft") and
      (.result.title == $title) and
      (.result.recipient == $recipient) and
      (.result.draft_id | type == "string" and length > 0) and
      (.undo_command_id == $command_id)
    ' <<<"${draft_result}" >/dev/null
  draft_effect_id="$(jq -r '.result.draft_id' <<<"${draft_result}")"

  draft_undo="$(json "${user_auth[@]}" -X POST \
    "${BASE_URL}/v1/phone/commands/${draft_command_id}/undo")"
  jq -e --arg effect_id "${draft_effect_id}" '
    (.state == "succeeded") and
    (.result.kind == "draft") and
    (.result.status == "draft") and
    (.result.undo.status == "cancelled") and
    (.result.undo.effect_id == $effect_id) and
    (.result.undo.already_cancelled == false) and
    (.undo_result == .result.undo) and
    (.undo_command_id == null)
  ' <<<"${draft_undo}" >/dev/null
  draft_undo_version="$(jq -r '.version' <<<"${draft_undo}")"

  duplicate_draft_undo="$(json "${user_auth[@]}" -X POST \
    "${BASE_URL}/v1/phone/commands/${draft_command_id}/undo")"
  jq -e \
    --arg effect_id "${draft_effect_id}" \
    --argjson expected_version "${draft_undo_version}" \
    --argjson first_result "$(jq -c '.result' <<<"${draft_undo}")" '
      (.state == "succeeded") and
      (.version == $expected_version) and
      (.result == $first_result) and
      (.undo_result.kind == "undo") and
      (.undo_result.status == "cancelled") and
      (.undo_result.effect_id == $effect_id) and
      (.undo_result.already_cancelled == true) and
      (.undo_command_id == null)
    ' <<<"${duplicate_draft_undo}" >/dev/null
  assert_single_draft_undo_fixture \
    "${draft_command_id}" "${draft_effect_id}" "${draft_undo_version}"
fi

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
  # A malformed succeeded row represents the crash/replay boundary. Local
  # finalization must reject its mismatched resource without a provider call.
  cancel_replay_id="cmd-cancel-replay-mismatch-$(date +%s%N)"
  cancel_replay_key="idem-cancel-replay-mismatch-$(date +%s%N)"
  cancel_replay="$(create_reminder "${cancel_replay_id}" "${cancel_replay_key}")"
  test "$(jq -r '.state' <<<"${cancel_replay}")" = "queued"
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  cancel_replay_first="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${cancel_replay_id}")"
  test "$(jq -r '.state' <<<"${cancel_replay_first}")" = "succeeded"
  insert_succeeded_cancel_fixture \
    "${cancel_replay_id}" "mock-rem-not-the-requested-resource"
  if [[ -n "${PROVIDER_LOG}" ]]; then
    cancel_replay_calls_before="$(count_provider_requests '/reminders/cancel' "${PROVIDER_LOG}")"
  fi
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  cancel_replay_final="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${cancel_replay_id}")"
  test "$(jq -r '.state' <<<"${cancel_replay_final}")" = "succeeded"
  jq -e '.result.status == "scheduled" and (.result.undo? == null)' \
    <<<"${cancel_replay_final}" >/dev/null
  if [[ -n "${PROVIDER_LOG}" ]]; then
    cancel_replay_calls_after="$(count_provider_requests '/reminders/cancel' "${PROVIDER_LOG}")"
    test "${cancel_replay_calls_after}" = "${cancel_replay_calls_before}"
  fi

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
  cancel_missing_replay="$(curl --silent --show-error --max-time "${HTTP_TIMEOUT_SECONDS:-10}" \
    -H 'content-type: application/json' "${user_auth[@]}" \
    -X POST "${BASE_URL}/v1/phone/commands/${cancel_missing_id}/undo" \
    -w $'\n%{http_code}')" || {
      echo "provider lifecycle failed: missing-ID cancellation replay had a transport failure" >&2
      exit 1
    }
  test "${cancel_missing_replay##*$'\n'}" = "503"
  assert_error_response "${cancel_missing_replay%$'\n'*}" provider_cancel_mismatch
  cancel_missing_final="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${cancel_missing_id}")"
  jq -e '.result.status == "scheduled" and (.result.undo? == null)' \
    <<<"${cancel_missing_final}" >/dev/null

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
  cancel_mismatch_final="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${cancel_mismatch_id}")"
  jq -e '.result.status == "scheduled" and (.result.undo? == null)' \
    <<<"${cancel_mismatch_final}" >/dev/null

  message_mismatch_id="cmd-message-status-mismatch-$(date +%s%N)"
  message_mismatch_key="idem-message-status-mismatch-$(date +%s%N)"
  if [[ -n "${PROVIDER_LOG}" ]]; then
    message_mismatch_delivery_before="$(count_provider_requests '/messages/deliver' "${PROVIDER_LOG}")"
    message_mismatch_status_before="$(count_provider_requests '/messages/status' "${PROVIDER_LOG}")"
  fi
  message_mismatch="$(create_message "${message_mismatch_id}" "${message_mismatch_key}")"
  test "$(jq -r '.state' <<<"${message_mismatch}")" = "awaiting_confirmation"
  message_mismatch_token="$(jq -r '.confirmation_token' <<<"${message_mismatch}")"
  json "${user_auth[@]}" -X POST \
    "${BASE_URL}/v1/phone/commands/${message_mismatch_id}/confirm" \
    -d "$(jq -nc --arg token "${message_mismatch_token}" '{confirmation_token:$token}')" >/dev/null
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  message_mismatch_first="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${message_mismatch_id}")"
  jq -e '(.state == "unknown" or .state == "retryable")' \
    <<<"${message_mismatch_first}" >/dev/null
  test "$(jq -r '.error.code' <<<"${message_mismatch_first}")" = "provider_pending"
  replace_message_provider_id_fixture \
    "${message_mismatch_id}" "canonical-provider-message"
  sleep "${WAIT_SECONDS}"
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  message_mismatch_final="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${message_mismatch_id}")"
  assert_structured_command_error "${message_mismatch_final}"
  test "$(jq -r '.error.code' <<<"${message_mismatch_final}")" = "provider_id_mismatch"
  if [[ -n "${PROVIDER_LOG}" ]]; then
    message_mismatch_delivery_after="$(count_provider_requests '/messages/deliver' "${PROVIDER_LOG}")"
    message_mismatch_status_after="$(count_provider_requests '/messages/status' "${PROVIDER_LOG}")"
    test "${message_mismatch_delivery_after}" = "$((message_mismatch_delivery_before + 1))"
    test "${message_mismatch_status_after}" = "$((message_mismatch_status_before + 1))"
  fi

  message_missing_id="cmd-message-missing-id-$(date +%s%N)"
  message_missing_key="idem-message-missing-id-$(date +%s%N)"
  if [[ -n "${PROVIDER_LOG}" ]]; then
    message_missing_delivery_before="$(count_provider_requests '/messages/deliver' "${PROVIDER_LOG}")"
    message_missing_status_before="$(count_provider_requests '/messages/status' "${PROVIDER_LOG}")"
  fi
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
  sleep "${WAIT_SECONDS}"
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
  message_missing_reconciled="$(get_json "${user_auth[@]}" \
    "${BASE_URL}/v1/phone/commands/${message_missing_id}")"
  test "$(jq -r '.state' <<<"${message_missing_reconciled}")" = "succeeded"
  test "$(jq -r '.result.delivery_state' <<<"${message_missing_reconciled}")" = "sent"
  test "$(jq -r '.result.provider_id' <<<"${message_missing_reconciled}")" != "null"
  if [[ -n "${PROVIDER_LOG}" ]]; then
    message_missing_delivery_after="$(count_provider_requests '/messages/deliver' "${PROVIDER_LOG}")"
    message_missing_status_after="$(count_provider_requests '/messages/status' "${PROVIDER_LOG}")"
    test "${message_missing_delivery_after}" = "$((message_missing_delivery_before + 1))"
    test "${message_missing_status_after}" = "$((message_missing_status_before + 1))"
  fi
else
  echo "provider lifecycle smoke: strict provider-resource identity checks are disabled for this backend base"
fi

if [[ "${PROVIDER_STRICT_RESOURCE_IDENTITY}" == "true" ]]; then
  printf '%s\n' 'provider lifecycle smoke passed: owner-scoped history search, idempotent draft undo, provider IDs, atomic message identity, duplicate idempotency, cancellation safety, deleted-session reconciliation, zero-attempt cancellation, elapsed-deadline and expired-TTL recovery, asynchronous delivery, structured failures, and strict resource-identity fail-closed behavior'
else
  printf '%s\n' 'provider lifecycle smoke passed: owner-scoped history search, idempotent draft undo, provider IDs, atomic message identity, duplicate idempotency, cancellation safety, deleted-session reconciliation, zero-attempt cancellation, elapsed-deadline and expired-TTL recovery, asynchronous delivery, and structured failures (strict resource-identity checks opt-in)'
fi
