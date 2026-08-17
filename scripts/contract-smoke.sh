#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" health >/dev/null

BASE_URL="${BASE_URL:-http://127.0.0.1:8787}"
BASE_URL="${BASE_URL%/}"
AUTH_MODE="${SMOKE_AUTH_MODE:-register}"
PASSWORD="${SMOKE_PASSWORD:-password123}"
EMAIL="${SMOKE_EMAIL:-rust-smoke-$(date +%s)-$$@local.test}"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

case "${AUTH_MODE}" in
  register)
    OTHER_EMAIL="rust-contract-other-$(date +%s)-$$@local.test"
    OTHER_PASSWORD="${PASSWORD}"
    ;;
  login)
    : "${SMOKE_EMAIL:?SMOKE_EMAIL is required when SMOKE_AUTH_MODE=login}"
    : "${SMOKE_PASSWORD:?SMOKE_PASSWORD is required when SMOKE_AUTH_MODE=login}"
    OTHER_EMAIL="${SMOKE_OTHER_EMAIL:?SMOKE_OTHER_EMAIL is required when SMOKE_AUTH_MODE=login}"
    OTHER_PASSWORD="${SMOKE_OTHER_PASSWORD:?SMOKE_OTHER_PASSWORD is required when SMOKE_AUTH_MODE=login}"
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

auth_user() {
  local email="$1"
  local password="$2"
  local endpoint="register"
  if [[ "${AUTH_MODE}" == "login" ]]; then
    endpoint="login"
  fi
  json -X POST "$BASE_URL/v1/auth/${endpoint}" \
    -d "$(jq -nc --arg email "$email" --arg password "$password" '{email:$email,password:$password}')"
}

health="$(get "$BASE_URL/health")"
jq -e '
  (.ok == true) and
  (.api == "rust") and
  (.runtime == "cloudflare-worker") and
  (.version | type == "string") and
  (.apns_ready | type == "boolean") and
  (.action_provider_ready | type == "boolean")
' <<<"$health" >/dev/null
v1_health="$(get "$BASE_URL/v1/health")"
jq -e '(.ok == true) and (.api == "rust") and (.runtime == "cloudflare-worker")' <<<"$v1_health" >/dev/null
metrics="$(get "$BASE_URL/metrics")"
grep -q 'knock_knock_api_info' <<<"$metrics"
grep -q 'knock_knock_provider_ready' <<<"$metrics"
grep -q 'knock_knock_apns_ready' <<<"$metrics"
grep -Eq 'knock_knock_model_enabled[[:space:]]+[01]' <<<"$metrics"
request_headers="$(curl --fail-with-body --silent --show-error \
  -H 'x-request-id: contract-smoke-correlation' \
  -D - -o /dev/null "$BASE_URL/health")"
grep -qi '^x-request-id: contract-smoke-correlation' <<<"$request_headers"
invalid_request_headers="$(curl --fail-with-body --silent --show-error \
  -H 'x-request-id: invalid request id with spaces' \
  -D - -o /dev/null "$BASE_URL/health")"
invalid_request_id="$(awk -F': ' 'tolower($1) == "x-request-id" {gsub(/\r/, "", $2); print $2; exit}' <<<"$invalid_request_headers")"
test -n "$invalid_request_id"
test "$invalid_request_id" != 'invalid request id with spaces'

auth="$(auth_user "$EMAIL" "$PASSWORD")"
token="$(jq -r '.token' <<<"$auth")"
refresh="$(jq -r '.refresh_token' <<<"$auth")"
test -n "$token" && test "$token" != "null"
test -n "$refresh" && test "$refresh" != "null"

user_auth=(-H "authorization: Bearer $token")
other_auth_response="$(auth_user "$OTHER_EMAIL" "$OTHER_PASSWORD")"
other_token="$(jq -r '.token' <<<"$other_auth_response")"
test -n "$other_token" && test "$other_token" != "null"
other_auth=(-H "authorization: Bearer $other_token")
login="$(json -X POST "$BASE_URL/v1/auth/login" \
  -d "$(jq -nc --arg email "$EMAIL" --arg password "$PASSWORD" '{email:$email,password:$password}')")"
test -n "$(jq -r '.token' <<<"$login")"
agent="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/agents" \
  -d '{"label":"rust-contract-smoke","host_label":"local"}')"
agent_id="$(jq -r '.agent.agent_id' <<<"$agent")"
rotated_agent="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/agents/$agent_id/rotate-key")"
agent_key="$(jq -r '.api_key' <<<"$rotated_agent")"
test -n "$agent_key" && test "$agent_key" != "null"
agent_auth=(-H "x-agent-key: $agent_key")
agents="$(get "${user_auth[@]}" "$BASE_URL/v1/agents")"
test "$(jq -r '.agents | length' <<<"$agents")" -ge 1
skills="$(get "${user_auth[@]}" "$BASE_URL/v1/skills")"
test "$(jq -r '.skills | length' <<<"$skills")" -ge 1

device_correlation_id="device-contract-$(date +%s%N)"
device_push_token="contract-smoke-device-token-$(date +%s%N)"
device="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/devices" \
  -d "$(jq -nc --arg device_id "$device_correlation_id" \
    --arg push_token "$device_push_token" \
    '{platform:"ios",push_token:$push_token,locale:"zh-HK",timezone:"Asia/Hong_Kong",device_id:$device_id}')")"
jq -e --arg device_id "$device_correlation_id" \
  '(.device_id | type == "string" and length > 0) and (.platform == "ios")' <<<"$device" >/dev/null
device_again="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/devices" \
  -d "$(jq -nc --arg device_id "$device_correlation_id" \
    --arg push_token "$device_push_token" \
    '{platform:"ios",push_token:$push_token,locale:"zh-HK",timezone:"Asia/Hong_Kong",device_id:$device_id}')")"
test "$(jq -r '.device_id' <<<"$device_again")" = "$(jq -r '.device_id' <<<"$device")"
device_reinstall_correlation_id="${device_correlation_id}-reinstall"
device_after_reinstall="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/devices" \
  -d "$(jq -nc --arg device_id "$device_reinstall_correlation_id" \
    --arg push_token "$device_push_token" \
    '{platform:"ios",push_token:$push_token,locale:"zh-HK",timezone:"Asia/Hong_Kong",device_id:$device_id}')")"
test "$(jq -r '.device_id' <<<"$device_after_reinstall")" != "$(jq -r '.device_id' <<<"$device")"

command_key="command-smoke-$(date +%s%N)"
command="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/commands" \
  -d "$(jq -nc --arg key "$command_key" \
    '{schema_version:1,command_id:("cmd-smoke-" + ($key | split("-") | last)),intent:"search_history",args:{q:"history"},risk_level:"low",needs_confirmation:false,idempotency_key:$key,confidence:0.95,locale:"zh-Hans-HK",timezone:"Asia/Hong_Kong"}')")"
command_id="$(jq -r '.command_id' <<<"$command")"
test -n "$command_id" && test "$command_id" != "null"
jq -e '
  (.state == "succeeded") and
  (.state != "queued") and
  (.presentation.terminal == true)
' <<<"$command" >/dev/null
command_detail="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/commands/$command_id")"
jq -e --arg command_id "$command_id" \
  '(.command_id == $command_id) and (.state == "succeeded") and (.state != "queued") and
   (.presentation.terminal == true) and (.version | type == "number") and
   (.presentation.schema_version == 1) and (.presentation.display_text | type == "string")' \
  <<<"$command_detail" >/dev/null
command_headers="$(curl --fail-with-body --silent --show-error \
  "${user_auth[@]}" -D - -o /dev/null "$BASE_URL/v1/phone/commands/$command_id")"
grep -qi '^cache-control: private, no-store' <<<"$command_headers"
commands="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/commands?state=succeeded&limit=50")"
test "$(jq -r --arg id "$command_id" '[.commands[] | select(.command_id == $id)] | length' <<<"$commands")" = "1"
jq -e '
  all(.commands[];
    (.presentation.schema_version == 1) and
    (has("result") | not) and
    (has("error") | not) and
    (has("command") | not)
  )
' <<<"$commands" >/dev/null

cross_user_command_status="$(curl --silent --show-error \
  -o "${TMP_DIR}/cross-user-command.json" -w '%{http_code}' \
  "${other_auth[@]}" "$BASE_URL/v1/phone/commands/$command_id")"
test "$cross_user_command_status" = "404"
jq -e '.error.code == "not_found"' "${TMP_DIR}/cross-user-command.json" >/dev/null
cross_user_commands="$(get "${other_auth[@]}" "$BASE_URL/v1/phone/commands?limit=50")"
test "$(jq -r --arg id "$command_id" '[.commands[] | select(.command_id == $id)] | length' <<<"$cross_user_commands")" = "0"

conflicting_command_id="cmd-conflict-$(date +%s%N)"
idempotency_conflict_status="$(curl --silent --show-error \
  -o "${TMP_DIR}/command-idempotency-conflict.json" -w '%{http_code}' \
  "${user_auth[@]}" -H 'content-type: application/json' \
  -X POST "$BASE_URL/v1/phone/commands" \
  -d "$(jq -nc --arg id "$conflicting_command_id" --arg key "$command_key" \
    '{schema_version:1,command_id:$id,intent:"search_history",args:{q:"different history"},risk_level:"low",needs_confirmation:false,idempotency_key:$key,confidence:0.95,locale:"zh-Hans-HK",timezone:"Asia/Hong_Kong"}')")"
test "$idempotency_conflict_status" = "409"
jq -e '.error.code == "conflict" and .error.retryable == false' \
  "${TMP_DIR}/command-idempotency-conflict.json" >/dev/null

# A lost create response must be recoverable without weakening one-time
# confirmation. Exact idempotent replay rotates the token; the old token is
# retained as used for audit and can no longer authorize execution.
confirmation_key="command-confirmation-replay-$(date +%s%N)"
confirmation_id="cmd-confirmation-replay-$(date +%s%N)"
confirmation_body="$(jq -nc --arg id "$confirmation_id" --arg key "$confirmation_key" \
  '{schema_version:1,command_id:$id,intent:"send_message",args:{recipient:"contract-recipient",body:"private contract message"},risk_level:"low",needs_confirmation:false,idempotency_key:$key,confidence:0.99,locale:"en-HK",timezone:"Asia/Hong_Kong"}')"
confirmation_first="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/commands" \
  -d "$confirmation_body")"
test "$(jq -r '.state' <<<"$confirmation_first")" = "awaiting_confirmation"
confirmation_token_one="$(jq -r '.confirmation_token' <<<"$confirmation_first")"
test -n "$confirmation_token_one" && test "$confirmation_token_one" != "null"
confirmation_replay="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/commands" \
  -d "$confirmation_body")"
test "$(jq -r '.command_id' <<<"$confirmation_replay")" = "$confirmation_id"
confirmation_token_two="$(jq -r '.confirmation_token' <<<"$confirmation_replay")"
test -n "$confirmation_token_two" && test "$confirmation_token_two" != "null"
test "$confirmation_token_one" != "$confirmation_token_two"
stale_confirmation_status="$(curl --silent --show-error -o /dev/null -w '%{http_code}' \
  "${user_auth[@]}" -H 'content-type: application/json' \
  -X POST "$BASE_URL/v1/phone/commands/$confirmation_id/confirm" \
  -d "$(jq -nc --arg token "$confirmation_token_one" '{confirmation_token:$token}')")"
test "$stale_confirmation_status" = "409"
confirmation_result="$(json "${user_auth[@]}" \
  -X POST "$BASE_URL/v1/phone/commands/$confirmation_id/confirm" \
  -d "$(jq -nc --arg token "$confirmation_token_two" '{confirmation_token:$token}')")"
message_enabled="$(jq -r '.action_message_enabled' <<<"$health")"
provider_ready="$(jq -r '.action_provider_ready' <<<"$health")"
if [[ "$message_enabled" == "true" ]]; then
  jq -e '
    (.state == "succeeded") and
    (.state != "queued") and
    (.presentation.terminal == true)
  ' <<<"$confirmation_result" >/dev/null
else
  jq -e '
    (.state == "failed") and
    (.state != "queued") and
    (.error.code == "action_disabled") and
    (.error.retryable == false) and
    (.presentation.terminal == true)
  ' <<<"$confirmation_result" >/dev/null
fi
confirmation_detail="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/commands/$confirmation_id")"
test "$(jq -r '.state' <<<"$confirmation_detail")" = "$(jq -r '.state' <<<"$confirmation_result")"
test "$(jq -r '.state' <<<"$confirmation_detail")" != "queued"
if [[ "$message_enabled" != "true" ]]; then
  test "$(jq -r '.error.code' <<<"$confirmation_detail")" = "action_disabled"
  health_after_disabled_send="$(get "$BASE_URL/health")"
  test "$(jq -r '.action_message_enabled' <<<"$health_after_disabled_send")" = "false"
  if [[ "$provider_ready" == "false" ]]; then
    test "$(jq -r '.action_provider_ready' <<<"$health_after_disabled_send")" = "false"
  fi
fi

command_two="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/commands" \
  -d "$(jq -nc --arg key "command-smoke-two-$(date +%s%N)" \
    '{schema_version:1,command_id:("cmd-smoke-two-" + ($key | split("-") | last)),intent:"search_history",args:{q:"second"},risk_level:"low",needs_confirmation:false,idempotency_key:$key,confidence:0.95,locale:"zh-Hans-HK",timezone:"Asia/Hong_Kong"}')")"
command_two_id="$(jq -r '.command_id' <<<"$command_two")"
test "$(jq -r '.state' <<<"$command_two")" = "succeeded"
sleep 1
command_three="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/commands" \
  -d "$(jq -nc --arg key "command-smoke-three-$(date +%s%N)" \
    '{schema_version:1,command_id:("cmd-smoke-three-" + ($key | split("-") | last)),intent:"search_history",args:{q:"third"},risk_level:"low",needs_confirmation:false,idempotency_key:$key,confidence:0.95,locale:"zh-Hans-HK",timezone:"Asia/Hong_Kong"}')")"
command_three_id="$(jq -r '.command_id' <<<"$command_three")"
test "$(jq -r '.state' <<<"$command_three")" = "succeeded"
command_page_one="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/commands?state=succeeded&limit=1")"
command_page_one_cursor="$(jq -r '.next_cursor' <<<"$command_page_one")"
command_page_one_id="$(jq -r '.commands[0].command_id' <<<"$command_page_one")"
test -n "$command_page_one_cursor" && test "$command_page_one_cursor" != "null"
test "$(jq -r '.commands | length' <<<"$command_page_one")" = "1"
test "$(jq -r --arg id "$command_three_id" '[.commands[] | select(.command_id == $id)] | length' <<<"$command_page_one")" = "1"
command_page_two="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/commands?state=succeeded&limit=1&before=$(jq -rn --arg value "$command_page_one_cursor" '$value | @uri')")"
test "$(jq -r '.commands | length' <<<"$command_page_two")" = "1"
test "$(jq -r --arg id "$command_three_id" '[.commands[] | select(.command_id == $id)] | length' <<<"$command_page_two")" = "0"
test "$(jq -r '.commands[0].command_id' <<<"$command_page_two")" != "$command_page_one_id"
test "$(jq -r --arg id "$command_two_id" '[.commands[] | select(.command_id == $id)] | length' <<<"$command_page_two")" = "1"

pairing="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/pairing/code" \
  -d '{"ttl_sec":600}')"
pairing_code="$(jq -r '.code' <<<"$pairing")"
test -n "$pairing_code" && test "$pairing_code" != "null"
pairing_status="$(get "${user_auth[@]}" "$BASE_URL/v1/pairing/code/$pairing_code")"
test "$(jq -r '.status' <<<"$pairing_status")" = "waiting"
cross_user_pairing_status="$(curl --silent --show-error -o /dev/null -w '%{http_code}' \
  "${other_auth[@]}" "$BASE_URL/v1/pairing/code/$pairing_code")"
test "$cross_user_pairing_status" = "404"
paired="$(json -X POST "$BASE_URL/v1/pairing/claim" \
  -d "$(jq -nc --arg code "$pairing_code" \
    '{code:$code,label:"paired-smoke",host_label:"local"}')")"
test -n "$(jq -r '.api_key' <<<"$paired")"
pairing_status_claimed="$(get "${user_auth[@]}" "$BASE_URL/v1/pairing/code/$pairing_code")"
test "$(jq -r '.status' <<<"$pairing_status_claimed")" = "claimed"
second_claim_status="$(curl --silent --show-error -o /dev/null -w '%{http_code}' \
  -H 'content-type: application/json' -X POST "$BASE_URL/v1/pairing/claim" \
  -d "$(jq -nc --arg code "$pairing_code" \
    '{code:$code,label:"paired-again",host_label:"local"}')")"
test "$second_claim_status" = "409"

skills="$(get "${agent_auth[@]}" "$BASE_URL/v1/skills")"
test "$(jq -r '.skills | length' <<<"$skills")" -ge 1

chat_id="rust-contract-chat-$(date +%s%N)"
session="$(json "${agent_auth[@]}" -X POST "$BASE_URL/v1/sessions" \
  -d "$(jq -nc --arg key "rust-contract-$(date +%s%N)" --arg chat "$chat_id" \
    '{skill_id:"deploy.result",idempotency_key:$key,title:"Rust contract smoke",chat_id:$chat,facts:{service:"knock-knock",env:"local"}}')")"
session_id="$(jq -r '.session_id' <<<"$session")"
test -n "$session_id" && test "$session_id" != "null"
test "$(jq -r '.chat_id' <<<"$session")" = "$chat_id"

progress_unknown="$(json "${agent_auth[@]}" -X POST "$BASE_URL/v1/sessions/$session_id/progress" \
  -d '{"status":"running","message":"Rust contract smoke — estimate unknown"}')"
test "$(jq -r '.progress_percent' <<<"$progress_unknown")" = "null"
progress="$(json "${agent_auth[@]}" -X POST "$BASE_URL/v1/sessions/$session_id/progress" \
  -d '{"status":"running","message":"Rust contract smoke — measured milestone","percent":25}')"
test "$(jq -r '.progress_status' <<<"$progress")" = "running"
test "$(jq -r '.progress_percent == 25' <<<"$progress")" = "true"

session_view="$(get "${user_auth[@]}" "$BASE_URL/v1/sessions/$session_id")"
test "$(jq -r '.session_id' <<<"$session_view")" = "$session_id"

event="$(json "${agent_auth[@]}" -X POST "$BASE_URL/v1/sessions/$session_id/events" \
  -d "$(jq -nc --arg key "needs-user-$(date +%s%N)" \
    '{status:"needs_user",idempotency_key:$key,facts:{status:"waiting"},actions:[{id:"rollback",risk:"destructive",confirm:true,title:"Rollback deployment",payload:{scope:"service"}},{id:"ack",risk:"low",confirm:false,title:"Acknowledge"}],retrievals:[range(0;51) | {title:("export source " + tostring),url:("https://example.com/export/" + tostring),snippet:"export fixture",content_hash:($key + "-" + tostring)}]}')")"
test "$(jq -r '.session.state' <<<"$event")" = "needs_user"
test "$(jq -r '.pushed' <<<"$event")" = "true"
test "$(jq -r '[.session.available_action_descriptors[] | select(.action_key == "rollback" and .risk == "destructive" and .confirm_required == true and (.title | type == "string") and (.title | length > 0) and .payload.scope == "service")] | length' <<<"$event")" = "1"

exported_session="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/sessions/$session_id/export")"
test "$(jq -r '.retrieval_items | length' <<<"$exported_session")" = "51"
test "$(jq -r '.truncated' <<<"$exported_session")" = "false"

offered="$(get "${agent_auth[@]}" "$BASE_URL/v1/sessions/$session_id/actions/pending?claim=false")"
test "$(jq -r '.actions | length' <<<"$offered")" = "0"

phone="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/sessions")"
test "$(jq -r '.sessions | length' <<<"$phone")" -ge 1
test "$(jq -r --arg id "$session_id" '[.sessions[] | select(.session_id == $id)] | length' <<<"$phone")" = "1"

reply="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/sessions/$session_id/reply" \
  -d '{"action_key":"rollback","utterance":"确认回滚"}')"
test "$(jq -r '.needs_confirm' <<<"$reply")" = "true"
action_id="$(jq -r '.action.action_id' <<<"$reply")"

confirm="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/sessions/$session_id/confirm" \
  -d "$(jq -nc --arg action_id "$action_id" '{action_id:$action_id,confirm:true}')")"
test "$(jq -r '.action.status' <<<"$confirm")" = "queued"

pending="$(get "${agent_auth[@]}" "$BASE_URL/v1/sessions/$session_id/actions/pending?claim=true")"
claimed="$(jq -r '.actions[0].status' <<<"$pending")"
test "$claimed" = "claimed"

result="$(json "${agent_auth[@]}" -X POST "$BASE_URL/v1/actions/$action_id/result" \
  -d '{"ok":true,"message":"done","output":{"smoke":true}}')"
test "$(jq -r '.status' <<<"$result")" = "done"

resumed="$(json "${agent_auth[@]}" -X POST "$BASE_URL/v1/sessions" \
  -d "$(jq -nc --arg session "$session_id" --arg chat "$chat_id" \
    '{skill_id:"deploy.result",session_id:$session,chat_id:$chat,title:"Rust contract smoke"}')")"
test "$(jq -r '.session_id' <<<"$resumed")" = "$session_id"
test "$(jq -r '.chat_id' <<<"$resumed")" = "$chat_id"

event_two="$(json "${agent_auth[@]}" -X POST "$BASE_URL/v1/sessions/$session_id/events" \
  -d "$(jq -nc --arg key "needs-user-two-$(date +%s%N)" \
    '{status:"needs_user",idempotency_key:$key,facts:{status:"follow-up"},actions:["ack"]}')")"
test "$(jq -r '.session.session_id' <<<"$event_two")" = "$session_id"
test "$(jq -r '.pushed' <<<"$event_two")" = "true"

reply_two="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/sessions/$session_id/reply" \
  -d '{"action_key":"ack","utterance":"确认第二轮"}')"
test "$(jq -r '.needs_confirm' <<<"$reply_two")" = "false"
test "$(jq -r '.session.session_id' <<<"$reply_two")" = "$session_id"
action_two_id="$(jq -r '.action.action_id' <<<"$reply_two")"
test -n "$action_two_id" && test "$action_two_id" != "null"

pending_two="$(get "${agent_auth[@]}" "$BASE_URL/v1/sessions/$session_id/actions/pending?claim=true")"
test "$(jq -r '.actions | length' <<<"$pending_two")" = "1"
test "$(jq -r '.actions[0].action_id' <<<"$pending_two")" = "$action_two_id"
test "$(jq -r '.actions[0].status' <<<"$pending_two")" = "claimed"

result_two="$(json "${agent_auth[@]}" -X POST "$BASE_URL/v1/actions/$action_two_id/result" \
  -d '{"ok":true,"message":"follow-up done","output":{"smoke":true,"turn":2}}')"
test "$(jq -r '.status' <<<"$result_two")" = "done"

final_session="$(get "${agent_auth[@]}" "$BASE_URL/v1/sessions/$session_id")"
test "$(jq -r '.session_id' <<<"$final_session")" = "$session_id"
test "$(jq -r '.chat_id' <<<"$final_session")" = "$chat_id"
test "$(jq -r '.state' <<<"$final_session")" = "running"

sync_page="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/sync?limit=50")"
jq -e '
  (.cursor | type == "string" and length > 0) and
  (.changes | type == "array") and
  (.has_more | type == "boolean")
' <<<"$sync_page" >/dev/null
sync_cursor="$(jq -r '.cursor' <<<"$sync_page")"
sync_after="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/sync?limit=50&after=$(jq -rn --arg value "$sync_cursor" '$value | @uri')")"
jq -e '(.cursor | type == "string" and length > 0) and (.changes | type == "array")' <<<"$sync_after" >/dev/null

pushes="$(get "${user_auth[@]}" "$BASE_URL/v1/dev/pushes")"
test "$(jq -r '.pushes | length' <<<"$pushes")" -ge 1
push_id="$(jq -r '.pushes[0].push_id' <<<"$pushes")"
read_push="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/pushes/$push_id/read")"
read_at="$(jq -r '.read_at' <<<"$read_push")"
test -n "$read_at" && test "$read_at" != "null"
cross_user_push_status="$(curl --silent --show-error -o /dev/null -w '%{http_code}' \
  "${other_auth[@]}" -X POST "$BASE_URL/v1/phone/pushes/$push_id/dismiss")"
test "$cross_user_push_status" = "404"
dismissed="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/pushes/$push_id/dismiss")"
test "$(jq -r '.push_id' <<<"$dismissed")" = "$push_id"
test "$(jq -r '.dismissed_at != null' <<<"$dismissed")" = "true"
test "$(jq -r --arg read_at "$read_at" '.read_at == $read_at' <<<"$dismissed")" = "true"

expired_pairing="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/pairing/code" \
  -d '{"ttl_sec":1}')"
expired_pairing_code="$(jq -r '.code' <<<"$expired_pairing")"
sleep 2
expired_pairing_status="$(get "${user_auth[@]}" "$BASE_URL/v1/pairing/code/$expired_pairing_code")"
test "$(jq -r '.status' <<<"$expired_pairing_status")" = "expired"

history="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/sessions/$session_id/history")"
test "$(jq -r '.entries | length' <<<"$history")" -ge 2

rotated="$(json -X POST "$BASE_URL/v1/auth/refresh" \
  -d "$(jq -nc --arg refresh "$refresh" '{refresh_token:$refresh}')")"
rotated_token="$(jq -r '.token' <<<"$rotated")"
rotated_refresh="$(jq -r '.refresh_token' <<<"$rotated")"
test -n "$rotated_token"
test -n "$rotated_refresh"
logout="$(json -X POST "$BASE_URL/v1/auth/logout" \
  -d "$(jq -nc --arg refresh "$rotated_refresh" '{refresh_token:$refresh}')")"
test "$(jq -r '.ok' <<<"$logout")" = "true"

BASE_URL="$BASE_URL" "${ROOT_DIR}/scripts/rate-limit-smoke.sh"

printf '%s\n' 'rust contract smoke passed: health/auth/agent/skill/session/chat/multi-turn/phone/export-pagination/command-isolation-and-idempotency/command-pagination/pairing-isolation-and-expiry/push-isolation-and-dismissal/action-descriptors/confirm/claim/result/refresh'
