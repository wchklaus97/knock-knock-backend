#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${ROOT_DIR}"
"${ROOT_DIR}/scripts/ci-prerequisites.sh" dynamic >/dev/null

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/knock-knock-provider-local.XXXXXX")"
PROVIDER_PORT="${PROVIDER_PORT:-}"
WORKER_PORT="${WORKER_PORT:-}"
PERSIST_TO="${TMP_DIR}/state"
TOKEN="knock-knock-local-provider-fixture"
PROVIDER_PID=""
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
  for log_file in "${TMP_DIR}/migrations.log" "${TMP_DIR}/worker.log" "${TMP_DIR}/provider.log"; do
    if [[ -f "${log_file}" ]]; then
      sed "s|${TOKEN}|[REDACTED]|g" "${log_file}" \
        | "${ROOT_DIR}/scripts/ci-log-sanitize.sh" >&2 || true
    fi
  done
}

cleanup() {
  local status=$?
  trap - EXIT INT TERM
  stop_process "${WORKER_PID}"
  stop_process "${PROVIDER_PID}"
  if ((status != 0)); then
    echo "provider-local-gate failed${FAILURE_REASON:+: ${FAILURE_REASON}}" >&2
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

wait_for_http() {
  local url="$1"
  local attempts="${2:-30}"
  for _ in $(seq 1 "${attempts}"); do
    if curl --fail --silent --show-error --max-time 2 "${url}" >/dev/null 2>&1; then
      return 0
    fi
    if [[ -n "${PROVIDER_PID}" ]] && ! kill -0 "${PROVIDER_PID}" 2>/dev/null; then
      return 1
    fi
    if [[ -n "${WORKER_PID}" ]] && ! kill -0 "${WORKER_PID}" 2>/dev/null; then
      return 1
    fi
    sleep 1
  done
  return 1
}

wait_for_worker_ready() {
  for _ in $(seq 1 120); do
    if curl --fail --silent --show-error --max-time 2 \
      "http://127.0.0.1:${WORKER_PORT}/health" 2>/dev/null \
      | jq -e '.ok == true and .action_provider_mode == "external" and .action_provider_ready == true' \
      >/dev/null; then
      return 0
    fi
    if [[ -n "${WORKER_PID}" ]] && ! kill -0 "${WORKER_PID}" 2>/dev/null; then
      return 1
    fi
    sleep 1
  done
  return 1
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

PROVIDER_PORT="${PROVIDER_PORT:-$(pick_port)}"
WORKER_PORT="${WORKER_PORT:-$(pick_port)}"

cat >"${TMP_DIR}/provider.env" <<EOF
NODE_ENV=development
JWT_SECRET=knock-knock-provider-local-jwt-secret
CORS_ORIGIN=*
PUSH_MODE=dev
SERVICE_VERSION=provider-local
ACTION_PROVIDER_MODE=external
ACTION_REMINDER_ENABLED=true
ACTION_MESSAGE_ENABLED=true
ACTION_REMINDER_URL=http://127.0.0.1:${PROVIDER_PORT}/reminders/deliver
ACTION_REMINDER_STATUS_URL=http://127.0.0.1:${PROVIDER_PORT}/reminders/status
ACTION_REMINDER_CANCEL_URL=http://127.0.0.1:${PROVIDER_PORT}/reminders/cancel
ACTION_MESSAGE_URL=http://127.0.0.1:${PROVIDER_PORT}/messages/deliver
ACTION_MESSAGE_STATUS_URL=http://127.0.0.1:${PROVIDER_PORT}/messages/status
ACTION_REMINDER_TOKEN=${TOKEN}
ACTION_MESSAGE_TOKEN=${TOKEN}
EOF

python3 "${ROOT_DIR}/scripts/provider-mock.py" \
  --host 127.0.0.1 --port "${PROVIDER_PORT}" --token "${TOKEN}" \
  >"${TMP_DIR}/provider.log" 2>&1 &
PROVIDER_PID=$!

if ! wait_for_http "http://127.0.0.1:${PROVIDER_PORT}/health"; then
  fail "mock provider did not become ready"
fi

if ! wrangler d1 migrations apply DB --local \
  --persist-to "${PERSIST_TO}" \
  --config "${ROOT_DIR}/wrangler.toml" \
  --env-file "${TMP_DIR}/provider.env" \
  >"${TMP_DIR}/migrations.log" 2>&1; then
  fail "local provider D1 migrations failed"
fi

wrangler dev --local --port "${WORKER_PORT}" --test-scheduled \
  --persist-to "${PERSIST_TO}" \
  --env-file "${TMP_DIR}/provider.env" \
  --log-level error \
  >"${TMP_DIR}/worker.log" 2>&1 &
WORKER_PID=$!

if ! wait_for_worker_ready; then
  fail "Worker did not become external/ready"
fi

BASE_URL="http://127.0.0.1:${WORKER_PORT}" \
EXPECTED_PROVIDER_READY=true \
EXPECTED_APNS_READY=false \
  "${ROOT_DIR}/scripts/provider-observability-smoke.sh"

if grep -Fq "${TOKEN}" "${TMP_DIR}/provider.log" "${TMP_DIR}/worker.log"; then
  fail "provider credentials appeared in local logs"
fi

BASE_URL="http://127.0.0.1:${WORKER_PORT}" \
PROVIDER_RECONCILE_WAIT_SECONDS="${PROVIDER_RECONCILE_WAIT_SECONDS:-6}" \
PROVIDER_LOG="${TMP_DIR}/provider.log" \
SMOKE_EMAIL="${SMOKE_EMAIL:-provider-local-$(date +%s)-$$@local.test}" \
  "${ROOT_DIR}/scripts/provider-lifecycle-smoke.sh" || fail "provider lifecycle smoke failed"

echo "provider-local-gate passed: isolated Worker/D1 plus deterministic local provider with redacted diagnostics"
