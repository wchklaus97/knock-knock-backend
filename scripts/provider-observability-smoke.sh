#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${BASE_URL:-http://127.0.0.1:8787}"
BASE_URL="${BASE_URL%/}"

get() {
  curl --fail-with-body --silent --show-error "$@"
}

metrics="$(get "${BASE_URL}/metrics")"
grep -q 'knock_knock_api_info{runtime="cloudflare-worker",api="rust"} 1' <<<"${metrics}"
grep -q 'knock_knock_provider_ready ' <<<"${metrics}"
grep -q 'knock_knock_apns_ready ' <<<"${metrics}"
grep -q 'knock_knock_model_enabled ' <<<"${metrics}"

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

echo "provider-observability-smoke passed: readiness gauges, request correlation validation, and generated fallback request IDs"
