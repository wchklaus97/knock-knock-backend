#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASE_URL="${BASE_URL:?Set BASE_URL to the deployed staging Worker URL}"
BASE_URL="${BASE_URL%/}"
: "${SMOKE_EMAIL:?Set SMOKE_EMAIL to a staging Supabase UAT account}"
: "${SMOKE_PASSWORD:?Set SMOKE_PASSWORD to the staging Supabase UAT password}"

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

printf '%s\n' 'staging contract gate passed: fail-closed health, Supabase auth, and full route smoke'
