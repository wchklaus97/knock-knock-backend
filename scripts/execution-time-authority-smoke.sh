#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DB_FILE="$(mktemp -t knock-knock-execution-time.XXXXXX)"
trap 'rm -f "${DB_FILE}"' EXIT

DB_NOW="strftime('%Y-%m-%dT%H:%M:%fZ','now')"

# Keep this executable SQLite check coupled to the production authorization
# predicates. Caller-captured timestamps may be used as metadata, but never as
# the authority for confirmation expiry or an active outbox lease.
grep -Fq "replay_token.expires_at > ${DB_NOW}" "${ROOT_DIR}/src/commands.rs"
grep -Fq "expires_at > ${DB_NOW}" "${ROOT_DIR}/src/commands.rs"
grep -Fq "claim.lease_expires_at > ${DB_NOW}" "${ROOT_DIR}/src/outbox.rs"
grep -Fq "claim.lease_expires_at <= ${DB_NOW}" "${ROOT_DIR}/src/outbox.rs"
grep -Fq "active_claim.lease_expires_at > ${DB_NOW}" "${ROOT_DIR}/src/action_effects.rs"
grep -Fq "RECOVERABLE_ACTION_ATTEMPT_EXISTS_SQL" "${ROOT_DIR}/src/commands.rs"
grep -Fq "ACTION_EFFECT_MAY_HAVE_STARTED_SQL" "${ROOT_DIR}/src/commands.rs"
grep -Fq "commands::RECOVERABLE_ACTION_ATTEMPT_EXISTS_SQL" "${ROOT_DIR}/src/outbox.rs"

sqlite3 "${DB_FILE}" <<'SQL'
PRAGMA foreign_keys = ON;

CREATE TABLE sessions (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  deleted_at TEXT
);
CREATE TABLE commands (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL,
  session_id TEXT,
  intent TEXT NOT NULL,
  idempotency_key TEXT NOT NULL,
  needs_confirmation INTEGER NOT NULL,
  state TEXT NOT NULL,
  command_hash TEXT NOT NULL,
  version INTEGER NOT NULL,
  expires_at TEXT,
  error_code TEXT,
  updated_at TEXT NOT NULL
);
CREATE TABLE confirmation_tokens (
  id TEXT PRIMARY KEY,
  command_id TEXT NOT NULL,
  user_id TEXT NOT NULL,
  token_hash TEXT NOT NULL,
  command_hash TEXT NOT NULL,
  expires_at TEXT NOT NULL,
  used_at TEXT,
  created_at TEXT NOT NULL
);
CREATE UNIQUE INDEX one_unused_confirmation
  ON confirmation_tokens(command_id, user_id, command_hash)
  WHERE used_at IS NULL;
CREATE TABLE outbox_events (
  id TEXT PRIMARY KEY,
  user_id TEXT,
  topic TEXT NOT NULL,
  aggregate_id TEXT NOT NULL,
  payload_json TEXT NOT NULL DEFAULT '{}',
  idempotency_key TEXT NOT NULL,
  state TEXT NOT NULL,
  lease_token TEXT,
  lease_expires_at TEXT,
  updated_at TEXT NOT NULL
);
CREATE TABLE action_attempts (
  command_id TEXT,
  user_id TEXT,
  state TEXT NOT NULL,
  provider TEXT NOT NULL DEFAULT '',
  provider_idempotency_key TEXT NOT NULL DEFAULT '',
  request_hash TEXT NOT NULL DEFAULT '',
  response_json TEXT,
  attempts INTEGER NOT NULL DEFAULT 1,
  next_attempt_at TEXT,
  last_error TEXT,
  updated_at TEXT
);
CREATE TABLE results (name TEXT PRIMARY KEY, value INTEGER NOT NULL);

-- An intentionally stale application timestamp says this token was live, but
-- SQLite execution time says it is expired. Replay must not rotate authority.
INSERT INTO commands VALUES (
  'cmd_confirm', 'usr', NULL, 'send_message', 'idem_confirm', 1,
  'awaiting_confirmation', 'hash_confirm', 2,
  '2099-01-01T00:00:00.000Z', NULL, '1999-01-01T00:00:00.000Z'
);
INSERT INTO confirmation_tokens VALUES (
  'tok_expired', 'cmd_confirm', 'usr', 'token_hash', 'hash_confirm',
  '2000-01-01T00:00:00.000Z', NULL, '1999-01-01T00:00:00.000Z'
);

UPDATE commands SET version = version + 1, updated_at = '1999-01-01T00:00:00.000Z'
 WHERE id = 'cmd_confirm' AND user_id = 'usr'
   AND idempotency_key = 'idem_confirm' AND command_hash = 'hash_confirm'
   AND version = 2 AND state = 'awaiting_confirmation' AND needs_confirmation = 1
   AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
   AND EXISTS (
     SELECT 1 FROM confirmation_tokens AS replay_token
      WHERE replay_token.command_id = commands.id
        AND replay_token.user_id = commands.user_id
        AND replay_token.command_hash = commands.command_hash
        AND replay_token.used_at IS NULL
        AND replay_token.expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
   );
INSERT INTO results VALUES ('expired_replay_command', changes());

UPDATE confirmation_tokens SET used_at = '1999-01-01T00:00:00.000Z'
 WHERE command_id = 'cmd_confirm' AND user_id = 'usr'
   AND command_hash = 'hash_confirm' AND used_at IS NULL
   AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
   AND changes() = 1;
INSERT INTO results VALUES ('expired_replay_token', changes());

-- Direct confirmation is guarded by the same database execution clock.
UPDATE confirmation_tokens SET used_at = '1999-01-01T00:00:00.000Z'
 WHERE id = 'tok_expired' AND user_id = 'usr' AND command_hash = 'hash_confirm'
   AND used_at IS NULL
   AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
   AND EXISTS (
     SELECT 1 FROM commands
      WHERE id = 'cmd_confirm' AND user_id = 'usr'
        AND state = 'awaiting_confirmation' AND version = 2
        AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
   );
UPDATE commands SET state = 'queued', version = 3
 WHERE id = 'cmd_confirm' AND user_id = 'usr'
   AND state = 'awaiting_confirmation' AND version = 2
   AND changes() = 1
   AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now');
INSERT INTO results VALUES ('expired_direct_queue', changes());

-- A matching stale lease cannot obtain the pre-effect permit. The distinct
-- recovery fence can still advance that exact expired generation.
INSERT INTO commands VALUES (
  'cmd_lease_expired', 'usr', NULL, 'create_reminder', 'idem_lease', 0,
  'running', 'hash_lease', 4, '2099-01-01T00:00:00.000Z', NULL,
  '2000-01-01T00:00:00.000Z'
);
INSERT INTO outbox_events (
  id, user_id, topic, aggregate_id, idempotency_key, state,
  lease_token, lease_expires_at, updated_at
) VALUES (
  'out_expired', 'usr', 'command.execute', 'cmd_lease_expired',
  'outbox_key', 'running', 'lease_a', '2000-01-01T00:00:00.000Z',
  '2000-01-01T00:00:00.000Z'
);
UPDATE commands SET version = version
 WHERE id = 'cmd_lease_expired' AND user_id = 'usr'
   AND state = 'running' AND version = 4
   AND EXISTS (
     SELECT 1 FROM outbox_events AS claim
      WHERE claim.id = 'out_expired' AND claim.user_id = 'usr'
        AND claim.user_id = commands.user_id
        AND claim.topic = 'command.execute'
        AND claim.aggregate_id = 'cmd_lease_expired'
        AND claim.aggregate_id = commands.id
        AND claim.idempotency_key = 'outbox_key'
        AND claim.state = 'running' AND claim.lease_token = 'lease_a'
        AND claim.lease_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
   );
INSERT INTO results VALUES ('expired_active_permit', changes());

UPDATE commands SET state = 'retryable', version = 5
 WHERE id = 'cmd_lease_expired' AND user_id = 'usr'
   AND state = 'running' AND version = 4
   AND EXISTS (
     SELECT 1 FROM outbox_events AS claim
      WHERE claim.id = 'out_expired' AND claim.user_id = 'usr'
        AND claim.user_id = commands.user_id
        AND claim.topic = 'command.execute'
        AND claim.aggregate_id = 'cmd_lease_expired'
        AND claim.aggregate_id = commands.id
        AND claim.idempotency_key = 'outbox_key'
        AND claim.state = 'running' AND claim.lease_token = 'lease_a'
        AND claim.lease_expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now')
   );
INSERT INTO results VALUES ('expired_recovery', changes());

-- A genuinely live exact lease remains usable.
INSERT INTO sessions VALUES ('ses_lease_live', 'usr', NULL);
INSERT INTO commands VALUES (
  'cmd_lease_live', 'usr', 'ses_lease_live', 'create_reminder', 'idem_live', 0,
  'running', 'hash_live', 4, '2099-01-01T00:00:00.000Z', NULL,
  '2000-01-01T00:00:00.000Z'
);
INSERT INTO outbox_events (
  id, user_id, topic, aggregate_id, idempotency_key, state,
  lease_token, lease_expires_at, updated_at
) VALUES (
  'out_live', 'usr', 'command.execute', 'cmd_lease_live',
  'outbox_live_key', 'running', 'lease_live', '2099-01-01T00:00:00.000Z',
  '2000-01-01T00:00:00.000Z'
);
UPDATE commands SET version = version
 WHERE id = 'cmd_lease_live' AND user_id = 'usr'
   AND state = 'running' AND version = 4
   AND EXISTS (
     SELECT 1 FROM outbox_events AS claim
      WHERE claim.id = 'out_live' AND claim.user_id = 'usr'
        AND claim.user_id = commands.user_id
        AND claim.topic = 'command.execute'
        AND claim.aggregate_id = 'cmd_lease_live'
        AND claim.aggregate_id = commands.id
        AND claim.idempotency_key = 'outbox_live_key'
        AND claim.state = 'running' AND claim.lease_token = 'lease_live'
        AND claim.lease_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
   );
INSERT INTO results VALUES ('live_active_permit', changes());

INSERT INTO action_attempts (
  command_id, user_id, state, provider, provider_idempotency_key,
  request_hash, attempts, last_error, updated_at
) VALUES (
  'cmd_lease_live', 'usr', 'running', 'action.reminder',
  'provider_live_key', 'hash_live', 0, 'execution_permit',
  '2000-01-01T00:00:00.000Z'
);
UPDATE action_attempts
   SET state = 'running', attempts = attempts + 1, response_json = NULL,
       next_attempt_at = NULL, last_error = NULL,
       updated_at = '2000-01-01T00:00:00.000Z'
 WHERE user_id = 'usr' AND command_id = 'cmd_lease_live'
   AND provider = 'action.reminder'
   AND provider_idempotency_key = 'provider_live_key'
   AND request_hash = 'hash_live'
   AND state IN ('running', 'retrying', 'unknown')
   AND EXISTS (
     SELECT 1
       FROM commands AS active_command
       JOIN outbox_events AS active_claim
         ON active_claim.user_id = active_command.user_id
        AND active_claim.aggregate_id = active_command.id
      WHERE active_command.id = 'cmd_lease_live'
        AND active_command.user_id = 'usr'
        AND active_command.state = 'running'
        AND active_command.version = 4
        AND active_claim.id = 'out_live'
        AND active_claim.user_id = 'usr'
        AND active_claim.topic = 'command.execute'
        AND active_claim.aggregate_id = 'cmd_lease_live'
        AND active_claim.idempotency_key = 'outbox_live_key'
        AND active_claim.state = 'running'
        AND active_claim.lease_token = 'lease_live'
        AND active_claim.lease_expires_at IS NOT NULL
        AND active_claim.lease_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
   );
INSERT INTO results VALUES ('live_effect_begin', changes());

-- If the session disappears after attempts>=1, recovery must preserve the
-- command for reconciliation. It cannot report cancelled, and a replacement
-- active claim may restart the exact command generation despite the tombstone.
UPDATE sessions SET deleted_at = strftime('%Y-%m-%dT%H:%M:%fZ','now')
 WHERE id = 'ses_lease_live' AND user_id = 'usr';
UPDATE outbox_events SET lease_expires_at = '2000-01-01T00:00:00.000Z'
 WHERE id = 'out_live';
UPDATE commands SET state = 'retryable', version = 5
 WHERE id = 'cmd_lease_live' AND user_id = 'usr'
   AND state = 'running' AND version = 4
   AND EXISTS (
     SELECT 1 FROM outbox_events AS claim
      WHERE claim.id = 'out_live' AND claim.user_id = 'usr'
        AND claim.aggregate_id = commands.id AND claim.state = 'running'
        AND claim.lease_token = 'lease_live'
        AND claim.lease_expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now')
   );
INSERT INTO results VALUES ('started_effect_recovery', changes());
UPDATE commands SET state = 'cancelled', version = version + 1
 WHERE id = 'cmd_lease_live' AND user_id = 'usr'
   AND state IN ('queued', 'retryable', 'unknown') AND version = 5
   AND NOT EXISTS (
     SELECT 1 FROM action_attempts AS started_attempt
      WHERE started_attempt.command_id = commands.id
        AND started_attempt.user_id = commands.user_id
        AND (
          started_attempt.state = 'succeeded'
          OR (
            started_attempt.state IN ('running', 'unknown', 'retrying')
            AND started_attempt.attempts >= 1
          )
        )
   );
INSERT INTO results VALUES ('started_effect_deleted_cancel', changes());
UPDATE outbox_events
   SET state = 'running', lease_token = 'lease_reconcile',
       lease_expires_at = '2099-01-01T00:00:00.000Z'
 WHERE id = 'out_live';
UPDATE commands SET state = 'running', version = 6
 WHERE id = 'cmd_lease_live' AND user_id = 'usr'
   AND state = 'retryable' AND version = 5
   AND (
     session_id IS NULL
     OR EXISTS (
       SELECT 1 FROM sessions
        WHERE id = commands.session_id AND user_id = 'usr' AND deleted_at IS NULL
     )
     OR EXISTS (
       SELECT 1 FROM action_attempts AS started_attempt
        WHERE started_attempt.command_id = commands.id
          AND started_attempt.user_id = commands.user_id
          AND (
            started_attempt.state = 'succeeded'
            OR (
              started_attempt.state IN ('running', 'unknown', 'retrying')
              AND started_attempt.attempts >= 1
            )
          )
     )
   )
   AND EXISTS (
     SELECT 1 FROM outbox_events AS claim
      WHERE claim.id = 'out_live' AND claim.user_id = 'usr'
        AND claim.aggregate_id = commands.id AND claim.state = 'running'
        AND claim.lease_token = 'lease_reconcile'
        AND claim.lease_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
   );
INSERT INTO results VALUES ('started_effect_reconcile_start', changes());

-- The execution permit and attempts=0 row are one durable boundary. If the
-- lease expires immediately afterward, recovery may change the command
-- generation, but cancellation must see the attempt and refuse to orphan the
-- stale Worker's authorized effect.
INSERT INTO commands VALUES (
  'cmd_post_permit', 'usr', NULL, 'create_reminder', 'idem_post_permit', 0,
  'running', 'hash_post_permit', 4, '2099-01-01T00:00:00.000Z', NULL,
  '2000-01-01T00:00:00.000Z'
);
INSERT INTO outbox_events (
  id, user_id, topic, aggregate_id, idempotency_key, state,
  lease_token, lease_expires_at, updated_at
) VALUES (
  'out_post_permit', 'usr', 'command.execute', 'cmd_post_permit',
  'post_permit_key', 'running', 'lease_permit', '2099-01-01T00:00:00.000Z',
  '2000-01-01T00:00:00.000Z'
);
UPDATE commands SET version = version
 WHERE id = 'cmd_post_permit' AND user_id = 'usr'
   AND state = 'running' AND version = 4
   AND EXISTS (
     SELECT 1 FROM outbox_events AS claim
      WHERE claim.id = 'out_post_permit' AND claim.user_id = 'usr'
        AND claim.aggregate_id = commands.id AND claim.state = 'running'
        AND claim.lease_token = 'lease_permit'
        AND claim.lease_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
   );
INSERT INTO action_attempts (
  command_id, user_id, state, provider, provider_idempotency_key,
  request_hash, attempts, last_error, updated_at
)
  SELECT 'cmd_post_permit', 'usr', 'running', 'action.reminder',
         'provider_post_permit_key', 'hash_post_permit', 0,
         'execution_permit', '2000-01-01T00:00:00.000Z'
   WHERE changes() = 1;
INSERT INTO results VALUES ('post_permit_attempt', changes());

UPDATE outbox_events SET lease_expires_at = '2000-01-01T00:00:00.000Z'
 WHERE id = 'out_post_permit';
UPDATE commands SET state = 'retryable', version = 5
 WHERE id = 'cmd_post_permit' AND user_id = 'usr'
   AND state = 'running' AND version = 4
   AND EXISTS (
     SELECT 1 FROM outbox_events AS claim
      WHERE claim.id = 'out_post_permit' AND claim.user_id = 'usr'
        AND claim.aggregate_id = commands.id AND claim.state = 'running'
        AND claim.lease_token = 'lease_permit'
        AND claim.lease_expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now')
   );
INSERT INTO results VALUES ('post_permit_recovery', changes());

-- The paused Worker still holds its in-memory lease token, but both the DB
-- command generation and DB lease have moved on. It must not cross the final
-- boundary into a provider/local side effect.
UPDATE action_attempts
   SET state = 'running', attempts = attempts + 1, response_json = NULL,
       next_attempt_at = NULL, last_error = NULL,
       updated_at = '2000-01-01T00:00:00.000Z'
 WHERE user_id = 'usr' AND command_id = 'cmd_post_permit'
   AND provider = 'action.reminder'
   AND provider_idempotency_key = 'provider_post_permit_key'
   AND request_hash = 'hash_post_permit'
   AND state IN ('running', 'retrying', 'unknown')
   AND EXISTS (
     SELECT 1
       FROM commands AS active_command
       JOIN outbox_events AS active_claim
         ON active_claim.user_id = active_command.user_id
        AND active_claim.aggregate_id = active_command.id
      WHERE active_command.id = 'cmd_post_permit'
        AND active_command.user_id = 'usr'
        AND active_command.state = 'running'
        AND active_command.version = 4
        AND active_claim.id = 'out_post_permit'
        AND active_claim.user_id = 'usr'
        AND active_claim.topic = 'command.execute'
        AND active_claim.aggregate_id = 'cmd_post_permit'
        AND active_claim.idempotency_key = 'post_permit_key'
        AND active_claim.state = 'running'
        AND active_claim.lease_token = 'lease_permit'
        AND active_claim.lease_expires_at IS NOT NULL
        AND active_claim.lease_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
   );
INSERT INTO results VALUES ('stale_effect_begin', changes());
UPDATE commands SET state = 'cancelled', version = version + 1
 WHERE id = 'cmd_post_permit' AND user_id = 'usr' AND state = 'retryable'
   AND NOT EXISTS (
     SELECT 1 FROM action_attempts AS recovery_attempt
      WHERE recovery_attempt.command_id = commands.id
        AND recovery_attempt.user_id = commands.user_id
        AND recovery_attempt.state IN ('succeeded', 'running', 'unknown', 'retrying')
   );
INSERT INTO results VALUES ('post_permit_cancel', changes());
UPDATE commands SET state = 'cancelled', version = version + 1
 WHERE id = 'cmd_post_permit' AND user_id = 'usr' AND state = 'retryable'
   AND NOT EXISTS (
     SELECT 1 FROM action_attempts AS started_attempt
      WHERE started_attempt.command_id = commands.id
        AND started_attempt.user_id = commands.user_id
        AND (
          started_attempt.state = 'succeeded'
          OR (
            started_attempt.state IN ('running', 'unknown', 'retrying')
            AND started_attempt.attempts >= 1
          )
        )
   );
INSERT INTO results VALUES ('post_permit_deleted_cancel', changes());

-- Command TTL prevents an effect that never started, but cannot erase proof
-- that an effect succeeded or is still being reconciled. Those commands may
-- cross the expired TTL boundary only to reuse/status-settle the existing
-- provider attempt; the normal outbox/provider idempotency fences still apply.
INSERT INTO commands VALUES (
  'cmd_ttl_fresh', 'usr', NULL, 'create_reminder', 'idem_ttl_fresh', 0,
  'retryable', 'hash_ttl_fresh', 1, '2000-01-01T00:00:00.000Z', NULL,
  '2000-01-01T00:00:00.000Z'
);
INSERT INTO commands VALUES (
  'cmd_ttl_succeeded', 'usr', NULL, 'create_reminder', 'idem_ttl_succeeded', 0,
  'retryable', 'hash_ttl_succeeded', 1, '2000-01-01T00:00:00.000Z', NULL,
  '2000-01-01T00:00:00.000Z'
);
INSERT INTO commands VALUES (
  'cmd_ttl_reconciling', 'usr', NULL, 'create_reminder', 'idem_ttl_reconciling', 0,
  'unknown', 'hash_ttl_reconciling', 1, '2000-01-01T00:00:00.000Z', NULL,
  '2000-01-01T00:00:00.000Z'
);
INSERT INTO commands VALUES (
  'cmd_ttl_unknown', 'usr', NULL, 'send_message', 'idem_ttl_unknown', 1,
  'unknown', 'hash_ttl_unknown', 1, '2000-01-01T00:00:00.000Z', NULL,
  '2000-01-01T00:00:00.000Z'
);
INSERT INTO commands VALUES (
  'cmd_ttl_retrying', 'usr', NULL, 'send_message', 'idem_ttl_retrying', 1,
  'retryable', 'hash_ttl_retrying', 1, '2000-01-01T00:00:00.000Z', NULL,
  '2000-01-01T00:00:00.000Z'
);
INSERT INTO action_attempts (command_id, user_id, state) VALUES ('cmd_ttl_succeeded', 'usr', 'succeeded');
INSERT INTO action_attempts (command_id, user_id, state) VALUES ('cmd_ttl_reconciling', 'usr', 'running');
INSERT INTO action_attempts (command_id, user_id, state) VALUES ('cmd_ttl_unknown', 'usr', 'unknown');
INSERT INTO action_attempts (command_id, user_id, state) VALUES ('cmd_ttl_retrying', 'usr', 'retrying');

UPDATE commands SET state = 'expired', version = version + 1
 WHERE state IN ('pending', 'validated', 'awaiting_confirmation', 'queued', 'retryable', 'unknown')
   AND expires_at IS NOT NULL
   AND expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now')
   AND NOT EXISTS (
     SELECT 1 FROM action_attempts AS recovery_attempt
      WHERE recovery_attempt.command_id = commands.id
        AND recovery_attempt.user_id = commands.user_id
        AND recovery_attempt.state IN ('succeeded', 'running', 'unknown', 'retrying')
   );
INSERT INTO results VALUES ('ttl_sweep_expired', changes());

UPDATE commands SET state = 'running', version = version + 1
 WHERE id = 'cmd_ttl_succeeded' AND user_id = 'usr' AND state = 'retryable'
   AND (
     expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
     OR EXISTS (
       SELECT 1 FROM action_attempts AS recovery_attempt
        WHERE recovery_attempt.command_id = commands.id
          AND recovery_attempt.user_id = commands.user_id
          AND recovery_attempt.state IN ('succeeded', 'running', 'unknown', 'retrying')
     )
   );
INSERT INTO results VALUES ('ttl_succeeded_start', changes());

UPDATE commands SET state = 'running', version = version + 1
 WHERE id = 'cmd_ttl_reconciling' AND user_id = 'usr' AND state = 'unknown'
   AND (
     expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')
     OR EXISTS (
       SELECT 1 FROM action_attempts AS recovery_attempt
        WHERE recovery_attempt.command_id = commands.id
          AND recovery_attempt.user_id = commands.user_id
          AND recovery_attempt.state IN ('succeeded', 'running', 'unknown', 'retrying')
     )
   );
INSERT INTO results VALUES ('ttl_reconciling_start', changes());

UPDATE commands SET state = 'running', version = version + 1
 WHERE id = 'cmd_ttl_fresh' AND user_id = 'usr' AND state = 'retryable'
   AND expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now');
INSERT INTO results VALUES ('ttl_fresh_start', changes());

-- Even an inconsistent awaiting-confirmation row must preserve evidence of a
-- recoverable effect instead of letting token expiry erase it.
INSERT INTO commands VALUES (
  'cmd_token_recovery', 'usr', NULL, 'send_message', 'idem_token_recovery', 1,
  'awaiting_confirmation', 'hash_token_recovery', 1,
  '2099-01-01T00:00:00.000Z', NULL, '2000-01-01T00:00:00.000Z'
);
INSERT INTO confirmation_tokens VALUES (
  'tok_recovery', 'cmd_token_recovery', 'usr', 'token_recovery_hash',
  'hash_token_recovery', '2000-01-01T00:00:00.000Z', NULL,
  '2000-01-01T00:00:00.000Z'
);
INSERT INTO action_attempts (command_id, user_id, state) VALUES ('cmd_token_recovery', 'usr', 'unknown');
UPDATE commands SET state = 'expired', version = version + 1
 WHERE id = 'cmd_token_recovery' AND user_id = 'usr'
   AND state = 'awaiting_confirmation'
   AND EXISTS (
     SELECT 1 FROM confirmation_tokens
      WHERE command_id = commands.id AND user_id = commands.user_id
        AND used_at IS NULL
        AND expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now')
   )
   AND NOT EXISTS (
     SELECT 1 FROM action_attempts AS recovery_attempt
      WHERE recovery_attempt.command_id = commands.id
        AND recovery_attempt.user_id = commands.user_id
        AND recovery_attempt.state IN ('succeeded', 'running', 'unknown', 'retrying')
   );
INSERT INTO results VALUES ('token_recovery_expired', changes());

-- Cancellation is not an Undo. Once an effect may exist, cancellation must
-- leave both the command and outbox available for reconciliation.
INSERT INTO commands VALUES (
  'cmd_cancel_recovery', 'usr', NULL, 'send_message', 'idem_cancel_recovery', 1,
  'retryable', 'hash_cancel_recovery', 1, '2099-01-01T00:00:00.000Z', NULL,
  '2000-01-01T00:00:00.000Z'
);
INSERT INTO action_attempts (command_id, user_id, state) VALUES ('cmd_cancel_recovery', 'usr', 'retrying');
INSERT INTO outbox_events (
  id, user_id, topic, aggregate_id, idempotency_key, state, updated_at
) VALUES (
  'out_cancel_recovery', 'usr', 'command.execute', 'cmd_cancel_recovery',
  'cancel_outbox_key', 'retrying', '2000-01-01T00:00:00.000Z'
);
UPDATE commands SET state = 'cancelled', version = version + 1
 WHERE id = 'cmd_cancel_recovery' AND user_id = 'usr'
   AND state IN ('pending', 'validated', 'awaiting_confirmation', 'queued', 'retryable')
   AND NOT EXISTS (
     SELECT 1 FROM action_attempts AS recovery_attempt
      WHERE recovery_attempt.command_id = commands.id
        AND recovery_attempt.user_id = commands.user_id
        AND recovery_attempt.state IN ('succeeded', 'running', 'unknown', 'retrying')
   );
UPDATE outbox_events SET state = 'failed'
 WHERE aggregate_id = 'cmd_cancel_recovery' AND user_id = 'usr'
   AND state IN ('queued', 'retrying', 'unknown') AND changes() = 1;
SQL

checks="$(sqlite3 "${DB_FILE}" "SELECT name || '=' || value FROM results ORDER BY name;")"
expected=$'expired_active_permit=0\nexpired_direct_queue=0\nexpired_recovery=1\nexpired_replay_command=0\nexpired_replay_token=0\nlive_active_permit=1\nlive_effect_begin=1\npost_permit_attempt=1\npost_permit_cancel=0\npost_permit_deleted_cancel=1\npost_permit_recovery=1\nstale_effect_begin=0\nstarted_effect_deleted_cancel=0\nstarted_effect_reconcile_start=1\nstarted_effect_recovery=1\ntoken_recovery_expired=0\nttl_fresh_start=0\nttl_reconciling_start=1\nttl_succeeded_start=1\nttl_sweep_expired=1'

if [[ "${checks}" != "${expected}" ]]; then
  printf 'execution-time authority smoke failed:\n%s\n' "${checks}" >&2
  exit 1
fi

state_checks="$(sqlite3 "${DB_FILE}" "SELECT version || ':' || state FROM commands WHERE id = 'cmd_confirm'; SELECT used_at IS NULL FROM confirmation_tokens WHERE id = 'tok_expired'; SELECT version || ':' || state FROM commands WHERE id = 'cmd_lease_expired'; SELECT version || ':' || state FROM commands WHERE id = 'cmd_post_permit'; SELECT COUNT(*) FROM action_attempts WHERE command_id = 'cmd_post_permit' AND state = 'running'; SELECT id || ':' || version || ':' || state FROM commands WHERE id LIKE 'cmd_ttl_%' ORDER BY id; SELECT version || ':' || state FROM commands WHERE id = 'cmd_token_recovery'; SELECT version || ':' || state FROM commands WHERE id = 'cmd_cancel_recovery'; SELECT state FROM outbox_events WHERE id = 'out_cancel_recovery';")"
if [[ "${state_checks}" != $'2:awaiting_confirmation\n1\n5:retryable\n6:cancelled\n1\ncmd_ttl_fresh:2:expired\ncmd_ttl_reconciling:2:running\ncmd_ttl_retrying:1:retryable\ncmd_ttl_succeeded:2:running\ncmd_ttl_unknown:1:unknown\n1:awaiting_confirmation\n1:retryable\nretrying' ]]; then
  printf 'execution-time authority state check failed:\n%s\n' "${state_checks}" >&2
  exit 1
fi

echo "execution-time authority smoke passed: DB-time expiry, exact pre-effect fencing, deleted-session reconciliation, zero-attempt cancellation, and command-TTL recovery hold"
