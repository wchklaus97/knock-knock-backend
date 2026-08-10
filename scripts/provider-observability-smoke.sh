#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" health >/dev/null

BASE_URL="${BASE_URL:-http://127.0.0.1:8787}"
BASE_URL="${BASE_URL%/}"
EXPECTED_PROVIDER_READY="${EXPECTED_PROVIDER_READY:-}"
EXPECTED_APNS_READY="${EXPECTED_APNS_READY:-}"
EXPECTED_APNS_PRODUCTION="${EXPECTED_APNS_PRODUCTION:-}"
EXPECTED_MODEL_ENABLED="${EXPECTED_MODEL_ENABLED:-}"
MODEL_ID="${MODEL_ID:-}"

get() {
  curl --fail-with-body --silent --show-error --max-time "${HTTP_TIMEOUT_SECONDS:-10}" "$@"
}

health="$(get "${BASE_URL}/health")"
jq -e '
  (.ok == true) and
  (.api == "rust") and
  (.runtime == "cloudflare-worker") and
  (.version | type == "string") and
  (.push_mode | type == "string") and
  (.apns_ready | type == "boolean") and
  (.apns_production | type == "boolean") and
  (.action_provider_ready | type == "boolean")
' <<<"${health}" >/dev/null

if [[ -n "${EXPECTED_PROVIDER_READY}" ]]; then
  jq -e --arg expected "${EXPECTED_PROVIDER_READY}" \
    '(.action_provider_ready | tostring) == $expected' <<<"${health}" >/dev/null
fi
if [[ -n "${EXPECTED_APNS_READY}" ]]; then
  jq -e --arg expected "${EXPECTED_APNS_READY}" \
    '(.apns_ready | tostring) == $expected' <<<"${health}" >/dev/null
fi
if [[ -n "${EXPECTED_APNS_PRODUCTION}" ]]; then
  jq -e --arg expected "${EXPECTED_APNS_PRODUCTION}" \
    '(.apns_production | tostring) == $expected' <<<"${health}" >/dev/null
fi

metrics="$(get "${BASE_URL}/metrics")"
grep -q 'knock_knock_api_info{runtime="cloudflare-worker",api="rust"} 1' <<<"${metrics}"
expected_provider_ready="$(jq -r 'if .action_provider_ready then 1 else 0 end' <<<"${health}")"
expected_apns_ready="$(jq -r 'if .apns_ready then 1 else 0 end' <<<"${health}")"
grep -Eq "knock_knock_provider_ready[[:space:]]+${expected_provider_ready}" <<<"${metrics}"
grep -Eq "knock_knock_apns_ready[[:space:]]+${expected_apns_ready}" <<<"${metrics}"
model_metric="$(awk '$1 == "knock_knock_model_enabled" {print $2; exit}' <<<"${metrics}")"
test "${model_metric}" = "0" || test "${model_metric}" = "1"
if [[ -n "${EXPECTED_MODEL_ENABLED}" ]]; then
  test "${model_metric}" = "${EXPECTED_MODEL_ENABLED}"
fi

if grep -Eiq 'Bearer[[:space:]]|Authorization:[[:space:]]|APNS_KEY|SUPABASE_.*KEY|JWT_SECRET|BEGIN .*PRIVATE KEY' \
  <<<"${health}\n${metrics}"; then
  echo "provider-observability-smoke failed: readiness output contains a secret-shaped value" >&2
  exit 1
fi

expected_request_id="provider-observability-correlation"
valid_headers="$(curl --fail-with-body --silent --show-error \
  -H "x-request-id: ${expected_request_id}" \
  -D - -o /dev/null "${BASE_URL}/health")"
valid_request_id="$(awk -F': ' 'tolower($1) == "x-request-id" {gsub(/\r/, "", $2); print $2; exit}' <<<"${valid_headers}")"
test "${valid_request_id}" = "${expected_request_id}"

invalid_headers="$(curl --fail-with-body --silent --show-error \
  -H 'x-request-id: invalid request id with spaces' \
  -D - -o /dev/null "${BASE_URL}/health")"
invalid_request_id="$(awk -F': ' 'tolower($1) == "x-request-id" {gsub(/\r/, "", $2); print $2; exit}' <<<"${invalid_headers}")"
test -n "${invalid_request_id}"
test "${invalid_request_id}" != "invalid request id with spaces"

if [[ -n "${MODEL_ID}" ]]; then
  model="$(get -H 'accept: application/json' "${BASE_URL}/v1/phone/models/${MODEL_ID}")"
  jq -e --arg model_id "${MODEL_ID}" \
    '(.model_id == $model_id) and (.manifest.model_id == $model_id) and (.download_url | startswith("https://"))' \
    <<<"${model}" >/dev/null
fi

echo "provider-observability-smoke passed: local readiness gauges, request-ID validation, and secret-safe metrics (no APNs delivery evidence)"
