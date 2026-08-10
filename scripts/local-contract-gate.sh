#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"
TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/knock-knock-local-contract.XXXXXX")"
PERSIST_TO="${TMP_DIR}/state"
WORKER_PORT="${WORKER_PORT:-}"
WORKER_PID=""

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

cleanup() {
  stop_process "${WORKER_PID}"
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

wait_for_health() {
  # A clean CI runner may compile the Rust Worker before Wrangler can serve
  # /health. Keep this bounded, but allow enough time for the first build.
  for _ in $(seq 1 180); do
    if curl --fail --silent --max-time 2 "http://127.0.0.1:${WORKER_PORT}/health" >/dev/null 2>&1; then
      return 0
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

wrangler d1 migrations apply DB --local \
  --persist-to "${PERSIST_TO}" \
  --config "${ROOT_DIR}/wrangler.toml" \
  --env-file "${TMP_DIR}/local.env" \
  >"${TMP_DIR}/migrations.log" 2>&1

wrangler dev --local --port "${WORKER_PORT}" --test-scheduled \
  --persist-to "${PERSIST_TO}" \
  --env-file "${TMP_DIR}/local.env" \
  --log-level error \
  >"${TMP_DIR}/worker.log" 2>&1 &
WORKER_PID=$!

if ! wait_for_health; then
  echo "local-contract-gate: Worker did not become ready" >&2
  sed -E 's/(JWT_SECRET=)[^[:space:]]+/\1[REDACTED]/g' "${TMP_DIR}/migrations.log" "${TMP_DIR}/worker.log" >&2 || true
  exit 1
fi

BASE_URL="http://127.0.0.1:${WORKER_PORT}" \
SMOKE_EMAIL="${SMOKE_EMAIL:-local-contract-$(date +%s)-$$@local.test}" \
  "${ROOT_DIR}/scripts/contract-smoke.sh"

BASE_URL="http://127.0.0.1:${WORKER_PORT}" \
R2_SMOKE_PERSIST_TO="${PERSIST_TO}" \
R2_SMOKE_WRANGLER_CONFIG="${ROOT_DIR}/wrangler.toml" \
R2_SMOKE_BUCKET="knock-knock-local" \
SMOKE_EMAIL="${R2_SMOKE_EMAIL:-local-r2-$(date +%s)-$$@local.test}" \
  "${ROOT_DIR}/scripts/r2-download-smoke.sh"

echo "local-contract-gate passed: isolated Worker/D1/R2 contract, retrieval, retention, and isolation smoke"
