use serde::Deserialize;
use serde_json::{json, Map, Value};
use worker::D1Database;

use crate::action_effects;
use crate::auth::new_id;
use crate::commands;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::history;
use crate::models::{CommandRow, OutboxEventRow};
use crate::providers::{self, ActionProviderConfig};

const BATCH_SIZE: i64 = 20;
const MAX_ATTEMPTS: i32 = 3;
const LEASE_SECONDS: i64 = 300;

#[derive(Debug, Deserialize)]
struct CommandPayload {
    command_id: String,
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
    recover_stale_claims(db).await?;
    let now = db::now_iso();
    let rows: Vec<OutboxEventRow> = db::all(
        db,
        "SELECT id, user_id, topic, aggregate_id, payload_json, idempotency_key, state, attempts, next_attempt_at, last_error, created_at, updated_at FROM outbox_events WHERE state IN ('queued', 'retrying') AND (next_attempt_at IS NULL OR next_attempt_at <= ?) ORDER BY created_at ASC LIMIT ?",
        vec![db::text(&now), db::number(BATCH_SIZE)],
    )
    .await?;

    let mut processed = 0;
    for row in rows {
        if claim(db, &row).await? {
            processed += 1;
            if let Err(error) = process_claimed(db, env, &row, provider_config.clone()).await {
                settle_processing_error(db, &row, &error).await?;
            }
        }
    }
    Ok(processed)
}

/// A Worker can terminate after claiming an outbox row but before it settles
/// the command. Move leases older than the bounded execution window to the
/// explicit unknown/retryable state and increment the command version. The
/// version fence prevents the old invocation from reporting a late success.
async fn recover_stale_claims(db: &D1Database) -> ApiResult<()> {
    let cutoff = db::add_seconds_iso(-LEASE_SECONDS);
    let rows: Vec<OutboxEventRow> = db::all(
        db,
        "SELECT id, user_id, topic, aggregate_id, payload_json, idempotency_key, state, attempts, next_attempt_at, last_error, created_at, updated_at FROM outbox_events WHERE state = 'running' AND updated_at <= ? ORDER BY updated_at ASC LIMIT ?",
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
        if command.state != "running" {
            settle_orphan(db, &row, "command_claim_not_running").await?;
            continue;
        }

        if row.attempts >= MAX_ATTEMPTS {
            finish_failure(
                db,
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
                "UPDATE commands SET state = 'unknown', error_code = 'worker_lease_expired', version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'running' AND version = ? AND updated_at <= ?",
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
                "UPDATE outbox_events SET state = 'retrying', next_attempt_at = ?, last_error = 'worker_lease_expired', updated_at = ? WHERE id = ? AND state = 'running' AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'unknown' AND version = ?)",
                vec![
                    db::text(&now),
                    db::text(&now),
                    db::text(&row.id),
                    db::text(&command.id),
                    db::text(user_id),
                    db::number(next_version),
                ],
            )?,
            db::prepare(
                db,
                "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.unknown', ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'unknown' AND version = ?)",
                vec![
                    db::text(&new_id("aud")?),
                    db::text(user_id),
                    db::optional_text(command.session_id.as_deref()),
                    db::text(&json!({"command_id": command.id, "reason": "worker_lease_expired"}).to_string()),
                    db::text(&now),
                    db::text(&command.id),
                    db::text(user_id),
                    db::number(next_version),
                ],
            )?,
            db::prepare(
                db,
                "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'unknown' AND version = ?)",
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

async fn claim(db: &D1Database, row: &OutboxEventRow) -> ApiResult<bool> {
    let now = db::now_iso();
    let result = db::run(
        db,
        "UPDATE outbox_events SET state = 'running', attempts = attempts + 1, updated_at = ? WHERE id = ? AND state IN ('queued', 'retrying') AND (next_attempt_at IS NULL OR next_attempt_at <= ?)",
        vec![db::text(&now), db::text(&row.id), db::text(&now)],
    )
    .await?;
    Ok(db::changes(&result) == 1)
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

    let command = commands::get_for_user(db, user_id, &payload.command_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Command for outbox event was not found"))?;

    if let Some(session_id) = command.session_id.as_deref() {
        match commands::ensure_session_live(db, user_id, session_id).await {
            Ok(()) => {}
            Err(error) if error.status == 404 => {
                return settle_deleted_command(db, row, user_id, &command).await;
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

    start_command(db, user_id, &command).await?;
    let current = commands::get_for_user(db, user_id, &command.id)
        .await?
        .ok_or_else(|| ApiError::new(500, "command_error", "Command disappeared"))?;

    match execute_command(env, db, user_id, &current, provider_config).await {
        Ok(result) => finish_success(db, row, user_id, &current, result).await,
        Err(failure) => finish_failure(db, row, user_id, &current, failure, "running").await,
    }
}

async fn start_command(db: &D1Database, user_id: &str, command: &CommandRow) -> ApiResult<()> {
    let now = db::now_iso();
    let next_version = command.version + 1;
    let statements = vec![
        db::prepare(
            db,
            "UPDATE commands SET state = 'running', version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state IN ('queued', 'unknown') AND version = ? AND (session_id IS NULL OR EXISTS (SELECT 1 FROM sessions WHERE id = commands.session_id AND user_id = ? AND deleted_at IS NULL))",
            vec![
                db::number(next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(command.version),
                db::text(user_id),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.running', ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'running' AND version = ?)",
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
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'running' AND version = ?)",
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
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
) -> ApiResult<()> {
    let now = db::now_iso();
    let next_version = command.version + 1;
    let statements = vec![
        db::prepare(
            db,
            "UPDATE commands SET state = 'cancelled', error_code = 'session_deleted', version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state IN ('queued', 'unknown') AND version = ?",
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
            "UPDATE outbox_events SET state = 'failed', next_attempt_at = NULL, last_error = 'session_deleted', updated_at = ? WHERE id = ? AND state = 'running'",
            vec![db::text(&now), db::text(&row.id)],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.cancelled', ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'cancelled' AND version = ?)",
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
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'cancelled' AND version = ?)",
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
    Ok(())
}

async fn execute_command(
    env: &worker::Env,
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    provider_config: ActionProviderConfig,
) -> Result<Value, ExecutionFailure> {
    let args = serde_json::from_str::<Value>(&command.args_json)
        .ok()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    match command.intent.as_str() {
        "search_history" => {
            let query = string_arg(&args, &["q", "query", "text"]).ok_or_else(|| {
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
            action_effects::execute(env, db, user_id, command, &args, provider_config)
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

async fn finish_success(
    db: &D1Database,
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
            "UPDATE outbox_events SET state = 'succeeded', next_attempt_at = NULL, last_error = NULL, updated_at = ? WHERE id = ? AND state = 'running' AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'succeeded' AND version = ?)",
            vec![
                db::text(&now),
                db::text(&row.id),
                db::text(&command.id),
                db::text(user_id),
                db::number(version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.succeeded', ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'succeeded' AND version = ?)",
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
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'succeeded' AND version = ?)",
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
    db.batch(statements).await?;
    Ok(())
}

async fn finish_failure(
    db: &D1Database,
    row: &OutboxEventRow,
    user_id: &str,
    command: &CommandRow,
    failure: ExecutionFailure,
    expected_state: &str,
) -> ApiResult<()> {
    let now = db::now_iso();
    let attempt = row.attempts + 1;
    let retry = failure.retryable() && attempt < MAX_ATTEMPTS;
    let command_state = if retry { "unknown" } else { "failed" };
    let outbox_state = if retry { "retrying" } else { "failed" };
    let retry_at = retry.then(|| db::add_seconds_iso(backoff_seconds(row.attempts)));
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
            "UPDATE outbox_events SET state = ?, next_attempt_at = ?, last_error = ?, updated_at = ? WHERE id = ? AND state = 'running' AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = ? AND version = ?)",
            vec![
                db::text(outbox_state),
                db::optional_text(retry_at.as_deref()),
                db::text(&error.code),
                db::text(&now),
                db::text(&row.id),
                db::text(&command.id),
                db::text(user_id),
                db::text(command_state),
                db::number(version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = ? AND version = ?)",
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(if retry { "command.retrying" } else { "command.failed" }),
                db::text(&json!({"command_id": command.id, "error_code": error.code, "retryable": retry, "version": version}).to_string()),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::text(command_state),
                db::number(version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = ? AND version = ?)",
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
            "INSERT INTO action_attempts (id, user_id, command_id, action_id, provider, provider_idempotency_key, state, request_hash, response_json, attempts, next_attempt_at, last_error, created_at, updated_at) VALUES (?, ?, ?, NULL, ?, ?, ?, ?, NULL, ?, ?, ?, ?, ?) ON CONFLICT(provider, provider_idempotency_key) DO UPDATE SET state = excluded.state, attempts = excluded.attempts, next_attempt_at = excluded.next_attempt_at, last_error = excluded.last_error, updated_at = excluded.updated_at",
            vec![
                db::text(&new_id("attempt")?),
                db::text(user_id),
                db::text(&command.id),
                db::text(provider),
                db::text(&provider_idempotency_key),
                db::text(if retry { "retrying" } else { "failed" }),
                db::text(&command.command_hash),
                db::number(attempt as i64),
                db::optional_text(retry_at.as_deref()),
                db::text(&error.code),
                db::text(&now),
                db::text(&now),
            ],
        )?);
    }
    db.batch(statements).await?;
    Ok(())
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
    let attempt = row.attempts + 1;
    let retry = attempt < MAX_ATTEMPTS;
    let now = db::now_iso();
    db::run(
        db,
        "UPDATE outbox_events SET state = ?, next_attempt_at = ?, last_error = ?, updated_at = ? WHERE id = ? AND state = 'running'",
        vec![
            db::text(if retry { "retrying" } else { "failed" }),
            db::optional_text(
                retry
                    .then(|| db::add_seconds_iso(backoff_seconds(row.attempts)))
                    .as_deref(),
            ),
            db::text(&error.code),
            db::text(&now),
            db::text(&row.id),
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
        "pending" | "validated" | "queued" | "unknown" | "running"
    ) {
        return finish_failure(
            db,
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
        "UPDATE outbox_events SET state = 'failed', next_attempt_at = NULL, last_error = ?, updated_at = ? WHERE id = ? AND state = 'running'",
        vec![
            db::text(reason),
            db::text(&db::now_iso()),
            db::text(&row.id),
        ],
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn provider_names_are_stable_for_attempt_reconciliation() {
        assert_eq!(
            providers::action_attempt_provider("create_reminder"),
            Some("action.reminder")
        );
        assert_eq!(
            providers::action_attempt_provider("send_message"),
            Some("action.message")
        );
        assert_eq!(providers::action_attempt_provider("create_draft"), None);
    }
}
