#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" health >/dev/null

BASE_URL="${BASE_URL:-http://127.0.0.1:8787}"
BASE_URL="${BASE_URL%/}"
REQUEST_ID_PREFIX="rate-limit-smoke-$(date +%s)-$$"
limited_body=""
limited_status=""
limited_headers=""

for attempt in $(seq 1 12); do
  response_file="$(mktemp)"
  header_file="$(mktemp)"
  trap 'rm -f "${response_file:-}" "${header_file:-}"' EXIT
  if ! status="$(curl --silent --show-error \
    -H 'content-type: application/json' \
    -H "x-request-id: ${REQUEST_ID_PREFIX}-${attempt}" \
    -D "${header_file}" \
    -o "${response_file}" \
    -X POST "${BASE_URL}/v1/pairing/claim" \
    -d '{"code":"rate-limit-smoke-invalid"}' \
    -w '%{http_code}')"; then
    echo "rate-limit smoke failed: pairing request had a transport failure" >&2
    exit 1
  fi
  if [[ "${status}" == "429" ]]; then
    limited_body="$(sed -n '1,$p' "${response_file}")"
    limited_status="${status}"
    limited_headers="$(sed -n '1,$p' "${header_file}")"
    rm -f "${response_file}" "${header_file}"
    break
  fi
  rm -f "${response_file}" "${header_file}"
done

if [[ "${limited_status}" != "429" ]]; then
  echo "rate-limit smoke failed: pairing bucket did not return HTTP 429" >&2
  exit 1
fi

jq -e '
  (.error.code == "rate_limited") and
  (.error.message | type == "string" and length > 0) and
  (.error.retryable == true) and
  (.error.retry_after == 60) and
  (.error.request_id | type == "string" and length > 0)
' <<<"${limited_body}" >/dev/null
grep -Eq '^Retry-After:[[:space:]]*60' <<<"${limited_headers}"

echo "rate-limit smoke passed: unauthenticated pairing bucket returns a structured 429 with retry metadata and request correlation"
