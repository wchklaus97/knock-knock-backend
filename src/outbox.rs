use serde::Deserialize;
use serde_json::{json, Map, Value};
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

fn outbox_select() -> &'static str {
    "SELECT id, user_id, topic, aggregate_id, payload_json, idempotency_key, state, attempts, next_attempt_at, last_error, created_at, updated_at, lease_token, lease_expires_at FROM outbox_events"
}

fn lease_fence_sql() -> &'static str {
    " AND (lease_token = ? OR (lease_token IS NULL AND ? IS NULL))"
}

fn confirmation_fence_sql() -> &'static str {
    " AND (needs_confirmation = 0 OR EXISTS (SELECT 1 FROM confirmation_tokens WHERE command_id = commands.id AND user_id = commands.user_id AND command_hash = commands.command_hash AND used_at IS NOT NULL))"
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
/// the command. Move leases older than the bounded execution window to the
/// explicit unknown/retryable state and increment the command version. The
/// version fence prevents the old invocation from reporting a late success.
async fn recover_stale_claims(db: &D1Database, env: &worker::Env) -> ApiResult<()> {
    let cutoff = db::add_seconds_iso(-LEASE_SECONDS);
    let now = db::now_iso();
    let rows: Vec<OutboxEventRow> = db::all(
        db,
        &format!(
            "{} WHERE state = 'running' AND ((lease_expires_at IS NOT NULL AND lease_expires_at <= ?) OR (lease_expires_at IS NULL AND updated_at <= ?)) ORDER BY updated_at ASC LIMIT ?",
            outbox_select()
        ),
        vec![db::text(&now), db::text(&cutoff), db::number(BATCH_SIZE)],
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
        if command.state != "running" {
            settle_orphan(db, &row, "command_claim_not_running").await?;
            continue;
        }

        if row.attempts >= MAX_ATTEMPTS {
            finish_failure(
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
                "running",
            )
            .await?;
            continue;
        }

        let now = db::now_iso();
        let next_version = command.version + 1;
        let statements = vec![
            db::prepare(
                db,
                "UPDATE commands SET state = 'retryable', error_code = 'worker_lease_expired', version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'running' AND version = ? AND updated_at <= ?",
                vec![
                    db::number(next_version),
                    db::text(&now),
                    db::text(&command.id),
                    db::text(user_id),
                    db::number(command.version),
                    db::text(&cutoff),
                ],
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

    let mut command = commands::get_for_user(db, user_id, &payload.command_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Command for outbox event was not found"))?;

    if command.expires_at.as_deref().is_some_and(db::is_expired)
        && matches!(
            command.state.as_str(),
            "pending" | "validated" | "queued" | "retryable" | "unknown"
        )
    {
        command = commands::expire_if_due(db, user_id, &command.id)
            .await?
            .ok_or_else(|| ApiError::new(500, "command_error", "Command disappeared"))?;
    }

    if let Some(session_id) = command.session_id.as_deref() {
        match commands::ensure_session_live(db, user_id, session_id).await {
            Ok(()) => {}
            Err(error) if error.status == 404 => {
                return settle_deleted_command(db, env, row, user_id, &command).await;
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

    start_command(db, user_id, &command).await?;
    let current = commands::get_for_user(db, user_id, &command.id)
        .await?
        .ok_or_else(|| ApiError::new(500, "command_error", "Command disappeared"))?;

    match execute_command(env, db, user_id, &current, &args, provider_config).await {
        Ok(result) => finish_success(db, env, row, user_id, &current, result).await,
        Err(failure) => finish_failure(db, env, row, user_id, &current, failure, "running").await,
    }
}

fn validated_command_args(intent: &str, args_json: &str) -> ApiResult<Map<String, Value>> {
    let args = serde_json::from_str::<Value>(args_json)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .ok_or_else(|| ApiError::validation("Persisted command arguments must be an object"))?;
    commands::validate_action_args(intent, &args)
        .map_err(|error| ApiError::validation(error.to_string()))?;
    Ok(args)
}

async fn start_command(db: &D1Database, user_id: &str, command: &CommandRow) -> ApiResult<()> {
    let now = db::now_iso();
    let next_version = command.version + 1;
    let statements = vec![
        db::prepare(
            db,
            &format!(
                "UPDATE commands SET state = 'running', version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state IN ('queued', 'retryable', 'unknown') AND version = ? AND (expires_at IS NULL OR expires_at > ?) AND (session_id IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE id = commands.session_id AND user_id = ? AND deleted_at IS NULL)){}",
                confirmation_fence_sql()
            ),
            vec![
                db::number(next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(command.version),
                db::text(&now),
                db::text(user_id),
            ],
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
        return Err(ApiError::conflict("Command is no longer executable"));
    }
    Ok(())
}

async fn settle_deleted_command(
    db: &D1Database,
    env: &worker::Env,
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
) -> ApiResult<()> {
    let now = db::now_iso();
    let next_version = command.version + 1;
    let statements = vec![
        db::prepare(
            db,
            "UPDATE commands SET state = 'cancelled', error_code = 'session_deleted', version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state IN ('queued', 'retryable', 'unknown') AND version = ?",
            vec![
                db::number(next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(command.version),
            ],
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
        &command.id,
        "cancelled",
        results.first().map(db::changes).unwrap_or(0),
    )
    .await;
    Ok(())
}

async fn execute_command(
    env: &worker::Env,
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
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
            action_effects::execute(env, db, user_id, command, args, provider_config)
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
        db::prepare(
            db,
            "UPDATE commands SET state = 'succeeded', result_json = ?, error_code = NULL, version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'running' AND version = ?",
            vec![
                db::text(&result.to_string()),
                db::number(version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(command.version),
            ],
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
        &command.id,
        "succeeded",
        results.first().map(db::changes).unwrap_or(0),
    )
    .await;
    Ok(())
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
    let now = db::now_iso();
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
    let mut statements = vec![
        db::prepare(
            db,
            "UPDATE commands SET state = ?, result_json = NULL, error_code = ?, version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = ? AND version = ?",
            vec![
                db::text(command_state),
                db::text(&error.code),
                db::number(version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::text(expected_state),
                db::number(command.version),
            ],
        )?,
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
                db::text(&now),
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
                db::text(&now),
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
                db::text(&now),
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
                db::text(&now),
                db::text(&now),
            ],
        )?);
    }
    let results = db.batch(statements).await?;
    notify_terminal_transition(
        db,
        env,
        user_id,
        &command.id,
        command_state,
        results.first().map(db::changes).unwrap_or(0),
    )
    .await;
    Ok(())
}

fn should_attempt_command_wakeup(command_state: &str, transition_changes: usize) -> bool {
    transition_changes == 1
        && matches!(
            command_state,
            "succeeded" | "failed" | "expired" | "cancelled"
        )
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
    command_id: &str,
    command_state: &str,
    transition_changes: usize,
) {
    if let Some(attempt) = command_wakeup_attempt(command_state, transition_changes, || {
        attempt_command_wakeup(db, env, user_id, command_id)
    }) {
        attempt.await;
    }
}

fn command_wakeup_token_query() -> &'static str {
    "SELECT push_token FROM devices WHERE user_id = ? AND platform = 'ios' AND push_token IS NOT NULL AND push_token != ''"
}

async fn attempt_command_wakeup(
    db: &D1Database,
    env: &worker::Env,
    user_id: &str,
    command_id: &str,
) {
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
        let _ = apns::send_command_wakeup(env, &token, command_id).await;
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
        assert!(should_attempt_command_wakeup("expired", 1));
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
    fn execution_query_requires_used_confirmation_for_protected_commands() {
        let query = confirmation_fence_sql();
        assert!(query.contains("needs_confirmation = 0"));
        assert!(query.contains("confirmation_tokens"));
        assert!(query.contains("used_at IS NOT NULL"));
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
