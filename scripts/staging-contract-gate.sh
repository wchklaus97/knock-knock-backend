#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE_URL="${BASE_URL:?Set BASE_URL to the deployed staging Worker URL}"
BASE_URL="${BASE_URL%/}"
: "${SMOKE_EMAIL:?Set SMOKE_EMAIL to a staging Supabase UAT account}"
: "${SMOKE_PASSWORD:?Set SMOKE_PASSWORD to the staging Supabase UAT password}"
: "${STAGING_WRANGLER_CONFIG:?Set STAGING_WRANGLER_CONFIG to a materialized staging Wrangler config}"
: "${R2_SMOKE_BUCKET:?Set R2_SMOKE_BUCKET to the private staging R2 bucket}"

health="$(curl --fail-with-body --silent --show-error "${BASE_URL}/health")"
jq -e '
  (.ok == true) and
  (.api == "rust") and
  (.runtime == "cloudflare-worker") and
  (.push_mode == "dev") and
  (.action_provider_mode == "disabled") and
  (.action_provider_ready == false) and
  (.action_reminder_enabled == false) and
  (.action_message_enabled == false)
' <<<"${health}" >/dev/null

BASE_URL="${BASE_URL}" \
SMOKE_EMAIL="${SMOKE_EMAIL}" \
SMOKE_PASSWORD="${SMOKE_PASSWORD}" \
  "${ROOT_DIR}/scripts/supabase-auth-smoke.sh"

# The staging project must allow sign-up for this disposable contract account;
# the generated account is only used by this run and no production endpoint is
# accepted by this script's health-policy assertions.
BASE_URL="${BASE_URL}" \
SMOKE_EMAIL="staging-contract-$(date +%s)-$$@local.test" \
SMOKE_PASSWORD="${SMOKE_PASSWORD}" \
  "${ROOT_DIR}/scripts/contract-smoke.sh"

BASE_URL="${BASE_URL}" \
SMOKE_EMAIL="staging-r2-$(date +%s)-$$@local.test" \
SMOKE_PASSWORD="${SMOKE_PASSWORD}" \
R2_SMOKE_BUCKET="${R2_SMOKE_BUCKET}" \
R2_SMOKE_REMOTE=true \
R2_SMOKE_WRANGLER_CONFIG="${STAGING_WRANGLER_CONFIG}" \
  "${ROOT_DIR}/scripts/r2-download-smoke.sh"

printf '%s\n' 'staging contract gate passed: fail-closed health, Supabase auth, D1 routes, R2 download, retention, and isolation'
