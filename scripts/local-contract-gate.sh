#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" dynamic >/dev/null

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/knock-knock-local-contract.XXXXXX")"
PERSIST_TO="${TMP_DIR}/state"
WORKER_PORT="${WORKER_PORT:-}"
WORKER_PID=""
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
  for log_file in "${TMP_DIR}/migrations.log" "${TMP_DIR}/worker.log"; do
    if [[ -f "${log_file}" ]]; then
      "${ROOT_DIR}/scripts/ci-log-sanitize.sh" "${log_file}" >&2 || true
    fi
  done
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  stop_process "${WORKER_PID}"
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
  for _ in $(seq 1 180); do
    if curl --fail --silent --show-error --max-time 2 \
      "http://127.0.0.1:${WORKER_PORT}/health" >/dev/null 2>&1; then
      return 0
    fi
    if [[ -n "${WORKER_PID}" ]] && ! kill -0 "${WORKER_PID}" 2>/dev/null; then
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

if ! wrangler d1 migrations apply DB --local \
  --persist-to "${PERSIST_TO}" \
  --config "${ROOT_DIR}/wrangler.toml" \
  --env-file "${TMP_DIR}/local.env" \
  >"${TMP_DIR}/migrations.log" 2>&1; then
  fail "local D1 migrations failed"
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

echo "local-contract-gate passed: isolated Worker/D1/R2 contract, readiness, correlation, retention, and isolation smokes"
