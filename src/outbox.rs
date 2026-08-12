use serde::Deserialize;
use serde_json::{json, Map, Value};
use worker::wasm_bindgen::JsValue;
use worker::D1Database;

use crate::action_effects;
use crate::apns;
use crate::auth::{config_value, new_id};
use crate::commands;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::history;
use crate::models::{CommandRow, OutboxEventRow};
use crate::providers::{self, ActionProviderConfig};

const BATCH_SIZE: i64 = 20;
const MAX_ATTEMPTS: i32 = 3;
const LEASE_SECONDS: i64 = 300;
const UNKNOWN_RECONCILE_SECONDS: i64 = 300;
const COMMAND_EXECUTE_TOPIC: &str = "command.execute";
const ACTIVE_COMMAND_CLAIM_FENCE_BIND_COUNT: usize = 7;
const RECOVERY_COMMAND_CLAIM_FENCE_BIND_COUNT: usize = 9;

#[cfg(test)]
fn db_now_sql() -> &'static str {
    "strftime('%Y-%m-%dT%H:%M:%fZ','now')"
}

fn outbox_select() -> &'static str {
    "SELECT id, user_id, topic, aggregate_id, payload_json, idempotency_key, state, attempts, next_attempt_at, last_error, created_at, updated_at, lease_token, lease_expires_at FROM outbox_events"
}

fn lease_fence_sql() -> &'static str {
    " AND (lease_token = ? OR (lease_token IS NULL AND ? IS NULL))"
}

fn active_command_claim_fence_sql() -> &'static str {
    " AND EXISTS (SELECT 1 FROM outbox_events AS claim WHERE claim.id = ? AND claim.user_id = ? AND claim.user_id = commands.user_id AND claim.topic = ? AND claim.topic = 'command.execute' AND claim.aggregate_id = ? AND claim.aggregate_id = commands.id AND claim.idempotency_key = ? AND claim.idempotency_key = ? AND claim.state = 'running' AND claim.lease_token = ? AND claim.lease_expires_at IS NOT NULL AND claim.lease_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now'))"
}

fn recovery_command_claim_fence_sql() -> &'static str {
    " AND EXISTS (SELECT 1 FROM outbox_events AS claim WHERE claim.id = ? AND claim.user_id = ? AND claim.user_id = commands.user_id AND claim.topic = ? AND claim.topic = 'command.execute' AND claim.aggregate_id = ? AND claim.aggregate_id = commands.id AND claim.idempotency_key = ? AND claim.idempotency_key = ? AND claim.state = 'running' AND (claim.lease_token = ? OR (claim.lease_token IS NULL AND ? IS NULL)) AND ((claim.lease_expires_at IS NOT NULL AND claim.lease_expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now')) OR (claim.lease_expires_at IS NULL AND claim.updated_at <= ?)))"
}

fn sql_parameter_count(sql: &str) -> usize {
    sql.bytes().filter(|byte| *byte == b'?').count()
}

fn expected_execution_outbox_key(
    user_id: &str,
    command_id: &str,
    command_idempotency_key: &str,
    needs_confirmation: bool,
) -> String {
    if needs_confirmation {
        providers::scoped_idempotency_key(user_id, "command.execute.confirm", command_id)
    } else {
        providers::scoped_idempotency_key(user_id, "command.execute", command_idempotency_key)
    }
}

#[derive(Debug, Eq, PartialEq)]
struct CommandClaimFence<'a> {
    outbox_id: &'a str,
    owner_id: &'a str,
    topic: &'a str,
    aggregate_id: &'a str,
    expected_aggregate_id: &'a str,
    outbox_idempotency_key: &'a str,
    expected_idempotency_key: String,
    lease_token: Option<&'a str>,
}

#[derive(Clone, Copy)]
struct ExpiredLeaseWindow<'a> {
    recovery_now: &'a str,
    legacy_cutoff: &'a str,
}

#[derive(Debug, Eq, PartialEq)]
enum ClaimFenceBinding<'a> {
    Text(&'a str),
    OptionalText(Option<&'a str>),
}

impl ClaimFenceBinding<'_> {
    fn into_js_value(self) -> JsValue {
        match self {
            Self::Text(value) => db::text(value),
            Self::OptionalText(value) => db::optional_text(value),
        }
    }
}

impl<'a> CommandClaimFence<'a> {
    fn new(row: &'a OutboxEventRow, user_id: &'a str, command: &'a CommandRow) -> Self {
        Self {
            outbox_id: &row.id,
            owner_id: user_id,
            topic: &row.topic,
            aggregate_id: &row.aggregate_id,
            expected_aggregate_id: &command.id,
            outbox_idempotency_key: &row.idempotency_key,
            expected_idempotency_key: expected_execution_outbox_key(
                user_id,
                &command.id,
                &command.idempotency_key,
                command.needs_confirmation != 0,
            ),
            lease_token: row.lease_token.as_deref(),
        }
    }

    fn authorizes_identity(&self, current: &OutboxEventRow) -> bool {
        current.id == self.outbox_id
            && current.user_id.as_deref() == Some(self.owner_id)
            && current.topic == self.topic
            && current.topic == COMMAND_EXECUTE_TOPIC
            && current.aggregate_id == self.aggregate_id
            && current.aggregate_id == self.expected_aggregate_id
            && current.idempotency_key == self.outbox_idempotency_key
            && current.idempotency_key == self.expected_idempotency_key
            && current.state == "running"
            && current.lease_token.as_deref() == self.lease_token
    }

    fn authorizes_active(&self, current: &OutboxEventRow, transition_now: &str) -> bool {
        self.authorizes_identity(current)
            && self.lease_token.is_some()
            && current
                .lease_expires_at
                .as_deref()
                .is_some_and(|lease_expires_at| lease_expires_at > transition_now)
    }

    fn authorizes_recovery(
        &self,
        current: &OutboxEventRow,
        recovery_now: &str,
        legacy_cutoff: &str,
    ) -> bool {
        self.authorizes_identity(current)
            && match current.lease_expires_at.as_deref() {
                Some(lease_expires_at) => lease_expires_at <= recovery_now,
                None => current.updated_at.as_str() <= legacy_cutoff,
            }
    }

    fn active_bindings(&self) -> [ClaimFenceBinding<'_>; ACTIVE_COMMAND_CLAIM_FENCE_BIND_COUNT] {
        [
            ClaimFenceBinding::Text(self.outbox_id),
            ClaimFenceBinding::Text(self.owner_id),
            ClaimFenceBinding::Text(self.topic),
            ClaimFenceBinding::Text(self.aggregate_id),
            ClaimFenceBinding::Text(self.outbox_idempotency_key),
            ClaimFenceBinding::Text(&self.expected_idempotency_key),
            ClaimFenceBinding::OptionalText(self.lease_token),
        ]
    }

    fn recovery_bindings<'b>(
        &'b self,
        legacy_cutoff: &'b str,
    ) -> [ClaimFenceBinding<'b>; RECOVERY_COMMAND_CLAIM_FENCE_BIND_COUNT] {
        [
            ClaimFenceBinding::Text(self.outbox_id),
            ClaimFenceBinding::Text(self.owner_id),
            ClaimFenceBinding::Text(self.topic),
            ClaimFenceBinding::Text(self.aggregate_id),
            ClaimFenceBinding::Text(self.outbox_idempotency_key),
            ClaimFenceBinding::Text(&self.expected_idempotency_key),
            ClaimFenceBinding::OptionalText(self.lease_token),
            ClaimFenceBinding::OptionalText(self.lease_token),
            ClaimFenceBinding::Text(legacy_cutoff),
        ]
    }

    fn append_active_bindings(&self, values: &mut Vec<JsValue>) {
        values.extend(
            self.active_bindings()
                .into_iter()
                .map(ClaimFenceBinding::into_js_value),
        );
    }

    fn append_recovery_bindings(&self, values: &mut Vec<JsValue>, legacy_cutoff: &str) {
        values.extend(
            self.recovery_bindings(legacy_cutoff)
                .into_iter()
                .map(ClaimFenceBinding::into_js_value),
        );
    }
}

fn prepare_active_claimed_command_transition(
    db: &D1Database,
    sql: &str,
    mut values: Vec<JsValue>,
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
) -> ApiResult<worker::D1PreparedStatement> {
    let base_bind_count = values.len();
    CommandClaimFence::new(row, user_id, command).append_active_bindings(&mut values);
    debug_assert_eq!(
        values.len(),
        base_bind_count + ACTIVE_COMMAND_CLAIM_FENCE_BIND_COUNT
    );
    debug_assert_eq!(sql_parameter_count(sql), values.len());
    db::prepare(db, sql, values)
}

fn prepare_recovery_command_transition(
    db: &D1Database,
    sql: &str,
    mut values: Vec<JsValue>,
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
    legacy_cutoff: &str,
) -> ApiResult<worker::D1PreparedStatement> {
    let base_bind_count = values.len();
    CommandClaimFence::new(row, user_id, command)
        .append_recovery_bindings(&mut values, legacy_cutoff);
    debug_assert_eq!(
        values.len(),
        base_bind_count + RECOVERY_COMMAND_CLAIM_FENCE_BIND_COUNT
    );
    debug_assert_eq!(sql_parameter_count(sql), values.len());
    db::prepare(db, sql, values)
}

fn outbox_identity_authorizes_execution(
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
) -> bool {
    CommandClaimFence::new(row, user_id, command).authorizes_identity(row)
}

fn outbox_authorizes_active_execution(
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
    transition_now: &str,
) -> bool {
    CommandClaimFence::new(row, user_id, command).authorizes_active(row, transition_now)
}

fn outbox_authorizes_recovery(
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
    recovery_now: &str,
    legacy_cutoff: &str,
) -> bool {
    CommandClaimFence::new(row, user_id, command).authorizes_recovery(
        row,
        recovery_now,
        legacy_cutoff,
    )
}

#[derive(Debug, Deserialize)]
struct CommandPayload {
    command_id: String,
}

#[derive(Debug, Deserialize)]
struct CommandWakeTokenRow {
    push_token: Option<String>,
}

#[derive(Debug)]
enum ExecutionFailure {
    Permanent(ApiError),
    Retryable(ApiError),
}

impl ExecutionFailure {
    fn error(&self) -> &ApiError {
        match self {
            Self::Permanent(error) | Self::Retryable(error) => error,
        }
    }

    fn retryable(&self) -> bool {
        matches!(self, Self::Retryable(_))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpiredClaimRecovery {
    Resume { outbox_state: &'static str },
    ReconcileRunning,
    SettleOrphan,
}

fn expired_claim_recovery(command_state: &str) -> ExpiredClaimRecovery {
    match command_state {
        "queued" => ExpiredClaimRecovery::Resume {
            outbox_state: "queued",
        },
        "retryable" => ExpiredClaimRecovery::Resume {
            outbox_state: "retrying",
        },
        "unknown" => ExpiredClaimRecovery::Resume {
            outbox_state: "unknown",
        },
        "running" => ExpiredClaimRecovery::ReconcileRunning,
        _ => ExpiredClaimRecovery::SettleOrphan,
    }
}

fn resume_expired_claim_sql() -> String {
    format!(
        "UPDATE outbox_events SET state = ?, next_attempt_at = ?, last_error = 'worker_lease_expired_before_start', lease_token = NULL, lease_expires_at = NULL, updated_at = ? WHERE id = ? AND user_id = ? AND topic = ? AND aggregate_id = ? AND idempotency_key = ? AND state = 'running'{} AND ((lease_expires_at IS NOT NULL AND lease_expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now')) OR (lease_expires_at IS NULL AND updated_at <= ?)) AND EXISTS (SELECT 1 FROM commands WHERE commands.id = ? AND commands.user_id = ? AND commands.state = ? AND commands.version = ? AND commands.id = outbox_events.aggregate_id AND commands.user_id = outbox_events.user_id)",
        lease_fence_sql()
    )
}

fn recover_stale_claims_sql() -> String {
    format!(
        "{} WHERE state = 'running' AND ((lease_expires_at IS NOT NULL AND lease_expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now')) OR (lease_expires_at IS NULL AND updated_at <= ?)) ORDER BY updated_at ASC LIMIT ?",
        outbox_select()
    )
}

fn recover_running_command_sql() -> String {
    format!(
        "UPDATE commands SET state = 'retryable', error_code = 'worker_lease_expired', version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'running' AND version = ? AND updated_at <= ?{}",
        recovery_command_claim_fence_sql()
    )
}

fn recover_failure_command_sql() -> String {
    format!(
        "UPDATE commands SET state = ?, result_json = NULL, error_code = ?, version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = ? AND version = ?{}",
        recovery_command_claim_fence_sql()
    )
}

async fn resume_expired_claim(
    db: &D1Database,
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
    outbox_state: &str,
    now: &str,
    cutoff: &str,
) -> ApiResult<()> {
    db::run(
        db,
        &resume_expired_claim_sql(),
        vec![
            db::text(outbox_state),
            db::text(now),
            db::text(now),
            db::text(&row.id),
            db::text(user_id),
            db::text(&row.topic),
            db::text(&row.aggregate_id),
            db::text(&row.idempotency_key),
            db::optional_text(row.lease_token.as_deref()),
            db::optional_text(row.lease_token.as_deref()),
            db::text(cutoff),
            db::text(&command.id),
            db::text(user_id),
            db::text(&command.state),
            db::number(command.version),
        ],
    )
    .await?;
    Ok(())
}

pub async fn drain(db: &D1Database, env: &worker::Env) -> ApiResult<usize> {
    let provider_config = providers::load(env)?;
    commands::expire_due(db).await?;
    recover_stale_claims(db, env).await?;
    let now = db::now_iso();
    let rows: Vec<OutboxEventRow> = db::all(
        db,
        &format!(
            "{} WHERE state IN ('queued', 'retrying', 'unknown') AND lease_token IS NULL AND (next_attempt_at IS NULL OR next_attempt_at <= ?) ORDER BY created_at ASC LIMIT ?",
            outbox_select()
        ),
        vec![db::text(&now), db::number(BATCH_SIZE)],
    )
    .await?;

    let mut processed = 0;
    for row in rows {
        if let Some(claimed) = claim(db, &row).await? {
            processed += 1;
            if let Err(error) = process_claimed(db, env, &claimed, provider_config.clone()).await {
                settle_processing_error(db, env, &claimed, &error).await?;
            }
        }
    }
    Ok(processed)
}

/// A Worker can terminate after claiming an outbox row but before it settles
/// the command. If command execution never started, release the same outbox
/// row so its owner-scoped, idempotent work can resume. If the command is
/// already running, move it to the explicit unknown/retryable path and bump
/// its version so the expired invocation cannot report a late success.
async fn recover_stale_claims(db: &D1Database, env: &worker::Env) -> ApiResult<()> {
    let cutoff = db::add_seconds_iso(-LEASE_SECONDS);
    let now = db::now_iso();
    let rows: Vec<OutboxEventRow> = db::all(
        db,
        &recover_stale_claims_sql(),
        vec![db::text(&cutoff), db::number(BATCH_SIZE)],
    )
    .await?;

    for row in rows {
        let Some(user_id) = row.user_id.as_deref() else {
            settle_orphan(db, &row, "missing_user_scope").await?;
            continue;
        };
        let Some(command) = commands::get_for_user(db, user_id, &row.aggregate_id).await? else {
            settle_orphan(db, &row, "command_not_found").await?;
            continue;
        };
        if !outbox_identity_authorizes_execution(&row, user_id, &command) {
            settle_orphan(db, &row, "outbox_execution_not_authorized").await?;
            continue;
        }
        if !outbox_authorizes_recovery(&row, user_id, &command, &now, &cutoff) {
            continue;
        }
        match expired_claim_recovery(&command.state) {
            ExpiredClaimRecovery::Resume { outbox_state } => {
                resume_expired_claim(db, &row, user_id, &command, outbox_state, &now, &cutoff)
                    .await?;
                continue;
            }
            ExpiredClaimRecovery::ReconcileRunning => {}
            ExpiredClaimRecovery::SettleOrphan => {
                settle_orphan(db, &row, "command_claim_not_running").await?;
                continue;
            }
        }

        if row.attempts >= MAX_ATTEMPTS {
            finish_recovery_failure(
                db,
                env,
                &row,
                user_id,
                &command,
                ExecutionFailure::Retryable(ApiError::new(
                    503,
                    "worker_lease_exhausted",
                    "The outbox worker lease expired too many times",
                )),
                ExpiredLeaseWindow {
                    recovery_now: &now,
                    legacy_cutoff: &cutoff,
                },
            )
            .await?;
            continue;
        }

        let now = db::now_iso();
        let next_version = command.version + 1;
        let statements = vec![
            prepare_recovery_command_transition(
                db,
                &recover_running_command_sql(),
                vec![
                    db::number(next_version),
                    db::text(&now),
                    db::text(&command.id),
                    db::text(user_id),
                    db::number(command.version),
                    db::text(&cutoff),
                ],
                &row,
                user_id,
                &command,
                &cutoff,
            )?,
            db::prepare(
                db,
                &format!(
                    "UPDATE outbox_events SET state = 'retrying', next_attempt_at = ?, last_error = 'worker_lease_expired', lease_token = NULL, lease_expires_at = NULL, updated_at = ? WHERE id = ? AND state = 'running' AND changes() = 1{} AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'retryable' AND version = ?)",
                    lease_fence_sql()
                ),
                vec![
                    db::text(&now),
                    db::text(&now),
                    db::text(&row.id),
                    db::optional_text(row.lease_token.as_deref()),
                    db::optional_text(row.lease_token.as_deref()),
                    db::text(&command.id),
                    db::text(user_id),
                    db::number(next_version),
                ],
            )?,
            db::prepare(
                db,
                    "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.retryable', ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'retryable' AND version = ?)",
                    vec![
                        db::text(&new_id("aud")?),
                        db::text(user_id),
                        db::optional_text(command.session_id.as_deref()),
                        db::text(&json!({"command_id": command.id, "reason": "worker_lease_expired", "retryable": true}).to_string()),
                    db::text(&now),
                    db::text(&command.id),
                    db::text(user_id),
                    db::number(next_version),
                ],
            )?,
            db::prepare(
                db,
                    "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'retryable' AND version = ?)",
                vec![
                    db::text(user_id),
                    db::text(&command.id),
                    db::optional_text(command.session_id.as_deref()),
                    db::number(next_version),
                    db::text(&now),
                    db::text(&command.id),
                    db::text(user_id),
                    db::number(next_version),
                ],
            )?,
        ];
        db.batch(statements).await?;
    }
    Ok(())
}

async fn claim(db: &D1Database, row: &OutboxEventRow) -> ApiResult<Option<OutboxEventRow>> {
    let now = db::now_iso();
    let lease_token = new_id("lease")?;
    let lease_expires_at = db::add_seconds_iso(LEASE_SECONDS);
    let result = db::run(
        db,
        "UPDATE outbox_events SET state = 'running', attempts = attempts + 1, lease_token = ?, lease_expires_at = ?, updated_at = ? WHERE id = ? AND state IN ('queued', 'retrying', 'unknown') AND lease_token IS NULL AND (next_attempt_at IS NULL OR next_attempt_at <= ?)",
        vec![
            db::text(&lease_token),
            db::text(&lease_expires_at),
            db::text(&now),
            db::text(&row.id),
            db::text(&now),
        ],
    )
    .await?;
    if db::changes(&result) != 1 {
        return Ok(None);
    }
    let mut claimed = row.clone();
    claimed.state = "running".to_string();
    claimed.attempts += 1;
    claimed.updated_at = now;
    claimed.lease_token = Some(lease_token);
    claimed.lease_expires_at = Some(lease_expires_at);
    Ok(Some(claimed))
}

async fn process_claimed(
    db: &D1Database,
    env: &worker::Env,
    row: &OutboxEventRow,
    provider_config: ActionProviderConfig,
) -> ApiResult<()> {
    let Some(user_id) = row.user_id.as_deref() else {
        return settle_orphan(db, row, "missing_user_scope").await;
    };
    let payload: CommandPayload = serde_json::from_str(&row.payload_json)
        .map_err(|_| ApiError::validation("Outbox payload is invalid"))?;
    if payload.command_id.trim().is_empty() {
        return settle_orphan(db, row, "missing_command_id").await;
    }
    if payload.command_id != row.aggregate_id {
        return settle_orphan(db, row, "outbox_command_mismatch").await;
    }

    let command = commands::get_for_user(db, user_id, &payload.command_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Command for outbox event was not found"))?;

    // A protected command receives its outbox row only from the atomic
    // confirmation transaction, whose user-scoped key uses the command ID.
    // Exact replay merely rotates a token and cannot manufacture this key.
    if !outbox_identity_authorizes_execution(row, user_id, &command) {
        return settle_orphan(db, row, "outbox_execution_not_authorized").await;
    }
    let authorization_now = db::now_iso();
    if !outbox_authorizes_active_execution(row, user_id, &command, &authorization_now) {
        return Ok(());
    }

    if let Some(session_id) = command.session_id.as_deref() {
        match commands::ensure_session_live(db, user_id, session_id).await {
            Ok(()) => {}
            Err(error) if error.status == 404 => {
                if settle_deleted_command(db, env, row, user_id, &command).await? {
                    return Ok(());
                }
                // The settlement CAS loses when the durable attempt crosses
                // attempts>=1 concurrently. At that point an effect may exist,
                // so this exact claim must continue reconciliation. start_command
                // rechecks both that predicate and this claim atomically.
            }
            Err(error) => return Err(error),
        }
    }

    if matches!(
        command.state.as_str(),
        "succeeded" | "failed" | "expired" | "cancelled"
    ) {
        return settle_orphan(
            db,
            row,
            if command.state == "succeeded" {
                "command_already_succeeded"
            } else {
                "command_already_terminal"
            },
        )
        .await;
    }

    if !commands::execution_policy_matches(
        &command.intent,
        &command.risk_level,
        command.needs_confirmation != 0,
    ) {
        return Err(ApiError::new(
            422,
            "command_policy_mismatch",
            "Command policy does not match the registered action",
        ));
    }

    let args = validated_command_args(&command.intent, &command.args_json)?;

    let Some(expected_running_version) = start_command(db, user_id, &command, row).await? else {
        // D1 execution time is authoritative. If the TTL crossed after this
        // Worker loaded the row, expire only never-started work. A recoverable
        // attempt blocks expiration only after attempts>=1 proves the effect
        // may have started. Another worker/generation can then reconcile it
        // without this stale invocation being classified as provider failure.
        let _ = expire_claimed_command(db, row, user_id, &command).await?;
        return Ok(());
    };
    let current = commands::get_for_user(db, user_id, &command.id)
        .await?
        .ok_or_else(|| ApiError::new(500, "command_error", "Command disappeared"))?;
    if !command_matches_expected_running_generation(&current, expected_running_version) {
        return Ok(());
    }
    if !acquire_command_execution_permit(db, row, user_id, &current, expected_running_version)
        .await?
    {
        return Ok(());
    }

    match execute_command(env, db, user_id, &current, row, &args, provider_config).await {
        Ok(result) => finish_success(db, env, row, user_id, &current, result).await,
        Err(failure) => finish_failure(db, env, row, user_id, &current, failure, "running").await,
    }
}

fn validated_command_args(intent: &str, args_json: &str) -> ApiResult<Map<String, Value>> {
    let args = serde_json::from_str::<Value>(args_json)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or_else(|| ApiError::validation("Persisted command arguments must be an object"))?;
    // Persisted commands must remain structurally valid, but a time-relative
    // reminder check belongs at the effect boundary. Running it here would
    // prevent crash recovery from settling a provider effect that already
    // succeeded before its due_at elapsed.
    commands::validate_action_args_shape(intent, &args)
        .map_err(|error| ApiError::validation(error.to_string()))?;
    Ok(args)
}

fn expire_claimed_command_sql() -> String {
    format!(
        "UPDATE commands SET state = 'expired', error_code = 'command_expired', result_json = NULL, version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state IN ('pending', 'validated', 'queued', 'retryable', 'unknown') AND version = ? AND expires_at IS NOT NULL AND expires_at <= strftime('%Y-%m-%dT%H:%M:%fZ','now') AND NOT {}{}",
        commands::ACTION_EFFECT_MAY_HAVE_STARTED_SQL,
        active_command_claim_fence_sql()
    )
}

fn expire_claimed_outbox_sql() -> String {
    format!(
        "UPDATE outbox_events SET state = 'failed', next_attempt_at = NULL, last_error = 'command_expired', lease_token = NULL, lease_expires_at = NULL, updated_at = ? WHERE id = ? AND user_id = ? AND topic = ? AND aggregate_id = ? AND idempotency_key = ? AND state = 'running' AND changes() = 1{} AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'expired' AND version = ?)",
        lease_fence_sql()
    )
}

async fn expire_claimed_command(
    db: &D1Database,
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
) -> ApiResult<bool> {
    let now = db::now_iso();
    let next_version = command.version + 1;
    let statements = vec![
        prepare_active_claimed_command_transition(
            db,
            &expire_claimed_command_sql(),
            vec![
                db::number(next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(command.version),
            ],
            row,
            user_id,
            command,
        )?,
        db::prepare(
            db,
            &expire_claimed_outbox_sql(),
            vec![
                db::text(&now),
                db::text(&row.id),
                db::text(user_id),
                db::text(&row.topic),
                db::text(&row.aggregate_id),
                db::text(&row.idempotency_key),
                db::optional_text(row.lease_token.as_deref()),
                db::optional_text(row.lease_token.as_deref()),
                db::text(&command.id),
                db::text(user_id),
                db::number(next_version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.expired', ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'expired' AND version = ?)",
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(
                    &json!({"command_id": command.id, "reason": "command_expired", "version": next_version})
                        .to_string(),
                ),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(next_version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'expired' AND version = ?)",
            vec![
                db::text(user_id),
                db::text(&command.id),
                db::optional_text(command.session_id.as_deref()),
                db::number(next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(next_version),
            ],
        )?,
    ];
    let results = db.batch(statements).await?;
    Ok(results.first().map(db::changes).unwrap_or(0) == 1)
}

fn start_command_sql() -> String {
    format!(
        "UPDATE commands SET state = 'running', version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state IN ('queued', 'retryable', 'unknown') AND version = ? AND (expires_at IS NULL OR expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now') OR {}) AND (session_id IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE id = commands.session_id AND user_id = ? AND deleted_at IS NULL) OR {}){}",
        commands::ACTION_EFFECT_MAY_HAVE_STARTED_SQL,
        commands::ACTION_EFFECT_MAY_HAVE_STARTED_SQL,
        active_command_claim_fence_sql()
    )
}

async fn start_command(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    row: &OutboxEventRow,
) -> ApiResult<Option<i64>> {
    let now = db::now_iso();
    let next_version = command.version + 1;
    let statements = vec![
        prepare_active_claimed_command_transition(
            db,
            &start_command_sql(),
            vec![
                db::number(next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(command.version),
                db::text(user_id),
            ],
            row,
            user_id,
            command,
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.running', ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'running' AND version = ?)",
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(&json!({"command_id": command.id, "version": next_version}).to_string()),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(next_version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'running' AND version = ?)",
            vec![
                db::text(user_id),
                db::text(&command.id),
                db::optional_text(command.session_id.as_deref()),
                db::number(next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(next_version),
            ],
        )?,
    ];
    let result = db.batch(statements).await?;
    if result.first().map(db::changes).unwrap_or(0) == 0 {
        return Ok(None);
    }
    Ok(Some(next_version))
}

fn settle_deleted_command_sql() -> String {
    format!(
        "UPDATE commands SET state = 'cancelled', error_code = 'session_deleted', version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state IN ('queued', 'retryable', 'unknown') AND version = ? AND NOT {}{}",
        commands::ACTION_EFFECT_MAY_HAVE_STARTED_SQL,
        active_command_claim_fence_sql()
    )
}

async fn settle_deleted_command(
    db: &D1Database,
    env: &worker::Env,
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
) -> ApiResult<bool> {
    let now = db::now_iso();
    let next_version = command.version + 1;
    let statements = vec![
        prepare_active_claimed_command_transition(
            db,
            &settle_deleted_command_sql(),
            vec![
                db::number(next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(command.version),
            ],
            row,
            user_id,
            command,
        )?,
        db::prepare(
            db,
            &format!(
                "UPDATE outbox_events SET state = 'failed', next_attempt_at = NULL, last_error = 'session_deleted', lease_token = NULL, lease_expires_at = NULL, updated_at = ? WHERE id = ? AND state = 'running' AND changes() = 1{}",
                lease_fence_sql()
            ),
            vec![
                db::text(&now),
                db::text(&row.id),
                db::optional_text(row.lease_token.as_deref()),
                db::optional_text(row.lease_token.as_deref()),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.cancelled', ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'cancelled' AND version = ?)",
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(&json!({"command_id": command.id, "reason": "session_deleted"}).to_string()),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(next_version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'cancelled' AND version = ?)",
            vec![
                db::text(user_id),
                db::text(&command.id),
                db::optional_text(command.session_id.as_deref()),
                db::number(next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(next_version),
            ],
        )?,
    ];
    let results = db.batch(statements).await?;
    notify_terminal_transition(
        db,
        env,
        user_id,
        "cancelled",
        results.first().map(db::changes).unwrap_or(0),
    )
    .await;
    Ok(results.first().map(db::changes).unwrap_or(0) == 1)
}

fn command_matches_expected_running_generation(
    command: &CommandRow,
    expected_running_version: i64,
) -> bool {
    command.state == "running" && command.version == expected_running_version
}

fn command_execution_permit_sql() -> String {
    format!(
        "UPDATE commands SET version = version WHERE id = ? AND user_id = ? AND state = 'running' AND version = ? AND (session_id IS NULL OR EXISTS (SELECT 1 FROM sessions AS permit_session WHERE permit_session.id = commands.session_id AND permit_session.user_id = commands.user_id AND permit_session.deleted_at IS NULL) OR {}){}",
        commands::ACTION_EFFECT_MAY_HAVE_STARTED_SQL,
        active_command_claim_fence_sql()
    )
}

fn command_execution_attempt_permit_sql() -> &'static str {
    "INSERT INTO action_attempts (id, user_id, command_id, action_id, provider, provider_idempotency_key, state, request_hash, response_json, attempts, next_attempt_at, last_error, created_at, updated_at) SELECT ?, ?, ?, NULL, ?, ?, 'running', ?, NULL, 0, NULL, 'execution_permit', ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'running' AND version = ?) ON CONFLICT(provider, provider_idempotency_key) DO NOTHING"
}

async fn acquire_command_execution_permit(
    db: &D1Database,
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
    expected_running_version: i64,
) -> ApiResult<bool> {
    if !command_matches_expected_running_generation(command, expected_running_version) {
        return Ok(false);
    }

    let statement = prepare_active_claimed_command_transition(
        db,
        &command_execution_permit_sql(),
        vec![
            db::text(&command.id),
            db::text(user_id),
            db::number(expected_running_version),
        ],
        row,
        user_id,
        command,
    )?;
    let mut statements = vec![statement];
    if let Some(provider) = providers::action_attempt_provider(&command.intent) {
        let now = db::now_iso();
        let provider_idempotency_key = providers::scoped_action_idempotency_key(
            user_id,
            &command.intent,
            &command.idempotency_key,
        );
        statements.push(db::prepare(
            db,
            command_execution_attempt_permit_sql(),
            vec![
                db::text(&new_id("attempt")?),
                db::text(user_id),
                db::text(&command.id),
                db::text(provider),
                db::text(&provider_idempotency_key),
                db::text(&command.command_hash),
                db::text(&now),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(expected_running_version),
            ],
        )?);
    }
    let results = db.batch(statements).await?;
    Ok(results.first().map(db::changes).unwrap_or(0) == 1)
}

async fn execute_command(
    env: &worker::Env,
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    claim: &OutboxEventRow,
    args: &Map<String, Value>,
    provider_config: ActionProviderConfig,
) -> Result<Value, ExecutionFailure> {
    match command.intent.as_str() {
        "search_history" => {
            let query = string_arg(args, &["q", "query", "text"]).ok_or_else(|| {
                ExecutionFailure::Permanent(ApiError::validation(
                    "search_history requires args.q or args.query",
                ))
            })?;
            history::search(db, user_id, query, 50)
                .await
                .map(|result| json!({"kind": "history_search", "data": result}))
                .map_err(classify_error)
        }
        "create_draft" | "create_reminder" | "send_message" => {
            action_effects::execute(env, db, user_id, command, claim, args, provider_config)
                .await
                .map_err(classify_error)
        }
        intent => Err(ExecutionFailure::Permanent(ApiError::new(
            422,
            "unsupported_intent",
            format!("No executor is registered for intent: {intent}"),
        ))),
    }
}

fn string_arg<'a>(args: &'a Map<String, Value>, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| args.get(*name).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn classify_error(error: ApiError) -> ExecutionFailure {
    if matches!(error.status, 408 | 425 | 429 | 500..=599) {
        ExecutionFailure::Retryable(error)
    } else {
        ExecutionFailure::Permanent(error)
    }
}

fn command_failure_state(retryable: bool, automatic_retry: bool) -> &'static str {
    if automatic_retry {
        "retryable"
    } else if retryable {
        "unknown"
    } else {
        "failed"
    }
}

fn finish_success_command_sql() -> String {
    format!(
        "UPDATE commands SET state = 'succeeded', result_json = ?, error_code = NULL, version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'running' AND version = ?{}",
        active_command_claim_fence_sql()
    )
}

async fn finish_success(
    db: &D1Database,
    env: &worker::Env,
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
    result: Value,
) -> ApiResult<()> {
    let now = db::now_iso();
    let version = command.version + 1;
    let statements = vec![
        prepare_active_claimed_command_transition(
            db,
            &finish_success_command_sql(),
            vec![
                db::text(&result.to_string()),
                db::number(version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(command.version),
            ],
            row,
            user_id,
            command,
        )?,
        db::prepare(
            db,
            &format!(
                "UPDATE outbox_events SET state = 'succeeded', next_attempt_at = NULL, last_error = NULL, lease_token = NULL, lease_expires_at = NULL, updated_at = ? WHERE id = ? AND state = 'running' AND changes() = 1{} AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'succeeded' AND version = ?)",
                lease_fence_sql()
            ),
            vec![
                db::text(&now),
                db::text(&row.id),
                db::optional_text(row.lease_token.as_deref()),
                db::optional_text(row.lease_token.as_deref()),
                db::text(&command.id),
                db::text(user_id),
                db::number(version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.succeeded', ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'succeeded' AND version = ?)",
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(&json!({"command_id": command.id, "version": version}).to_string()),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'succeeded' AND version = ?)",
            vec![
                db::text(user_id),
                db::text(&command.id),
                db::optional_text(command.session_id.as_deref()),
                db::number(version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(version),
            ],
        )?,
    ];
    let results = db.batch(statements).await?;
    notify_terminal_transition(
        db,
        env,
        user_id,
        "succeeded",
        results.first().map(db::changes).unwrap_or(0),
    )
    .await;
    Ok(())
}

fn finish_failure_command_sql() -> String {
    format!(
        "UPDATE commands SET state = ?, result_json = NULL, error_code = ?, version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = ? AND version = ?{}",
        active_command_claim_fence_sql()
    )
}

#[derive(Clone, Copy)]
enum FailureTransitionFence<'a> {
    Active,
    ExpiredRecovery { legacy_cutoff: &'a str },
}

async fn finish_failure(
    db: &D1Database,
    env: &worker::Env,
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
    failure: ExecutionFailure,
    expected_state: &str,
) -> ApiResult<()> {
    let transition_now = db::now_iso();
    finish_failure_with_fence(
        db,
        env,
        row,
        user_id,
        command,
        failure,
        expected_state,
        &transition_now,
        FailureTransitionFence::Active,
    )
    .await
}

async fn finish_recovery_failure(
    db: &D1Database,
    env: &worker::Env,
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
    failure: ExecutionFailure,
    window: ExpiredLeaseWindow<'_>,
) -> ApiResult<()> {
    finish_failure_with_fence(
        db,
        env,
        row,
        user_id,
        command,
        failure,
        "running",
        window.recovery_now,
        FailureTransitionFence::ExpiredRecovery {
            legacy_cutoff: window.legacy_cutoff,
        },
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn finish_failure_with_fence(
    db: &D1Database,
    env: &worker::Env,
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
    failure: ExecutionFailure,
    expected_state: &str,
    transition_now: &str,
    fence: FailureTransitionFence<'_>,
) -> ApiResult<()> {
    let attempt = row.attempts.max(1);
    let retryable = failure.retryable();
    let retry = retryable && attempt < MAX_ATTEMPTS;
    // Automatic transient failures remain explicitly `retryable` while the
    // outbox has another scheduled attempt. Once the retry budget is spent,
    // the outcome becomes `unknown` and must be reconciled before success is
    // ever reported. Only validation/business failures become `failed`.
    let command_state = command_failure_state(retryable, retry);
    let outbox_state = if retry {
        "retrying"
    } else if retryable {
        "unknown"
    } else {
        "failed"
    };
    let retry_at = if retry {
        Some(db::add_seconds_iso(backoff_seconds(row.attempts)))
    } else if retryable {
        Some(db::add_seconds_iso(UNKNOWN_RECONCILE_SECONDS))
    } else {
        None
    };
    let version = command.version + 1;
    let error = failure.error();
    let command_transition = match fence {
        FailureTransitionFence::Active => prepare_active_claimed_command_transition(
            db,
            &finish_failure_command_sql(),
            vec![
                db::text(command_state),
                db::text(&error.code),
                db::number(version),
                db::text(transition_now),
                db::text(&command.id),
                db::text(user_id),
                db::text(expected_state),
                db::number(command.version),
            ],
            row,
            user_id,
            command,
        )?,
        FailureTransitionFence::ExpiredRecovery { legacy_cutoff } => {
            prepare_recovery_command_transition(
                db,
                &recover_failure_command_sql(),
                vec![
                    db::text(command_state),
                    db::text(&error.code),
                    db::number(version),
                    db::text(transition_now),
                    db::text(&command.id),
                    db::text(user_id),
                    db::text(expected_state),
                    db::number(command.version),
                ],
                row,
                user_id,
                command,
                legacy_cutoff,
            )?
        }
    };
    let mut statements = vec![
        command_transition,
        db::prepare(
            db,
            &format!(
                "UPDATE outbox_events SET state = ?, next_attempt_at = ?, last_error = ?, lease_token = NULL, lease_expires_at = NULL, updated_at = ? WHERE id = ? AND state = 'running' AND changes() = 1{} AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = ? AND version = ?)",
                lease_fence_sql()
            ),
            vec![
                db::text(outbox_state),
                db::optional_text(retry_at.as_deref()),
                db::text(&error.code),
                db::text(transition_now),
                db::text(&row.id),
                db::optional_text(row.lease_token.as_deref()),
                db::optional_text(row.lease_token.as_deref()),
                db::text(&command.id),
                db::text(user_id),
                db::text(command_state),
                db::number(version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, ?, ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = ? AND version = ?)",
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(if retry {
                    "command.retrying"
                } else if retryable {
                    "command.unknown"
                } else {
                    "command.failed"
                }),
                db::text(&json!({"command_id": command.id, "error_code": error.code, "retryable": retryable, "auto_retry": retry, "version": version}).to_string()),
                db::text(transition_now),
                db::text(&command.id),
                db::text(user_id),
                db::text(command_state),
                db::number(version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE changes() = 1 AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = ? AND version = ?)",
            vec![
                db::text(user_id),
                db::text(&command.id),
                db::optional_text(command.session_id.as_deref()),
                db::number(version),
                db::text(transition_now),
                db::text(&command.id),
                db::text(user_id),
                db::text(command_state),
                db::number(version),
            ],
        )?,
    ];
    if let Some(provider) = providers::action_attempt_provider(&command.intent) {
        let provider_idempotency_key = providers::scoped_action_idempotency_key(
            user_id,
            &command.intent,
            &command.idempotency_key,
        );
        statements.push(db::prepare(
            db,
            "INSERT INTO action_attempts (id, user_id, command_id, action_id, provider, provider_idempotency_key, state, request_hash, response_json, attempts, next_attempt_at, last_error, created_at, updated_at) SELECT ?, ?, ?, NULL, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ? WHERE changes() = 1 ON CONFLICT(provider, provider_idempotency_key) DO UPDATE SET state = excluded.state, attempts = excluded.attempts, next_attempt_at = excluded.next_attempt_at, last_error = excluded.last_error, updated_at = excluded.updated_at",
            vec![
                db::text(&new_id("attempt")?),
                db::text(user_id),
                db::text(&command.id),
                db::text(provider),
                db::text(&provider_idempotency_key),
                db::text(if retry {
                    "retrying"
                } else if retryable {
                    "unknown"
                } else {
                    "failed"
                }),
                db::text(&command.command_hash),
                db::number(attempt as i64),
                db::optional_text(retry_at.as_deref()),
                db::text(&error.code),
                db::text(transition_now),
                db::text(transition_now),
            ],
        )?);
    }
    let results = db.batch(statements).await?;
    notify_terminal_transition(
        db,
        env,
        user_id,
        command_state,
        results.first().map(db::changes).unwrap_or(0),
    )
    .await;
    Ok(())
}

fn should_attempt_command_wakeup(command_state: &str, transition_changes: usize) -> bool {
    transition_changes == 1 && matches!(command_state, "succeeded" | "failed" | "cancelled")
}

fn command_wakeup_attempt<T>(
    command_state: &str,
    transition_changes: usize,
    attempt: impl FnOnce() -> T,
) -> Option<T> {
    should_attempt_command_wakeup(command_state, transition_changes).then(attempt)
}

async fn notify_terminal_transition(
    db: &D1Database,
    env: &worker::Env,
    user_id: &str,
    command_state: &str,
    transition_changes: usize,
) {
    if let Some(attempt) = command_wakeup_attempt(command_state, transition_changes, || {
        attempt_command_wakeup(db, env, user_id)
    }) {
        attempt.await;
    }
}

fn command_wakeup_token_query() -> &'static str {
    "SELECT push_token FROM devices WHERE user_id = ? AND platform = 'ios' AND push_token IS NOT NULL AND push_token != ''"
}

async fn attempt_command_wakeup(db: &D1Database, env: &worker::Env, user_id: &str) {
    let push_mode = config_value(env, "PUSH_MODE", "dev");
    if !matches!(push_mode.as_str(), "apns" | "both") || !apns::is_ready(env) {
        return;
    }

    let rows: Vec<CommandWakeTokenRow> =
        match db::all(db, command_wakeup_token_query(), vec![db::text(user_id)]).await {
            Ok(rows) => rows,
            Err(_) => return,
        };
    let mut tokens = rows
        .into_iter()
        .filter_map(|row| row.push_token)
        .filter(|token| apns::looks_like_token(token))
        .collect::<Vec<_>>();
    tokens.sort();
    tokens.dedup();

    for token in tokens {
        let _ = apns::send_command_wakeup(env, &token).await;
    }
}

fn backoff_seconds(attempts: i32) -> i64 {
    match attempts {
        0 | 1 => 5,
        2 => 30,
        _ => 300,
    }
}

async fn release_runtime_error(
    db: &D1Database,
    row: &OutboxEventRow,
    error: &ApiError,
) -> ApiResult<()> {
    let attempt = row.attempts.max(1);
    let retryable = error.retryable;
    let retry = retryable && attempt < MAX_ATTEMPTS;
    let next_attempt_at = if retry {
        Some(db::add_seconds_iso(backoff_seconds(row.attempts)))
    } else if retryable {
        Some(db::add_seconds_iso(UNKNOWN_RECONCILE_SECONDS))
    } else {
        None
    };
    let now = db::now_iso();
    db::run(
        db,
        &format!(
            "UPDATE outbox_events SET state = ?, next_attempt_at = ?, last_error = ?, lease_token = NULL, lease_expires_at = NULL, updated_at = ? WHERE id = ? AND state = 'running'{}",
            lease_fence_sql()
        ),
        vec![
            db::text(if retry {
                "retrying"
            } else if retryable {
                "unknown"
            } else {
                "failed"
            }),
            db::optional_text(next_attempt_at.as_deref()),
            db::text(&error.code),
            db::text(&now),
            db::text(&row.id),
            db::optional_text(row.lease_token.as_deref()),
            db::optional_text(row.lease_token.as_deref()),
        ],
    )
    .await?;
    Ok(())
}

/// Reconcile errors that happen before `process_claimed` reaches the normal
/// command executor. Without this fence a queued command could stay queued
/// forever while its outbox row is repeatedly retried or eventually failed.
async fn settle_processing_error(
    db: &D1Database,
    env: &worker::Env,
    row: &OutboxEventRow,
    error: &ApiError,
) -> ApiResult<()> {
    let Some(user_id) = row.user_id.as_deref() else {
        return release_runtime_error(db, row, error).await;
    };
    let Some(command) = commands::get_for_user(db, user_id, &row.aggregate_id).await? else {
        return release_runtime_error(db, row, error).await;
    };
    if matches!(
        command.state.as_str(),
        "pending" | "validated" | "queued" | "retryable" | "unknown" | "running"
    ) {
        return finish_failure(
            db,
            env,
            row,
            user_id,
            &command,
            classify_error(error.clone()),
            &command.state,
        )
        .await;
    }
    release_runtime_error(db, row, error).await
}

async fn settle_orphan(db: &D1Database, row: &OutboxEventRow, reason: &str) -> ApiResult<()> {
    db::run(
        db,
        &format!(
            "UPDATE outbox_events SET state = 'failed', next_attempt_at = NULL, last_error = ?, lease_token = NULL, lease_expires_at = NULL, updated_at = ? WHERE id = ? AND state = 'running'{}",
            lease_fence_sql()
        ),
        vec![
            db::text(reason),
            db::text(&db::now_iso()),
            db::text(&row.id),
            db::optional_text(row.lease_token.as_deref()),
            db::optional_text(row.lease_token.as_deref()),
        ],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn test_command(needs_confirmation: bool) -> CommandRow {
        CommandRow {
            id: "cmd_test".to_string(),
            user_id: "usr_test".to_string(),
            device_id: None,
            session_id: Some("ses_test".to_string()),
            schema_version: 1,
            intent: "send_message".to_string(),
            args_json: json!({"recipient": "Ada", "body": "Hello"}).to_string(),
            risk_level: if needs_confirmation { "high" } else { "low" }.to_string(),
            needs_confirmation: i32::from(needs_confirmation),
            idempotency_key: "command_idem".to_string(),
            confidence: Some(1.0),
            locale: "en".to_string(),
            timezone: "UTC".to_string(),
            state: "queued".to_string(),
            command_hash: "command_hash".to_string(),
            result_json: None,
            error_code: None,
            expires_at: None,
            model_version: None,
            version: 7,
            created_at: "2026-08-12T00:00:00.000Z".to_string(),
            updated_at: "2026-08-12T00:00:00.000Z".to_string(),
        }
    }

    fn test_claim(command: &CommandRow, lease_token: &str) -> OutboxEventRow {
        OutboxEventRow {
            id: "out_test".to_string(),
            user_id: Some(command.user_id.clone()),
            topic: COMMAND_EXECUTE_TOPIC.to_string(),
            aggregate_id: command.id.clone(),
            payload_json: json!({"command_id": command.id}).to_string(),
            idempotency_key: expected_execution_outbox_key(
                &command.user_id,
                &command.id,
                &command.idempotency_key,
                command.needs_confirmation != 0,
            ),
            state: "running".to_string(),
            attempts: 1,
            next_attempt_at: None,
            last_error: None,
            created_at: "2026-08-12T00:00:00.000Z".to_string(),
            updated_at: "2026-08-12T00:00:00.000Z".to_string(),
            lease_token: Some(lease_token.to_string()),
            lease_expires_at: Some("2026-08-12T00:05:00.000Z".to_string()),
        }
    }

    #[test]
    fn expired_running_outbox_resumes_queued_command() {
        assert_eq!(
            expired_claim_recovery("queued"),
            ExpiredClaimRecovery::Resume {
                outbox_state: "queued"
            }
        );
    }

    #[test]
    fn expired_running_outbox_resumes_retryable_command() {
        assert_eq!(
            expired_claim_recovery("retryable"),
            ExpiredClaimRecovery::Resume {
                outbox_state: "retrying"
            }
        );
    }

    #[test]
    fn expired_running_outbox_resumes_unknown_command() {
        assert_eq!(
            expired_claim_recovery("unknown"),
            ExpiredClaimRecovery::Resume {
                outbox_state: "unknown"
            }
        );
    }

    #[test]
    fn expired_running_outbox_reconciles_already_running_command() {
        assert_eq!(
            expired_claim_recovery("running"),
            ExpiredClaimRecovery::ReconcileRunning
        );
    }

    #[test]
    fn expired_claim_resume_query_keeps_all_recovery_fences() {
        let query = resume_expired_claim_sql();

        assert!(query.contains("id = ? AND user_id = ? AND topic = ?"));
        assert!(query.contains("aggregate_id = ? AND idempotency_key = ?"));
        assert!(query.contains("state = 'running'"));
        assert!(query.contains("lease_token = ?"));
        assert!(query.contains(&format!("lease_expires_at <= {}", db_now_sql())));
        assert!(!query.contains("lease_expires_at <= ?"));
        assert!(query.contains("commands.user_id = outbox_events.user_id"));
        assert!(query.contains("commands.state = ? AND commands.version = ?"));
        assert_eq!(sql_parameter_count(&query), 15);

        let selection = recover_stale_claims_sql();
        assert!(selection.contains(&format!("lease_expires_at <= {}", db_now_sql())));
        assert!(!selection.contains("lease_expires_at <= ?"));
        assert!(selection.contains("lease_expires_at IS NULL AND updated_at <= ?"));
        assert_eq!(sql_parameter_count(&selection), 2);
    }

    #[test]
    fn backoff_is_bounded_and_increases() {
        assert_eq!(backoff_seconds(1), 5);
        assert_eq!(backoff_seconds(2), 30);
        assert_eq!(backoff_seconds(3), 300);
    }

    #[test]
    fn string_arg_accepts_common_search_names() {
        let args =
            serde_json::from_value::<Map<String, Value>>(json!({"query": "  history  "})).unwrap();
        assert_eq!(string_arg(&args, &["q", "query"]), Some("history"));
    }

    #[test]
    fn only_transient_errors_are_retryable() {
        assert!(classify_error(ApiError::new(503, "timeout", "retry")).retryable());
        assert!(classify_error(ApiError::new(425, "stale", "retry")).retryable());
        assert!(!classify_error(ApiError::validation("bad input")).retryable());
    }

    #[test]
    fn command_failures_keep_retryable_distinct_from_unknown() {
        assert_eq!(command_failure_state(true, true), "retryable");
        assert_eq!(command_failure_state(true, false), "unknown");
        assert_eq!(command_failure_state(false, false), "failed");
    }

    #[test]
    fn terminal_transition_attempts_wakeup_exactly_once() {
        let attempts = Cell::new(0);
        for transition_changes in [1, 0] {
            let _ = command_wakeup_attempt("succeeded", transition_changes, || {
                attempts.set(attempts.get() + 1);
            });
        }

        assert_eq!(attempts.get(), 1);
        assert!(should_attempt_command_wakeup("failed", 1));
        assert!(should_attempt_command_wakeup("cancelled", 1));
        assert!(!should_attempt_command_wakeup("retryable", 1));
        assert!(!should_attempt_command_wakeup("unknown", 1));
    }

    #[test]
    fn command_wakeup_tokens_are_scoped_to_the_authenticated_owner() {
        let query = command_wakeup_token_query();

        assert!(query.contains("WHERE user_id = ?"));
        assert!(query.contains("platform = 'ios'"));
    }

    #[test]
    fn persisted_command_arguments_are_revalidated_before_execution() {
        let valid = json!({"q": "history"}).to_string();
        assert_eq!(
            validated_command_args("search_history", &valid).unwrap(),
            serde_json::from_value::<Map<String, Value>>(json!({"q": "history"})).unwrap()
        );

        let unexpected = json!({"q": "history", "unexpected": true}).to_string();
        let error = validated_command_args("search_history", &unexpected).unwrap_err();
        assert_eq!(error.status, 400);
        assert_eq!(error.code, "validation_error");

        let error = validated_command_args("search_history", "[]").unwrap_err();
        assert_eq!(error.status, 400);
        assert_eq!(error.code, "validation_error");
    }

    #[test]
    fn outbox_queries_include_explicit_lease_columns_and_fence() {
        assert!(outbox_select().contains("lease_token, lease_expires_at"));
        assert!(lease_fence_sql().contains("lease_token = ?"));
        assert!(lease_fence_sql().contains("lease_token IS NULL"));
    }

    #[test]
    fn active_and_recovery_fences_require_the_same_exact_claim_identity() {
        for query in [
            active_command_claim_fence_sql(),
            recovery_command_claim_fence_sql(),
        ] {
            assert!(query.contains("claim.id = ? AND claim.user_id = ?"));
            assert!(query.contains("claim.user_id = commands.user_id"));
            assert!(query.contains("claim.topic = ? AND claim.topic = 'command.execute'"));
            assert!(query.contains("claim.aggregate_id = ? AND claim.aggregate_id = commands.id"));
            assert_eq!(query.matches("claim.idempotency_key = ?").count(), 2);
            assert!(query.contains("claim.state = 'running'"));
            assert!(query.contains("claim.lease_token = ?"));
        }

        let active = active_command_claim_fence_sql();
        assert!(active.contains(&format!("claim.lease_expires_at > {}", db_now_sql())));
        assert!(!active.contains("claim.lease_expires_at > ?"));
        assert!(!active.contains("claim.lease_token IS NULL"));
        assert_eq!(
            sql_parameter_count(active),
            ACTIVE_COMMAND_CLAIM_FENCE_BIND_COUNT
        );

        let recovery = recovery_command_claim_fence_sql();
        assert!(recovery.contains("claim.lease_token IS NULL AND ? IS NULL"));
        assert!(recovery.contains(&format!("claim.lease_expires_at <= {}", db_now_sql())));
        assert!(!recovery.contains("claim.lease_expires_at <= ?"));
        assert!(recovery.contains("claim.updated_at <= ?"));
        assert_eq!(
            sql_parameter_count(recovery),
            RECOVERY_COMMAND_CLAIM_FENCE_BIND_COUNT
        );
    }

    #[test]
    fn active_and_recovery_bindings_follow_sql_placeholder_order() {
        let command = test_command(false);
        let mut claim = test_claim(&command, "lease_order");
        claim.idempotency_key = "snapshot_outbox_key".to_string();
        let expected_key = expected_execution_outbox_key(
            &command.user_id,
            &command.id,
            &command.idempotency_key,
            false,
        );
        let fence = CommandClaimFence::new(&claim, &command.user_id, &command);

        assert_eq!(
            fence.active_bindings(),
            [
                ClaimFenceBinding::Text("out_test"),
                ClaimFenceBinding::Text("usr_test"),
                ClaimFenceBinding::Text(COMMAND_EXECUTE_TOPIC),
                ClaimFenceBinding::Text("cmd_test"),
                ClaimFenceBinding::Text("snapshot_outbox_key"),
                ClaimFenceBinding::Text(&expected_key),
                ClaimFenceBinding::OptionalText(Some("lease_order")),
            ]
        );
        assert_eq!(
            fence.recovery_bindings("2026-08-12T00:01:00.000Z"),
            [
                ClaimFenceBinding::Text("out_test"),
                ClaimFenceBinding::Text("usr_test"),
                ClaimFenceBinding::Text(COMMAND_EXECUTE_TOPIC),
                ClaimFenceBinding::Text("cmd_test"),
                ClaimFenceBinding::Text("snapshot_outbox_key"),
                ClaimFenceBinding::Text(&expected_key),
                ClaimFenceBinding::OptionalText(Some("lease_order")),
                ClaimFenceBinding::OptionalText(Some("lease_order")),
                ClaimFenceBinding::Text("2026-08-12T00:01:00.000Z"),
            ]
        );
    }

    #[test]
    fn every_active_command_update_uses_unexpired_fence_and_matching_bind_count() {
        let transitions = [
            ("expire", expire_claimed_command_sql(), 5),
            ("start", start_command_sql(), 6),
            ("deleted", settle_deleted_command_sql(), 5),
            ("success", finish_success_command_sql(), 6),
            ("failure", finish_failure_command_sql(), 8),
            ("pre_effect_permit", command_execution_permit_sql(), 3),
        ];

        for (name, sql, base_bind_count) in transitions {
            assert!(
                sql.ends_with(active_command_claim_fence_sql()),
                "{name} SQL omitted the active exact-claim fence: {sql}"
            );
            assert_eq!(
                sql_parameter_count(&sql),
                base_bind_count + ACTIVE_COMMAND_CLAIM_FENCE_BIND_COUNT,
                "{name} SQL bind count drifted"
            );
            assert!(sql.contains("version = ?"), "{name} lost its version fence");
            assert!(sql.contains(&format!("claim.lease_expires_at > {}", db_now_sql())));
            assert!(!sql.contains("claim.lease_expires_at > ?"));
            assert!(!sql.contains("expires_at > ?"));
            assert!(!sql.contains("expires_at <= ?"));
            assert!(!sql.contains("confirmation_tokens"));
            assert!(!sql.contains("used_at"));
        }

        assert!(start_command_sql().contains(&format!("expires_at > {}", db_now_sql())));
        assert!(expire_claimed_command_sql().contains(&format!("expires_at <= {}", db_now_sql())));
        assert!(start_command_sql().contains(&format!(
            "OR {}",
            commands::ACTION_EFFECT_MAY_HAVE_STARTED_SQL
        )));
        assert!(expire_claimed_command_sql().contains(&format!(
            "AND NOT {}",
            commands::ACTION_EFFECT_MAY_HAVE_STARTED_SQL
        )));
        assert!(start_command_sql().contains(&format!(
            "OR {}",
            commands::ACTION_EFFECT_MAY_HAVE_STARTED_SQL
        )));
        assert!(settle_deleted_command_sql().contains(&format!(
            "AND NOT {}",
            commands::ACTION_EFFECT_MAY_HAVE_STARTED_SQL
        )));
        assert!(commands::ACTION_EFFECT_MAY_HAVE_STARTED_SQL
            .contains("started_attempt.state = 'succeeded'"));
        assert!(
            commands::ACTION_EFFECT_MAY_HAVE_STARTED_SQL.contains("started_attempt.attempts >= 1")
        );
        let permit = command_execution_permit_sql();
        assert!(permit.contains("SET version = version"));
        assert!(permit.contains("state = 'running' AND version = ?"));
        assert!(permit.contains("permit_session.id = commands.session_id"));
        assert!(permit.contains("permit_session.user_id = commands.user_id"));
        assert!(permit.contains("permit_session.deleted_at IS NULL"));
        assert!(permit.contains(&format!(
            "OR {}",
            commands::ACTION_EFFECT_MAY_HAVE_STARTED_SQL
        )));
        assert_eq!(sql_parameter_count(&expire_claimed_outbox_sql()), 11);
    }

    #[test]
    fn recovery_updates_use_expired_identity_fence_and_matching_bind_count() {
        for (name, sql, base_bind_count) in [
            ("recover_running", recover_running_command_sql(), 6),
            ("recover_failure", recover_failure_command_sql(), 8),
        ] {
            assert!(
                sql.ends_with(recovery_command_claim_fence_sql()),
                "{name} SQL omitted the expired exact-claim fence: {sql}"
            );
            assert_eq!(
                sql_parameter_count(&sql),
                base_bind_count + RECOVERY_COMMAND_CLAIM_FENCE_BIND_COUNT,
                "{name} SQL bind count drifted"
            );
            assert!(sql.contains("version = ?"), "{name} lost its version fence");
            assert!(sql.contains(&format!("claim.lease_expires_at <= {}", db_now_sql())));
            assert!(!sql.contains("claim.lease_expires_at <= ?"));
        }
    }

    #[test]
    fn matching_token_with_expired_lease_blocks_all_active_transitions_but_allows_recovery() {
        let command = test_command(false);
        let claim = test_claim(&command, "lease_expired");
        let fence = CommandClaimFence::new(&claim, &command.user_id, &command);
        let transition_now = "2026-08-12T00:05:00.000Z";
        let legacy_cutoff = "2026-08-12T00:00:00.000Z";

        assert!(fence.authorizes_identity(&claim));
        assert!(!fence.authorizes_active(&claim, transition_now));
        assert!(fence.authorizes_recovery(&claim, transition_now, legacy_cutoff));
        assert!(outbox_identity_authorizes_execution(
            &claim,
            &command.user_id,
            &command
        ));
        assert!(!outbox_authorizes_active_execution(
            &claim,
            &command.user_id,
            &command,
            transition_now
        ));
        assert!(outbox_authorizes_recovery(
            &claim,
            &command.user_id,
            &command,
            transition_now,
            legacy_cutoff
        ));

        for sql in [
            expire_claimed_command_sql(),
            start_command_sql(),
            settle_deleted_command_sql(),
            finish_success_command_sql(),
            finish_failure_command_sql(),
            command_execution_permit_sql(),
        ] {
            assert!(sql.contains(&format!("claim.lease_expires_at > {}", db_now_sql())));
            assert!(!sql.contains("claim.lease_expires_at > ?"));
        }
    }

    #[test]
    fn recovered_and_reclaimed_outbox_rejects_stale_worker_claim() {
        let command = test_command(false);
        let worker_a = test_claim(&command, "lease_a");
        let worker_a_fence = CommandClaimFence::new(&worker_a, &command.user_id, &command);
        assert!(worker_a_fence.authorizes_active(&worker_a, "2026-08-12T00:04:00.000Z"));

        let mut recovered = worker_a.clone();
        recovered.state = "queued".to_string();
        recovered.lease_token = None;
        recovered.lease_expires_at = None;
        assert!(!worker_a_fence.authorizes_identity(&recovered));

        let mut worker_b = recovered.clone();
        worker_b.state = "running".to_string();
        worker_b.lease_token = Some("lease_b".to_string());
        worker_b.lease_expires_at = Some("2026-08-12T00:10:00.000Z".to_string());
        assert!(!worker_a_fence.authorizes_active(&worker_b, "2026-08-12T00:06:00.000Z"));

        let worker_b_fence = CommandClaimFence::new(&worker_b, &command.user_id, &command);
        assert!(worker_b_fence.authorizes_active(&worker_b, "2026-08-12T00:06:00.000Z"));
    }

    #[test]
    fn paused_worker_cannot_permit_effect_after_recovery_and_new_generation_start() {
        let command_before_start = test_command(false);
        let worker_a = test_claim(&command_before_start, "lease_a");
        let worker_a_fence = CommandClaimFence::new(
            &worker_a,
            &command_before_start.user_id,
            &command_before_start,
        );
        let worker_a_expected_running_version = command_before_start.version + 1;

        let mut worker_a_started = command_before_start.clone();
        worker_a_started.state = "running".to_string();
        worker_a_started.version = worker_a_expected_running_version;
        assert!(command_matches_expected_running_generation(
            &worker_a_started,
            worker_a_expected_running_version
        ));

        let mut worker_b_started = worker_a_started.clone();
        worker_b_started.version += 2;
        let worker_a_still_has_claim =
            worker_a_fence.authorizes_active(&worker_a, "2026-08-12T00:04:00.000Z");
        let worker_a_has_generation = command_matches_expected_running_generation(
            &worker_b_started,
            worker_a_expected_running_version,
        );
        assert!(worker_a_still_has_claim);
        assert!(!worker_a_has_generation);
        assert!(!(worker_a_has_generation && worker_a_still_has_claim));

        let mut worker_b = worker_a.clone();
        worker_b.lease_token = Some("lease_b".to_string());
        worker_b.lease_expires_at = Some("2026-08-12T00:10:00.000Z".to_string());

        let worker_a_has_claim =
            worker_a_fence.authorizes_active(&worker_b, "2026-08-12T00:06:00.000Z");
        assert!(!worker_a_has_claim);
        assert!(!(worker_a_has_generation && worker_a_has_claim));

        let permit = command_execution_permit_sql();
        assert!(permit.contains("state = 'running' AND version = ?"));
        assert!(permit.contains("permit_session.deleted_at IS NULL"));
        assert!(permit.contains("started_attempt.attempts >= 1"));
        assert!(permit.ends_with(active_command_claim_fence_sql()));

        let durable_attempt = command_execution_attempt_permit_sql();
        assert!(durable_attempt.starts_with("INSERT INTO action_attempts"));
        assert!(durable_attempt.contains("'running', ?, NULL, 0"));
        assert!(durable_attempt.contains("WHERE changes() = 1"));
        assert!(durable_attempt.contains(
            "EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'running' AND version = ?)"
        ));
        assert!(
            durable_attempt.ends_with("ON CONFLICT(provider, provider_idempotency_key) DO NOTHING")
        );
        assert_eq!(sql_parameter_count(durable_attempt), 11);
    }

    #[test]
    fn protected_execution_uses_confirm_transaction_key_not_rotated_token_state() {
        let user_id = "usr_test";
        let command_id = "cmd_message";
        let command_key = "idem_message";
        let confirm_key =
            providers::scoped_idempotency_key(user_id, "command.execute.confirm", command_id);
        assert_eq!(
            expected_execution_outbox_key(user_id, command_id, command_key, true),
            confirm_key
        );

        assert_eq!(
            expected_execution_outbox_key(user_id, command_id, command_key, false),
            providers::scoped_idempotency_key(user_id, "command.execute", command_key)
        );

        let protected = test_command(true);
        let confirmed_claim = test_claim(&protected, "lease_confirmed");
        assert!(outbox_identity_authorizes_execution(
            &confirmed_claim,
            &protected.user_id,
            &protected
        ));

        let mut replay_only_claim = confirmed_claim.clone();
        replay_only_claim.idempotency_key = providers::scoped_idempotency_key(
            &protected.user_id,
            COMMAND_EXECUTE_TOPIC,
            &protected.idempotency_key,
        );
        assert!(!outbox_identity_authorizes_execution(
            &replay_only_claim,
            &protected.user_id,
            &protected
        ));
    }

    #[test]
    fn existing_unprotected_queued_command_key_remains_authorized() {
        let command = test_command(false);
        let claim = test_claim(&command, "lease_existing");

        assert_eq!(
            claim.idempotency_key,
            providers::scoped_idempotency_key(
                &command.user_id,
                COMMAND_EXECUTE_TOPIC,
                &command.idempotency_key
            )
        );
        assert!(outbox_identity_authorizes_execution(
            &claim,
            &command.user_id,
            &command
        ));
    }

    #[test]
    fn provider_names_are_stable_for_attempt_reconciliation() {
        assert_eq!(
            providers::action_attempt_provider("create_reminder"),
            Some("action.reminder")
        );
        assert_eq!(
            providers::action_attempt_provider("send_message"),
            Some("action.message")
        );
        assert_eq!(
            providers::action_attempt_provider("create_draft"),
            Some("local.draft")
        );
    }
}
