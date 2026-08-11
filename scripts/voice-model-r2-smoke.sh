#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" storage >/dev/null

BASE_URL="${BASE_URL:-http://127.0.0.1:8787}"
BASE_URL="${BASE_URL%/}"
MODEL_ID="${MODEL_ID:-gemma-command}"
EXPECTED_ARTIFACT="${EXPECTED_MODEL_ARTIFACT:?EXPECTED_MODEL_ARTIFACT is required}"
AUTH_MODE="${SMOKE_AUTH_MODE:-register}"
EMAIL="${SMOKE_EMAIL:-voice-model-$(date +%s)-$$@local.test}"
PASSWORD="${SMOKE_PASSWORD:-password123}"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf -- "$TMP_DIR"' EXIT

if [[ ! -s "$EXPECTED_ARTIFACT" ]]; then
  echo "expected model artifact is missing" >&2
  exit 64
fi

json() {
  curl --fail-with-body --silent --show-error \
    -H 'content-type: application/json' "$@"
}

case "$AUTH_MODE" in
  register|login) ;;
  *) echo "SMOKE_AUTH_MODE must be register or login" >&2; exit 64 ;;
esac
auth="$(json -X POST "${BASE_URL}/v1/auth/${AUTH_MODE}" \
  -d "$(jq -nc --arg email "$EMAIL" --arg password "$PASSWORD" \
    '{email:$email,password:$password}')")"
token="$(jq -r '.token' <<<"$auth")"
test -n "$token" && test "$token" != "null"
authorization=(-H "authorization: Bearer ${token}")

descriptor="$(json "${authorization[@]}" "${BASE_URL}/v1/phone/models/${MODEL_ID}")"
download_url="$(jq -r '.download_url' <<<"$descriptor")"
test "$download_url" = "${BASE_URL}/v1/phone/models/${MODEL_ID}/artifact"
test "$(jq -r '.model_id' <<<"$descriptor")" = "$MODEL_ID"
test "$(jq -r '.manifest.model_id' <<<"$descriptor")" = "$MODEL_ID"
test "$(jq -r '.manifest.size_bytes' <<<"$descriptor")" = "$(wc -c < "$EXPECTED_ARTIFACT" | tr -d ' ')"
if grep -q 'VOICE_MODEL_R2_KEY\|local-contract-1\.0\.0\.litertlm' <<<"$descriptor"; then
  echo "model descriptor exposed its private R2 key" >&2
  exit 1
fi

unauthorized_status="$(curl --silent --show-error -o /dev/null -w '%{http_code}' "$download_url")"
test "$unauthorized_status" = "401"

curl --fail-with-body --silent --show-error "${authorization[@]}" \
  -D "$TMP_DIR/headers" -o "$TMP_DIR/model.litertlm" "$download_url"
cmp "$EXPECTED_ARTIFACT" "$TMP_DIR/model.litertlm"
grep -qi '^cache-control: private, no-store' "$TMP_DIR/headers"
grep -qi '^content-type: application/octet-stream' "$TMP_DIR/headers"
grep -qi "^content-disposition: attachment; filename=\"${MODEL_ID}\.litertlm\"" "$TMP_DIR/headers"
grep -qi '^x-content-type-options: nosniff' "$TMP_DIR/headers"

wrong_model_status="$(curl --silent --show-error -o /dev/null -w '%{http_code}' \
  "${authorization[@]}" "${BASE_URL}/v1/phone/models/other-model")"
test "$wrong_model_status" = "503"

echo "voice model R2 smoke passed: authenticated descriptor, private-key isolation, streamed bytes, and response hardening"
