#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" health >/dev/null

API="${1:-https://knock-knock-backend-production.wch-klaus.workers.dev}"
API="${API%/}"
PROBE="$(date +%s)"

health="$(curl --fail-with-body --silent --show-error --max-time 20 "$API/health?probe=$PROBE")"
jq -e '(.ok == true) and (.api == "rust") and (.runtime == "cloudflare-worker") and (.push_mode == "apns" or .push_mode == "both") and (.apns_ready == true) and (.apns_production == true)' <<<"$health" >/dev/null

if [[ -n "${KNOCK_KNOCK_EXPECTED_PRODUCTION_VERSION:-}" ]]; then
  jq -e --arg expected "$KNOCK_KNOCK_EXPECTED_PRODUCTION_VERSION" '.version == $expected' <<<"$health" >/dev/null
fi

metrics="$(curl --fail-with-body --silent --show-error --max-time 20 "$API/metrics?probe=$PROBE")"
grep -q 'knock_knock_api_info{runtime="cloudflare-worker",api="rust"} 1' <<<"$metrics"
if [[ "${KNOCK_KNOCK_REQUIRE_RELEASE_READINESS_GAUGES:-0}" == "1" ]]; then
  grep -Eq 'knock_knock_provider_ready[[:space:]]+[01]' <<<"$metrics"
  grep -Eq 'knock_knock_apns_ready[[:space:]]+1' <<<"$metrics"
fi

jq -c '{ok,api,runtime,version,push_mode,apns_ready,apns_production}' <<<"$health"
echo "production healthcheck passed: $API"
