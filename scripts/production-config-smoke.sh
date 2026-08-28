#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
LOCAL_CONFIG="$ROOT/wrangler.toml"
LOCAL_EXAMPLE="$ROOT/wrangler.toml.example"
PRODUCTION_EXAMPLE="$ROOT/wrangler.production.toml.example"
STAGING_EXAMPLE="$ROOT/wrangler.staging.toml.example"
BACKUP_WORKFLOW="$ROOT/.github/workflows/production-backup.yml"
PRODUCTION_RELEASE_WORKFLOW="$ROOT/.github/workflows/production-release.yml"
STAGING_DEPLOY_WORKFLOW="$ROOT/.github/workflows/staging-deploy.yml"
PRODUCTION_HEALTHCHECK="$ROOT/scripts/production-healthcheck.sh"

for file in "$LOCAL_CONFIG" "$LOCAL_EXAMPLE" "$PRODUCTION_EXAMPLE" "$STAGING_EXAMPLE"; do
  test -f "$file"
done

grep -q '^NODE_ENV = "development"$' "$LOCAL_CONFIG"
grep -q '^PUSH_MODE = "dev"$' "$LOCAL_CONFIG"
grep -q '^ACTION_PROVIDER_MODE = "internal"$' "$LOCAL_CONFIG"
grep -q '^ACTION_REMINDER_ENABLED = "true"$' "$LOCAL_CONFIG"
grep -q '^ACTION_MESSAGE_ENABLED = "true"$' "$LOCAL_CONFIG"
grep -q '^NODE_ENV = "development"$' "$LOCAL_EXAMPLE"
grep -q '^NODE_ENV = "production"$' "$PRODUCTION_EXAMPLE"
grep -q '^AUTH_PROVIDER = "supabase"$' "$PRODUCTION_EXAMPLE"
grep -q '^SUPABASE_URL = "REPLACE_WITH_SUPABASE_PROJECT_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^PUSH_MODE = "both"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_PROVIDER_MODE = "external"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_REMINDER_ENABLED = "false"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_MESSAGE_ENABLED = "false"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_REMINDER_URL = "REPLACE_WITH_REMINDER_PROVIDER_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_MESSAGE_URL = "REPLACE_WITH_MESSAGE_PROVIDER_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_REMINDER_CANCEL_URL = "REPLACE_WITH_REMINDER_CANCEL_PROVIDER_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_REMINDER_STATUS_URL = "REPLACE_WITH_REMINDER_STATUS_PROVIDER_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^ACTION_MESSAGE_STATUS_URL = "REPLACE_WITH_MESSAGE_STATUS_PROVIDER_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^CORS_ORIGIN = "REPLACE_WITH_ALLOWED_ORIGIN"$' "$PRODUCTION_EXAMPLE"
grep -q '^SERVICE_VERSION = "REPLACE_WITH_RELEASE_VERSION"$' "$PRODUCTION_EXAMPLE"
grep -q '^binding = "R2"$' "$PRODUCTION_EXAMPLE"
grep -q '^bucket_name = "REPLACE_WITH_R2_BUCKET_NAME"$' "$PRODUCTION_EXAMPLE"
grep -q '^VOICE_MODEL_ENABLED = "true"$' "$PRODUCTION_EXAMPLE"
grep -q '^VOICE_MODEL_URL = "REPLACE_WITH_SIGNED_MODEL_URL"$' "$PRODUCTION_EXAMPLE"
grep -q '^VOICE_MODEL_R2_KEY = "REPLACE_WITH_SIGNED_MODEL_R2_KEY"$' "$PRODUCTION_EXAMPLE"
grep -q '^VOICE_MODEL_MANIFEST_JSON = "REPLACE_WITH_SIGNED_MODEL_MANIFEST_JSON"$' "$PRODUCTION_EXAMPLE"
grep -q '^NODE_ENV = "staging"$' "$STAGING_EXAMPLE"
grep -q '^AUTH_PROVIDER = "supabase"$' "$STAGING_EXAMPLE"
grep -q '^PUSH_MODE = "both"$' "$STAGING_EXAMPLE"
grep -q '^APNS_PRODUCTION = "false"$' "$STAGING_EXAMPLE"
grep -q '^APNS_BUNDLE_ID = "hk.knockknock.app"$' "$STAGING_EXAMPLE"
grep -q '^ACTION_PROVIDER_MODE = "disabled"$' "$STAGING_EXAMPLE"
grep -q '^ACTION_REMINDER_ENABLED = "false"$' "$STAGING_EXAMPLE"
grep -q '^ACTION_MESSAGE_ENABLED = "false"$' "$STAGING_EXAMPLE"
grep -q '^CORS_ORIGIN = "REPLACE_WITH_STAGING_ALLOWED_ORIGIN"$' "$STAGING_EXAMPLE"
grep -q '^SERVICE_VERSION = "REPLACE_WITH_STAGING_RELEASE_VERSION"$' "$STAGING_EXAMPLE"
grep -q '^binding = "R2"$' "$STAGING_EXAMPLE"
grep -q '^bucket_name = "REPLACE_WITH_STAGING_R2_BUCKET_NAME"$' "$STAGING_EXAMPLE"

grep -q 'BACKUP_BUCKET' "$BACKUP_WORKFLOW"
grep -q 'BACKUP_PASSPHRASE' "$BACKUP_WORKFLOW"
grep -q 'gpg' "$BACKUP_WORKFLOW"
grep -Fq 'name: production-backup' "$BACKUP_WORKFLOW"
grep -Fq 'group: knock-knock-production-backup' "$BACKUP_WORKFLOW"
grep -Fq "[[ -z \"\$BACKUP_BUCKET\" ]]" "$BACKUP_WORKFLOW"
grep -Fq '^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$' "$BACKUP_WORKFLOW"
grep -Fq -- "-e \"s|REPLACE_WITH_R2_BUCKET_NAME|\$BACKUP_BUCKET|g\"" "$BACKUP_WORKFLOW"
grep -Fq "REPLACE_WITH_(D1_DATABASE_ID|ALLOWED_ORIGIN|RELEASE_VERSION|R2_BUCKET_NAME)" "$BACKUP_WORKFLOW"
grep -Fq "\`production-backup\` environment" "$ROOT/docs/PRODUCTION_RELEASE_RUNBOOK.md"
grep -Fq 'workspace = pathlib.Path(os.environ["GITHUB_WORKSPACE"]).resolve()' "$PRODUCTION_RELEASE_WORKFLOW"
grep -Fq 'migrations_dir = workspace / "migrations"' "$PRODUCTION_RELEASE_WORKFLOW"
grep -Fq 'production migrations directory is missing' "$PRODUCTION_RELEASE_WORKFLOW"
grep -Fq 'migrations_dir = "{toml_string(str(migrations_dir))}"' "$PRODUCTION_RELEASE_WORKFLOW"
# These assertions intentionally match the literal GitHub Actions shell source.
# shellcheck disable=SC2016
grep -Fq 'test -d "$GITHUB_WORKSPACE/migrations"' "$PRODUCTION_RELEASE_WORKFLOW"
# shellcheck disable=SC2016
grep -Fq 'grep -Fqx "migrations_dir = \"$GITHUB_WORKSPACE/migrations\"" "$config"' "$PRODUCTION_RELEASE_WORKFLOW"
test -f "$ROOT/migrations/0022_listener_lease_release.sql"
grep -Fq 'ALTER TABLE agent_listener_leases ADD COLUMN released_at TEXT' "$ROOT/migrations/0022_listener_lease_release.sql"
if grep -Fq 'apply_migrations' "$PRODUCTION_RELEASE_WORKFLOW"; then
  echo 'Production migration application must not be optional' >&2
  exit 1
fi
production_apply_line="$(grep -nF 'name: Apply mandatory Production migrations through 0022' "$PRODUCTION_RELEASE_WORKFLOW" | cut -d: -f1)"
production_verify_line="$(grep -nF 'name: Verify Production migration 0022 and schema before deploy' "$PRODUCTION_RELEASE_WORKFLOW" | cut -d: -f1)"
production_deploy_line="$(grep -nF 'name: Deploy exact approved Worker' "$PRODUCTION_RELEASE_WORKFLOW" | cut -d: -f1)"
test -n "$production_apply_line"
test -n "$production_verify_line"
test -n "$production_deploy_line"
test "$production_apply_line" -lt "$production_verify_line"
test "$production_verify_line" -lt "$production_deploy_line"
grep -Fq 'wrangler d1 migrations apply DB --remote' "$PRODUCTION_RELEASE_WORKFLOW"
grep -Fq '0022_listener_lease_release.sql' "$PRODUCTION_RELEASE_WORKFLOW"
grep -Fq 'schema_0022_compatible' "$PRODUCTION_RELEASE_WORKFLOW"
grep -Fq "pragma_table_info('agent_listener_leases')" "$PRODUCTION_RELEASE_WORKFLOW"
grep -Fq '.migration_tail == "0017_agent_chat_bindings.sql,0018_listener_lease_fencing.sql,0019_phone_ask_claim_fencing.sql,0020_session_event_apns_outbox.sql,0021_push_registration_uniqueness.sql,0022_listener_lease_release.sql"' "$PRODUCTION_RELEASE_WORKFLOW"
grep -Fq 'KNOCK_KNOCK_EXPECTED_PRODUCTION_VERSION' "$PRODUCTION_RELEASE_WORKFLOW"
grep -Fq '"$API/ready?probe=$PROBE-$attempt"' "$PRODUCTION_HEALTHCHECK"
grep -Fq '(.schema_0022_compatible == true)' "$PRODUCTION_HEALTHCHECK"
grep -Fq '(.runtime_configuration_ready == true)' "$PRODUCTION_HEALTHCHECK"
grep -Fq '(.version == $expected)' "$PRODUCTION_HEALTHCHECK"
grep -Fq '(.apns_bundle_id == "hk.knockknock.app")' "$PRODUCTION_HEALTHCHECK"
grep -Fq '(.apns_production == true)' "$PRODUCTION_HEALTHCHECK"
grep -Fq '(.action_provider_mode == "external")' "$PRODUCTION_HEALTHCHECK"
grep -Fq '(.action_provider_ready == false)' "$PRODUCTION_HEALTHCHECK"
grep -Fq '(.action_reminder_enabled == false)' "$PRODUCTION_HEALTHCHECK"
grep -Fq '(.action_message_enabled == false)' "$PRODUCTION_HEALTHCHECK"
if grep -q 'actions/upload-artifact' "$BACKUP_WORKFLOW"; then
  echo "production backups must not be retained as plaintext CI artifacts" >&2
  exit 1
fi

SMOKE_DIR="$(mktemp -d)"
trap 'rm -rf "$SMOKE_DIR"' EXIT
SMOKE_CONFIG="$SMOKE_DIR/wrangler.production.toml"
sed \
  -e 's|REPLACE_WITH_D1_DATABASE_ID|00000000-0000-0000-0000-000000000000|g' \
  -e 's|REPLACE_WITH_ALLOWED_ORIGIN|https://backup-smoke.invalid|g' \
  -e 's|REPLACE_WITH_RELEASE_VERSION|backup-smoke|g' \
  -e 's|REPLACE_WITH_R2_BUCKET_NAME|knock-knock-backup-smoke|g' \
  "$PRODUCTION_EXAMPLE" > "$SMOKE_CONFIG"
if grep -Eq 'REPLACE_WITH_(D1_DATABASE_ID|ALLOWED_ORIGIN|RELEASE_VERSION|R2_BUCKET_NAME)' "$SMOKE_CONFIG"; then
  echo "materialized backup config still contains a required placeholder" >&2
  exit 1
fi
grep -Fqx 'bucket_name = "knock-knock-backup-smoke"' "$SMOKE_CONFIG"
python3 - "$SMOKE_CONFIG" <<'PY'
import pathlib
import sys
import tomllib

with pathlib.Path(sys.argv[1]).open("rb") as config_file:
    tomllib.load(config_file)
PY
if command -v wrangler >/dev/null 2>&1; then
  env \
    -u CLOUDFLARE_API_TOKEN \
    -u CLOUDFLARE_ACCOUNT_ID \
    -u CLOUDFLARE_API_KEY \
    -u CLOUDFLARE_EMAIL \
    CI=1 \
    WRANGLER_SEND_METRICS=false \
    wrangler d1 export knock-knock \
      --local \
      --config "$SMOKE_CONFIG" \
      --output "$SMOKE_DIR/local-export.sql"
  test -s "$SMOKE_DIR/local-export.sql"
fi

if grep -q '^CORS_ORIGIN = "\*"$' "$PRODUCTION_EXAMPLE"; then
  echo "production config must not allow wildcard CORS" >&2
  exit 1
fi

grep -q 'pub fn runtime_configuration' "$ROOT/src/auth.rs"
grep -q 'runtime_configuration(&env)' "$ROOT/src/lib.rs"
grep -q 'must be configured for production APNs' "$ROOT/src/auth.rs"
grep -q 'PUSH_MODE must be apns or both in production' "$ROOT/src/auth.rs"
grep -q 'PUSH_MODE must be apns or both in staging' "$ROOT/src/auth.rs"
grep -q 'APNS_PRODUCTION must be false in staging' "$ROOT/src/auth.rs"
grep -q 'APNS_BUNDLE_ID must be hk.knockknock.app in staging' "$ROOT/src/auth.rs"
grep -q 'APNS_BUNDLE_ID must be hk.knockknock.app in production' "$ROOT/src/auth.rs"
grep -q 'APNS_PRODUCTION must be true in production' "$ROOT/src/auth.rs"
grep -q 'ACTION_PROVIDER_MODE must be external in production' "$ROOT/src/providers.rs"
grep -q 'ACTION_REMINDER_ENABLED and ACTION_MESSAGE_ENABLED must both be false in production' "$ROOT/src/providers.rs"
grep -q 'ACTION_PROVIDER_MODE must be disabled in staging' "$ROOT/src/providers.rs"
grep -q 'ACTION_REMINDER_ENABLED and ACTION_MESSAGE_ENABLED must both be false in staging' "$ROOT/src/providers.rs"
grep -Fq 'crate::providers::validate_deployment_policy(env, &node_env)' "$ROOT/src/auth.rs"
grep -Fq 'config_value(env, "APNS_BUNDLE_ID", "")' "$ROOT/src/apns.rs"
grep -q 'Staging APNs signing configuration is incomplete' "$ROOT/src/auth.rs"
grep -q 'SUPABASE_PUBLISHABLE_KEY must be configured' "$ROOT/src/auth.rs"
grep -Fq 'let health_ok = apns_gate_ready(' "$ROOT/src/lib.rs"
grep -Fq '"ok": health_ok' "$ROOT/src/lib.rs"
grep -Fq '"apns_identity_not_ready"' "$ROOT/src/lib.rs"
grep -Fq '"apns_not_ready"' "$ROOT/src/lib.rs"
grep -Fq '"schema_0022_compatible": schema_ready' "$ROOT/src/lib.rs"
if grep -Fq '"schema_0021_compatible": true' "$ROOT/src/lib.rs"; then
  echo 'health/readiness source still contains a hardcoded schema compatibility claim' >&2
  exit 1
fi
if grep -Fq 'APNS_PRODUCTION must be true or false in production' "$ROOT/src/auth.rs"; then
  echo 'production runtime still permits APNs sandbox mode' >&2
  exit 1
fi

grep -Fq "STAGING_RELEASE_VERSION: \${{ github.sha }}" "$ROOT/.github/workflows/staging-deploy.yml"
grep -Fq "STAGING_RELEASE_VERSION: \${{ github.sha }}" "$ROOT/.github/workflows/staging-contract-gate.yml"
if grep -Fq 'vars.KNOCK_KNOCK_STAGING_RELEASE_VERSION' "$ROOT/.github/workflows/staging-deploy.yml" \
  || grep -Fq 'vars.KNOCK_KNOCK_STAGING_RELEASE_VERSION' "$ROOT/.github/workflows/staging-contract-gate.yml"; then
  echo "staging release identity must come from github.sha, not a mutable repository variable" >&2
  exit 1
fi

extract_workflow_run_step() {
  local step_name="$1"
  local output_path="$2"
  python3 - "$STAGING_DEPLOY_WORKFLOW" "$step_name" "$output_path" <<'PY'
import pathlib
import sys

workflow_path = pathlib.Path(sys.argv[1])
step_name = sys.argv[2]
output_path = pathlib.Path(sys.argv[3])
lines = workflow_path.read_text().splitlines()
step_marker = f"      - name: {step_name}"

try:
    step_index = lines.index(step_marker)
except ValueError as error:
    raise SystemExit(f"workflow step not found: {step_name}") from error

run_index = step_index + 1
while run_index < len(lines) and lines[run_index] != "        run: |":
    if lines[run_index].startswith("      - name: "):
        raise SystemExit(f"workflow step has no run block: {step_name}")
    run_index += 1

if run_index == len(lines):
    raise SystemExit(f"workflow step has no run block: {step_name}")

script_lines = []
for line in lines[run_index + 1:]:
    if line.startswith("      - name: "):
        break
    if line and not line.startswith("          "):
        raise SystemExit(f"unexpected indentation in workflow step: {step_name}")
    script_lines.append(line[10:] if line else "")

output_path.write_text("#!/usr/bin/env bash\n" + "\n".join(script_lines) + "\n")
PY
  chmod +x "$output_path"
}

STAGING_LOCK_STEP="$SMOKE_DIR/staging-hard-lock-step.sh"
extract_workflow_run_step 'Materialize hard-locked Staging Wrangler config' "$STAGING_LOCK_STEP"

STAGING_REF='awrvwbgkzzvhbehwjues'
PRODUCTION_REF='ccisdatvaabuekflbabo'
STAGING_D1='cc97b563-2e1a-4ab2-96a2-3d0ba8201d9c'
PRODUCTION_D1='cd2674a1-6363-4343-8f9f-c8307134d732'
STAGING_ROUTE='knock-knock-backend-staging.wch-klaus.workers.dev'
PRODUCTION_ROUTE='knock-knock-backend-production.wch-klaus.workers.dev'
LOCK_ENV=(
  "PATH=$PATH"
  'CI=true'
  'CLOUDFLARE_API_TOKEN=local-safety-test'
  'CLOUDFLARE_ACCOUNT_ID=bb3bd088c02d3baa52f7bd175c3ce953'
  "STAGING_D1_DATABASE_ID=$STAGING_D1"
  "PRODUCTION_D1_DATABASE_ID=$PRODUCTION_D1"
  'STAGING_R2_BUCKET_NAME=knock-knock-staging'
  'PRODUCTION_R2_BUCKET_NAME=knock-knock-production'
  "STAGING_SUPABASE_URL=https://${STAGING_REF}.supabase.co"
  "PRODUCTION_SUPABASE_URL=https://${PRODUCTION_REF}.supabase.co"
  "STAGING_SUPABASE_PROJECT_REF=$STAGING_REF"
  "PRODUCTION_SUPABASE_PROJECT_REF=$PRODUCTION_REF"
  "STAGING_CORS_ORIGIN=https://${STAGING_ROUTE}"
  "STAGING_URL=https://${STAGING_ROUTE}"
  "PRODUCTION_URL=https://${PRODUCTION_ROUTE}"
  'STAGING_WORKER_NAME=knock-knock-backend-staging'
  'PRODUCTION_WORKER_NAME=knock-knock-backend-production'
  "STAGING_WORKER_ROUTE=$STAGING_ROUTE"
  "PRODUCTION_WORKER_ROUTE=$PRODUCTION_ROUTE"
  'STAGING_RELEASE_VERSION=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'
  'EXPECTED_CLOUDFLARE_ACCOUNT_ID=bb3bd088c02d3baa52f7bd175c3ce953'
  'EXPECTED_STAGING_D1_DATABASE_ID=cc97b563-2e1a-4ab2-96a2-3d0ba8201d9c'
  'EXPECTED_PRODUCTION_D1_DATABASE_ID=cd2674a1-6363-4343-8f9f-c8307134d732'
  'EXPECTED_STAGING_R2_BUCKET_NAME=knock-knock-staging'
  'EXPECTED_PRODUCTION_R2_BUCKET_NAME=knock-knock-production'
  'EXPECTED_STAGING_WORKER_NAME=knock-knock-backend-staging'
  'EXPECTED_PRODUCTION_WORKER_NAME=knock-knock-backend-production'
  'EXPECTED_STAGING_WORKER_ROUTE=knock-knock-backend-staging.wch-klaus.workers.dev'
  'EXPECTED_PRODUCTION_WORKER_ROUTE=knock-knock-backend-production.wch-klaus.workers.dev'
  'EXPECTED_STAGING_URL=https://knock-knock-backend-staging.wch-klaus.workers.dev'
  'EXPECTED_PRODUCTION_URL=https://knock-knock-backend-production.wch-klaus.workers.dev'
  'EXPECTED_STAGING_SUPABASE_PROJECT_REF=awrvwbgkzzvhbehwjues'
  'EXPECTED_PRODUCTION_SUPABASE_PROJECT_REF=ccisdatvaabuekflbabo'
  'EXPECTED_STAGING_SUPABASE_URL=https://awrvwbgkzzvhbehwjues.supabase.co'
  'EXPECTED_PRODUCTION_SUPABASE_URL=https://ccisdatvaabuekflbabo.supabase.co'
)

run_staging_lock() {
  local case_name="$1"
  shift
  local case_dir="$SMOKE_DIR/$case_name"
  mkdir -p "$case_dir"
  env -i \
    "${LOCK_ENV[@]}" \
    "RUNNER_TEMP=$case_dir" \
    "GITHUB_WORKSPACE=$ROOT" \
    "GITHUB_ENV=$case_dir/github.env" \
    "$@" \
    bash "$STAGING_LOCK_STEP"
}

run_staging_lock staging-lock-valid
STAGING_SMOKE_CONFIG="$SMOKE_DIR/staging-lock-valid/wrangler.staging.toml"
test -s "$STAGING_SMOKE_CONFIG"
python3 - "$STAGING_SMOKE_CONFIG" "$STAGING_D1" "$STAGING_ROUTE" "$STAGING_REF" <<'PY'
import pathlib
import sys
import tomllib

with pathlib.Path(sys.argv[1]).open("rb") as config_file:
    config = tomllib.load(config_file)

assert config["name"] == "knock-knock-backend-staging"
assert config["workers_dev"] is True
assert "route" not in config
assert "routes" not in config
assert config["d1_databases"][0]["database_id"] == sys.argv[2]
assert config["r2_buckets"][0]["bucket_name"] == "knock-knock-staging"
assert config["vars"]["SUPABASE_URL"] == f"https://{sys.argv[4]}.supabase.co"
PY

for required_target in \
  CLOUDFLARE_ACCOUNT_ID \
  STAGING_D1_DATABASE_ID PRODUCTION_D1_DATABASE_ID \
  STAGING_R2_BUCKET_NAME PRODUCTION_R2_BUCKET_NAME \
  STAGING_SUPABASE_URL PRODUCTION_SUPABASE_URL \
  STAGING_SUPABASE_PROJECT_REF PRODUCTION_SUPABASE_PROJECT_REF \
  STAGING_WORKER_NAME PRODUCTION_WORKER_NAME \
  STAGING_WORKER_ROUTE PRODUCTION_WORKER_ROUTE \
  STAGING_URL PRODUCTION_URL; do
  if run_staging_lock "missing-${required_target}" "${required_target}=" >/dev/null 2>&1; then
    echo "staging hard lock accepted missing target identity: ${required_target}" >&2
    exit 1
  fi
done

if run_staging_lock duplicate-d1 "PRODUCTION_D1_DATABASE_ID=$STAGING_D1" >/dev/null 2>&1; then
  echo 'staging hard lock accepted the Production D1 ID' >&2
  exit 1
fi
if run_staging_lock duplicate-r2 'PRODUCTION_R2_BUCKET_NAME=knock-knock-staging' >/dev/null 2>&1; then
  echo 'staging hard lock accepted the Production R2 bucket name' >&2
  exit 1
fi
if run_staging_lock duplicate-worker-name 'PRODUCTION_WORKER_NAME=knock-knock-backend-staging' >/dev/null 2>&1; then
  echo 'staging hard lock accepted the Production Worker name' >&2
  exit 1
fi
if run_staging_lock duplicate-worker-route "PRODUCTION_WORKER_ROUTE=$STAGING_ROUTE" >/dev/null 2>&1; then
  echo 'staging hard lock accepted the Production Worker route' >&2
  exit 1
fi
if run_staging_lock duplicate-supabase \
  "PRODUCTION_SUPABASE_PROJECT_REF=$STAGING_REF" \
  "PRODUCTION_SUPABASE_URL=https://${STAGING_REF}.supabase.co" >/dev/null 2>&1; then
  echo 'staging hard lock accepted the Production Supabase project ref' >&2
  exit 1
fi
if run_staging_lock supabase-suffix-confusion \
  "STAGING_SUPABASE_URL=https://${STAGING_REF}.supabase.co.evil.invalid" >/dev/null 2>&1; then
  echo 'staging hard lock accepted a Supabase hostname suffix attack' >&2
  exit 1
fi
if run_staging_lock supabase-path-confusion \
  "STAGING_SUPABASE_URL=https://${STAGING_REF}.supabase.co/projects/${PRODUCTION_REF}" >/dev/null 2>&1; then
  echo 'staging hard lock accepted a Supabase project ref in a URL path' >&2
  exit 1
fi
if run_staging_lock supabase-ref-mismatch \
  "STAGING_SUPABASE_PROJECT_REF=$PRODUCTION_REF" >/dev/null 2>&1; then
  echo 'staging hard lock accepted a mismatched Supabase project ref' >&2
  exit 1
fi

if run_staging_lock wrong-account-identity \
  'CLOUDFLARE_ACCOUNT_ID=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa' >/dev/null 2>&1; then
  echo 'staging hard lock accepted an alternate Cloudflare account' >&2
  exit 1
fi
if run_staging_lock wrong-staging-d1-identity \
  'STAGING_D1_DATABASE_ID=11111111-1111-1111-1111-111111111111' >/dev/null 2>&1; then
  echo 'staging hard lock accepted an alternate Staging D1 database' >&2
  exit 1
fi
if run_staging_lock wrong-production-d1-identity \
  'PRODUCTION_D1_DATABASE_ID=22222222-2222-2222-2222-222222222222' >/dev/null 2>&1; then
  echo 'staging hard lock accepted an alternate Production D1 database' >&2
  exit 1
fi
if run_staging_lock wrong-staging-r2-identity \
  'STAGING_R2_BUCKET_NAME=knock-knock-staging-alt' >/dev/null 2>&1; then
  echo 'staging hard lock accepted an alternate Staging R2 bucket' >&2
  exit 1
fi
if run_staging_lock wrong-production-r2-identity \
  'PRODUCTION_R2_BUCKET_NAME=knock-knock-production-alt' >/dev/null 2>&1; then
  echo 'staging hard lock accepted an alternate Production R2 bucket' >&2
  exit 1
fi
if run_staging_lock wrong-staging-worker-identity \
  'STAGING_WORKER_NAME=knock-knock-backend-staging-alt' >/dev/null 2>&1; then
  echo 'staging hard lock accepted an alternate Staging Worker name' >&2
  exit 1
fi
if run_staging_lock wrong-production-worker-identity \
  'PRODUCTION_WORKER_NAME=knock-knock-backend-production-alt' >/dev/null 2>&1; then
  echo 'staging hard lock accepted an alternate Production Worker name' >&2
  exit 1
fi
if run_staging_lock wrong-staging-route-identity \
  'STAGING_WORKER_ROUTE=knock-knock-backend-staging-alt.wch-klaus.workers.dev' >/dev/null 2>&1; then
  echo 'staging hard lock accepted an alternate Staging Worker route' >&2
  exit 1
fi
if run_staging_lock wrong-production-route-identity \
  'PRODUCTION_WORKER_ROUTE=knock-knock-backend-production-alt.wch-klaus.workers.dev' >/dev/null 2>&1; then
  echo 'staging hard lock accepted an alternate Production Worker route' >&2
  exit 1
fi
if run_staging_lock wrong-staging-supabase-identity \
  'STAGING_SUPABASE_PROJECT_REF=aaaaaaaaaaaaaaaaaaaa' \
  'STAGING_SUPABASE_URL=https://aaaaaaaaaaaaaaaaaaaa.supabase.co' >/dev/null 2>&1; then
  echo 'staging hard lock accepted an alternate Staging Supabase project' >&2
  exit 1
fi
if run_staging_lock wrong-production-supabase-identity \
  'PRODUCTION_SUPABASE_PROJECT_REF=bbbbbbbbbbbbbbbbbbbb' \
  'PRODUCTION_SUPABASE_URL=https://bbbbbbbbbbbbbbbbbbbb.supabase.co' >/dev/null 2>&1; then
  echo 'staging hard lock accepted an alternate Production Supabase project' >&2
  exit 1
fi
if run_staging_lock wrong-staging-url-identity \
  "STAGING_URL=https://${STAGING_ROUTE}/" >/dev/null 2>&1; then
  echo 'staging hard lock accepted an alternate Staging Worker URL' >&2
  exit 1
fi
if run_staging_lock wrong-production-url-identity \
  "PRODUCTION_URL=https://${PRODUCTION_ROUTE}/" >/dev/null 2>&1; then
  echo 'staging hard lock accepted an alternate Production Worker URL' >&2
  exit 1
fi

if run_staging_lock full-environment-swap \
  "STAGING_D1_DATABASE_ID=$PRODUCTION_D1" \
  "PRODUCTION_D1_DATABASE_ID=$STAGING_D1" \
  'STAGING_R2_BUCKET_NAME=knock-knock-production' \
  'PRODUCTION_R2_BUCKET_NAME=knock-knock-staging' \
  "STAGING_SUPABASE_URL=https://${PRODUCTION_REF}.supabase.co" \
  "PRODUCTION_SUPABASE_URL=https://${STAGING_REF}.supabase.co" \
  "STAGING_SUPABASE_PROJECT_REF=$PRODUCTION_REF" \
  "PRODUCTION_SUPABASE_PROJECT_REF=$STAGING_REF" \
  'STAGING_WORKER_NAME=knock-knock-backend-production' \
  'PRODUCTION_WORKER_NAME=knock-knock-backend-staging' \
  "STAGING_WORKER_ROUTE=$PRODUCTION_ROUTE" \
  "PRODUCTION_WORKER_ROUTE=$STAGING_ROUTE" \
  "STAGING_CORS_ORIGIN=https://${PRODUCTION_ROUTE}" \
  "STAGING_URL=https://${PRODUCTION_ROUTE}" \
  "PRODUCTION_URL=https://${STAGING_ROUTE}" >/dev/null 2>&1; then
  echo 'staging hard lock accepted a full Staging/Production identity swap' >&2
  exit 1
fi

if grep -Eiq '0021 is expand-only|Apply Staging expand migrations through 0021|staging-expand-migrations' "$STAGING_DEPLOY_WORKFLOW"; then
  echo 'staging workflow must not describe migration 0021 as expand-only' >&2
  exit 1
fi
migration_gate_line="$(grep -nF 'name: Apply Staging migrations through 0022 before Worker traffic' "$STAGING_DEPLOY_WORKFLOW" | cut -d: -f1)"
dry_run_line="$(grep -nF 'name: Dry-run hard-locked Staging Worker config before D1 changes' "$STAGING_DEPLOY_WORKFLOW" | cut -d: -f1)"
snapshot_line="$(grep -nF 'name: Record current Staging Worker and D1 restore points' "$STAGING_DEPLOY_WORKFLOW" | cut -d: -f1)"
worker_deploy_line="$(grep -nF 'name: Deploy code to the hard-locked Staging Worker' "$STAGING_DEPLOY_WORKFLOW" | cut -d: -f1)"
test -n "$dry_run_line"
test -n "$snapshot_line"
test -n "$migration_gate_line"
test -n "$worker_deploy_line"
test "$dry_run_line" -lt "$snapshot_line"
test "$snapshot_line" -lt "$migration_gate_line"
test "$migration_gate_line" -lt "$worker_deploy_line"
grep -Fq -- '--dry-run' "$STAGING_DEPLOY_WORKFLOW"
grep -Fq "grep -Fqx 'workers_dev = true' \"\$STAGING_WRANGLER_CONFIG\"" "$STAGING_DEPLOY_WORKFLOW"
grep -Fq "assert config[\"workers_dev\"] is True" "$STAGING_DEPLOY_WORKFLOW"
grep -Fq 'assert "routes" not in config' "$STAGING_DEPLOY_WORKFLOW"
grep -Fq 'trg_devices_push_token_normalized_insert' "$STAGING_DEPLOY_WORKFLOW"
grep -Fq 'trg_devices_push_token_normalized_update' "$STAGING_DEPLOY_WORKFLOW"
grep -Fq 'migration_0022' "$STAGING_DEPLOY_WORKFLOW"
grep -Fq 'schema_0022_compatible' "$STAGING_DEPLOY_WORKFLOW"
grep -Fq 'PREVIOUS_SCHEMA_0022_COMPATIBLE' "$STAGING_DEPLOY_WORKFLOW"
grep -Fq "pragma_table_info('agent_listener_leases')" "$STAGING_DEPLOY_WORKFLOW"
for migration_name in \
  0017_agent_chat_bindings.sql \
  0018_listener_lease_fencing.sql \
  0019_phone_ask_claim_fencing.sql \
  0020_session_event_apns_outbox.sql \
  0021_push_registration_uniqueness.sql \
  0022_listener_lease_release.sql; do
  grep -Fq "$migration_name" "$STAGING_DEPLOY_WORKFLOW"
done
grep -Fq 'Do not restore automatically: first review writes after the bookmark' "$STAGING_DEPLOY_WORKFLOW"
grep -Fq "wrangler d1 time-travel restore '\${STAGING_D1_DATABASE_ID}' --bookmark='\${d1_bookmark}'" "$STAGING_DEPLOY_WORKFLOW"
if grep -Eq '^[[:space:]]+wrangler d1 time-travel restore' "$STAGING_DEPLOY_WORKFLOW"; then
  echo 'staging workflow must not automatically overwrite D1 with Time Travel' >&2
  exit 1
fi
if command -v wrangler >/dev/null 2>&1; then
  WRANGLER_LOG_PATH="$SMOKE_DIR/wrangler-time-travel-help.log" \
    WRANGLER_SEND_METRICS=false \
    wrangler d1 time-travel restore --help 2>/dev/null \
    | grep -Fq -- '--bookmark'
fi

echo "production config smoke passed: local defaults are explicit and production is fail-closed"
