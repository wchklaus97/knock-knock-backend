#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" health >/dev/null

BASE_URL="${BASE_URL:-http://127.0.0.1:8787}"
BASE_URL="${BASE_URL%/}"
PASSWORD="${SMOKE_PASSWORD:-password123}"
EMAIL_A="${SMOKE_EMAIL:-memory-a-$(date +%s)-$$@local.test}"
EMAIL_B="${SMOKE_OTHER_EMAIL:-memory-b-$(date +%s)-$$@local.test}"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/knock-knock-memory-contract.XXXXXX")"
trap 'rm -rf "${TMP_DIR}"' EXIT

json() {
  curl --fail-with-body --silent --show-error \
    -H 'content-type: application/json' "$@"
}

get() {
  curl --fail-with-body --silent --show-error "$@"
}

register() {
  local email="$1"
  json -X POST "${BASE_URL}/v1/auth/register" \
    -d "$(jq -nc --arg email "$email" --arg password "$PASSWORD" \
      '{email:$email,password:$password}')"
}

post_status() {
  local output="$1"
  local token="$2"
  local path="$3"
  local body="$4"
  curl --silent --show-error -o "$output" -w '%{http_code}' \
    -H "authorization: Bearer ${token}" \
    -H 'content-type: application/json' \
    -X POST "${BASE_URL}${path}" -d "$body"
}

delete_status() {
  local output="$1"
  local token="$2"
  local path="$3"
  curl --silent --show-error -o "$output" -w '%{http_code}' \
    -H "authorization: Bearer ${token}" \
    -X DELETE "${BASE_URL}${path}"
}

get_status() {
  local output="$1"
  local token="$2"
  local path="$3"
  curl --silent --show-error -o "$output" -w '%{http_code}' \
    -H "authorization: Bearer ${token}" \
    "${BASE_URL}${path}"
}

memory_body() {
  local key="$1"
  local display_text="$2"
  jq -nc --arg key "$key" --arg display "$display_text" '
    {
      schema_version: 1,
      kind: "preference",
      subject: "user",
      predicate: "preferred_editor",
      value: {name: "Zed", nested: {a: 1, z: 2}},
      display_text: $display,
      locale: "en-HK",
      source_type: "explicit_user",
      user_confirmed: true,
      confidence: 1.0,
      idempotency_key: $key
    }
  '
}

auth_a="$(register "$EMAIL_A")"
auth_b="$(register "$EMAIL_B")"
token_a="$(jq -r '.token' <<<"$auth_a")"
token_b="$(jq -r '.token' <<<"$auth_b")"
test -n "$token_a" && test "$token_a" != "null"
test -n "$token_b" && test "$token_b" != "null"
auth_a_headers=(-H "authorization: Bearer ${token_a}")
auth_b_headers=(-H "authorization: Bearer ${token_b}")

# Build two authenticated B-owned sessions and one message. A must not be able
# to use those provenance IDs, and B must not pair the message with B's other
# session.
agent_b="$(json "${auth_b_headers[@]}" -X POST "${BASE_URL}/v1/agents" \
  -d '{"label":"memory-source-b","host_label":"local"}')"
agent_b_id="$(jq -r '.agent.agent_id' <<<"$agent_b")"
rotated_b="$(json "${auth_b_headers[@]}" -X POST \
  "${BASE_URL}/v1/agents/${agent_b_id}/rotate-key")"
agent_b_key="$(jq -r '.api_key' <<<"$rotated_b")"
agent_b_headers=(-H "x-agent-key: ${agent_b_key}")

session_b_one="$(json "${agent_b_headers[@]}" -X POST "${BASE_URL}/v1/sessions" \
  -d "$(jq -nc --arg key "memory-source-one-$(date +%s%N)" \
    '{skill_id:"deploy.result",idempotency_key:$key,title:"Memory source one"}')")"
session_b_one_id="$(jq -r '.session_id' <<<"$session_b_one")"
json "${agent_b_headers[@]}" -X POST \
  "${BASE_URL}/v1/sessions/${session_b_one_id}/events" \
  -d "$(jq -nc --arg key "memory-source-event-$(date +%s%N)" \
    '{status:"info",idempotency_key:$key,summary:"B-owned source message",facts:{}}')" \
  >/dev/null
messages_b="$(get "${auth_b_headers[@]}" \
  "${BASE_URL}/v1/phone/sessions/${session_b_one_id}/messages")"
message_b_id="$(jq -r '(.messages // .items)[0].message_id' <<<"$messages_b")"
test -n "$message_b_id" && test "$message_b_id" != "null"

session_b_two="$(json "${agent_b_headers[@]}" -X POST "${BASE_URL}/v1/sessions" \
  -d "$(jq -nc --arg key "memory-source-two-$(date +%s%N)" \
    '{skill_id:"deploy.result",idempotency_key:$key,title:"Memory source two"}')")"
session_b_two_id="$(jq -r '.session_id' <<<"$session_b_two")"

cross_source_key="memory-cross-source-$(date +%s%N)"
cross_source_body="$(memory_body "$cross_source_key" "The user prefers Zed.")"
cross_source_body="$(jq -c \
  --arg session "$session_b_one_id" --arg message "$message_b_id" \
  '. + {source_session_id:$session,source_message_id:$message}' \
  <<<"$cross_source_body")"
cross_source_status="$(post_status "${TMP_DIR}/cross-source.json" "$token_a" \
  '/v1/phone/memories' "$cross_source_body")"
test "$cross_source_status" = "400"
jq -e '.error.code == "validation_error"' "${TMP_DIR}/cross-source.json" >/dev/null

mismatched_source_key="memory-mismatched-source-$(date +%s%N)"
mismatched_source_body="$(memory_body "$mismatched_source_key" "The user prefers Zed.")"
mismatched_source_body="$(jq -c \
  --arg session "$session_b_two_id" --arg message "$message_b_id" \
  '. + {source_session_id:$session,source_message_id:$message}' \
  <<<"$mismatched_source_body")"
mismatched_source_status="$(post_status "${TMP_DIR}/mismatched-source.json" "$token_b" \
  '/v1/phone/memories' "$mismatched_source_body")"
test "$mismatched_source_status" = "400"
jq -e '.error.code == "validation_error"' "${TMP_DIR}/mismatched-source.json" >/dev/null

sync_before_first="$(get "${auth_a_headers[@]}" "${BASE_URL}/v1/phone/sync?limit=50")"
cursor_before_first="$(jq -r '.cursor' <<<"$sync_before_first")"

first_key="memory-replay-$(date +%s%N)"
first_body="$(memory_body "$first_key" "The user prefers Zed.")"
first_status="$(post_status "${TMP_DIR}/first.json" "$token_a" \
  '/v1/phone/memories' "$first_body")"
test "$first_status" = "201"
memory_id="$(jq -r '.memory_id' "${TMP_DIR}/first.json")"
test -n "$memory_id" && test "$memory_id" != "null"
jq -e '
  (.schema_version == 1) and
  (.source_type == "explicit_user") and
  (.user_confirmed == true) and
  (.value.nested.a == 1) and
  (has("id") | not) and
  (has("user_id") | not) and
  (has("value_json") | not) and
  (has("request_hash") | not) and
  (has("idempotency_key") | not) and
  (has("deleted_at") | not)
' "${TMP_DIR}/first.json" >/dev/null

# Same semantic value with recursively different object-key ordering is the
# same request. It must replay the same MemoryItem and emit no second change.
reordered_body="$(jq -nc --arg key "$first_key" '
  {
    schema_version: 1,
    kind: "preference",
    subject: "user",
    predicate: "preferred_editor",
    value: {nested: {z: 2, a: 1}, name: "Zed"},
    display_text: "The user prefers Zed.",
    locale: "en-HK",
    source_type: "explicit_user",
    user_confirmed: true,
    confidence: 1.0,
    idempotency_key: $key
  }
')"
replay_status="$(post_status "${TMP_DIR}/replay.json" "$token_a" \
  '/v1/phone/memories' "$reordered_body")"
test "$replay_status" = "200"
test "$(jq -r '.memory_id' "${TMP_DIR}/replay.json")" = "$memory_id"

encoded_cursor="$(jq -rn --arg value "$cursor_before_first" '$value | @uri')"
first_changes="$(get "${auth_a_headers[@]}" \
  "${BASE_URL}/v1/phone/sync?limit=50&after=${encoded_cursor}")"
jq -e --arg memory_id "$memory_id" '
  [.changes[] | select(.entity_type == "memory" and .entity_id == $memory_id)] | length == 1
' <<<"$first_changes" >/dev/null

changed_body="$(jq -c '.display_text = "The user prefers another editor."' \
  <<<"$first_body")"
conflict_status="$(post_status "${TMP_DIR}/conflict.json" "$token_a" \
  '/v1/phone/memories' "$changed_body")"
test "$conflict_status" = "409"
jq -e '.error.code == "conflict" and .error.retryable == false' \
  "${TMP_DIR}/conflict.json" >/dev/null

# Exercise the UNIQUE(user_id,idempotency_key) race with two concurrent
# requests. Exactly one returns 201, both return the same ID, list has one row,
# and sync has one insert change.
sync_before_race="$(get "${auth_a_headers[@]}" "${BASE_URL}/v1/phone/sync?limit=50")"
cursor_before_race="$(jq -r '.cursor' <<<"$sync_before_race")"
race_key="memory-race-$(date +%s%N)"
race_body="$(memory_body "$race_key" "The user prefers a modal editor.")"
curl --silent --show-error -o "${TMP_DIR}/race-one.json" -w '%{http_code}' \
  -H "authorization: Bearer ${token_a}" -H 'content-type: application/json' \
  -X POST "${BASE_URL}/v1/phone/memories" -d "$race_body" \
  >"${TMP_DIR}/race-one.status" &
race_one_pid=$!
curl --silent --show-error -o "${TMP_DIR}/race-two.json" -w '%{http_code}' \
  -H "authorization: Bearer ${token_a}" -H 'content-type: application/json' \
  -X POST "${BASE_URL}/v1/phone/memories" -d "$race_body" \
  >"${TMP_DIR}/race-two.status" &
race_two_pid=$!
wait "$race_one_pid"
wait "$race_two_pid"
race_statuses="$(sort "${TMP_DIR}/race-one.status" "${TMP_DIR}/race-two.status" | tr '\n' ' ')"
test "$race_statuses" = "200 201 "
race_memory_id="$(jq -r '.memory_id' "${TMP_DIR}/race-one.json")"
test "$race_memory_id" = "$(jq -r '.memory_id' "${TMP_DIR}/race-two.json")"

active_memories="$(get "${auth_a_headers[@]}" "${BASE_URL}/v1/phone/memories?limit=50")"
test "$(jq -r --arg id "$race_memory_id" \
  '[.memories[] | select(.memory_id == $id)] | length' <<<"$active_memories")" = "1"
race_cursor="$(jq -rn --arg value "$cursor_before_race" '$value | @uri')"
race_changes="$(get "${auth_a_headers[@]}" \
  "${BASE_URL}/v1/phone/sync?limit=50&after=${race_cursor}")"
jq -e --arg memory_id "$race_memory_id" '
  [.changes[] | select(.entity_type == "memory" and .entity_id == $memory_id)] | length == 1
' <<<"$race_changes" >/dev/null

# User B can neither observe nor delete A's MemoryItem.
cross_get_status="$(get_status "${TMP_DIR}/cross-get.json" "$token_b" \
  "/v1/phone/memories/${memory_id}")"
test "$cross_get_status" = "404"
cross_list="$(get "${auth_b_headers[@]}" "${BASE_URL}/v1/phone/memories?limit=50")"
test "$(jq -r --arg id "$memory_id" \
  '[.memories[] | select(.memory_id == $id)] | length' <<<"$cross_list")" = "0"
cross_delete_status="$(delete_status "${TMP_DIR}/cross-delete.json" "$token_b" \
  "/v1/phone/memories/${memory_id}")"
test "$cross_delete_status" = "404"

# Stable cursor traversal must never return the same MemoryItem twice.
page_one="$(get "${auth_a_headers[@]}" "${BASE_URL}/v1/phone/memories?limit=1")"
page_one_id="$(jq -r '.memories[0].memory_id' <<<"$page_one")"
page_one_cursor="$(jq -r '.next_cursor' <<<"$page_one")"
test -n "$page_one_cursor" && test "$page_one_cursor" != "null"
page_one_cursor="$(jq -rn --arg value "$page_one_cursor" '$value | @uri')"
page_two="$(get "${auth_a_headers[@]}" \
  "${BASE_URL}/v1/phone/memories?limit=1&before=${page_one_cursor}")"
page_two_id="$(jq -r '.memories[0].memory_id' <<<"$page_two")"
test -n "$page_two_id" && test "$page_two_id" != "null"
test "$page_two_id" != "$page_one_id"

# Confirmation, source authority, date parsing, and unknown-field behavior are
# enforced by the running Worker, not only by Rust string-contract tests.
unconfirmed_body="$(jq -c '.idempotency_key = "memory-unconfirmed-test" | .user_confirmed = false' \
  <<<"$first_body")"
test "$(post_status "${TMP_DIR}/unconfirmed.json" "$token_a" \
  '/v1/phone/memories' "$unconfirmed_body")" = "400"
trusted_body="$(jq -c '.idempotency_key = "memory-trusted-test" | .source_type = "trusted_system"' \
  <<<"$first_body")"
test "$(post_status "${TMP_DIR}/trusted.json" "$token_a" \
  '/v1/phone/memories' "$trusted_body")" = "400"
unknown_body="$(jq -c '.idempotency_key = "memory-unknown-test" | .unknown_policy = true' \
  <<<"$first_body")"
test "$(post_status "${TMP_DIR}/unknown.json" "$token_a" \
  '/v1/phone/memories' "$unknown_body")" = "400"
slash_date_body="$(jq -c '.idempotency_key = "memory-slash-date" | .retention_expires_at = "08/14/2099"' \
  <<<"$first_body")"
test "$(post_status "${TMP_DIR}/slash-date.json" "$token_a" \
  '/v1/phone/memories' "$slash_date_body")" = "400"
offsetless_body="$(jq -c '.idempotency_key = "memory-no-timezone" | .retention_expires_at = "2099-08-14T12:00:00"' \
  <<<"$first_body")"
test "$(post_status "${TMP_DIR}/offsetless.json" "$token_a" \
  '/v1/phone/memories' "$offsetless_body")" = "400"

# DELETE is soft, disappears from reads, and emits one durable tombstone.
sync_before_delete="$(get "${auth_a_headers[@]}" "${BASE_URL}/v1/phone/sync?limit=50")"
cursor_before_delete="$(jq -r '.cursor' <<<"$sync_before_delete")"
delete_status_code="$(delete_status "${TMP_DIR}/delete.json" "$token_a" \
  "/v1/phone/memories/${memory_id}")"
test "$delete_status_code" = "200"
jq -e --arg memory_id "$memory_id" \
  '.ok == true and .memory_id == $memory_id and (.deleted_at | type == "string")' \
  "${TMP_DIR}/delete.json" >/dev/null
test "$(get_status "${TMP_DIR}/deleted-get.json" "$token_a" \
  "/v1/phone/memories/${memory_id}")" = "404"
after_delete_list="$(get "${auth_a_headers[@]}" "${BASE_URL}/v1/phone/memories?limit=50")"
test "$(jq -r --arg id "$memory_id" \
  '[.memories[] | select(.memory_id == $id)] | length' <<<"$after_delete_list")" = "0"
delete_cursor="$(jq -rn --arg value "$cursor_before_delete" '$value | @uri')"
delete_changes="$(get "${auth_a_headers[@]}" \
  "${BASE_URL}/v1/phone/sync?limit=50&after=${delete_cursor}")"
jq -e --arg memory_id "$memory_id" '
  [.changes[] | select(
    .entity_type == "memory" and
    .entity_id == $memory_id and
    (.deleted_at | type == "string")
  )] | length == 1
' <<<"$delete_changes" >/dev/null

# Expiry converges through the same soft-delete/tombstone path when an
# authenticated history read invokes purge_expired.
retention_timestamp="$(python3 - <<'PY'
from datetime import datetime, timedelta, timezone
print((datetime.now(timezone.utc) + timedelta(seconds=3)).isoformat(timespec="milliseconds").replace("+00:00", "Z"))
PY
)"
retention_key="memory-retention-$(date +%s%N)"
retention_body="$(memory_body "$retention_key" "The user temporarily prefers Zed.")"
retention_body="$(jq -c --arg expiry "$retention_timestamp" \
  '.retention_expires_at = $expiry' <<<"$retention_body")"
test "$(post_status "${TMP_DIR}/retention.json" "$token_a" \
  '/v1/phone/memories' "$retention_body")" = "201"
retention_memory_id="$(jq -r '.memory_id' "${TMP_DIR}/retention.json")"
sync_before_expiry="$(get "${auth_a_headers[@]}" "${BASE_URL}/v1/phone/sync?limit=50")"
cursor_before_expiry="$(jq -r '.cursor' <<<"$sync_before_expiry")"
sleep 4
test "$(get_status "${TMP_DIR}/expired-get.json" "$token_a" \
  "/v1/phone/memories/${retention_memory_id}")" = "404"
expiry_cursor="$(jq -rn --arg value "$cursor_before_expiry" '$value | @uri')"
expiry_changes="$(get "${auth_a_headers[@]}" \
  "${BASE_URL}/v1/phone/sync?limit=50&after=${expiry_cursor}")"
jq -e --arg memory_id "$retention_memory_id" '
  [.changes[] | select(
    .entity_type == "memory" and
    .entity_id == $memory_id and
    (.deleted_at | type == "string")
  )] | length == 1
' <<<"$expiry_changes" >/dev/null

printf '%s\n' 'memory contract smoke passed: canonical replay/concurrency, one-row/one-change idempotency, source ownership, isolation, pagination, confirmation, strict retention, soft-delete, and sync tombstones'
