#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" storage >/dev/null

BASE_URL="${BASE_URL:-http://127.0.0.1:8787}"
BASE_URL="${BASE_URL%/}"
AUTH_MODE="${SMOKE_AUTH_MODE:-register}"
BUCKET="${R2_SMOKE_BUCKET:-knock-knock-local}"
PERSIST_TO="${R2_SMOKE_PERSIST_TO:-.wrangler/state}"
WRANGLER_CONFIG="${R2_SMOKE_WRANGLER_CONFIG:-${ROOT_DIR}/wrangler.toml}"
REMOTE="${R2_SMOKE_REMOTE:-false}"
REMOTE_RETENTION_TIMEOUT_SEC="${R2_SMOKE_REMOTE_RETENTION_TIMEOUT_SEC:-120}"
REMOTE_RETENTION_POLL_SEC="${R2_SMOKE_REMOTE_RETENTION_POLL_SEC:-5}"
PASSWORD="${SMOKE_PASSWORD:-password123}"
EMAIL="${SMOKE_EMAIL:-r2-download-$(date +%s)-$$@local.test}"
if [[ "${AUTH_MODE}" == "login" ]]; then
  : "${SMOKE_EMAIL:?SMOKE_EMAIL is required when SMOKE_AUTH_MODE=login}"
  : "${SMOKE_PASSWORD:?SMOKE_PASSWORD is required when SMOKE_AUTH_MODE=login}"
  OTHER_EMAIL="${SMOKE_OTHER_EMAIL:?SMOKE_OTHER_EMAIL is required when SMOKE_AUTH_MODE=login}"
  OTHER_PASSWORD="${SMOKE_OTHER_PASSWORD:?SMOKE_OTHER_PASSWORD is required when SMOKE_AUTH_MODE=login}"
elif [[ "${AUTH_MODE}" == "register" ]]; then
  OTHER_EMAIL="r2-download-other-$(date +%s)-$$@local.test"
  OTHER_PASSWORD="${PASSWORD}"
else
  echo "SMOKE_AUTH_MODE must be register or login" >&2
  exit 64
fi
FIXTURE="${ROOT_DIR}/scripts/fixtures/retrieval-download.txt"
KEY=""
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

if [[ "$REMOTE" == "true" ]]; then
  STORAGE_ARGS=(--remote)
else
  STORAGE_ARGS=(--local --persist-to "$PERSIST_TO")
fi

json() {
  curl --fail-with-body --silent --show-error \
    -H 'content-type: application/json' "$@"
}

auth_user() {
  local email="$1"
  local password="$2"
  local endpoint="register"
  if [[ "${AUTH_MODE}" == "login" ]]; then
    endpoint="login"
  fi
  json -X POST "${BASE_URL}/v1/auth/${endpoint}" \
    -d "$(jq -nc --arg email "${email}" --arg password "${password}" \
      '{email:$email,password:$password}')"
}

wait_for_remote_expired_download() {
  local download_path="$1"
  local deadline=$((SECONDS + REMOTE_RETENTION_TIMEOUT_SEC))
  local response_status

  while (( SECONDS < deadline )); do
    response_status="$(curl --silent --show-error -o /dev/null -w '%{http_code}' \
      "${user_auth[@]}" "${BASE_URL}${download_path}")"
    if [[ "$response_status" == "404" ]]; then
      return 0
    fi
    sleep "$REMOTE_RETENTION_POLL_SEC"
  done

  echo "remote retention did not expire ${download_path} within ${REMOTE_RETENTION_TIMEOUT_SEC}s" >&2
  return 1
}

wait_for_remote_r2_object_deleted() {
  local object_key="$1"
  local output_path="$2"
  local deadline=$((SECONDS + REMOTE_RETENTION_TIMEOUT_SEC))

  while (( SECONDS < deadline )); do
    if ! wrangler r2 object get "${BUCKET}/${object_key}" \
      --config "${WRANGLER_CONFIG}" \
      "${STORAGE_ARGS[@]}" \
      --file "$output_path" >/dev/null 2>&1; then
      return 0
    fi
    sleep "$REMOTE_RETENTION_POLL_SEC"
  done

  echo "remote retention did not delete ${object_key} within ${REMOTE_RETENTION_TIMEOUT_SEC}s" >&2
  return 1
}

run_scheduled_sweep() {
  if [[ "$REMOTE" == "true" ]]; then
    # Deployed Workers receive scheduled events from the Cloudflare cron
    # trigger. The local /__scheduled test route is unavailable remotely.
    return 0
  fi
  curl --fail-with-body --silent --show-error "${BASE_URL}/__scheduled" >/dev/null
}

auth="$(auth_user "${EMAIL}" "${PASSWORD}")"
token="$(jq -r '.token' <<<"${auth}")"
user_id="$(jq -r '.user_id' <<<"${auth}")"
user_auth=(-H "authorization: Bearer ${token}")
KEY="users/${user_id}/retrievals/r2-smoke-${RANDOM}-$$.txt"

wrangler r2 object put "${BUCKET}/${KEY}" \
  --config "${WRANGLER_CONFIG}" \
  "${STORAGE_ARGS[@]}" \
  --file "${FIXTURE}" \
  --content-type text/plain \
  --force >/dev/null

agent="$(json "${user_auth[@]}" -X POST "${BASE_URL}/v1/agents" \
  -d '{"label":"r2-download-smoke"}')"
agent_key="$(jq -r '.api_key' <<<"${agent}")"

session="$(json -H "x-agent-key: ${agent_key}" -X POST \
  "${BASE_URL}/v1/sessions" -d '{"skill_id":"deploy.result","title":"R2 download smoke"}')"
session_id="$(jq -r '.session_id' <<<"${session}")"
event="$(json -H "x-agent-key: ${agent_key}" -X POST \
  "${BASE_URL}/v1/sessions/${session_id}/events" \
  -d "$(jq -nc --arg key "r2-download-event-$(date +%s%N)" --arg r2_key "${KEY}" \
    '{status:"info",idempotency_key:$key,retrievals:[{title:"R2 smoke source",url:"https://example.com/r2-smoke",snippet:"private fixture",content_hash:$key,r2_key:$r2_key}]}')")"

detail="$(curl --fail-with-body --silent --show-error "${user_auth[@]}" \
  "${BASE_URL}/v1/phone/sessions/${session_id}")"
retrieval_id="$(jq -r '.retrieval_items[0].retrieval_id' <<<"${detail}")"
download_path="$(jq -r '.retrieval_items[0].download_path' <<<"${detail}")"
test "${download_path}" = "/v1/phone/retrievals/${retrieval_id}/download"
test "$(jq -r '.retrieval_items[0] | has("r2_key")' <<<"${detail}")" = "false"

curl --fail-with-body --silent --show-error "${user_auth[@]}" \
  -D "${TMP_DIR}/headers" -o "${TMP_DIR}/body" \
  "${BASE_URL}${download_path}"
cmp "${FIXTURE}" "${TMP_DIR}/body"
grep -qi '^cache-control: private, no-store' "${TMP_DIR}/headers"
grep -qi '^content-disposition: attachment; filename="retrieval.bin"' "${TMP_DIR}/headers"
grep -qi '^x-content-type-options: nosniff' "${TMP_DIR}/headers"

shared_hash="r2-shared-$(date +%s%N)"
json -H "x-agent-key: ${agent_key}" -X POST \
  "${BASE_URL}/v1/sessions/${session_id}/events" \
  -d "$(jq -nc --arg key "r2-shared-event-$(date +%s%N)" --arg hash "${shared_hash}" --arg r2_key "${KEY}" \
    '{status:"info",idempotency_key:$key,retrievals:[{title:"R2 shared source",url:"https://example.com/r2-shared",snippet:"shared fixture",content_hash:$hash,r2_key:$r2_key}]}')" >/dev/null
detail_with_shared="$(curl --fail-with-body --silent --show-error "${user_auth[@]}" \
  "${BASE_URL}/v1/phone/sessions/${session_id}")"
shared_retrieval_id="$(jq -r --arg hash "${shared_hash}" '.retrieval_items[] | select(.content_hash == $hash) | .retrieval_id' <<<"${detail_with_shared}")"
test -n "${shared_retrieval_id}" && test "${shared_retrieval_id}" != "null"

other_auth="$(auth_user "${OTHER_EMAIL}" "${OTHER_PASSWORD}")"
other_token="$(jq -r '.token' <<<"${other_auth}")"
other_status="$(curl --silent --show-error -o /dev/null -w '%{http_code}' \
  -H "authorization: Bearer ${other_token}" "${BASE_URL}${download_path}")"
test "${other_status}" = "404"

wrangler d1 execute DB \
  --config "${WRANGLER_CONFIG}" \
  "${STORAGE_ARGS[@]}" \
  --command "UPDATE retrieval_items SET retention_expires_at = '2000-01-01T00:00:00.000Z' WHERE id = '${retrieval_id}'" \
  >/dev/null
run_scheduled_sweep
if [[ "$REMOTE" == "true" ]]; then
  wait_for_remote_expired_download "$download_path"
else
  expired_status="$(curl --silent --show-error -o /dev/null -w '%{http_code}' \
    "${user_auth[@]}" "${BASE_URL}${download_path}")"
  test "${expired_status}" = "404"
fi

# The second retrieval still references the same immutable object, so the
# first row's retention sweep must not delete it prematurely.
curl --fail-with-body --silent --show-error "${user_auth[@]}" \
  -o "${TMP_DIR}/shared-body" \
  "${BASE_URL}/v1/phone/retrievals/${shared_retrieval_id}/download"
cmp "${FIXTURE}" "${TMP_DIR}/shared-body"
if ! wrangler r2 object get "${BUCKET}/${KEY}" \
  --config "${WRANGLER_CONFIG}" \
  "${STORAGE_ARGS[@]}" \
  --file "${TMP_DIR}/still-referenced" >/dev/null 2>&1; then
  echo 'shared R2 retrieval object was deleted too early' >&2
  exit 1
fi

wrangler d1 execute DB \
  --config "${WRANGLER_CONFIG}" \
  "${STORAGE_ARGS[@]}" \
  --command "UPDATE retrieval_items SET retention_expires_at = '2000-01-01T00:00:00.000Z' WHERE id = '${shared_retrieval_id}'" \
  >/dev/null
run_scheduled_sweep
shared_download_path="/v1/phone/retrievals/${shared_retrieval_id}/download"
if [[ "$REMOTE" == "true" ]]; then
  wait_for_remote_expired_download "$shared_download_path"
else
  shared_expired_status="$(curl --silent --show-error -o /dev/null -w '%{http_code}' \
    "${user_auth[@]}" "${BASE_URL}${shared_download_path}")"
  test "${shared_expired_status}" = "404"
fi
if [[ "$REMOTE" == "true" ]]; then
  wait_for_remote_r2_object_deleted "${KEY}" "${TMP_DIR}/deleted"
elif wrangler r2 object get "${BUCKET}/${KEY}" \
    --config "${WRANGLER_CONFIG}" \
    "${STORAGE_ARGS[@]}" \
    --file "${TMP_DIR}/deleted" >/dev/null 2>&1; then
  echo 'expired R2 retrieval object was not deleted' >&2
  exit 1
fi

printf '%s\n' 'r2 download smoke passed: R2 stream, metadata, user namespace, shared-key retention, and cross-user isolation'
