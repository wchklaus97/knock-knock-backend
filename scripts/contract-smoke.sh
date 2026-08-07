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
metrics="$(get "$BASE_URL/metrics")"
grep -q 'knock_knock_api_info' <<<"$metrics"

auth="$(json -X POST "$BASE_URL/v1/auth/register" \
  -d "$(jq -nc --arg email "$EMAIL" --arg password "$PASSWORD" '{email:$email,password:$password}')")"
token="$(jq -r '.token' <<<"$auth")"
refresh="$(jq -r '.refresh_token' <<<"$auth")"
test -n "$token" && test "$token" != "null"
test -n "$refresh" && test "$refresh" != "null"

user_auth=(-H "authorization: Bearer $token")
login="$(json -X POST "$BASE_URL/v1/auth/login" \
  -d "$(jq -nc --arg email "$EMAIL" --arg password "$PASSWORD" '{email:$email,password:$password}')")"
test -n "$(jq -r '.token' <<<"$login")"
agent="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/agents" \
  -d '{"label":"rust-contract-smoke","host_label":"local"}')"
agent_key="$(jq -r '.api_key' <<<"$agent")"
test -n "$agent_key" && test "$agent_key" != "null"
agent_auth=(-H "x-agent-key: $agent_key")
agents="$(get "${user_auth[@]}" "$BASE_URL/v1/agents")"
test "$(jq -r '.agents | length' <<<"$agents")" -ge 1

device="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/phone/devices" \
  -d '{"platform":"ios","locale":"zh-HK"}')"
test "$(jq -r '.platform' <<<"$device")" = "ios"

pairing="$(json "${user_auth[@]}" -X POST "$BASE_URL/v1/pairing/code" \
  -d '{"ttl_sec":600}')"
pairing_code="$(jq -r '.code' <<<"$pairing")"
test -n "$pairing_code" && test "$pairing_code" != "null"
paired="$(json -X POST "$BASE_URL/v1/pairing/claim" \
  -d "$(jq -nc --arg code "$pairing_code" \
    '{code:$code,label:"paired-smoke",host_label:"local"}')")"
test -n "$(jq -r '.api_key' <<<"$paired")"
second_claim_status="$(curl --silent --show-error -o /dev/null -w '%{http_code}' \
  -H 'content-type: application/json' -X POST "$BASE_URL/v1/pairing/claim" \
  -d "$(jq -nc --arg code "$pairing_code" \
    '{code:$code,label:"paired-again",host_label:"local"}')")"
test "$second_claim_status" = "409"

skills="$(get "${agent_auth[@]}" "$BASE_URL/v1/skills")"
test "$(jq -r '.skills | length' <<<"$skills")" -ge 1

session="$(json "${agent_auth[@]}" -X POST "$BASE_URL/v1/sessions" \
  -d "$(jq -nc --arg key "rust-contract-$(date +%s%N)" \
    '{skill_id:"deploy.result",idempotency_key:$key,title:"Rust contract smoke",facts:{service:"knock-knock",env:"local"}}')")"
session_id="$(jq -r '.session_id' <<<"$session")"
test -n "$session_id" && test "$session_id" != "null"

progress="$(json "${agent_auth[@]}" -X POST "$BASE_URL/v1/sessions/$session_id/progress" \
  -d '{"status":"running","message":"Rust contract smoke","percent":25}')"
test "$(jq -r '.progress_status' <<<"$progress")" = "running"

session_view="$(get "${user_auth[@]}" "$BASE_URL/v1/sessions/$session_id")"
test "$(jq -r '.session_id' <<<"$session_view")" = "$session_id"

event="$(json "${agent_auth[@]}" -X POST "$BASE_URL/v1/sessions/$session_id/events" \
  -d "$(jq -nc --arg key "needs-user-$(date +%s%N)" \
    '{status:"needs_user",idempotency_key:$key,facts:{status:"waiting"},actions:["rollback","ack"]}')")"
test "$(jq -r '.session.state' <<<"$event")" = "needs_user"
test "$(jq -r '.pushed' <<<"$event")" = "true"

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

pushes="$(get "${user_auth[@]}" "$BASE_URL/v1/dev/pushes")"
test "$(jq -r '.pushes | length' <<<"$pushes")" -ge 1

history="$(get "${user_auth[@]}" "$BASE_URL/v1/phone/sessions/$session_id/history")"
test "$(jq -r '.entries | length' <<<"$history")" -ge 1

rotated="$(json -X POST "$BASE_URL/v1/auth/refresh" \
  -d "$(jq -nc --arg refresh "$refresh" '{refresh_token:$refresh}')")"
rotated_token="$(jq -r '.token' <<<"$rotated")"
rotated_refresh="$(jq -r '.refresh_token' <<<"$rotated")"
test -n "$rotated_token"
test -n "$rotated_refresh"
logout="$(json -X POST "$BASE_URL/v1/auth/logout" \
  -d "$(jq -nc --arg refresh "$rotated_refresh" '{refresh_token:$refresh}')")"
test "$(jq -r '.ok' <<<"$logout")" = "true"

printf '%s\n' 'rust contract smoke passed: health/auth/agent/skill/session/event/phone/confirm/claim/result/push/refresh'
