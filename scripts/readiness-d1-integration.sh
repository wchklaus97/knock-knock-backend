#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/knock-knock-readiness-d1.XXXXXX")"
PERSIST_TO="${TMP_DIR}/state"
WORKER_PID=""

stop_worker() {
  [[ -n "${WORKER_PID}" ]] || return 0
  kill "${WORKER_PID}" 2>/dev/null || true
  wait "${WORKER_PID}" 2>/dev/null || true
}

cleanup() {
  stop_worker
  rm -rf "${TMP_DIR}"
}
trap cleanup EXIT INT TERM

pick_port() {
  python3 - <<'PY'
import socket

sock = socket.socket()
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
}

cat >"${TMP_DIR}/local.env" <<'EOF'
NODE_ENV=development
JWT_SECRET=knock-knock-readiness-integration-secret
CORS_ORIGIN=*
PUSH_MODE=dev
APNS_BUNDLE_ID=hk.knockknock.app
SERVICE_VERSION=readiness-integration
ACTION_PROVIDER_MODE=internal
ACTION_REMINDER_ENABLED=true
ACTION_MESSAGE_ENABLED=true
EOF

wrangler d1 migrations apply DB --local \
  --persist-to "${PERSIST_TO}" \
  --config "${ROOT_DIR}/wrangler.toml" \
  --env-file "${TMP_DIR}/local.env" >/dev/null

d1_json() {
  wrangler d1 execute DB --local \
    --persist-to "${PERSIST_TO}" \
    --config "${ROOT_DIR}/wrangler.toml" \
    --json --command "$1"
}

d1_exec() {
  d1_json "$1" >/dev/null
}

trigger_catalog="$(d1_json "SELECT name, sql FROM sqlite_master WHERE type = 'trigger' AND name IN ('trg_devices_push_token_normalized_insert','trg_devices_push_token_normalized_update') ORDER BY name;")"
insert_trigger_sql="$(jq -er '.[0].results[] | select(.name == "trg_devices_push_token_normalized_insert") | .sql' <<<"${trigger_catalog}")"
update_trigger_sql="$(jq -er '.[0].results[] | select(.name == "trg_devices_push_token_normalized_update") | .sql' <<<"${trigger_catalog}")"

PORT="$(pick_port)"
wrangler dev --local --port "${PORT}" \
  --persist-to "${PERSIST_TO}" \
  --env-file "${TMP_DIR}/local.env" \
  --config "${ROOT_DIR}/wrangler.toml" \
  --log-level error >"${TMP_DIR}/worker.log" 2>&1 &
WORKER_PID=$!

for _ in $(seq 1 180); do
  if curl --fail --silent --show-error --max-time 2 "http://127.0.0.1:${PORT}/health" >/dev/null 2>&1; then
    break
  fi
  if ! kill -0 "${WORKER_PID}" 2>/dev/null; then
    cat "${TMP_DIR}/worker.log" >&2
    exit 1
  fi
  sleep 1
done

assert_ready() {
  local body="${TMP_DIR}/ready.json"
  local status
  status="$(curl --silent --show-error --output "${body}" --write-out '%{http_code}' "http://127.0.0.1:${PORT}/ready")"
  test "${status}" = "200"
  jq -e '
    .ok == true and
    .schema_ready == true and
    .runtime_configuration_ready == true and
    .schema_0022_compatible == true and
    .apns_bundle_id == "hk.knockknock.app" and
    .apns_bundle_id_ready == true
  ' "${body}" >/dev/null
}

assert_not_ready() {
  local label="$1"
  local body="${TMP_DIR}/not-ready-${label}.json"
  local status
  status="$(curl --silent --show-error --output "${body}" --write-out '%{http_code}' "http://127.0.0.1:${PORT}/ready")"
  test "${status}" = "503"
  jq -e '
    .ok == false and
    .schema_ready == false and
    .code == "schema_not_ready" and
    .schema_0022_compatible == false and
    .apns_bundle_id == "hk.knockknock.app" and
    .apns_bundle_id_ready == true
  ' "${body}" >/dev/null
}

assert_ready

d1_exec "DROP TRIGGER trg_devices_push_token_normalized_insert;"
assert_not_ready "missing-insert-trigger"
d1_exec "${insert_trigger_sql}"
assert_ready

d1_exec "DROP TRIGGER trg_devices_push_token_normalized_update;"
assert_not_ready "missing-update-trigger"
d1_exec "${update_trigger_sql}"
assert_ready

for migration in \
  0017_agent_chat_bindings.sql \
  0018_listener_lease_fencing.sql \
  0019_phone_ask_claim_fencing.sql \
  0020_session_event_apns_outbox.sql \
  0021_push_registration_uniqueness.sql \
  0022_listener_lease_release.sql; do
  d1_exec "DELETE FROM d1_migrations WHERE name = '${migration}';"
  assert_not_ready "missing-${migration%.sql}"
  d1_exec "INSERT INTO d1_migrations (name) VALUES ('${migration}');"
  assert_ready
done

echo "readiness D1 integration passed: fully migrated ready; each 0021 trigger and each 0017-0022 ledger row fail closed when removed"
