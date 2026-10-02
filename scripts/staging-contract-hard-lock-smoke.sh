#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GATE="${ROOT_DIR}/scripts/staging-contract-gate.sh"
CONTRACT_WORKFLOW="${ROOT_DIR}/.github/workflows/staging-contract-gate.yml"
DEPLOY_WORKFLOW="${ROOT_DIR}/.github/workflows/staging-deploy.yml"
CANONICAL_STAGING_ORIGIN='https://knock-knock-backend-staging.wch-klaus.workers.dev'
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "${TMP_DIR}"' EXIT

fail() {
  printf 'staging contract hard-lock smoke failed: %s\n' "$1" >&2
  exit 1
}

mkdir -p "${TMP_DIR}/bin"
cat >"${TMP_DIR}/bin/curl" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
: >"${CURL_CALLED_FILE:?}"
exit 99
SH
chmod +x "${TMP_DIR}/bin/curl"

bad_origins=(
  ''
  'http://knock-knock-backend-staging.wch-klaus.workers.dev'
  'https://knock-knock-backend-staging.wch-klaus.workers.dev/'
  'https://knock-knock-backend-staging.wch-klaus.workers.dev/path'
  'https://knock-knock-backend-staging.wch-klaus.workers.dev?query=1'
  'https://knock-knock-backend-staging.wch-klaus.workers.dev#fragment'
  'https://user:password@knock-knock-backend-staging.wch-klaus.workers.dev'
  'https://knock-knock-backend-staging.wch-klaus.workers.dev.evil.invalid'
  'https://knock-knock-backend-staging-wch-klaus.workers.dev'
  'https://redirect.example.invalid/?to=https://knock-knock-backend-staging.wch-klaus.workers.dev'
  'https://knock-knock-backend-production.wch-klaus.workers.dev'
)

for index in "${!bad_origins[@]}"; do
  marker="${TMP_DIR}/curl-${index}.called"
  stderr_file="${TMP_DIR}/bad-origin-${index}.stderr"
  if PATH="${TMP_DIR}/bin:${PATH}" \
    CURL_CALLED_FILE="${marker}" \
    BASE_URL="${bad_origins[$index]}" \
    SMOKE_EMAIL='must-not-be-sent@example.invalid' \
    SMOKE_PASSWORD='must-not-be-sent' \
    SMOKE_OTHER_EMAIL='must-not-be-sent-other@example.invalid' \
    SMOKE_OTHER_PASSWORD='must-not-be-sent-other' \
    CLOUDFLARE_API_TOKEN='must-not-be-sent-token' \
    bash "${GATE}" >"${TMP_DIR}/bad-origin-${index}.stdout" 2>"${stderr_file}"; then
    fail "gate accepted rejected origin case ${index}"
  fi
  [[ ! -e "${marker}" ]] || fail "gate sent a request for rejected origin case ${index}"
  grep -Fq 'requires the exact canonical Staging origin' "${stderr_file}" \
    || fail "gate did not fail at the origin lock for case ${index}"
done

canonical_stderr="${TMP_DIR}/canonical-missing-secret.stderr"
if PATH="${TMP_DIR}/bin:${PATH}" \
  CURL_CALLED_FILE="${TMP_DIR}/canonical.called" \
  BASE_URL="${CANONICAL_STAGING_ORIGIN}" \
  bash "${GATE}" >"${TMP_DIR}/canonical-missing-secret.stdout" 2>"${canonical_stderr}"; then
  fail 'canonical origin did not fail closed when login credentials were absent'
fi
[[ ! -e "${TMP_DIR}/canonical.called" ]] \
  || fail 'canonical origin sent a request before required credentials were validated'
grep -Fq 'SMOKE_EMAIL' "${canonical_stderr}" \
  || fail 'canonical origin did not reach the fail-closed credential check'

grep -Fq "CANONICAL_STAGING_ORIGIN: ${CANONICAL_STAGING_ORIGIN}" "${CONTRACT_WORKFLOW}"
if grep -Fq 'inputs.staging_url' "${CONTRACT_WORKFLOW}"; then
  fail 'contract workflow still accepts a user-selected URL'
fi
if awk '
  /^    env:/ { in_job_env = 1 }
  /^    steps:/ { in_job_env = 0 }
  in_job_env { print }
' "${CONTRACT_WORKFLOW}" | grep -Fq 'secrets.'; then
  fail 'contract workflow exposes secrets at job scope before the origin lock'
fi
if awk '
  /^    env:/ { in_job_env = 1 }
  /^    steps:/ { in_job_env = 0 }
  in_job_env { print }
' "${DEPLOY_WORKFLOW}" | grep -Fq 'secrets.'; then
  fail 'deploy workflow exposes secrets at job scope'
fi

grep -Eq 'uses: actions/checkout@[0-9a-f]{40}' "${CONTRACT_WORKFLOW}" \
  || fail 'contract workflow checkout action is not pinned to a commit SHA'
grep -Eq 'uses: actions/checkout@[0-9a-f]{40}' "${DEPLOY_WORKFLOW}" \
  || fail 'deploy workflow checkout action is not pinned to a commit SHA'
grep -Eq 'uses: dtolnay/rust-toolchain@[0-9a-f]{40}' "${DEPLOY_WORKFLOW}" \
  || fail 'deploy workflow Rust toolchain action is not pinned to a commit SHA'
token_scope_count="$(grep -Fc 'CLOUDFLARE_API_TOKEN: ${{ secrets.CLOUDFLARE_API_TOKEN }}' "${DEPLOY_WORKFLOW}")"
[[ "${token_scope_count}" = "5" ]] \
  || fail 'deploy workflow Cloudflare token is not scoped to the five exact remote Wrangler steps'

contract_lock_line="$(grep -nF 'name: Verify exact canonical Staging origin before credentials' "${CONTRACT_WORKFLOW}" | cut -d: -f1)"
contract_run_line="$(grep -nF 'name: Run deployed staging contract gate' "${CONTRACT_WORKFLOW}" | cut -d: -f1)"
[[ -n "${contract_lock_line}" && -n "${contract_run_line}" && "${contract_lock_line}" -lt "${contract_run_line}" ]] \
  || fail 'contract workflow does not lock the origin before its credentialed step'
grep -Fq 'SMOKE_AUTH_MODE: login' "${CONTRACT_WORKFLOW}"
# The GitHub expression is intentionally matched literally.
# shellcheck disable=SC2016
grep -Fq 'BASE_URL: ${{ env.CANONICAL_STAGING_ORIGIN }}' "${CONTRACT_WORKFLOW}"

deploy_lock_line="$(grep -nF 'name: Lock post-deploy contract origin before login credentials' "${DEPLOY_WORKFLOW}" | cut -d: -f1)"
deploy_contract_line="$(grep -nF 'name: Run the deployed Staging API contract in login mode' "${DEPLOY_WORKFLOW}" | cut -d: -f1)"
rollback_line="$(grep -nF 'name: Restore the previous Staging Worker version on gate failure' "${DEPLOY_WORKFLOW}" | cut -d: -f1)"
hold_line="$(grep -nF 'name: HOLD for manual forward recovery when Worker rollback is unsafe' "${DEPLOY_WORKFLOW}" | cut -d: -f1)"
[[ -n "${deploy_lock_line}" && -n "${deploy_contract_line}" && -n "${rollback_line}" && -n "${hold_line}" ]] \
  || fail 'post-deploy contract, compatible rollback, or HOLD step is missing'
[[ "${deploy_lock_line}" -lt "${deploy_contract_line}" && "${deploy_contract_line}" -lt "${rollback_line}" && "${rollback_line}" -lt "${hold_line}" ]] \
  || fail 'post-deploy origin lock, login contract, compatible rollback, and HOLD ordering is unsafe'
grep -Fq "env.PREVIOUS_SCHEMA_0022_COMPATIBLE == 'true'" "${DEPLOY_WORKFLOW}" \
  || fail 'automatic Worker rollback is not fenced by schema 0022 compatibility evidence'
grep -Fq 'STAGING_D1_MIGRATION_ATTEMPTED: "false"' "${DEPLOY_WORKFLOW}" \
  || fail 'workflow does not initialize the conservative migration-attempt fence'
grep -Fq "echo 'STAGING_D1_MIGRATION_ATTEMPTED=true'" "${DEPLOY_WORKFLOW}" \
  || fail 'workflow does not record the migration attempt before invoking Wrangler'
grep -Fq "env.STAGING_D1_MIGRATION_ATTEMPTED == 'true'" "${DEPLOY_WORKFLOW}" \
  || fail 'manual forward-recovery HOLD is not fenced by migration-attempt evidence'
attempt_marker_line="$(grep -nF "echo 'STAGING_D1_MIGRATION_ATTEMPTED=true'" "${DEPLOY_WORKFLOW}" | cut -d: -f1)"
migration_command_line="$(grep -nF 'wrangler d1 migrations apply DB' "${DEPLOY_WORKFLOW}" | cut -d: -f1)"
[[ -n "${attempt_marker_line}" && -n "${migration_command_line}" && "${attempt_marker_line}" -lt "${migration_command_line}" ]] \
  || fail 'migration-attempt fence must be persisted before the remote D1 command'
grep -Fq 'schema_0022_compatible' "${DEPLOY_WORKFLOW}"
grep -Fq 'manual forward recovery required' "${DEPLOY_WORKFLOW}"
grep -Fq 'APNS_BUNDLE_ID = "hk.knockknock.app"' "${ROOT_DIR}/wrangler.staging.toml.example"
grep -Fq 'APNS_BUNDLE_ID = "hk.knockknock.app"' "${DEPLOY_WORKFLOW}"
grep -Fq 'config_value(env, "APNS_BUNDLE_ID", "")' "${ROOT_DIR}/src/apns.rs" \
  || fail 'runtime APNs topic still has a permissive implicit default'
grep -Fq '.apns_bundle_id == "hk.knockknock.app"' "${DEPLOY_WORKFLOW}"
grep -Fq '.apns_bundle_id_ready == true' "${DEPLOY_WORKFLOW}"
apns_ready_gate_count="$(grep -Fc '(.apns_ready == true)' "${DEPLOY_WORKFLOW}")"
[[ "${apns_ready_gate_count}" = "2" ]] \
  || fail 'Staging health and readiness gates must both require APNs readiness'
grep -Fq 'SMOKE_AUTH_MODE: login' "${DEPLOY_WORKFLOW}"
for secret_name in \
  KNOCK_KNOCK_STAGING_SMOKE_EMAIL \
  KNOCK_KNOCK_STAGING_SMOKE_PASSWORD \
  KNOCK_KNOCK_STAGING_SMOKE_OTHER_EMAIL \
  KNOCK_KNOCK_STAGING_SMOKE_OTHER_PASSWORD; do
  grep -Fq "secrets.${secret_name}" "${CONTRACT_WORKFLOW}"
  grep -Fq "secrets.${secret_name}" "${DEPLOY_WORKFLOW}"
done
if grep -Fq 'SMOKE_AUTH_MODE: register' "${CONTRACT_WORKFLOW}" \
  || grep -Fq 'SMOKE_AUTH_MODE: register' "${DEPLOY_WORKFLOW}"; then
  fail 'a remote Staging workflow still permits registration fallback'
fi
if grep -Eq '^[[:space:]]+wrangler d1 time-travel restore' "${DEPLOY_WORKFLOW}"; then
  fail 'Staging deploy workflow must not automatically restore D1'
fi

printf '%s\n' 'staging contract hard-lock smoke passed: canonical-only origin, step-scoped secrets, pinned actions, APNs topic lock, login-only workflows, compatible rollback, and HOLD ordering'
