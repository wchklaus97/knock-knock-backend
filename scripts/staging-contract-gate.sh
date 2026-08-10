#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" staging >/dev/null

BASE_URL="${BASE_URL:?Set BASE_URL to the deployed staging Worker URL}"
BASE_URL="${BASE_URL%/}"
: "${SMOKE_EMAIL:?Set SMOKE_EMAIL to a staging Supabase UAT account}"
: "${SMOKE_PASSWORD:?Set SMOKE_PASSWORD to the staging Supabase UAT password}"
: "${SMOKE_OTHER_EMAIL:?Set SMOKE_OTHER_EMAIL to a second staging Supabase UAT account}"
: "${SMOKE_OTHER_PASSWORD:?Set SMOKE_OTHER_PASSWORD to the second staging Supabase UAT password}"
: "${STAGING_WRANGLER_CONFIG:?Set STAGING_WRANGLER_CONFIG to a materialized staging Wrangler config}"
: "${R2_SMOKE_BUCKET:?Set R2_SMOKE_BUCKET to the private staging R2 bucket}"

case "${BASE_URL}" in
  https://*) ;;
  *)
    echo "staging contract gate requires an HTTPS staging URL" >&2
    exit 64
    ;;
esac
if [[ "${BASE_URL}" == *production* ]]; then
  echo "staging contract gate refuses a production-looking URL" >&2
  exit 64
fi

health="$(curl --fail-with-body --silent --show-error "${BASE_URL}/health")"
jq -e '
  (.ok == true) and
  (.api == "rust") and
  (.runtime == "cloudflare-worker") and
  (.push_mode == "dev") and
  (.apns_ready == false) and
  (.action_provider_mode == "disabled") and
  (.action_provider_ready == false) and
  (.action_reminder_enabled == false) and
  (.action_message_enabled == false)
' <<<"${health}" >/dev/null

BASE_URL="${BASE_URL}" \
EXPECTED_PROVIDER_READY=false \
EXPECTED_APNS_READY=false \
EXPECTED_MODEL_ENABLED=0 \
  "${ROOT_DIR}/scripts/provider-observability-smoke.sh"

BASE_URL="${BASE_URL}" \
SMOKE_EMAIL="${SMOKE_EMAIL}" \
SMOKE_PASSWORD="${SMOKE_PASSWORD}" \
  "${ROOT_DIR}/scripts/supabase-auth-smoke.sh"

# Hosted Supabase email sending is rate-limited, so staging uses two
# pre-provisioned UAT accounts. Local contract/R2 gates keep their default
# registration mode; this script still refuses production-looking URLs.
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
