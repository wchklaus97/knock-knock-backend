#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${BASE_URL:-http://127.0.0.1:8787}"
BASE_URL="${BASE_URL%/}"
PASSWORD="${SMOKE_PASSWORD:-password123}"
EMAIL="${SMOKE_EMAIL:-rust-smoke-$(date +%s)-$$@local.test}"

json() {
  curl --fail-with-body --silent --show-error \
    -H 'content-type: application/json' "$@"
}

get() {
  curl --fail-with-body --silent --show-error "$@"
}

health="$(get "$BASE_URL/health")"
test "$(jq -r '.ok' <<<"$health")" = "true"
test "$(jq -r '.api' <<<"$health")" = "rust"
v1_health="$(get "$BASE_URL/v1/health")"
test "$(jq -r '.ok' <<<"$v1_health")" = "true"
test "$(jq -r '.api' <<<"$v1_health")" = "rust"
metrics="$(get "$BASE_URL/metrics")"
grep -q 'knock_knock_api_info' <<<"$metrics"
grep -q 'knock_knock_provider_ready' <<<"$metrics"
grep -q 'knock_knock_apns_ready' <<<"$metrics"
request_headers="$(curl --fail-with-body --silent --show-error \
  -H 'x-request-id: contract-smoke-correlation' \
  -D - -o /dev/null "$BASE_URL/health")"
grep -qi '^x-request-id: contract-smoke-correlation' <<<"$request_headers"

auth="$(json -X POST "$BASE_URL/v1/auth/register" \
  -d "$(jq -nc --arg email "$EMAIL" --arg password "$PASSWORD" '{email:$email,password:$password}')")"
token="$(jq -r '.token' <<<"$auth")"
refresh="$(jq -r '.refresh_token' <<<"$auth")"
test -n "$token" && test "$token" != "null"
test -n "$refresh" && test "$refresh" != "null"

user_auth=(-H "authorization: Bearer $token")
OTHER_EMAIL="rust-contract-other-$(date +%s)-$$@local.test"
other_auth_response="$(json -X POST "$BASE_URL/v1/auth/register" \
  -d "$(jq -nc --arg email "$OTHER_EMAIL" --arg password "$PASSWORD" '{email:$email,password:$password}')")"
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

device="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/devices" \
  -d '{"platform":"ios","locale":"zh-HK"}')"
test "$(jq -r '.platform' <<<"$device")" = "ios"

command="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/commands" \
  -d "$(jq -nc --arg key "command-smoke-$(date +%s%N)" \
    '{schema_version:1,command_id:("cmd-smoke-" + ($key | split("-") | last)),intent:"search_history",args:{q:"history"},risk_level:"low",needs_confirmation:false,idempotency_key:$key,confidence:0.95,locale:"zh-Hans-HK",timezone:"Asia/Hong_Kong"}')")"
command_id="$(jq -r '.command_id' <<<"$command")"
test -n "$command_id" && test "$command_id" != "null"
test "$(jq -r '.state' <<<"$command")" = "queued"
commands="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/commands?state=queued&limit=50")"
test "$(jq -r --arg id "$command_id" '[.commands[] | select(.command_id == $id)] | length' <<<"$commands")" = "1"

command_two="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/commands" \
  -d "$(jq -nc --arg key "command-smoke-two-$(date +%s%N)" \
    '{schema_version:1,command_id:("cmd-smoke-two-" + ($key | split("-") | last)),intent:"search_history",args:{q:"second"},risk_level:"low",needs_confirmation:false,idempotency_key:$key,confidence:0.95,locale:"zh-Hans-HK",timezone:"Asia/Hong_Kong"}')")"
command_two_id="$(jq -r '.command_id' <<<"$command_two")"
test "$(jq -r '.state' <<<"$command_two")" = "queued"
sleep 1
command_three="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/commands" \
  -d "$(jq -nc --arg key "command-smoke-three-$(date +%s%N)" \
    '{schema_version:1,command_id:("cmd-smoke-three-" + ($key | split("-") | last)),intent:"search_history",args:{q:"third"},risk_level:"low",needs_confirmation:false,idempotency_key:$key,confidence:0.95,locale:"zh-Hans-HK",timezone:"Asia/Hong_Kong"}')")"
command_three_id="$(jq -r '.command_id' <<<"$command_three")"
test "$(jq -r '.state' <<<"$command_three")" = "queued"
command_page_one="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/commands?state=queued&limit=1")"
command_page_one_cursor="$(jq -r '.next_cursor' <<<"$command_page_one")"
command_page_one_id="$(jq -r '.commands[0].command_id' <<<"$command_page_one")"
test -n "$command_page_one_cursor" && test "$command_page_one_cursor" != "null"
test "$(jq -r '.commands | length' <<<"$command_page_one")" = "1"
test "$(jq -r --arg id "$command_three_id" '[.commands[] | select(.command_id == $id)] | length' <<<"$command_page_one")" = "1"
command_page_two="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/commands?state=queued&limit=1&before=$(jq -rn --arg value "$command_page_one_cursor" '$value | @uri')")"
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

printf '%s\n' 'rust contract smoke passed: health/auth/agent/skill/session/chat/multi-turn/phone/export-pagination/command-pagination/pairing-isolation-and-expiry/push-isolation-and-dismissal/action-descriptors/confirm/claim/result/refresh'
