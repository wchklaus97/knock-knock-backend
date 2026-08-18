#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" dynamic >/dev/null

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/knock-knock-local-contract.XXXXXX")"
PERSIST_TO="${TMP_DIR}/state"
WORKER_PORT="${WORKER_PORT:-}"
WORKER_PID=""
DISABLED_WORKER_PID=""
FAILURE_REASON=""

stop_process() {
  local pid="$1"
  [[ -n "${pid}" ]] || return 0
  if ! kill -0 "${pid}" 2>/dev/null; then
    wait "${pid}" 2>/dev/null || true
    return 0
  fi
  kill "${pid}" 2>/dev/null || true
  for _ in $(seq 1 20); do
    if ! kill -0 "${pid}" 2>/dev/null; then
      wait "${pid}" 2>/dev/null || true
      return 0
    fi
    sleep 0.25
  done
  kill -KILL "${pid}" 2>/dev/null || true
  wait "${pid}" 2>/dev/null || true
}

print_sanitized_logs() {
  local log_file
  for log_file in "${TMP_DIR}/migrations.log" "${TMP_DIR}/memory-audit.log" "${TMP_DIR}/worker.log" \
    "${TMP_DIR}/disabled-migrations.log" "${TMP_DIR}/disabled-worker.log"; do
    if [[ -f "${log_file}" ]]; then
      "${ROOT_DIR}/scripts/ci-log-sanitize.sh" "${log_file}" >&2 || true
    fi
  done
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  stop_process "${WORKER_PID}"
  stop_process "${DISABLED_WORKER_PID}"
  if ((status != 0)); then
    echo "local-contract-gate failed${FAILURE_REASON:+: ${FAILURE_REASON}}" >&2
    print_sanitized_logs
  fi
  rm -rf "${TMP_DIR}"
  exit "${status}"
}
trap cleanup EXIT INT TERM

fail() {
  FAILURE_REASON="$1"
  exit 1
}

pick_port() {
  python3 - <<'PY'
import socket

sock = socket.socket()
sock.bind(("127.0.0.1", 0))
print(sock.getsockname()[1])
sock.close()
PY
}

wait_for_health() {
  local port="${1:-${WORKER_PORT}}"
  local pid="${2:-${WORKER_PID}}"
  for _ in $(seq 1 180); do
    if curl --fail --silent --show-error --max-time 2 \
      "http://127.0.0.1:${port}/health" >/dev/null 2>&1; then
      return 0
    fi
    if [[ -n "${pid}" ]] && ! kill -0 "${pid}" 2>/dev/null; then
      return 1
    fi
    sleep 1
  done
  return 1
}

WORKER_PORT="${WORKER_PORT:-$(pick_port)}"
cat >"${TMP_DIR}/local.env" <<EOF
NODE_ENV=development
JWT_SECRET=knock-knock-local-contract-jwt-secret
CORS_ORIGIN=*
PUSH_MODE=dev
SERVICE_VERSION=local-contract
ACTION_PROVIDER_MODE=internal
ACTION_REMINDER_ENABLED=true
ACTION_MESSAGE_ENABLED=true
EOF

MODEL_ID="gemma-command"
MODEL_R2_KEY="models/${MODEL_ID}/local-contract-1.0.0.litertlm"
MODEL_ARTIFACT="${TMP_DIR}/${MODEL_ID}.litertlm"
MODEL_PRIVATE_KEY="${TMP_DIR}/voice-model-signing-private.pem"
MODEL_RELEASE_DIR="${TMP_DIR}/voice-model-release"
printf 'local contract model artifact bytes\n' >"${MODEL_ARTIFACT}"
openssl genpkey -algorithm ED25519 -out "${MODEL_PRIVATE_KEY}" >/dev/null 2>&1
"${ROOT_DIR}/scripts/voice-model-release.sh" \
  --artifact "${MODEL_ARTIFACT}" \
  --private-key "${MODEL_PRIVATE_KEY}" \
  --model-version 1.0.0 \
  --output-dir "${MODEL_RELEASE_DIR}" \
  --model-id "${MODEL_ID}" \
  --minimum-capability cpu-v1 >/dev/null
MODEL_MANIFEST_JSON="$(jq -c . "${MODEL_RELEASE_DIR}/manifest.json")"
cat >>"${TMP_DIR}/local.env" <<EOF
VOICE_MODEL_ENABLED=true
VOICE_MODEL_R2_KEY=${MODEL_R2_KEY}
VOICE_MODEL_MANIFEST_JSON=${MODEL_MANIFEST_JSON}
EOF

if ! wrangler d1 migrations apply DB --local \
  --persist-to "${PERSIST_TO}" \
  --config "${ROOT_DIR}/wrangler.toml" \
  --env-file "${TMP_DIR}/local.env" \
  >"${TMP_DIR}/migrations.log" 2>&1; then
  fail "local D1 migrations failed"
fi

if ! wrangler r2 object put "knock-knock-local/${MODEL_R2_KEY}" \
  --local \
  --persist-to "${PERSIST_TO}" \
  --config "${ROOT_DIR}/wrangler.toml" \
  --file "${MODEL_ARTIFACT}" \
  --content-type application/octet-stream \
  --force >"${TMP_DIR}/model-r2.log" 2>&1; then
  fail "local voice model R2 upload failed"
fi

wrangler dev --local --port "${WORKER_PORT}" --test-scheduled \
  --persist-to "${PERSIST_TO}" \
  --env-file "${TMP_DIR}/local.env" \
  --log-level error \
  >"${TMP_DIR}/worker.log" 2>&1 &
WORKER_PID=$!

if ! wait_for_health; then
  fail "Worker did not become ready"
fi

BASE_URL="http://127.0.0.1:${WORKER_PORT}" \
  "${ROOT_DIR}/scripts/provider-observability-smoke.sh"

BASE_URL="http://127.0.0.1:${WORKER_PORT}" \
SMOKE_EMAIL="${SMOKE_EMAIL:-local-contract-$(date +%s)-$$@local.test}" \
  "${ROOT_DIR}/scripts/contract-smoke.sh"

BASE_URL="http://127.0.0.1:${WORKER_PORT}" \
R2_SMOKE_PERSIST_TO="${PERSIST_TO}" \
R2_SMOKE_WRANGLER_CONFIG="${ROOT_DIR}/wrangler.toml" \
R2_SMOKE_BUCKET="knock-knock-local" \
SMOKE_EMAIL="${R2_SMOKE_EMAIL:-local-r2-$(date +%s)-$$@local.test}" \
  "${ROOT_DIR}/scripts/r2-download-smoke.sh"

BASE_URL="http://127.0.0.1:${WORKER_PORT}" \
MODEL_ID="${MODEL_ID}" \
EXPECTED_MODEL_ARTIFACT="${MODEL_ARTIFACT}" \
SMOKE_EMAIL="${MODEL_SMOKE_EMAIL:-local-model-$(date +%s)-$$@local.test}" \
  "${ROOT_DIR}/scripts/voice-model-r2-smoke.sh"

BASE_URL="http://127.0.0.1:${WORKER_PORT}" \
SMOKE_EMAIL="${MEMORY_SMOKE_EMAIL:-local-memory-$(date +%s)-$$@local.test}" \
  "${ROOT_DIR}/scripts/memory-contract-smoke.sh"

if ! wrangler d1 execute DB --local \
  --persist-to "${PERSIST_TO}" \
  --config "${ROOT_DIR}/wrangler.toml" \
  --command "SELECT action, metadata_json FROM audit_logs WHERE action IN ('memory.create', 'memory.delete') ORDER BY created_at, id" \
  --json >"${TMP_DIR}/memory-audit.json" 2>"${TMP_DIR}/memory-audit.log"; then
  fail "local Memory audit query failed"
fi
jq -e '
  .[0].results as $rows |
  ([$rows[] | select(.action == "memory.create")] | length >= 4) and
  ([$rows[] | select(.action == "memory.delete")] | length >= 1) and
  any($rows[];
    .action == "memory.create" and
    ((.metadata_json | fromjson).result == "created")
  ) and
  any($rows[];
    .action == "memory.create" and
    ((.metadata_json | fromjson).result == "replay")
  ) and
  all($rows[];
    (.metadata_json | fromjson) as $metadata |
    if .action == "memory.create" then
      (($metadata | keys | sort) == ["kind", "memory_id", "result", "source_type"])
    else
      (($metadata | keys | sort) == ["kind", "memory_id", "source_type"])
    end
  )
' "${TMP_DIR}/memory-audit.json" >/dev/null || fail "Memory audit metadata was missing or unsafe"

DISABLED_PERSIST_TO="${TMP_DIR}/disabled-state"
DISABLED_WORKER_PORT="$(pick_port)"
cat >"${TMP_DIR}/disabled.env" <<EOF
NODE_ENV=development
JWT_SECRET=knock-knock-disabled-contract-jwt-secret
CORS_ORIGIN=*
PUSH_MODE=dev
SERVICE_VERSION=local-disabled-contract
ACTION_PROVIDER_MODE=disabled
ACTION_REMINDER_ENABLED=false
ACTION_MESSAGE_ENABLED=false
EOF

if ! wrangler d1 migrations apply DB --local \
  --persist-to "${DISABLED_PERSIST_TO}" \
  --config "${ROOT_DIR}/wrangler.toml" \
  --env-file "${TMP_DIR}/disabled.env" \
  >"${TMP_DIR}/disabled-migrations.log" 2>&1; then
  fail "disabled-provider D1 migrations failed"
fi

wrangler dev --local --port "${DISABLED_WORKER_PORT}" --test-scheduled \
  --persist-to "${DISABLED_PERSIST_TO}" \
  --env-file "${TMP_DIR}/disabled.env" \
  --log-level error \
  >"${TMP_DIR}/disabled-worker.log" 2>&1 &
DISABLED_WORKER_PID=$!

if ! wait_for_health "${DISABLED_WORKER_PORT}" "${DISABLED_WORKER_PID}"; then
  fail "disabled-provider Worker did not become ready"
fi

BASE_URL="http://127.0.0.1:${DISABLED_WORKER_PORT}" \
SMOKE_EMAIL="${DISABLED_SMOKE_EMAIL:-local-disabled-$(date +%s)-$$@local.test}" \
  "${ROOT_DIR}/scripts/disabled-send-message-smoke.sh"

echo "local-contract-gate passed: isolated Worker/D1/R2 contract, readiness, correlation, retention, model streaming, isolation smokes, and disabled send_message fail-closed"
