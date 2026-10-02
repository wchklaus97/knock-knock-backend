#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
CANONICAL_STAGING_ORIGIN='https://knock-knock-backend-staging.wch-klaus.workers.dev'
BASE_URL="${BASE_URL:-}"

if [[ "${BASE_URL}" != "${CANONICAL_STAGING_ORIGIN}" ]]; then
  echo "staging contract gate requires the exact canonical Staging origin" >&2
  exit 64
fi

: "${SMOKE_EMAIL:?Set SMOKE_EMAIL to a staging Supabase UAT account}"
: "${SMOKE_PASSWORD:?Set SMOKE_PASSWORD to the staging Supabase UAT password}"
: "${SMOKE_OTHER_EMAIL:?Set SMOKE_OTHER_EMAIL to a second staging Supabase UAT account}"
: "${SMOKE_OTHER_PASSWORD:?Set SMOKE_OTHER_PASSWORD to the second staging Supabase UAT password}"
: "${STAGING_WRANGLER_CONFIG:?Set STAGING_WRANGLER_CONFIG to a materialized staging Wrangler config}"
: "${R2_SMOKE_BUCKET:?Set R2_SMOKE_BUCKET to the private staging R2 bucket}"
: "${STAGING_RELEASE_VERSION:?Set STAGING_RELEASE_VERSION to the exact deployed commit SHA}"
: "${CLOUDFLARE_API_TOKEN:?Set CLOUDFLARE_API_TOKEN to a staging-scoped token}"

if [[ "${SMOKE_AUTH_MODE:-login}" != "login" ]]; then
  echo "staging contract gate requires login mode" >&2
  exit 64
fi

"${ROOT_DIR}/scripts/ci-prerequisites.sh" staging >/dev/null

health_ok=false
for attempt in $(seq 1 6); do
  health=""
  ready=""
  if health="$(curl --fail-with-body --silent --show-error --connect-timeout 10 --max-time 20 "${BASE_URL}/health")" \
    && ready="$(curl --fail-with-body --silent --show-error --connect-timeout 10 --max-time 20 "${BASE_URL}/ready")" \
    && jq -e --arg expected_version "${STAGING_RELEASE_VERSION}" '
      (.ok == true) and
      (.api == "rust") and
      (.runtime == "cloudflare-worker") and
      (.version == $expected_version) and
      (.push_mode == "both") and
      (.apns_ready == true) and
      (.apns_production == false) and
      (.apns_bundle_id == "hk.knockknock.app") and
      (.apns_bundle_id_ready == true) and
      (.action_provider_mode == "disabled") and
      (.action_provider_ready == false) and
      (.action_reminder_enabled == false) and
      (.action_message_enabled == false)
    ' <<<"${health}" >/dev/null \
    && jq -e --arg expected_version "${STAGING_RELEASE_VERSION}" '
      (.ok == true) and
      (.runtime_configuration_ready == true) and
      (.schema_ready == true) and
      (.schema_0022_compatible == true) and
      (.environment == "staging") and
      (.version == $expected_version) and
      (.push_mode == "both") and
      (.apns_production == false) and
      (.apns_bundle_id == "hk.knockknock.app") and
      (.apns_bundle_id_ready == true) and
      (.action_provider_mode == "disabled") and
      (.action_provider_ready == false) and
      (.action_reminder_enabled == false) and
      (.action_message_enabled == false) and
      (.required_migrations == ["0017", "0018", "0019", "0020", "0021", "0022"])
    ' <<<"${ready}" >/dev/null; then
    health_ok=true
    break
  fi
  if (( attempt < 6 )); then
    echo "staging health is temporarily unavailable or stale (attempt ${attempt}/6); retrying" >&2
    sleep 5
  fi
done
if [[ "${health_ok}" != true ]]; then
  echo 'staging health/readiness did not match the expected release contract' >&2
  exit 1
fi

BASE_URL="${BASE_URL}" \
EXPECTED_PROVIDER_READY=false \
EXPECTED_APNS_READY=true \
EXPECTED_APNS_PRODUCTION=false \
EXPECTED_MODEL_ENABLED=0 \
  "${ROOT_DIR}/scripts/provider-observability-smoke.sh"

BASE_URL="${BASE_URL}" \
SMOKE_EMAIL="${SMOKE_EMAIL}" \
SMOKE_PASSWORD="${SMOKE_PASSWORD}" \
  "${ROOT_DIR}/scripts/supabase-auth-smoke.sh"

# Hosted Supabase email sending is rate-limited, so staging always uses two
# pre-provisioned UAT accounts and never falls back to registration mode.
BASE_URL="${BASE_URL}" \
SMOKE_AUTH_MODE=login \
SMOKE_EMAIL="${SMOKE_EMAIL}" \
SMOKE_PASSWORD="${SMOKE_PASSWORD}" \
SMOKE_OTHER_EMAIL="${SMOKE_OTHER_EMAIL}" \
SMOKE_OTHER_PASSWORD="${SMOKE_OTHER_PASSWORD}" \
  "${ROOT_DIR}/scripts/contract-smoke.sh"

BASE_URL="${BASE_URL}" \
SMOKE_AUTH_MODE=login \
SMOKE_EMAIL="${SMOKE_EMAIL}" \
SMOKE_PASSWORD="${SMOKE_PASSWORD}" \
SMOKE_OTHER_EMAIL="${SMOKE_OTHER_EMAIL}" \
SMOKE_OTHER_PASSWORD="${SMOKE_OTHER_PASSWORD}" \
R2_SMOKE_BUCKET="${R2_SMOKE_BUCKET}" \
R2_SMOKE_REMOTE=true \
R2_SMOKE_WRANGLER_CONFIG="${STAGING_WRANGLER_CONFIG}" \
  "${ROOT_DIR}/scripts/r2-download-smoke.sh"

printf '%s\n' 'staging contract gate passed: fail-closed health, Supabase auth, D1 routes, R2 download, retention, and isolation'
