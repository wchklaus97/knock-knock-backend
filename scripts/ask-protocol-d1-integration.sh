#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
STATE_DIR="$(mktemp -d "${TMPDIR:-/tmp}/knock-knock-ask-d1.XXXXXX")"
SQL_DIR="${STATE_DIR}/sql"
mkdir -p "${SQL_DIR}"
trap 'rm -rf "${STATE_DIR}"' EXIT

d1() {
  wrangler d1 execute DB --local --persist-to "${STATE_DIR}" --config "${ROOT_DIR}/wrangler.toml" "$@"
}

wrangler d1 migrations apply DB --local --persist-to "${STATE_DIR}" --config "${ROOT_DIR}/wrangler.toml"

cat >"${SQL_DIR}/seed.sql" <<'SQL'
INSERT INTO users (id, email, password_hash, created_at)
VALUES ('usr_atomic', 'atomic@example.com', 'x', '2026-08-27T00:00:00.000Z');
INSERT INTO agents (id, user_id, label, api_key_hash, created_at, last_seen_at)
VALUES ('agt_atomic', 'usr_atomic', 'Atomic', 'hash', '2026-08-27T00:00:00.000Z', '2026-08-27T00:00:00.000Z');
INSERT INTO agent_chat_bindings (id, user_id, agent_id, chat_id, chat_title, listener_instance_id, status, last_seen_at, expires_at, revoked_at, lease_id, generation, created_at, updated_at)
VALUES
  ('bind_a', 'usr_atomic', 'agt_atomic', 'chat_a', 'A', 'instance_a', 'revoked', '2026-08-27T00:00:00.000Z', '2099-01-01T00:00:00.000Z', '2026-08-27T00:00:01.000Z', 'lease_aa', 1, '2026-08-27T00:00:00.000Z', '2026-08-27T00:00:01.000Z'),
  ('bind_b', 'usr_atomic', 'agt_atomic', 'chat_b', 'B', 'instance_b', 'active', '2026-08-27T00:00:01.000Z', '2099-01-01T00:00:00.000Z', NULL, 'lease_bb', 2, '2026-08-27T00:00:01.000Z', '2026-08-27T00:00:01.000Z'),
  ('bind_c', 'usr_atomic', 'agt_atomic', 'chat_c', 'C', 'instance_c', 'offline', '2026-08-27T00:00:02.000Z', '2099-01-01T00:00:00.000Z', NULL, NULL, 0, '2026-08-27T00:00:02.000Z', '2026-08-27T00:00:02.000Z');
INSERT INTO agent_listener_leases (agent_id, user_id, binding_id, chat_id, chat_title, listener_instance_id, lease_id, generation, acquired_at, last_seen_at, expires_at, updated_at)
VALUES ('agt_atomic', 'usr_atomic', 'bind_b', 'chat_b', 'B', 'instance_b', 'lease_bb', 2, '2026-08-27T00:00:01.000Z', '2026-08-27T00:00:01.000Z', '2099-01-01T00:00:00.000Z', '2026-08-27T00:00:01.000Z');
SQL
d1 --file "${SQL_DIR}/seed.sql" >/dev/null

# The caller preflighted generation 1, but generation 2 took over before the
# atomic D1 batch. Both conditional inserts must write zero rows.
cat >"${SQL_DIR}/stale-fence.sql" <<'SQL'
INSERT INTO sessions (id, agent_id, user_id, skill_id, state, title, chat_id, facts_json, idempotency_key, expires_at, retention_expires_at, created_at, updated_at)
SELECT 'ses_stale', 'agt_atomic', 'usr_atomic', 'phone.ask', 'open', 'stale', 'chat_a', '{}', 'ask:turn-stale-0001', '2099-01-01T00:00:00.000Z', '2099-01-01T00:00:00.000Z', '2026-08-27T00:00:02.000Z', '2026-08-27T00:00:02.000Z'
WHERE EXISTS (SELECT 1 FROM agent_listener_leases WHERE agent_id = 'agt_atomic' AND binding_id = 'bind_a' AND chat_id = 'chat_a' AND lease_id = 'lease_aa' AND generation = 1 AND expires_at > '2026-08-27T00:00:02.000Z');
INSERT INTO phone_asks (id, user_id, agent_id, transcript, locale, idempotency_key, client_turn_id, conversation_id, session_id, binding_id, lease_id, listener_generation, target_chat_id, status, attempt_count, expires_at, created_at, updated_at)
SELECT 'ask_stale', 'usr_atomic', 'agt_atomic', 'stale', 'en', 'turn-stale-0001', 'turn-stale-0001', 'chat_a', 'ses_stale', 'bind_a', 'lease_aa', 1, 'chat_a', 'queued', 0, '2099-01-01T00:00:00.000Z', '2026-08-27T00:00:02.000Z', '2026-08-27T00:00:02.000Z'
WHERE EXISTS (SELECT 1 FROM sessions WHERE id = 'ses_stale' AND user_id = 'usr_atomic' AND agent_id = 'agt_atomic' AND skill_id = 'phone.ask' AND chat_id = 'chat_a')
  AND EXISTS (SELECT 1 FROM agent_listener_leases WHERE agent_id = 'agt_atomic' AND binding_id = 'bind_a' AND chat_id = 'chat_a' AND lease_id = 'lease_aa' AND generation = 1 AND expires_at > '2026-08-27T00:00:02.000Z');
SQL
d1 --file "${SQL_DIR}/stale-fence.sql" >/dev/null
stale_result="$(d1 --command "SELECT CASE WHEN (SELECT count(*) FROM sessions WHERE id = 'ses_stale') = 0 AND (SELECT count(*) FROM phone_asks WHERE id = 'ask_stale') = 0 THEN 'STALE_FENCE_ATOMIC_OK' ELSE 'STALE_FENCE_ATOMIC_FAILED' END AS result;")"
grep -q 'STALE_FENCE_ATOMIC_OK' <<<"${stale_result}"

cat >"${SQL_DIR}/valid-fence.sql" <<'SQL'
INSERT INTO sessions (id, agent_id, user_id, skill_id, state, title, chat_id, facts_json, idempotency_key, expires_at, retention_expires_at, created_at, updated_at)
SELECT 'ses_exact', 'agt_atomic', 'usr_atomic', 'phone.ask', 'running', 'exact', 'chat_b', '{}', 'ask:turn-exact-0001', '2099-01-01T00:00:00.000Z', '2099-01-01T00:00:00.000Z', '2026-08-27T00:00:03.000Z', '2026-08-27T00:00:03.000Z'
WHERE EXISTS (SELECT 1 FROM agent_listener_leases WHERE agent_id = 'agt_atomic' AND binding_id = 'bind_b' AND chat_id = 'chat_b' AND lease_id = 'lease_bb' AND generation = 2 AND expires_at > '2026-08-27T00:00:03.000Z');
INSERT INTO phone_asks (id, user_id, agent_id, transcript, locale, idempotency_key, client_turn_id, conversation_id, session_id, binding_id, lease_id, listener_generation, target_chat_id, status, attempt_count, expires_at, created_at, updated_at)
SELECT 'ask_exact', 'usr_atomic', 'agt_atomic', 'exact', 'en', 'idem-exact-0001', 'turn-exact-0001', 'chat_b', 'ses_exact', 'bind_b', 'lease_bb', 2, 'chat_b', 'queued', 0, '2099-01-01T00:00:00.000Z', '2026-08-27T00:00:03.000Z', '2026-08-27T00:00:03.000Z'
WHERE EXISTS (SELECT 1 FROM sessions WHERE id = 'ses_exact' AND user_id = 'usr_atomic' AND agent_id = 'agt_atomic' AND skill_id = 'phone.ask' AND chat_id = 'chat_b')
  AND EXISTS (SELECT 1 FROM agent_listener_leases WHERE agent_id = 'agt_atomic' AND binding_id = 'bind_b' AND chat_id = 'chat_b' AND lease_id = 'lease_bb' AND generation = 2 AND expires_at > '2026-08-27T00:00:03.000Z');
INSERT INTO session_messages (id, user_id, session_id, role, content, metadata_json, sequence, retention_expires_at, created_at)
SELECT 'msg_phone_ask_exact', 'usr_atomic', 'ses_exact', 'user', 'exact', '{"ask_id":"ask_exact"}', 1, '2099-01-01T00:00:00.000Z', '2026-08-27T00:00:03.000Z'
WHERE EXISTS (SELECT 1 FROM phone_asks WHERE id = 'ask_exact' AND session_id = 'ses_exact' AND binding_id = 'bind_b' AND lease_id = 'lease_bb' AND listener_generation = 2 AND target_chat_id = 'chat_b');
SQL
d1 --file "${SQL_DIR}/valid-fence.sql" >/dev/null
valid_result="$(d1 --command "SELECT CASE WHEN (SELECT count(*) FROM sessions WHERE id = 'ses_exact') = 1 AND (SELECT count(*) FROM phone_asks WHERE id = 'ask_exact' AND user_id = 'usr_atomic' AND agent_id = 'agt_atomic' AND client_turn_id = 'turn-exact-0001' AND conversation_id = 'chat_b' AND target_chat_id = 'chat_b') = 1 AND (SELECT count(*) FROM session_messages WHERE id = 'msg_phone_ask_exact') = 1 THEN 'EXACT_REPLAY_IDENTITY_OK' ELSE 'EXACT_REPLAY_IDENTITY_FAILED' END AS result;")"
grep -q 'EXACT_REPLAY_IDENTITY_OK' <<<"${valid_result}"

# A later takeover cannot reinterpret the prior turn as belonging to chat C.
d1 --command "UPDATE agent_listener_leases SET binding_id = 'bind_c', chat_id = 'chat_c', chat_title = 'C', listener_instance_id = 'instance_c', lease_id = 'lease_cc', generation = 3, updated_at = '2026-08-27T00:00:04.000Z' WHERE agent_id = 'agt_atomic' AND lease_id = 'lease_bb' AND generation = 2;" >/dev/null
cross_chat_result="$(d1 --command "SELECT CASE WHEN (SELECT count(*) FROM phone_asks WHERE user_id = 'usr_atomic' AND agent_id = 'agt_atomic' AND client_turn_id = 'turn-exact-0001' AND binding_id = 'bind_b' AND lease_id = 'lease_bb' AND listener_generation = 2 AND target_chat_id = 'chat_b' AND conversation_id = 'chat_b' AND session_id = 'ses_exact') = 1 AND (SELECT count(*) FROM sessions WHERE agent_id = 'agt_atomic' AND idempotency_key = 'ask:turn-exact-0001' AND user_id = 'usr_atomic' AND skill_id = 'phone.ask' AND chat_id = 'chat_b') = 1 THEN 'CROSS_CHAT_REPLAY_FENCED_OK' ELSE 'CROSS_CHAT_REPLAY_FENCED_FAILED' END AS result;")"
grep -q 'CROSS_CHAT_REPLAY_FENCED_OK' <<<"${cross_chat_result}"

cat >"${SQL_DIR}/reliability-seed.sql" <<'SQL'
INSERT INTO agents (id, user_id, label, api_key_hash, created_at, last_seen_at)
VALUES ('agt_reliability', 'usr_atomic', 'Reliability', 'hash_reliability', '2026-08-27T01:00:00.000Z', '2026-08-27T01:00:00.000Z');
INSERT INTO agent_chat_bindings (id, user_id, agent_id, chat_id, chat_title, listener_instance_id, status, last_seen_at, expires_at, revoked_at, lease_id, generation, created_at, updated_at)
VALUES ('bind_rel', 'usr_atomic', 'agt_reliability', 'chat_rel', 'Reliability', 'instance_rel', 'active', '2026-08-27T01:00:00.000Z', '2099-01-01T00:00:00.000Z', NULL, 'lease_rel_4', 4, '2026-08-27T01:00:00.000Z', '2026-08-27T01:00:00.000Z');
INSERT INTO agent_listener_leases (agent_id, user_id, binding_id, chat_id, chat_title, listener_instance_id, lease_id, generation, acquired_at, last_seen_at, expires_at, updated_at)
VALUES ('agt_reliability', 'usr_atomic', 'bind_rel', 'chat_rel', 'Reliability', 'instance_rel', 'lease_rel_4', 4, '2026-08-27T01:00:00.000Z', '2026-08-27T01:00:00.000Z', '2099-01-01T00:00:00.000Z', '2026-08-27T01:00:00.000Z');
INSERT INTO devices (id, user_id, platform, device_id, push_token, locale, timezone, created_at, updated_at)
VALUES ('dev_rel', 'usr_atomic', 'ios', 'physical_rel', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 'en', 'UTC', '2026-08-27T01:00:00.000Z', '2026-08-27T01:00:00.000Z');
INSERT INTO sessions (id, agent_id, user_id, skill_id, state, title, chat_id, facts_json, idempotency_key, expires_at, retention_expires_at, created_at, updated_at)
VALUES ('ses_expired', 'agt_reliability', 'usr_atomic', 'phone.ask', 'open', 'expired', 'chat_rel', '{}', 'manual-expired-session', '2020-01-01T00:00:00.000Z', '2099-01-01T00:00:00.000Z', '2026-08-27T01:00:00.000Z', '2026-08-27T01:00:00.000Z');
SQL
d1 --file "${SQL_DIR}/reliability-seed.sql" >/dev/null

# An explicit expired session fails the same atomic predicates used by runtime.
# No Ask, message, event, or session mutation is permitted, leaving the client
# turn free for a sessionless retry.
cat >"${SQL_DIR}/expired-session.sql" <<'SQL'
INSERT INTO phone_asks (id, user_id, agent_id, transcript, locale, idempotency_key, client_turn_id, conversation_id, session_id, binding_id, lease_id, listener_generation, target_chat_id, status, attempt_count, expires_at, created_at, updated_at)
SELECT 'ask_expired', 'usr_atomic', 'agt_reliability', 'expired', 'en', 'turn-expired-0001', 'turn-expired-0001', 'chat_rel', 'ses_expired', 'bind_rel', 'lease_rel_4', 4, 'chat_rel', 'queued', 0, '2099-01-01T00:00:00.000Z', '2026-08-27T01:00:01.000Z', '2026-08-27T01:00:01.000Z'
WHERE EXISTS (SELECT 1 FROM sessions s WHERE s.id = 'ses_expired' AND s.user_id = 'usr_atomic' AND s.agent_id = 'agt_reliability' AND s.skill_id = 'phone.ask' AND s.chat_id = 'chat_rel' AND s.deleted_at IS NULL AND s.state != 'expired' AND s.expires_at > '2026-08-27T01:00:01.000Z')
  AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = 'agt_reliability' AND l.binding_id = 'bind_rel' AND l.chat_id = 'chat_rel' AND l.lease_id = 'lease_rel_4' AND l.generation = 4 AND l.expires_at > '2026-08-27T01:00:01.000Z');
INSERT INTO session_messages (id, user_id, session_id, role, content, metadata_json, sequence, retention_expires_at, created_at)
SELECT 'msg_phone_ask_expired', 'usr_atomic', 'ses_expired', 'user', 'expired', '{"ask_id":"ask_expired"}', 1, '2099-01-01T00:00:00.000Z', '2026-08-27T01:00:01.000Z'
WHERE EXISTS (SELECT 1 FROM phone_asks WHERE id = 'ask_expired');
INSERT INTO events (id, session_id, status, idempotency_key, payload_json, pushed, summary_text, voice_script, created_at)
SELECT 'evt_expired', 'ses_expired', 'info', 'evt-expired-0001', '{}', 0, 'invalid', 'invalid', '2026-08-27T01:00:01.000Z'
WHERE EXISTS (SELECT 1 FROM phone_asks WHERE id = 'ask_expired');
SQL
d1 --file "${SQL_DIR}/expired-session.sql" >/dev/null
expired_result="$(d1 --command "SELECT CASE WHEN (SELECT count(*) FROM phone_asks WHERE id = 'ask_expired') = 0 AND (SELECT count(*) FROM session_messages WHERE id = 'msg_phone_ask_expired') = 0 AND (SELECT count(*) FROM events WHERE id = 'evt_expired') = 0 AND (SELECT count(*) FROM sessions WHERE id = 'ses_expired' AND state = 'open' AND facts_json = '{}' AND updated_at = '2026-08-27T01:00:00.000Z') = 1 THEN 'EXPIRED_SESSION_ZERO_WRITES_OK' ELSE 'EXPIRED_SESSION_ZERO_WRITES_FAILED' END AS result;")"
grep -q 'EXPIRED_SESSION_ZERO_WRITES_OK' <<<"${expired_result}"

cat >"${SQL_DIR}/sessionless-retry.sql" <<'SQL'
INSERT INTO sessions (id, agent_id, user_id, skill_id, state, title, chat_id, facts_json, idempotency_key, expires_at, retention_expires_at, created_at, updated_at)
SELECT 'ses_retry', 'agt_reliability', 'usr_atomic', 'phone.ask', 'open', 'retry', 'chat_rel', '{}', 'ask:turn-expired-0001', '2099-01-01T00:00:00.000Z', '2099-01-01T00:00:00.000Z', '2026-08-27T01:00:02.000Z', '2026-08-27T01:00:02.000Z'
WHERE EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = 'agt_reliability' AND l.binding_id = 'bind_rel' AND l.chat_id = 'chat_rel' AND l.lease_id = 'lease_rel_4' AND l.generation = 4 AND l.expires_at > '2026-08-27T01:00:02.000Z');
INSERT INTO phone_asks (id, user_id, agent_id, transcript, locale, idempotency_key, client_turn_id, conversation_id, session_id, binding_id, lease_id, listener_generation, target_chat_id, status, attempt_count, expires_at, created_at, updated_at)
SELECT 'ask_retry', 'usr_atomic', 'agt_reliability', 'retry', 'en', 'turn-expired-0001', 'turn-expired-0001', 'chat_rel', 'ses_retry', 'bind_rel', 'lease_rel_4', 4, 'chat_rel', 'queued', 0, '2099-01-01T00:00:00.000Z', '2026-08-27T01:00:02.000Z', '2026-08-27T01:00:02.000Z'
WHERE EXISTS (SELECT 1 FROM sessions s WHERE s.id = 'ses_retry' AND s.user_id = 'usr_atomic' AND s.agent_id = 'agt_reliability' AND s.skill_id = 'phone.ask' AND s.chat_id = 'chat_rel' AND s.deleted_at IS NULL AND s.state != 'expired' AND s.expires_at > '2026-08-27T01:00:02.000Z')
  AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = 'agt_reliability' AND l.binding_id = 'bind_rel' AND l.chat_id = 'chat_rel' AND l.lease_id = 'lease_rel_4' AND l.generation = 4 AND l.expires_at > '2026-08-27T01:00:02.000Z');
INSERT INTO session_messages (id, user_id, session_id, role, content, metadata_json, sequence, retention_expires_at, created_at)
SELECT 'msg_phone_ask_retry', 'usr_atomic', 'ses_retry', 'user', 'retry', '{"ask_id":"ask_retry"}', 1, '2099-01-01T00:00:00.000Z', '2026-08-27T01:00:02.000Z'
WHERE EXISTS (SELECT 1 FROM phone_asks WHERE id = 'ask_retry');
UPDATE sessions SET state = 'running', updated_at = '2026-08-27T01:00:02.000Z' WHERE id = 'ses_retry' AND EXISTS (SELECT 1 FROM phone_asks WHERE id = 'ask_retry');
SQL
d1 --file "${SQL_DIR}/sessionless-retry.sql" >/dev/null
retry_result="$(d1 --command "SELECT CASE WHEN (SELECT count(*) FROM phone_asks WHERE id = 'ask_retry' AND client_turn_id = 'turn-expired-0001' AND session_id = 'ses_retry') = 1 AND (SELECT count(*) FROM session_messages WHERE id = 'msg_phone_ask_retry') = 1 THEN 'SESSIONLESS_RETRY_OK' ELSE 'SESSIONLESS_RETRY_FAILED' END AS result;")"
grep -q 'SESSIONLESS_RETRY_OK' <<<"${retry_result}"

# Claim generation 4, take over the same bound chat at generation 5, then
# prove the unexpired old claim is visible and immediately reclaimable.
d1 --command "UPDATE phone_asks SET status = 'claimed', claimed_at = '2026-08-27T01:00:03.000Z', claimed_by_chat_id = 'chat_rel', claim_token = 'claim_rel_old', claim_deadline = '2099-01-01T00:00:00.000Z', claim_generation = 4, attempt_count = attempt_count + 1, updated_at = '2026-08-27T01:00:03.000Z' WHERE id = 'ask_retry' AND status = 'queued';" >/dev/null
d1 --command "UPDATE agent_listener_leases SET lease_id = 'lease_rel_5', generation = 5, acquired_at = '2026-08-27T01:00:04.000Z', last_seen_at = '2026-08-27T01:00:04.000Z', updated_at = '2026-08-27T01:00:04.000Z' WHERE agent_id = 'agt_reliability' AND lease_id = 'lease_rel_4' AND generation = 4; UPDATE agent_chat_bindings SET lease_id = 'lease_rel_5', generation = 5, updated_at = '2026-08-27T01:00:04.000Z' WHERE id = 'bind_rel';" >/dev/null
takeover_visible="$(d1 --command "SELECT CASE WHEN EXISTS (SELECT 1 FROM phone_asks WHERE id = 'ask_retry' AND binding_id = 'bind_rel' AND target_chat_id = 'chat_rel' AND claimed_by_chat_id = 'chat_rel' AND answered_at IS NULL AND claim_token IS NOT NULL AND claim_generation IS NOT NULL AND claim_generation <> 5 AND claim_deadline > '2026-08-27T01:00:04.000Z') THEN 'TAKEOVER_VISIBLE_OK' ELSE 'TAKEOVER_VISIBLE_FAILED' END AS result;")"
grep -q 'TAKEOVER_VISIBLE_OK' <<<"${takeover_visible}"
d1 --command "UPDATE phone_asks SET status = 'claimed', claimed_at = '2026-08-27T01:00:04.000Z', claimed_by_chat_id = 'chat_rel', claim_token = 'claim_rel_new', claim_deadline = '2099-01-01T00:00:00.000Z', claim_generation = 5, attempt_count = attempt_count + 1, updated_at = '2026-08-27T01:00:04.000Z' WHERE id = 'ask_retry' AND binding_id = 'bind_rel' AND target_chat_id = 'chat_rel' AND COALESCE(conversation_id, target_chat_id) = 'chat_rel' AND answered_at IS NULL AND expires_at > '2026-08-27T01:00:04.000Z' AND status = 'claimed' AND claim_token IS NOT NULL AND claim_deadline IS NOT NULL AND ((claim_generation IS NOT NULL AND claim_generation <> 5) OR claim_deadline <= '2026-08-27T01:00:04.000Z') AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = 'agt_reliability' AND l.binding_id = 'bind_rel' AND l.chat_id = 'chat_rel' AND l.lease_id = 'lease_rel_5' AND l.generation = 5 AND l.expires_at > '2026-08-27T01:00:04.000Z');" >/dev/null
reclaim_result="$(d1 --command "SELECT CASE WHEN (SELECT count(*) FROM phone_asks WHERE id = 'ask_retry' AND claim_token = 'claim_rel_new' AND claim_generation = 5 AND attempt_count = 2) = 1 THEN 'IMMEDIATE_RECLAIM_OK' ELSE 'IMMEDIATE_RECLAIM_FAILED' END AS result;")"
grep -q 'IMMEDIATE_RECLAIM_OK' <<<"${reclaim_result}"

# The stale listener cannot mutate progress or reserve the answer. The current
# claim can update progress, and the terminal actionless answer creates one
# durable refresh intent for the physical iOS registration.
d1 --command "UPDATE sessions SET progress_status = 'running', facts_json = '{\"phase\":\"stale\"}', updated_at = '2026-08-27T01:00:05.000Z' WHERE id = 'ses_retry' AND EXISTS (SELECT 1 FROM phone_asks a WHERE a.id = 'ask_retry' AND a.session_id = sessions.id AND a.claim_token = 'claim_rel_old' AND a.claim_generation = 4 AND a.claim_deadline > '2026-08-27T01:00:05.000Z' AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = sessions.agent_id AND l.binding_id = a.binding_id AND l.chat_id = a.target_chat_id AND l.lease_id = 'lease_rel_4' AND l.generation = 4 AND l.expires_at > '2026-08-27T01:00:05.000Z')); UPDATE phone_asks SET answered_at = '2026-08-27T01:00:05.000Z', reply_event_id = 'evt_stale' WHERE id = 'ask_retry' AND claim_token = 'claim_rel_old' AND claim_generation = 4 AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = phone_asks.agent_id AND l.binding_id = phone_asks.binding_id AND l.chat_id = phone_asks.target_chat_id AND l.generation = 4 AND l.expires_at > '2026-08-27T01:00:05.000Z');" >/dev/null
stale_mutation_result="$(d1 --command "SELECT CASE WHEN (SELECT count(*) FROM sessions WHERE id = 'ses_retry' AND progress_status IS NULL AND facts_json = '{}') = 1 AND (SELECT count(*) FROM phone_asks WHERE id = 'ask_retry' AND answered_at IS NULL AND reply_event_id IS NULL) = 1 THEN 'STALE_PROGRESS_ANSWER_ZERO_WRITES_OK' ELSE 'STALE_PROGRESS_ANSWER_ZERO_WRITES_FAILED' END AS result;")"
grep -q 'STALE_PROGRESS_ANSWER_ZERO_WRITES_OK' <<<"${stale_mutation_result}"
d1 --command "UPDATE sessions SET progress_status = 'running', facts_json = '{\"phase\":\"current\"}', updated_at = '2026-08-27T01:00:06.000Z' WHERE id = 'ses_retry' AND agent_id = 'agt_reliability' AND user_id = 'usr_atomic' AND state NOT IN ('expired', 'closed', 'completed', 'failed') AND expires_at > '2026-08-27T01:00:06.000Z' AND EXISTS (SELECT 1 FROM phone_asks a WHERE a.id = 'ask_retry' AND a.session_id = sessions.id AND a.agent_id = sessions.agent_id AND a.user_id = sessions.user_id AND a.status = 'claimed' AND a.answered_at IS NULL AND a.reply_event_id IS NULL AND a.claim_token = 'claim_rel_new' AND a.claim_generation = 5 AND a.claim_deadline > '2026-08-27T01:00:06.000Z' AND a.binding_id = 'bind_rel' AND a.target_chat_id = 'chat_rel' AND a.claimed_by_chat_id = 'chat_rel' AND COALESCE(a.conversation_id, a.target_chat_id) = 'chat_rel' AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = sessions.agent_id AND l.binding_id = a.binding_id AND l.chat_id = a.target_chat_id AND l.lease_id = 'lease_rel_5' AND l.generation = 5 AND l.expires_at > '2026-08-27T01:00:06.000Z'));" >/dev/null
progress_result="$(d1 --command "SELECT CASE WHEN (SELECT count(*) FROM sessions WHERE id = 'ses_retry' AND progress_status = 'running' AND facts_json = '{\"phase\":\"current\"}') = 1 THEN 'CURRENT_PROGRESS_OK' ELSE 'CURRENT_PROGRESS_FAILED' END AS result;")"
grep -q 'CURRENT_PROGRESS_OK' <<<"${progress_result}"

cat >"${SQL_DIR}/actionless-answer.sql" <<'SQL'
UPDATE phone_asks SET answered_at = '2026-08-27T01:00:07.000Z', reply_event_id = 'evt_actionless', updated_at = '2026-08-27T01:00:07.000Z'
WHERE id = 'ask_retry' AND session_id = 'ses_retry' AND agent_id = 'agt_reliability' AND user_id = 'usr_atomic' AND status = 'claimed' AND answered_at IS NULL AND reply_event_id IS NULL AND claim_token = 'claim_rel_new' AND claim_generation = 5 AND claim_deadline > '2026-08-27T01:00:07.000Z' AND target_chat_id = 'chat_rel' AND claimed_by_chat_id = 'chat_rel' AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = phone_asks.agent_id AND l.binding_id = phone_asks.binding_id AND l.chat_id = phone_asks.target_chat_id AND l.generation = 5 AND l.expires_at > '2026-08-27T01:00:07.000Z');
INSERT INTO events (id, session_id, status, idempotency_key, payload_json, pushed, summary_text, voice_script, created_at)
SELECT 'evt_actionless', 'ses_retry', 'succeeded', 'answer-actionless-0001', '{"actions":[]}', 1, 'Informational answer', 'Informational answer', '2026-08-27T01:00:07.000Z'
WHERE EXISTS (SELECT 1 FROM phone_asks WHERE id = 'ask_retry' AND reply_event_id = 'evt_actionless' AND answered_at = '2026-08-27T01:00:07.000Z')
ON CONFLICT(session_id, idempotency_key) DO NOTHING;
UPDATE sessions SET state = 'closed', summary_text = 'Informational answer', voice_script = 'Informational answer', updated_at = '2026-08-27T01:00:07.000Z'
WHERE id = 'ses_retry' AND EXISTS (SELECT 1 FROM events WHERE id = 'evt_actionless' AND session_id = 'ses_retry' AND idempotency_key = 'answer-actionless-0001');
INSERT OR IGNORE INTO pushes (id, user_id, session_id, title, body, voice_script, payload_json, dedupe_key, created_at, updated_at)
SELECT 'push_actionless', 'usr_atomic', 'ses_retry', 'retry', 'Informational answer', 'Informational answer', '{"event_id":"evt_actionless","actions":[]}', 'session.event.notification:usr_atomic:evt_actionless', '2026-08-27T01:00:07.000Z', '2026-08-27T01:00:07.000Z'
WHERE EXISTS (SELECT 1 FROM events WHERE id = 'evt_actionless' AND session_id = 'ses_retry' AND idempotency_key = 'answer-actionless-0001');
INSERT INTO outbox_events (id, user_id, topic, aggregate_id, payload_json, idempotency_key, state, attempts, next_attempt_at, last_error, created_at, updated_at, lease_token, lease_expires_at)
SELECT 'out_actionless', 'usr_atomic', 'session.event.notification', 'evt_actionless', json_object('event_id', 'evt_actionless', 'push_id', pushes.id, 'session_id', 'ses_retry', 'title', 'retry', 'body', 'Informational answer', 'voice_script', 'Informational answer', 'payload', json('{"event_id":"evt_actionless","actions":[]}')), 'session.event.notification:usr_atomic:evt_actionless', 'queued', 0, NULL, NULL, '2026-08-27T01:00:07.000Z', '2026-08-27T01:00:07.000Z', NULL, NULL
FROM pushes WHERE pushes.user_id = 'usr_atomic' AND pushes.dedupe_key = 'session.event.notification:usr_atomic:evt_actionless' AND EXISTS (SELECT 1 FROM events WHERE id = 'evt_actionless' AND session_id = 'ses_retry' AND idempotency_key = 'answer-actionless-0001')
ON CONFLICT(topic, idempotency_key) DO NOTHING;
SQL
d1 --file "${SQL_DIR}/actionless-answer.sql" >/dev/null
d1 --file "${SQL_DIR}/actionless-answer.sql" >/dev/null
answer_result="$(d1 --command "SELECT CASE WHEN (SELECT count(*) FROM events WHERE id = 'evt_actionless' AND status = 'succeeded' AND pushed = 1) = 1 AND (SELECT count(*) FROM actions WHERE session_id = 'ses_retry') = 0 AND (SELECT count(*) FROM pushes WHERE user_id = 'usr_atomic' AND dedupe_key = 'session.event.notification:usr_atomic:evt_actionless') = 1 AND (SELECT count(*) FROM outbox_events WHERE topic = 'session.event.notification' AND idempotency_key = 'session.event.notification:usr_atomic:evt_actionless' AND state = 'queued') = 1 AND (SELECT count(*) FROM devices WHERE user_id = 'usr_atomic' AND platform = 'ios' AND push_token IS NOT NULL) = 1 THEN 'ACTIONLESS_DURABLE_REFRESH_OK' ELSE 'ACTIONLESS_DURABLE_REFRESH_FAILED' END AS result;")"
grep -q 'ACTIONLESS_DURABLE_REFRESH_OK' <<<"${answer_result}"

echo "Ask protocol D1 integration passed: atomic expiry, immediate takeover, fenced progress, actionless durable refresh, and exact replay identity"
