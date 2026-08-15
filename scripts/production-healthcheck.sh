#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" health >/dev/null

API="${1:-https://knock-knock-backend-production.wch-klaus.workers.dev}"
API="${API%/}"
PROBE="$(date +%s)"
ATTEMPTS="${KNOCK_KNOCK_PRODUCTION_HEALTHCHECK_ATTEMPTS:-12}"
DELAY_SECONDS="${KNOCK_KNOCK_PRODUCTION_HEALTHCHECK_DELAY_SECONDS:-5}"

if [[ ! "$ATTEMPTS" =~ ^[1-9][0-9]*$ ]]; then
  echo "invalid production healthcheck attempt count: $ATTEMPTS" >&2
  exit 64
fi
if [[ ! "$DELAY_SECONDS" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
  echo "invalid production healthcheck delay: $DELAY_SECONDS" >&2
  exit 64
fi

health_contract_matches() {
  local payload="$1"
  if ! jq -e '
    (.ok == true) and
    (.api == "rust") and
    (.runtime == "cloudflare-worker") and
    (.push_mode == "apns" or .push_mode == "both") and
    (.apns_ready == true) and
    (.apns_production == true)
  ' <<<"$payload" >/dev/null; then
    return 1
  fi

  if [[ -n "${KNOCK_KNOCK_EXPECTED_PRODUCTION_VERSION:-}" ]] \
    && ! jq -e --arg expected "$KNOCK_KNOCK_EXPECTED_PRODUCTION_VERSION" \
      '.version == $expected' <<<"$payload" >/dev/null; then
    return 1
  fi
}

metrics_contract_matches() {
  local payload="$1"
  if ! grep -q 'knock_knock_api_info{runtime="cloudflare-worker",api="rust"} 1' <<<"$payload"; then
    return 1
  fi
  if [[ "${KNOCK_KNOCK_REQUIRE_RELEASE_READINESS_GAUGES:-0}" == "1" ]]; then
    grep -Eq 'knock_knock_provider_ready[[:space:]]+[01]' <<<"$payload" || return 1
    grep -Eq 'knock_knock_apns_ready[[:space:]]+1' <<<"$payload" || return 1
  fi
}

health=""
metrics=""
for attempt in $(seq 1 "$ATTEMPTS"); do
  health=""
  metrics=""
  if health="$(curl --fail-with-body --silent --show-error --connect-timeout 10 --max-time 20 "$API/health?probe=$PROBE-$attempt")" \
    && health_contract_matches "$health" \
    && metrics="$(curl --fail-with-body --silent --show-error --connect-timeout 10 --max-time 20 "$API/metrics?probe=$PROBE-$attempt")" \
    && metrics_contract_matches "$metrics"; then
    jq -c '{ok,api,runtime,version,push_mode,apns_ready,apns_production}' <<<"$health"
    echo "production healthcheck passed: $API (attempt $attempt/$ATTEMPTS)"
    exit 0
  fi

  if (( attempt < ATTEMPTS )); then
    echo "production health is not ready yet (attempt $attempt/$ATTEMPTS); retrying" >&2
    sleep "$DELAY_SECONDS"
  fi
done

echo "production health did not converge after $ATTEMPTS attempts" >&2
if jq -e . <<<"$health" >/dev/null 2>&1; then
  jq -c '{ok,api,runtime,version,push_mode,apns_ready,apns_production,action_provider_mode,action_provider_ready}' \
    <<<"$health" >&2
elif [[ -n "$health" ]]; then
  echo "last production health response was not valid JSON" >&2
else
  echo "last production health response was empty" >&2
fi
if [[ -n "$metrics" ]]; then
  grep -E '^knock_knock_(api_info|provider_ready|apns_ready|model_enabled)(\{|[[:space:]])' \
    <<<"$metrics" >&2 || true
else
  echo "last production metrics response was empty" >&2
fi

exit 1
