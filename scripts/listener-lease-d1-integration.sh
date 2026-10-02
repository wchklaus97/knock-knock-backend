#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR=""
TEMP_ROOT="${TMPDIR:-/tmp}"
TEMP_ROOT="${TEMP_ROOT%/}"
TEMP_DIR=""
WORKER_PID=""
WORKER_PGID=""
CLEANUP_STARTED=0
BACKGROUND_PIDS=()

track_background_pid() {
  BACKGROUND_PIDS+=("$1")
}

untrack_background_pid() {
  local completed_pid="$1"
  local tracked_pid
  local remaining=()
  for tracked_pid in "${BACKGROUND_PIDS[@]}"; do
    if [[ "${tracked_pid}" != "${completed_pid}" ]]; then
      remaining+=("${tracked_pid}")
    fi
  done
  if [[ "${#remaining[@]}" -eq 0 ]]; then
    BACKGROUND_PIDS=()
  else
    BACKGROUND_PIDS=("${remaining[@]}")
  fi
}

wait_tracked_background_pid() {
  local pid="$1"
  local status
  if wait "${pid}"; then
    untrack_background_pid "${pid}"
    return 0
  else
    status=$?
    untrack_background_pid "${pid}"
    return "${status}"
  fi
}

stop_background_processes() {
  local pid
  local watchdog_pid=""
  [[ "${#BACKGROUND_PIDS[@]}" -gt 0 ]] || return 0

  for pid in "${BACKGROUND_PIDS[@]}"; do
    kill -TERM "${pid}" 2>/dev/null || true
  done
  (
    sleep 2
    for pid in "${BACKGROUND_PIDS[@]}"; do
      kill -KILL "${pid}" 2>/dev/null || true
    done
  ) &
  watchdog_pid=$!
  for pid in "${BACKGROUND_PIDS[@]}"; do
    wait "${pid}" 2>/dev/null || true
  done
  kill "${watchdog_pid}" 2>/dev/null || true
  wait "${watchdog_pid}" 2>/dev/null || true
  BACKGROUND_PIDS=()
}

worker_group_alive() {
  [[ -n "${WORKER_PGID:-}" ]] \
    && kill -0 -- "-${WORKER_PGID}" 2>/dev/null
}

worker_pid_alive() {
  [[ -n "${WORKER_PID:-}" ]] \
    && kill -0 "${WORKER_PID}" 2>/dev/null
}

stop_worker() {
  local pid="${WORKER_PID:-}"
  local pgid="${WORKER_PGID:-}"
  local watchdog_pid=""
  local group_still_alive=0
  [[ -n "${pid}" || -n "${pgid}" ]] || return 0

  [[ -z "${pgid}" ]] || kill -TERM -- "-${pgid}" 2>/dev/null || true
  [[ -z "${pid}" ]] || kill -TERM "${pid}" 2>/dev/null || true

  if [[ -n "${pid}" ]]; then
    (
      sleep 5
      [[ -z "${pgid}" ]] || kill -KILL -- "-${pgid}" 2>/dev/null || true
      kill -KILL "${pid}" 2>/dev/null || true
    ) &
    watchdog_pid=$!
    wait "${pid}" 2>/dev/null || true
    kill "${watchdog_pid}" 2>/dev/null || true
    wait "${watchdog_pid}" 2>/dev/null || true
  fi

  for _ in $(seq 1 20); do
    if ! worker_group_alive && ! worker_pid_alive; then
      break
    fi
    [[ -z "${pgid}" ]] || kill -KILL -- "-${pgid}" 2>/dev/null || true
    [[ -z "${pid}" ]] || kill -KILL "${pid}" 2>/dev/null || true
    sleep 0.05
  done
  if worker_group_alive || worker_pid_alive; then
    group_still_alive=1
  else
    WORKER_PID=""
    WORKER_PGID=""
  fi
  [[ "${group_still_alive}" == "0" ]]
}

remove_temp_dir() {
  local attempt
  local remove_error=""
  [[ -n "${TEMP_DIR:-}" ]] || return 0
  case "${TEMP_DIR}" in
    "${TEMP_ROOT}"/knock-knock-listener-lease-d1.*) ;;
    *)
      printf 'listener-lease-d1-integration cleanup refused unsafe path: %s\n' \
        "${TEMP_DIR}" >&2
      return 1
      ;;
  esac

  for attempt in $(seq 1 30); do
    if [[ ! -e "${TEMP_DIR}" ]]; then
      TEMP_DIR=""
      return 0
    fi
    if ! remove_error="$(rm -rf -- "${TEMP_DIR}" 2>&1)"; then
      :
    fi
    if [[ ! -e "${TEMP_DIR}" ]]; then
      TEMP_DIR=""
      return 0
    fi
    sleep 0.1
  done
  printf 'listener-lease-d1-integration cleanup failed after 30 attempts: %s%s\n' \
    "${TEMP_DIR}" "${remove_error:+: ${remove_error}}" >&2
  return 1
}

cleanup() {
  local status="${1:-$?}"
  local cleanup_failed=0
  if [[ "${CLEANUP_STARTED}" == "1" ]]; then
    exit "${status}"
  fi
  CLEANUP_STARTED=1
  trap - EXIT HUP INT TERM

  stop_background_processes || cleanup_failed=1
  stop_worker || cleanup_failed=1
  remove_temp_dir || cleanup_failed=1
  if [[ "${status}" == "0" && "${cleanup_failed}" != "0" ]]; then
    status=1
  fi
  exit "${status}"
}

trap 'cleanup "$?"' EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TEMP_DIR="$(mktemp -d "${TEMP_ROOT}/knock-knock-listener-lease-d1.XXXXXX")"
PERSIST_TO="${TEMP_DIR}/state"
WRANGLER_CONFIG="${TEMP_DIR}/wrangler.toml"
MIGRATIONS_DIR="${TEMP_DIR}/migrations"
D1_STDERR="${TEMP_DIR}/d1.stderr"

fail() {
  printf 'listener-lease-d1-integration failed: %s\n' "$1" >&2
  exit 1
}

for command_name in wrangler node jq curl python3; do
  command -v "${command_name}" >/dev/null 2>&1 \
    || fail "required local command is unavailable: ${command_name}"
done

WRANGLER_BIN="$(command -v wrangler)"
NODE_BIN="$(command -v node)"
PYTHON_BIN="$(command -v python3)"
JQ_BIN="$(command -v jq)"
[[ "${WRANGLER_BIN}" = /* ]] || fail "wrangler must resolve to a local absolute path"
[[ "${NODE_BIN}" = /* ]] || fail "node must resolve to a local absolute path"
[[ "${PYTHON_BIN}" = /* ]] || fail "python3 must resolve to a local absolute path"
SAFE_PATH="$(dirname "${NODE_BIN}"):/usr/bin:/bin"

BASE_MIGRATIONS=(
  0001_initial.sql
  0002_supabase_auth.sql
  0003_architecture_foundation.sql
  0004_command_versions.sql
  0005_phone_change_triggers.sql
  0006_history_and_phone_idempotency.sql
  0007_rate_limits.sql
  0008_history_consistency.sql
  0009_phone_operation_claim_tokens.sql
  0010_vertical_action_effects.sql
  0011_command_pairing_action_descriptors.sql
  0012_reminder_delivery_state.sql
  0013_retrieval_retention_status.sql
  0014_command_safety.sql
  0015_structured_memory.sql
  0016_phone_asks.sql
  0017_agent_chat_bindings.sql
  0018_listener_lease_fencing.sql
  0019_phone_ask_claim_fencing.sql
  0020_session_event_apns_outbox.sql
)
PUSH_REGISTRATION_MIGRATION=0021_push_registration_uniqueness.sql
RELEASE_MIGRATION=0022_listener_lease_release.sql
MIGRATIONS=("${BASE_MIGRATIONS[@]}" "${PUSH_REGISTRATION_MIGRATION}" "${RELEASE_MIGRATION}")

mkdir -p "${MIGRATIONS_DIR}" "${PERSIST_TO}" "${TEMP_DIR}/tmp" "${TEMP_DIR}/xdg"
for migration in "${BASE_MIGRATIONS[@]}"; do
  source_path="${ROOT_DIR}/migrations/${migration}"
  [[ -f "${source_path}" ]] || fail "missing migration: ${migration}"
  ln -s "${source_path}" "${MIGRATIONS_DIR}/${migration}"
done

cat >"${WRANGLER_CONFIG}" <<'TOML'
name = "knock-knock-listener-lease-d1-integration"
main = "lease-worker.js"
compatibility_date = "2026-04-16"

[[d1_databases]]
binding = "DB"
database_name = "knock-knock-listener-lease-d1-integration"
database_id = "local"
migrations_dir = "migrations"
TOML

cat >"${TEMP_DIR}/lease-worker.js" <<'JS'
const ACQUIRE_SQL = `
INSERT INTO agent_listener_leases (
  agent_id, user_id, binding_id, chat_id, chat_title, listener_instance_id,
  lease_id, generation, acquired_at, last_seen_at, expires_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?)
ON CONFLICT(agent_id) DO UPDATE SET
  user_id = excluded.user_id,
  binding_id = excluded.binding_id,
  chat_id = excluded.chat_id,
  chat_title = excluded.chat_title,
  listener_instance_id = excluded.listener_instance_id,
  lease_id = excluded.lease_id,
  generation = agent_listener_leases.generation + 1,
  acquired_at = excluded.acquired_at,
  last_seen_at = excluded.last_seen_at,
  expires_at = excluded.expires_at,
  released_at = NULL,
  updated_at = excluded.updated_at
WHERE agent_listener_leases.expires_at <= excluded.acquired_at
   OR ? = 1
   OR (
     agent_listener_leases.chat_id = excluded.chat_id
     AND agent_listener_leases.lease_id = ?
     AND agent_listener_leases.generation = ?
   )
RETURNING
  agent_id, user_id, binding_id, chat_id, chat_title, listener_instance_id,
  lease_id, generation, acquired_at, last_seen_at, expires_at, updated_at
`;

const RELEASE_PUSH_TOKEN_SQL = `
UPDATE devices
SET push_token = NULL, updated_at = ?
WHERE push_token = ?
  AND NOT (user_id = ? AND platform = ? AND device_id = ?)
`;

const UPSERT_DEVICE_SQL = `
INSERT INTO devices (
  id, user_id, platform, device_id, push_token, locale, timezone,
  created_at, updated_at
) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT(user_id, platform, device_id) DO UPDATE SET
  push_token = excluded.push_token,
  locale = excluded.locale,
  timezone = excluded.timezone,
  updated_at = excluded.updated_at
RETURNING id, user_id, platform, device_id, push_token
`;

const CANDIDATES = Object.freeze({
  a: Object.freeze({
    bindingId: "binding-a",
    chatId: "chat-a",
    chatTitle: "Chat A",
    instanceId: "instance-a-0001",
    leaseId: "lease-parallel-a",
  }),
  b: Object.freeze({
    bindingId: "binding-b",
    chatId: "chat-b",
    chatTitle: "Chat B",
    instanceId: "instance-b-0001",
    leaseId: "lease-parallel-b",
  }),
});

const ACQUIRED_AT = "2040-01-01T00:00:00.000Z";
const EXPIRES_AT = "2040-01-01T00:01:30.000Z";

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    if (request.method === "GET" && url.pathname === "/health") {
      return Response.json({ ok: true });
    }
    if (request.method === "GET" && url.pathname === "/registration-state") {
      const pushToken = url.searchParams.get("pushToken");
      const userId = url.searchParams.get("userId");
      const deviceId = url.searchParams.get("deviceId");
      const result = pushToken
        ? await env.DB.prepare(
            "SELECT id, user_id, platform, device_id, push_token FROM devices WHERE push_token = ? ORDER BY id",
          ).bind(pushToken).all()
        : await env.DB.prepare(
            "SELECT id, user_id, platform, device_id, push_token FROM devices WHERE user_id = ? AND platform = 'ios' AND device_id = ? ORDER BY id",
          ).bind(userId, deviceId).all();
      return Response.json({ results: result.results ?? [] });
    }
    if (request.method === "POST" && url.pathname === "/register-device") {
      const body = await request.json();
      const now = "2040-01-01T00:00:00.000Z";
      const statements = [];
      if (body.pushToken) {
        statements.push(
          env.DB.prepare(RELEASE_PUSH_TOKEN_SQL).bind(
            now,
            body.pushToken,
            body.userId,
            "ios",
            body.deviceId,
          ),
        );
      }
      statements.push(
        env.DB.prepare(UPSERT_DEVICE_SQL).bind(
          body.id,
          body.userId,
          "ios",
          body.deviceId,
          body.pushToken ?? null,
          "en",
          "UTC",
          now,
          now,
        ),
      );
      const results = await env.DB.batch(statements);
      const upsert = results[results.length - 1];
      return Response.json((upsert.results ?? [])[0] ?? null);
    }
    if (request.method !== "POST" || url.pathname !== "/acquire") {
      return new Response("not found", { status: 404 });
    }

    const body = await request.json();
    const candidate = CANDIDATES[body.candidate];
    if (!candidate) {
      return Response.json({ error: "invalid candidate" }, { status: 400 });
    }

    const result = await env.DB.prepare(ACQUIRE_SQL)
      .bind(
        "agent-lease",
        "user-lease",
        candidate.bindingId,
        candidate.chatId,
        candidate.chatTitle,
        candidate.instanceId,
        candidate.leaseId,
        ACQUIRED_AT,
        ACQUIRED_AT,
        EXPIRES_AT,
        ACQUIRED_AT,
        0,
        null,
        0,
      )
      .all();
    return Response.json({ results: result.results ?? [] });
  },
};
JS

# A clean environment prevents inherited Cloudflare credentials or repository
# dotenv files from participating. Every invocation also requires --local and
# uses the temporary Miniflare persistence directory above.
run_wrangler() {
  (
    cd "${TEMP_DIR}"
    exec env -i \
      PATH="${SAFE_PATH}" \
      CI=true \
      NO_COLOR=1 \
      TMPDIR="${TEMP_DIR}/tmp" \
      XDG_CONFIG_HOME="${TEMP_DIR}/xdg" \
      WRANGLER_SEND_METRICS=false \
      WRANGLER_LOG_PATH="${TEMP_DIR}/wrangler-$$-${RANDOM}.log" \
      "${WRANGLER_BIN}" "$@"
  )
}

start_worker() {
  (
    cd "${TEMP_DIR}"
    exec env -i \
      PATH="${SAFE_PATH}" \
      CI=true \
      NO_COLOR=1 \
      TMPDIR="${TEMP_DIR}/tmp" \
      XDG_CONFIG_HOME="${TEMP_DIR}/xdg" \
      WRANGLER_SEND_METRICS=false \
      WRANGLER_LOG_PATH="${TEMP_DIR}/wrangler-$$-${RANDOM}.log" \
      "${PYTHON_BIN}" -c \
      'import os, sys; os.setsid(); os.execvpe(sys.argv[1], sys.argv[1:], os.environ)' \
      "${WRANGLER_BIN}" "$@"
  ) >"${TEMP_DIR}/worker.stdout" 2>"${TEMP_DIR}/worker.stderr" &
  WORKER_PID=$!
  WORKER_PGID="${WORKER_PID}"
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

wait_for_worker() {
  local url="$1"
  for _ in $(seq 1 100); do
    if curl --fail --silent --show-error --max-time 1 "${url}/health" >/dev/null 2>&1; then
      return 0
    fi
    if [[ -n "${WORKER_PID}" ]] && ! kill -0 "${WORKER_PID}" 2>/dev/null; then
      return 1
    fi
    sleep 0.1
  done
  return 1
}

d1_json() {
  local sql="$1"
  local output
  if ! output="$(run_wrangler d1 execute DB \
    --local \
    --persist-to "${PERSIST_TO}" \
    --config "${WRANGLER_CONFIG}" \
    --command "${sql}" \
    --json 2>>"${D1_STDERR}")"; then
    fail "local D1 statement failed"
  fi
  if ! "${JQ_BIN}" -e \
    'type == "array" and length == 1 and (.[0].results | type == "array")' \
    <<<"${output}" >/dev/null; then
    fail "local D1 returned an unexpected JSON shape"
  fi
  printf '%s\n' "${output}"
}

assert_result_count() {
  local json="$1"
  local expected="$2"
  local label="$3"
  local actual
  actual="$("${JQ_BIN}" -r '.[0].results | length' <<<"${json}")"
  [[ "${actual}" == "${expected}" ]] \
    || fail "${label}: expected ${expected} RETURNING row(s), got ${actual}"
}

assert_jq() {
  local json="$1"
  local filter="$2"
  local label="$3"
  "${JQ_BIN}" -e "${filter}" <<<"${json}" >/dev/null \
    || fail "${label}"
}

acquire_sql() {
  local binding_id="$1"
  local chat_id="$2"
  local chat_title="$3"
  local listener_instance_id="$4"
  local lease_id="$5"
  local acquired_at="$6"
  local expires_at="$7"
  local takeover="$8"
  local presented_lease_id="$9"
  local presented_generation="${10}"
  local presented_lease_sql="NULL"
  if [[ -n "${presented_lease_id}" ]]; then
    presented_lease_sql="'${presented_lease_id}'"
  fi

  cat <<SQL
INSERT INTO agent_listener_leases (
  agent_id, user_id, binding_id, chat_id, chat_title, listener_instance_id,
  lease_id, generation, acquired_at, last_seen_at, expires_at, updated_at
) VALUES (
  'agent-lease', 'user-lease', '${binding_id}', '${chat_id}', '${chat_title}',
  '${listener_instance_id}', '${lease_id}', 1, '${acquired_at}', '${acquired_at}',
  '${expires_at}', '${acquired_at}'
)
ON CONFLICT(agent_id) DO UPDATE SET
  user_id = excluded.user_id,
  binding_id = excluded.binding_id,
  chat_id = excluded.chat_id,
  chat_title = excluded.chat_title,
  listener_instance_id = excluded.listener_instance_id,
  lease_id = excluded.lease_id,
  generation = agent_listener_leases.generation + 1,
  acquired_at = excluded.acquired_at,
  last_seen_at = excluded.last_seen_at,
  expires_at = excluded.expires_at,
  released_at = NULL,
  updated_at = excluded.updated_at
WHERE agent_listener_leases.expires_at <= excluded.acquired_at
   OR ${takeover} = 1
   OR (
     agent_listener_leases.chat_id = excluded.chat_id
     AND agent_listener_leases.lease_id = ${presented_lease_sql}
     AND agent_listener_leases.generation = ${presented_generation}
   )
RETURNING
  agent_id, user_id, binding_id, chat_id, chat_title, listener_instance_id,
  lease_id, generation, acquired_at, last_seen_at, expires_at, updated_at;
SQL
}

heartbeat_sql() {
  local lease_id="$1"
  local generation="$2"
  local now="$3"
  local expires_at="$4"
  cat <<SQL
UPDATE agent_listener_leases
SET last_seen_at = '${now}', expires_at = '${expires_at}', updated_at = '${now}'
WHERE agent_id = 'agent-lease'
  AND lease_id = '${lease_id}'
  AND generation = ${generation}
  AND released_at IS NULL
  AND expires_at > '${now}'
RETURNING
  agent_id, user_id, binding_id, chat_id, chat_title, listener_instance_id,
  lease_id, generation, acquired_at, last_seen_at, expires_at, updated_at;
SQL
}

release_sql() {
  local chat_id="$1"
  local listener_instance_id="$2"
  local lease_id="$3"
  local generation="$4"
  local now="$5"
  cat <<SQL
UPDATE agent_listener_leases
SET
  expires_at = CASE WHEN expires_at > '${now}' THEN '${now}' ELSE expires_at END,
  released_at = COALESCE(released_at, '${now}'),
  updated_at = CASE WHEN released_at IS NULL THEN '${now}' ELSE updated_at END
WHERE agent_id = 'agent-lease'
  AND user_id = 'user-lease'
  AND chat_id = '${chat_id}'
  AND listener_instance_id = '${listener_instance_id}'
  AND lease_id = '${lease_id}'
  AND generation = ${generation}
  AND (expires_at > '${now}' OR released_at IS NOT NULL)
RETURNING
  agent_id, user_id, binding_id, chat_id, listener_instance_id,
  lease_id, generation, expires_at, released_at, updated_at;
SQL
}

if ! run_wrangler d1 migrations apply DB \
  --local \
  --persist-to "${PERSIST_TO}" \
  --config "${WRANGLER_CONFIG}" \
  >"${TEMP_DIR}/migrations.stdout" 2>"${TEMP_DIR}/migrations.stderr"; then
  sed -n '1,160p' "${TEMP_DIR}/migrations.stderr" >&2
  fail "Wrangler could not apply migrations 0001..0020"
fi

cat >"${TEMP_DIR}/push-migration-fixtures.sql" <<'SQL'
PRAGMA foreign_keys = ON;

INSERT INTO users (id, email, password_hash, created_at) VALUES
  ('user-push-a', 'push-a@example.test', 'local-fixture-only', '2025-12-31T00:00:00.000Z'),
  ('user-push-b', 'push-b@example.test', 'local-fixture-only', '2025-12-31T00:00:00.000Z');

INSERT INTO devices (
  id, user_id, platform, device_id, push_token, locale, timezone,
  created_at, updated_at
) VALUES
  ('dev-migrate-old', 'user-push-a', 'iOS', ' device-migrate ', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'en', 'UTC', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:01.000Z'),
  ('dev-migrate-latest', 'user-push-a', 'ios ', 'device-migrate', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 'en', 'UTC', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:02.000Z'),
  ('dev-migrate-invalid-newer', 'user-push-a', 'ios', 'device-migrate', 'not-hex', 'en', 'UTC', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:03.000Z'),
  ('dev-token-old-owner', 'user-push-a', 'ios', 'token-old-device', UPPER('cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc'), 'en', 'UTC', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:04.000Z'),
  ('dev-token-new-owner', 'user-push-b', 'ios', 'token-new-device', 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', 'en', 'UTC', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:05.000Z'),
  ('dev-legacy-old', 'user-push-b', 'ios', NULL, 'dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd', 'en', 'UTC', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:06.000Z'),
  ('dev-legacy-latest', 'user-push-b', 'IOS', '', 'eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee', 'en', 'UTC', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:07.000Z'),
  ('dev-case-same-old', 'user-push-a', 'ios', 'case-same-old', UPPER('abababababababababababababababababababababababababababababababab'), 'en', 'UTC', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:08.000Z'),
  ('dev-case-same-new', 'user-push-a', 'ios', 'case-same-new', 'abababababababababababababababababababababababababababababababab', 'en', 'UTC', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:09.000Z');

INSERT INTO commands (
  id, user_id, device_id, intent, risk_level, idempotency_key, locale,
  timezone, state, command_hash, created_at, updated_at
) VALUES
  ('cmd-registration-loser', 'user-push-a', 'dev-migrate-old', 'fixture.registration-loser', 'low', 'fixture-command-0001', 'en', 'UTC', 'succeeded', '1111111111111111111111111111111111111111111111111111111111111111', '2026-01-01T00:00:10.000Z', '2026-01-01T00:00:10.000Z'),
  ('cmd-case-token-loser', 'user-push-a', 'dev-case-same-old', 'fixture.case-token-loser', 'low', 'fixture-command-0002', 'en', 'UTC', 'succeeded', '2222222222222222222222222222222222222222222222222222222222222222', '2026-01-01T00:00:11.000Z', '2026-01-01T00:00:11.000Z'),
  ('cmd-cross-user-token-loser', 'user-push-a', 'dev-token-old-owner', 'fixture.cross-user-token-loser', 'low', 'fixture-command-0003', 'en', 'UTC', 'succeeded', '3333333333333333333333333333333333333333333333333333333333333333', '2026-01-01T00:00:12.000Z', '2026-01-01T00:00:12.000Z');
SQL

if ! run_wrangler d1 execute DB \
  --local \
  --persist-to "${PERSIST_TO}" \
  --config "${WRANGLER_CONFIG}" \
  --file "${TEMP_DIR}/push-migration-fixtures.sql" \
  >"${TEMP_DIR}/push-migration-fixtures.stdout" 2>>"${D1_STDERR}"; then
  fail "pre-0021 push registration duplicate fixtures could not be inserted"
fi

push_migration_source="${ROOT_DIR}/migrations/${PUSH_REGISTRATION_MIGRATION}"
[[ -f "${push_migration_source}" ]] || fail "missing migration: ${PUSH_REGISTRATION_MIGRATION}"
ln -s "${push_migration_source}" "${MIGRATIONS_DIR}/${PUSH_REGISTRATION_MIGRATION}"
if ! run_wrangler d1 migrations apply DB \
  --local \
  --persist-to "${PERSIST_TO}" \
  --config "${WRANGLER_CONFIG}" \
  >"${TEMP_DIR}/migration-0021.stdout" 2>"${TEMP_DIR}/migration-0021.stderr"; then
  sed -n '1,160p' "${TEMP_DIR}/migration-0021.stderr" >&2
  fail "Wrangler could not apply migration 0021"
fi

release_migration_source="${ROOT_DIR}/migrations/${RELEASE_MIGRATION}"
[[ -f "${release_migration_source}" ]] || fail "missing migration: ${RELEASE_MIGRATION}"
ln -s "${release_migration_source}" "${MIGRATIONS_DIR}/${RELEASE_MIGRATION}"
if ! run_wrangler d1 migrations apply DB \
  --local \
  --persist-to "${PERSIST_TO}" \
  --config "${WRANGLER_CONFIG}" \
  >"${TEMP_DIR}/migration-0022.stdout" 2>"${TEMP_DIR}/migration-0022.stderr"; then
  sed -n '1,160p' "${TEMP_DIR}/migration-0022.stderr" >&2
  fail "Wrangler could not apply migration 0022"
fi

EXPECTED_MIGRATIONS="$(IFS=,; printf '%s' "${MIGRATIONS[*]}")"
migration_rows="$(d1_json "SELECT COUNT(*) AS migration_count, (SELECT group_concat(name, ',') FROM (SELECT name FROM d1_migrations ORDER BY id)) AS names FROM d1_migrations")"
actual_migration_count="$("${JQ_BIN}" -r '.[0].results[0].migration_count' <<<"${migration_rows}")"
actual_migration_names="$("${JQ_BIN}" -r '.[0].results[0].names' <<<"${migration_rows}")"
[[ "${actual_migration_count}" == "22" ]] \
  || fail "expected 22 applied migrations, got ${actual_migration_count}"
[[ "${actual_migration_names}" == "${EXPECTED_MIGRATIONS}" ]] \
  || fail "migrations were not applied in the required 0001..0022 order"

listener_lease_columns="$(d1_json 'PRAGMA table_info(agent_listener_leases)')"
assert_jq "${listener_lease_columns}" \
  'any(.[0].results[]; .name == "released_at" and .notnull == 0)' \
  "0022 listener release marker is missing or non-nullable"

registration_owner_indexes="$(d1_json 'PRAGMA index_list(devices)')"
assert_jq "${registration_owner_indexes}" \
  'any(.[0].results[]; .name == "idx_devices_registration_owner" and .unique == 1 and .partial == 0)' \
  "0021 registration owner index is not unique"
assert_jq "${registration_owner_indexes}" \
  'any(.[0].results[]; .name == "idx_devices_active_push_token" and .unique == 1 and .partial == 1)' \
  "0021 active push token index is not unique and partial"
registration_owner_columns="$(d1_json 'PRAGMA index_info(idx_devices_registration_owner)')"
assert_jq "${registration_owner_columns}" \
  '.[0].results | sort_by(.seqno) | map(.name) == ["user_id","platform","device_id"]' \
  "0021 registration owner index has the wrong key columns"
push_token_triggers="$(d1_json "SELECT name FROM sqlite_master WHERE type = 'trigger' AND name LIKE 'trg_devices_push_token_normalized_%' ORDER BY name")"
assert_jq "${push_token_triggers}" \
  '.[0].results | map(.name) == ["trg_devices_push_token_normalized_insert","trg_devices_push_token_normalized_update"]' \
  "0021 normalized push-token write guards are missing"

migration_owner="$(d1_json "SELECT id, push_token FROM devices WHERE user_id = 'user-push-a' AND platform = 'ios' AND device_id = 'device-migrate'")"
assert_jq "${migration_owner}" \
  '.[0].results | length == 1 and .[0].id == "dev-migrate-latest" and .[0].push_token == "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"' \
  "0021 did not keep the latest valid owner registration"
migration_token_owner="$(d1_json "SELECT id, user_id, push_token FROM devices WHERE push_token = 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc'")"
assert_jq "${migration_token_owner}" \
  '.[0].results | length == 1 and .[0].id == "dev-token-new-owner" and .[0].user_id == "user-push-b"' \
  "0021 did not keep exactly one latest active token owner"
migration_token_loser="$(d1_json "SELECT id, push_token FROM devices WHERE id = 'dev-token-old-owner'")"
assert_jq "${migration_token_loser}" \
  '.[0].results | length == 1 and .[0].push_token == null' \
  "0021 did not retire the previous token owner"
migration_case_owners="$(d1_json "SELECT id, push_token FROM devices WHERE id IN ('dev-case-same-old', 'dev-case-same-new') ORDER BY id")"
assert_jq "${migration_case_owners}" \
  '.[0].results | length == 2 and .[0].id == "dev-case-same-new" and .[0].push_token == "abababababababababababababababababababababababababababababababab" and .[1].id == "dev-case-same-old" and .[1].push_token == null' \
  "0021 did not resolve same-user case-equivalent APNs tokens before normalization"
migration_legacy_owner="$(d1_json "SELECT id, device_id FROM devices WHERE user_id = 'user-push-b' AND platform = 'ios' AND device_id = '__legacy_device__'")"
assert_jq "${migration_legacy_owner}" \
  '.[0].results | length == 1 and .[0].id == "dev-legacy-latest"' \
  "0021 did not collapse legacy missing device IDs to one stable owner"
migration_registration_losers="$(d1_json "SELECT id, device_id, push_token FROM devices WHERE id IN ('dev-migrate-old', 'dev-migrate-invalid-newer', 'dev-legacy-old') ORDER BY id")"
assert_jq "${migration_registration_losers}" \
  '.[0].results | length == 3 and all(.[]; .device_id == null and .push_token == null)' \
  "0021 did not retire duplicate registration losers in place"
retained_devices="$(d1_json "SELECT COUNT(*) AS retained_count FROM devices WHERE id IN ('dev-migrate-old', 'dev-migrate-latest', 'dev-migrate-invalid-newer', 'dev-token-old-owner', 'dev-token-new-owner', 'dev-legacy-old', 'dev-legacy-latest', 'dev-case-same-old', 'dev-case-same-new')")"
assert_jq "${retained_devices}" \
  '.[0].results | length == 1 and .[0].retained_count == 9' \
  "0021 deleted a legacy device row"
preserved_command_references="$(d1_json "SELECT commands.id AS command_id, commands.device_id, devices.id AS referenced_device_id FROM commands LEFT JOIN devices ON devices.id = commands.device_id WHERE commands.id LIKE 'cmd-%-loser' ORDER BY commands.id")"
assert_jq "${preserved_command_references}" \
  '.[0].results | length == 3 and all(.[]; .device_id == .referenced_device_id and .referenced_device_id != null)' \
  "0021 broke command history references to retired device rows"
normalized_tokens="$(d1_json "SELECT COUNT(*) AS invalid_count FROM devices WHERE push_token IS NOT NULL AND push_token <> LOWER(TRIM(push_token))")"
assert_jq "${normalized_tokens}" \
  '.[0].results | length == 1 and .[0].invalid_count == 0' \
  "0021 left a non-normalized active APNs token"
normalized_token_duplicates="$(d1_json "SELECT COUNT(*) AS duplicate_count FROM (SELECT LOWER(TRIM(push_token)) FROM devices WHERE push_token IS NOT NULL GROUP BY LOWER(TRIM(push_token)) HAVING COUNT(*) > 1)")"
assert_jq "${normalized_token_duplicates}" \
  '.[0].results | length == 1 and .[0].duplicate_count == 0' \
  "0021 left more than one owner for a normalized APNs token"
foreign_key_violations="$(d1_json 'PRAGMA foreign_key_check')"
assert_jq "${foreign_key_violations}" \
  '.[0].results | length == 0' \
  "0021 left a foreign-key violation"

if run_wrangler d1 execute DB \
  --local \
  --persist-to "${PERSIST_TO}" \
  --config "${WRANGLER_CONFIG}" \
  --command "INSERT INTO devices (id, user_id, platform, device_id, push_token, created_at, updated_at) VALUES ('dev-owner-duplicate', 'user-push-a', 'ios', 'device-migrate', 'abababababababababababababababababababababababababababababababab', '2026-01-01T00:00:08.000Z', '2026-01-01T00:00:08.000Z')" \
  --json >"${TEMP_DIR}/duplicate-owner.stdout" 2>"${TEMP_DIR}/duplicate-owner.stderr"; then
  fail "0021 owner uniqueness accepted a duplicate user/platform/device"
fi

if run_wrangler d1 execute DB \
  --local \
  --persist-to "${PERSIST_TO}" \
  --config "${WRANGLER_CONFIG}" \
  --command "INSERT INTO devices (id, user_id, platform, device_id, push_token, created_at, updated_at) VALUES ('dev-token-duplicate', 'user-push-a', 'ios', 'token-duplicate-device', 'cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc', '2026-01-01T00:00:09.000Z', '2026-01-01T00:00:09.000Z')" \
  --json >"${TEMP_DIR}/duplicate-token.stdout" 2>"${TEMP_DIR}/duplicate-token.stderr"; then
  fail "0021 token uniqueness accepted two active owners"
fi

if run_wrangler d1 execute DB \
  --local \
  --persist-to "${PERSIST_TO}" \
  --config "${WRANGLER_CONFIG}" \
  --command "INSERT INTO devices (id, user_id, platform, device_id, push_token, created_at, updated_at) VALUES ('dev-token-not-normalized', 'user-push-a', 'ios', 'token-not-normalized-device', 'EFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEFEF', '2026-01-01T00:00:10.000Z', '2026-01-01T00:00:10.000Z')" \
  --json >"${TEMP_DIR}/non-normalized-token.stdout" 2>"${TEMP_DIR}/non-normalized-token.stderr"; then
  fail "0021 accepted a non-normalized active token"
fi

phone_ask_columns="$(d1_json 'PRAGMA table_info(phone_asks)')"
assert_jq "${phone_ask_columns}" \
  '([.[0].results[].name] | sort) | contains(["answered_at","attempt_count","claim_deadline","claim_generation","claim_token","client_turn_id","conversation_id","lease_id","listener_generation","reply_event_id"])' \
  "0019 phone_asks claim/answered columns are incomplete"

phone_ask_indexes="$(d1_json 'PRAGMA index_list(phone_asks)')"
assert_jq "${phone_ask_indexes}" \
  'any(.[0].results[]; .name == "idx_phone_asks_client_turn" and .unique == 1 and .partial == 1)' \
  "0019 client_turn index is not unique and partial"
client_turn_index="$(d1_json 'PRAGMA index_info(idx_phone_asks_client_turn)')"
assert_jq "${client_turn_index}" \
  '.[0].results | sort_by(.seqno) | map(.name) == ["user_id","agent_id","client_turn_id"]' \
  "0019 client_turn index has the wrong key columns"

cat >"${TEMP_DIR}/fixtures.sql" <<'SQL'
INSERT INTO users (id, email, password_hash, created_at)
VALUES ('user-lease', 'listener-lease@example.test', 'local-fixture-only', '2025-12-31T00:00:00.000Z');

INSERT INTO agents (id, user_id, label, host_label, api_key_hash, created_at, last_seen_at)
VALUES (
  'agent-lease', 'user-lease', 'Lease integration agent', 'local-miniflare',
  'local-fixture-api-key-hash', '2025-12-31T00:00:00.000Z', NULL
);

INSERT INTO agent_chat_bindings (
  id, user_id, agent_id, chat_id, chat_title, listener_instance_id,
  status, last_seen_at, expires_at, revoked_at, created_at, updated_at
) VALUES
  (
    'binding-a', 'user-lease', 'agent-lease', 'chat-a', 'Chat A', 'instance-a-0001',
    'offline', '2025-12-31T00:00:00.000Z', '2025-12-31T00:00:00.000Z', NULL,
    '2025-12-31T00:00:00.000Z', '2025-12-31T00:00:00.000Z'
  ),
  (
    'binding-b', 'user-lease', 'agent-lease', 'chat-b', 'Chat B', 'instance-b-0001',
    'offline', '2025-12-31T00:00:00.000Z', '2025-12-31T00:00:00.000Z', NULL,
    '2025-12-31T00:00:00.000Z', '2025-12-31T00:00:00.000Z'
  );
SQL

if ! run_wrangler d1 execute DB \
  --local \
  --persist-to "${PERSIST_TO}" \
  --config "${WRANGLER_CONFIG}" \
  --file "${TEMP_DIR}/fixtures.sql" \
  >"${TEMP_DIR}/fixtures.stdout" 2>>"${D1_STDERR}"; then
  fail "minimal user/agent/chat binding fixtures could not be inserted"
fi

first_turn="$(d1_json "INSERT INTO phone_asks (id, user_id, agent_id, transcript, locale, idempotency_key, session_id, status, claimed_at, expires_at, created_at, updated_at, binding_id, target_chat_id, claimed_by_chat_id, client_turn_id, conversation_id) VALUES ('ask-turn-a', 'user-lease', 'agent-lease', 'hello', 'en', 'ask-idem-0001', NULL, 'queued', NULL, '2026-01-01T01:00:00.000Z', '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z', 'binding-a', 'chat-a', NULL, 'turn-00000001', 'chat-a') RETURNING id, client_turn_id")"
assert_result_count "${first_turn}" 1 "first client turn insert"

if run_wrangler d1 execute DB \
  --local \
  --persist-to "${PERSIST_TO}" \
  --config "${WRANGLER_CONFIG}" \
  --command "INSERT INTO phone_asks (id, user_id, agent_id, transcript, locale, idempotency_key, session_id, status, claimed_at, expires_at, created_at, updated_at, binding_id, target_chat_id, claimed_by_chat_id, client_turn_id, conversation_id) VALUES ('ask-turn-b', 'user-lease', 'agent-lease', 'duplicate', 'en', 'ask-idem-0002', NULL, 'queued', NULL, '2026-01-01T01:00:00.000Z', '2026-01-01T00:00:01.000Z', '2026-01-01T00:00:01.000Z', 'binding-b', 'chat-b', NULL, 'turn-00000001', 'chat-b')" \
  --json >"${TEMP_DIR}/duplicate-turn.stdout" 2>"${TEMP_DIR}/duplicate-turn.stderr"; then
  fail "0019 client_turn uniqueness accepted a duplicate user/agent turn"
fi

claim_fields="$(d1_json "UPDATE phone_asks SET status = 'claimed', claimed_at = '2026-01-01T00:00:02.000Z', claim_token = 'claim-token-0001', claim_deadline = '2026-01-01T00:00:32.000Z', claim_generation = 1, answered_at = '2026-01-01T00:00:03.000Z', reply_event_id = 'reply-event-0001', attempt_count = attempt_count + 1, updated_at = '2026-01-01T00:00:03.000Z' WHERE id = 'ask-turn-a' RETURNING claim_token, claim_deadline, claim_generation, answered_at, reply_event_id, attempt_count")"
assert_jq "${claim_fields}" \
  '.[0].results | length == 1 and .[0].claim_token == "claim-token-0001" and .[0].claim_generation == 1 and .[0].answered_at == "2026-01-01T00:00:03.000Z" and .[0].attempt_count == 1' \
  "0019 claim/answered fields did not persist"

outbox_attempt_tables="$(d1_json "SELECT name FROM sqlite_master WHERE type = 'table' AND name IN ('outbox_events', 'action_attempts') ORDER BY name")"
assert_jq "${outbox_attempt_tables}" \
  '.[0].results | map(.name) == ["action_attempts","outbox_events"]' \
  "0020 requires outbox_events and action_attempts tables"

outbox_indexes="$(d1_json 'PRAGMA index_list(outbox_events)')"
assert_jq "${outbox_indexes}" \
  'any(.[0].results[]; .name == "idx_outbox_session_event_notification_due" and .partial == 1)' \
  "0020 outbox session-event due index is missing or not partial"
outbox_index_columns="$(d1_json 'PRAGMA index_info(idx_outbox_session_event_notification_due)')"
assert_jq "${outbox_index_columns}" \
  '.[0].results | sort_by(.seqno) | map(.name) == ["topic","state","next_attempt_at","created_at"]' \
  "0020 outbox session-event due index has the wrong columns"

attempt_indexes="$(d1_json 'PRAGMA index_list(action_attempts)')"
assert_jq "${attempt_indexes}" \
  'any(.[0].results[]; .name == "idx_action_attempts_session_event_apns_state" and .partial == 1)' \
  "0020 APNs attempt state index is missing or not partial"
attempt_index_columns="$(d1_json 'PRAGMA index_info(idx_action_attempts_session_event_apns_state)')"
assert_jq "${attempt_index_columns}" \
  '.[0].results | sort_by(.seqno) | map(.name) == ["provider","state","next_attempt_at","updated_at"]' \
  "0020 APNs attempt state index has the wrong columns"

event_outbox="$(d1_json "INSERT INTO outbox_events (id, user_id, topic, aggregate_id, payload_json, idempotency_key, state, attempts, next_attempt_at, last_error, created_at, updated_at) VALUES ('outbox-event-0020-a', 'user-lease', 'session.event.notification', 'event-0020', '{\"event_id\":\"event-0020\",\"notification_id\":\"notification-0020\"}', 'event-0020:notification-0020', 'queued', 0, '2026-01-01T00:00:10.000Z', NULL, '2026-01-01T00:00:10.000Z', '2026-01-01T00:00:10.000Z') RETURNING topic, aggregate_id, idempotency_key")"
assert_jq "${event_outbox}" \
  '.[0].results | length == 1 and .[0].topic == "session.event.notification" and .[0].aggregate_id == "event-0020" and .[0].idempotency_key == "event-0020:notification-0020"' \
  "0020 session-event notification outbox insert failed"

if run_wrangler d1 execute DB \
  --local \
  --persist-to "${PERSIST_TO}" \
  --config "${WRANGLER_CONFIG}" \
  --command "INSERT INTO outbox_events (id, user_id, topic, aggregate_id, payload_json, idempotency_key, state, attempts, next_attempt_at, last_error, created_at, updated_at) VALUES ('outbox-event-0020-b', 'user-lease', 'session.event.notification', 'event-0020', '{\"event_id\":\"event-0020\",\"notification_id\":\"notification-0020\"}', 'event-0020:notification-0020', 'queued', 0, '2026-01-01T00:00:11.000Z', NULL, '2026-01-01T00:00:11.000Z', '2026-01-01T00:00:11.000Z')" \
  --json >"${TEMP_DIR}/duplicate-event-outbox.stdout" 2>"${TEMP_DIR}/duplicate-event-outbox.stderr"; then
  fail "outbox accepted duplicate event/notification idempotency"
fi

apns_attempt="$(d1_json "INSERT INTO action_attempts (id, user_id, command_id, action_id, provider, provider_idempotency_key, state, request_hash, response_json, attempts, next_attempt_at, last_error, created_at, updated_at) VALUES ('attempt-event-0020-a', 'user-lease', NULL, NULL, 'apns.session_event', 'event-0020:notification-0020:device-0020', 'queued', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', NULL, 0, '2026-01-01T00:00:12.000Z', NULL, '2026-01-01T00:00:12.000Z', '2026-01-01T00:00:12.000Z') RETURNING provider, provider_idempotency_key")"
assert_jq "${apns_attempt}" \
  '.[0].results | length == 1 and .[0].provider == "apns.session_event" and .[0].provider_idempotency_key == "event-0020:notification-0020:device-0020"' \
  "0020 APNs session-event attempt insert failed"

if run_wrangler d1 execute DB \
  --local \
  --persist-to "${PERSIST_TO}" \
  --config "${WRANGLER_CONFIG}" \
  --command "INSERT INTO action_attempts (id, user_id, command_id, action_id, provider, provider_idempotency_key, state, request_hash, response_json, attempts, next_attempt_at, last_error, created_at, updated_at) VALUES ('attempt-event-0020-b', 'user-lease', NULL, NULL, 'apns.session_event', 'event-0020:notification-0020:device-0020', 'queued', 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', NULL, 0, '2026-01-01T00:00:13.000Z', NULL, '2026-01-01T00:00:13.000Z', '2026-01-01T00:00:13.000Z')" \
  --json >"${TEMP_DIR}/duplicate-apns-attempt.stdout" 2>"${TEMP_DIR}/duplicate-apns-attempt.stderr"; then
  fail "action_attempts accepted duplicate event/notification APNs idempotency"
fi

a_acquire="$(d1_json "$(acquire_sql \
  binding-a chat-a 'Chat A' instance-a-0001 lease-a-0001 \
  2026-01-01T00:00:00.000Z 2026-01-01T00:01:30.000Z 0 '' 0)")"
assert_result_count "${a_acquire}" 1 "listener A acquire"
assert_jq "${a_acquire}" \
  '.[0].results[0] | .chat_id == "chat-a" and .lease_id == "lease-a-0001" and .generation == 1' \
  "listener A did not acquire generation 1"

one_lease="$(d1_json "SELECT COUNT(*) AS lease_count FROM agent_listener_leases WHERE agent_id = 'agent-lease'")"
assert_jq "${one_lease}" '.[0].results[0].lease_count == 1' \
  "one agent acquired more than one lease row"

b_unfenced="$(d1_json "$(acquire_sql \
  binding-b chat-b 'Chat B' instance-b-0001 lease-b-unfenced \
  2026-01-01T00:00:01.000Z 2026-01-01T00:01:31.000Z 0 '' 0)")"
assert_result_count "${b_unfenced}" 0 "listener B unfenced acquire"

owner_after_reject="$(d1_json "SELECT chat_id, lease_id, generation FROM agent_listener_leases WHERE agent_id = 'agent-lease'")"
assert_jq "${owner_after_reject}" \
  '.[0].results | length == 1 and .[0].chat_id == "chat-a" and .[0].lease_id == "lease-a-0001" and .[0].generation == 1' \
  "unfenced listener B changed the active owner"

b_takeover="$(d1_json "$(acquire_sql \
  binding-b chat-b 'Chat B' instance-b-0001 lease-b-0002 \
  2026-01-01T00:00:02.000Z 2026-01-01T00:01:32.000Z \
  1 lease-a-0001 1)")"
assert_result_count "${b_takeover}" 1 "listener B explicit fenced takeover"
assert_jq "${b_takeover}" \
  '.[0].results[0] | .chat_id == "chat-b" and .lease_id == "lease-b-0002" and .generation == 2' \
  "listener B takeover did not advance the generation"

stale_a_heartbeat="$(d1_json "$(heartbeat_sql \
  lease-a-0001 1 2026-01-01T00:00:03.000Z 2026-01-01T00:01:33.000Z)")"
assert_result_count "${stale_a_heartbeat}" 0 "stale listener A heartbeat"

live_b_heartbeat="$(d1_json "$(heartbeat_sql \
  lease-b-0002 2 2026-01-01T00:00:03.000Z 2026-01-01T00:01:33.000Z)")"
assert_result_count "${live_b_heartbeat}" 1 "current listener B heartbeat"
assert_jq "${live_b_heartbeat}" \
  '.[0].results[0] | .lease_id == "lease-b-0002" and .generation == 2 and .expires_at == "2026-01-01T00:01:33.000Z"' \
  "current listener B heartbeat did not renew the exact lease fence"

stale_a_release="$(d1_json "$(release_sql \
  chat-a instance-a-0001 lease-a-0001 1 2026-01-01T00:00:04.000Z)")"
assert_result_count "${stale_a_release}" 0 "stale listener A release"

live_b_release="$(d1_json "$(release_sql \
  chat-b instance-b-0001 lease-b-0002 2 2026-01-01T00:00:04.000Z)")"
assert_result_count "${live_b_release}" 1 "current listener B release"
assert_jq "${live_b_release}" \
  '.[0].results[0] | .chat_id == "chat-b" and .lease_id == "lease-b-0002" and .generation == 2 and .expires_at == "2026-01-01T00:00:04.000Z" and .released_at == "2026-01-01T00:00:04.000Z"' \
  "current listener B release did not expire and mark the exact owner fence"

replayed_b_release="$(d1_json "$(release_sql \
  chat-b instance-b-0001 lease-b-0002 2 2026-01-01T00:00:05.000Z)")"
assert_result_count "${replayed_b_release}" 1 "idempotent listener B release replay"
assert_jq "${replayed_b_release}" \
  '.[0].results[0] | .expires_at == "2026-01-01T00:00:04.000Z" and .released_at == "2026-01-01T00:00:04.000Z" and .updated_at == "2026-01-01T00:00:04.000Z"' \
  "release replay changed the original release record"

released_listener_status="$(d1_json "SELECT lease_id FROM agent_listener_leases WHERE agent_id = 'agent-lease' AND released_at IS NULL AND expires_at > '2026-01-01T00:00:05.000Z'")"
assert_result_count "${released_listener_status}" 0 "released listener health status"

successor_a="$(d1_json "$(acquire_sql \
  binding-a chat-a 'Chat A' instance-a-0001 lease-after-release \
  2026-01-01T00:00:06.000Z 2026-01-01T00:01:36.000Z 0 '' 0)")"
assert_result_count "${successor_a}" 1 "successor acquire after explicit release"
assert_jq "${successor_a}" '.[0].results[0].generation == 3' \
  "successor acquire did not advance the released lease generation"

stale_b_release="$(d1_json "$(release_sql \
  chat-b instance-b-0001 lease-b-0002 2 2026-01-01T00:00:07.000Z)")"
assert_result_count "${stale_b_release}" 0 "stale release after successor acquire"
successor_owner="$(d1_json "SELECT chat_id, listener_instance_id, lease_id, generation, released_at FROM agent_listener_leases WHERE agent_id = 'agent-lease'")"
assert_jq "${successor_owner}" \
  '.[0].results | length == 1 and .[0].chat_id == "chat-a" and .[0].listener_instance_id == "instance-a-0001" and .[0].lease_id == "lease-after-release" and .[0].generation == 3 and .[0].released_at == null' \
  "stale release cleared or marked the successor lease"

boundary_a="$(d1_json "$(acquire_sql \
  binding-a chat-a 'Chat A' instance-a-0001 lease-boundary-a \
  2030-01-01T00:00:00.000Z 2030-01-01T00:01:30.000Z \
  1 lease-after-release 3)")"
assert_result_count "${boundary_a}" 1 "90-second boundary setup"
assert_jq "${boundary_a}" '.[0].results[0].generation == 4' \
  "90-second boundary setup did not advance generation"

before_boundary="$(d1_json "$(acquire_sql \
  binding-b chat-b 'Chat B' instance-b-0001 lease-before-boundary \
  2030-01-01T00:01:29.999Z 2030-01-01T00:02:59.999Z 0 '' 0)")"
assert_result_count "${before_boundary}" 0 "acquire at 89.999 seconds"

at_boundary_heartbeat="$(d1_json "$(heartbeat_sql \
  lease-boundary-a 4 2030-01-01T00:01:30.000Z 2030-01-01T00:03:00.000Z)")"
assert_result_count "${at_boundary_heartbeat}" 0 "heartbeat at exactly 90 seconds"

at_boundary_acquire="$(d1_json "$(acquire_sql \
  binding-b chat-b 'Chat B' instance-b-0001 lease-at-boundary \
  2030-01-01T00:01:30.000Z 2030-01-01T00:03:00.000Z 0 '' 0)")"
assert_result_count "${at_boundary_acquire}" 1 "acquire at exactly 90 seconds"
assert_jq "${at_boundary_acquire}" \
  '.[0].results[0] | .chat_id == "chat-b" and .lease_id == "lease-at-boundary" and .generation == 5' \
  "the exact 90-second boundary was not treated as expired"

WORKER_PORT="$(pick_port)"
WORKER_URL="http://127.0.0.1:${WORKER_PORT}"
start_worker dev \
  --local \
  --ip 127.0.0.1 \
  --port "${WORKER_PORT}" \
  --persist-to "${PERSIST_TO}" \
  --config "${WRANGLER_CONFIG}" \
  --log-level error

if ! wait_for_worker "${WORKER_URL}"; then
  sed -n '1,160p' "${TEMP_DIR}/worker.stderr" >&2
  fail "temporary local Miniflare worker did not become ready"
fi

rebind_token='f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0'
curl --fail --silent --show-error --max-time 10 \
  -H 'content-type: application/json' \
  -d "$(jq -nc --arg id 'dev-reinstall-old-resource' --arg userId 'user-push-a' --arg deviceId 'reinstall-old' --arg pushToken "${rebind_token}" '{id:$id,userId:$userId,deviceId:$deviceId,pushToken:$pushToken}')" \
  "${WORKER_URL}/register-device" >"${TEMP_DIR}/register-reinstall-old.json"
curl --fail --silent --show-error --max-time 10 \
  -H 'content-type: application/json' \
  -d "$(jq -nc --arg id 'dev-reinstall-new-resource' --arg userId 'user-push-a' --arg deviceId 'reinstall-new' --arg pushToken "${rebind_token}" '{id:$id,userId:$userId,deviceId:$deviceId,pushToken:$pushToken}')" \
  "${WORKER_URL}/register-device" >"${TEMP_DIR}/register-reinstall-new.json"
reinstall_state="$(curl --fail --silent --show-error --max-time 10 "${WORKER_URL}/registration-state?pushToken=${rebind_token}")"
assert_jq "${reinstall_state}" \
  '.results | length == 1 and .[0].user_id == "user-push-a" and .[0].device_id == "reinstall-new"' \
  "same-account reinstall did not transfer the token to the new device"

curl --fail --silent --show-error --max-time 10 \
  -H 'content-type: application/json' \
  -d "$(jq -nc --arg id 'dev-account-change-resource' --arg userId 'user-push-b' --arg deviceId 'account-change' --arg pushToken "${rebind_token}" '{id:$id,userId:$userId,deviceId:$deviceId,pushToken:$pushToken}')" \
  "${WORKER_URL}/register-device" >"${TEMP_DIR}/register-account-change.json"
account_change_state="$(curl --fail --silent --show-error --max-time 10 "${WORKER_URL}/registration-state?pushToken=${rebind_token}")"
assert_jq "${account_change_state}" \
  '.results | length == 1 and .[0].user_id == "user-push-b" and .[0].device_id == "account-change"' \
  "account change did not leave exactly one active token owner"
retired_reinstall_state="$(curl --fail --silent --show-error --max-time 10 "${WORKER_URL}/registration-state?userId=user-push-a&deviceId=reinstall-new")"
assert_jq "${retired_reinstall_state}" \
  '.results | length == 1 and .[0].push_token == null' \
  "account change did not retire the previous account binding"

concurrent_token='1010101010101010101010101010101010101010101010101010101010101010'
registration_pids=()
for attempt in $(seq 1 20); do
  curl --fail --silent --show-error --max-time 10 \
    -H 'content-type: application/json' \
    -d "$(jq -nc --arg id "dev-concurrent-${attempt}" --arg userId 'user-push-a' --arg deviceId 'concurrent-device' --arg pushToken "${concurrent_token}" '{id:$id,userId:$userId,deviceId:$deviceId,pushToken:$pushToken}')" \
    "${WORKER_URL}/register-device" \
    >"${TEMP_DIR}/register-concurrent-${attempt}.json" \
    2>"${TEMP_DIR}/register-concurrent-${attempt}.stderr" &
  registration_pid=$!
  track_background_pid "${registration_pid}"
  registration_pids+=("${registration_pid}")
done
registration_parallel_failed=0
for registration_pid in "${registration_pids[@]}"; do
  if ! wait_tracked_background_pid "${registration_pid}"; then
    registration_parallel_failed=1
  fi
done
[[ "${registration_parallel_failed}" == "0" ]] \
  || fail "20-way device registration requests did not all complete"
concurrent_resource_count="$(jq -s 'map(.id) | unique | length' "${TEMP_DIR}"/register-concurrent-*.json)"
[[ "${concurrent_resource_count}" == "1" ]] \
  || fail "20-way device registration returned more than one resource"
concurrent_resource_id="$(jq -r '.id' "${TEMP_DIR}/register-concurrent-1.json")"
concurrent_state="$(curl --fail --silent --show-error --max-time 10 "${WORKER_URL}/registration-state?userId=user-push-a&deviceId=concurrent-device")"
"${JQ_BIN}" -e --arg resource_id "${concurrent_resource_id}" \
  '.results | length == 1 and .[0].id == $resource_id and .[0].push_token == "1010101010101010101010101010101010101010101010101010101010101010"' \
  <<<"${concurrent_state}" >/dev/null \
  || fail "20-way UPSERT did not leave exactly one stable registration row"

curl --fail --silent --show-error --max-time 10 \
  -H 'content-type: application/json' \
  -d '{"candidate":"a"}' \
  "${WORKER_URL}/acquire" \
  >"${TEMP_DIR}/parallel-a.json" 2>"${TEMP_DIR}/parallel-a.stderr" &
parallel_pid_a=$!
track_background_pid "${parallel_pid_a}"
curl --fail --silent --show-error --max-time 10 \
  -H 'content-type: application/json' \
  -d '{"candidate":"b"}' \
  "${WORKER_URL}/acquire" \
  >"${TEMP_DIR}/parallel-b.json" 2>"${TEMP_DIR}/parallel-b.stderr" &
parallel_pid_b=$!
track_background_pid "${parallel_pid_b}"

parallel_failed=0
if ! wait_tracked_background_pid "${parallel_pid_a}"; then
  parallel_failed=1
fi
if ! wait_tracked_background_pid "${parallel_pid_b}"; then
  parallel_failed=1
fi
stop_worker || fail "temporary Wrangler/Workerd process group did not terminate"
if [[ "${parallel_failed}" != "0" ]]; then
  sed -n '1,120p' "${TEMP_DIR}/parallel-a.stderr" >&2
  sed -n '1,120p' "${TEMP_DIR}/parallel-b.stderr" >&2
  sed -n '1,160p' "${TEMP_DIR}/worker.stderr" >&2
  fail "parallel local D1 acquire process failed"
fi

if ! "${JQ_BIN}" -e 'type == "object" and (.results | type == "array")' \
  "${TEMP_DIR}/parallel-a.json" >/dev/null \
  || ! "${JQ_BIN}" -e 'type == "object" and (.results | type == "array")' \
  "${TEMP_DIR}/parallel-b.json" >/dev/null; then
  fail "parallel acquire responses had an unexpected JSON shape"
fi
parallel_returned_a="$("${JQ_BIN}" -r '.results | length' "${TEMP_DIR}/parallel-a.json")"
parallel_returned_b="$("${JQ_BIN}" -r '.results | length' "${TEMP_DIR}/parallel-b.json")"
[[ "$((parallel_returned_a + parallel_returned_b))" == "1" ]] \
  || fail "parallel acquires did not produce exactly one CAS winner"

parallel_owner="$(d1_json "SELECT chat_id, lease_id, generation FROM agent_listener_leases WHERE agent_id = 'agent-lease'")"
assert_jq "${parallel_owner}" \
  '.[0].results | length == 1 and .[0].generation == 6 and ((.[0].chat_id == "chat-a" and .[0].lease_id == "lease-parallel-a") or (.[0].chat_id == "chat-b" and .[0].lease_id == "lease-parallel-b"))' \
  "parallel acquires left anything other than one fenced owner"

printf '%s\n' \
  'listener-lease-d1-integration passed: real 0001..0022 migrations, migration dedupe/uniqueness, atomic registration UPSERT/rebind, 20-way stable resource identity, lease CAS/RETURNING, authenticated-contract release SQL, idempotent release replay, stale-release successor fencing, immediate not-listening status, takeover fencing, 90-second boundary, 0019 claim schema, 0020 APNs outbox idempotency, and parallel single-owner acquire'
