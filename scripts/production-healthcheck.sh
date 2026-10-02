#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" health >/dev/null

API="${1:-https://knock-knock-backend-production.wch-klaus.workers.dev}"
API="${API%/}"
PROBE="$(date +%s)"
ATTEMPTS="${KNOCK_KNOCK_PRODUCTION_HEALTHCHECK_ATTEMPTS:-12}"
DELAY_SECONDS="${KNOCK_KNOCK_PRODUCTION_HEALTHCHECK_DELAY_SECONDS:-5}"
EXPECTED_VERSION="${KNOCK_KNOCK_EXPECTED_PRODUCTION_VERSION:-}"

if [[ ! "$ATTEMPTS" =~ ^[1-9][0-9]*$ ]]; then
  echo "invalid production healthcheck attempt count: $ATTEMPTS" >&2
  exit 64
fi
if [[ ! "$DELAY_SECONDS" =~ ^[0-9]+([.][0-9]+)?$ ]]; then
  echo "invalid production healthcheck delay: $DELAY_SECONDS" >&2
  exit 64
fi
if [[ -z "$EXPECTED_VERSION" ]]; then
  echo "KNOCK_KNOCK_EXPECTED_PRODUCTION_VERSION is required" >&2
  exit 64
fi

health_contract_matches() {
  local payload="$1"
  if ! jq -e --arg expected "$EXPECTED_VERSION" '
    (.ok == true) and
    (.api == "rust") and
    (.runtime == "cloudflare-worker") and
    (.environment == "production") and
    (.version == $expected) and
    (.push_mode == "both") and
    (.apns_required == true) and
    (.apns_ready == true) and
    (.apns_production == true) and
    (.apns_bundle_id == "hk.knockknock.app") and
    (.apns_bundle_id_ready == true) and
    (.action_provider_mode == "external") and
    (.action_provider_ready == false) and
    (.action_reminder_enabled == false) and
    (.action_message_enabled == false)
  ' <<<"$payload" >/dev/null; then
    return 1
  fi
}

readiness_contract_matches() {
  local payload="$1"
  if ! jq -e --arg expected "$EXPECTED_VERSION" '
    (.ok == true) and
    (.runtime_configuration_ready == true) and
    (.schema_ready == true) and
    (.schema_0022_compatible == true) and
    (.environment == "production") and
    (.version == $expected) and
    (.push_mode == "both") and
    (.apns_required == true) and
    (.apns_ready == true) and
    (.apns_production == true) and
    (.apns_bundle_id == "hk.knockknock.app") and
    (.apns_bundle_id_ready == true) and
    (.action_provider_mode == "external") and
    (.action_provider_ready == false) and
    (.action_reminder_enabled == false) and
    (.action_message_enabled == false) and
    (.required_migrations == ["0017", "0018", "0019", "0020", "0021", "0022"])
  ' <<<"$payload" >/dev/null; then
    return 1
  fi
}

metrics_contract_matches() {
  local payload="$1"
  if ! grep -q 'knock_knock_api_info{runtime="cloudflare-worker",api="rust"} 1' <<<"$payload"; then
    return 1
  fi
  if [[ "${KNOCK_KNOCK_REQUIRE_RELEASE_READINESS_GAUGES:-0}" == "1" ]]; then
    grep -Eq 'knock_knock_provider_ready[[:space:]]+0' <<<"$payload" || return 1
    grep -Eq 'knock_knock_apns_ready[[:space:]]+1' <<<"$payload" || return 1
  fi
}

health=""
ready=""
metrics=""
for attempt in $(seq 1 "$ATTEMPTS"); do
  health=""
  ready=""
  metrics=""
  if health="$(curl --fail-with-body --silent --show-error --connect-timeout 10 --max-time 20 "$API/health?probe=$PROBE-$attempt")" \
    && health_contract_matches "$health" \
    && ready="$(curl --fail-with-body --silent --show-error --connect-timeout 10 --max-time 20 "$API/ready?probe=$PROBE-$attempt")" \
    && readiness_contract_matches "$ready" \
    && metrics="$(curl --fail-with-body --silent --show-error --connect-timeout 10 --max-time 20 "$API/metrics?probe=$PROBE-$attempt")" \
    && metrics_contract_matches "$metrics"; then
    jq -c '{ok,api,runtime,environment,version,push_mode,apns_ready,apns_production,apns_bundle_id,action_provider_mode,action_provider_ready,action_reminder_enabled,action_message_enabled}' <<<"$health"
    jq -c '{ok,runtime_configuration_ready,schema_ready,schema_0022_compatible,environment,version,required_migrations}' <<<"$ready"
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
  jq -c '{ok,api,runtime,environment,version,push_mode,apns_ready,apns_production,apns_bundle_id,action_provider_mode,action_provider_ready,action_reminder_enabled,action_message_enabled}' \
    <<<"$health" >&2
elif [[ -n "$health" ]]; then
  echo "last production health response was not valid JSON" >&2
else
  echo "last production health response was empty" >&2
fi
if jq -e . <<<"$ready" >/dev/null 2>&1; then
  jq -c '{ok,runtime_configuration_ready,schema_ready,schema_0022_compatible,environment,version,apns_ready,apns_production,apns_bundle_id,action_provider_mode,action_provider_ready,action_reminder_enabled,action_message_enabled,required_migrations,code}' \
    <<<"$ready" >&2
elif [[ -n "$ready" ]]; then
  echo "last production readiness response was not valid JSON" >&2
else
  echo "last production readiness response was empty" >&2
fi
if [[ -n "$metrics" ]]; then
  grep -E '^knock_knock_(api_info|provider_ready|apns_ready|model_enabled)(\{|[[:space:]])' \
    <<<"$metrics" >&2 || true
else
  echo "last production metrics response was empty" >&2
fi

exit 1
