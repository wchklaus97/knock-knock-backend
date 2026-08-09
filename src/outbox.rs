use serde::Deserialize;
use serde_json::{json, Map, Value};
use worker::D1Database;

use crate::auth::new_id;
use crate::commands;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::history;
use crate::models::{CommandRow, OutboxEventRow};

const BATCH_SIZE: i64 = 20;
const MAX_ATTEMPTS: i32 = 3;

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

pub async fn drain(db: &D1Database) -> ApiResult<usize> {
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
            if let Err(error) = process_claimed(db, &row).await {
                // A failure before the command can be loaded or transitioned
                // must still release the lease. The command remains queued or
                // is reconciled by the next scheduled run.
                release_runtime_error(db, &row, &error).await?;
            }
        }
    }
    Ok(processed)
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

async fn process_claimed(db: &D1Database, row: &OutboxEventRow) -> ApiResult<()> {
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

    match execute_command(db, user_id, &current).await {
        Ok(result) => finish_success(db, row, user_id, &current, result).await,
        Err(failure) => finish_failure(db, row, user_id, &current, failure).await,
    }
}

async fn start_command(db: &D1Database, user_id: &str, command: &CommandRow) -> ApiResult<()> {
    let now = db::now_iso();
    let next_version = command.version + 1;
    let statements = vec![
        db::prepare(
            db,
            "UPDATE commands SET state = 'running', version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state IN ('queued', 'unknown')",
            vec![
                db::number(next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) VALUES (?, ?, ?, 'command.running', ?, ?)",
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(&json!({"command_id": command.id, "version": next_version}).to_string()),
                db::text(&now),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) VALUES (?, 'command', ?, ?, ?, ?)",
            vec![
                db::text(user_id),
                db::text(&command.id),
                db::optional_text(command.session_id.as_deref()),
                db::number(next_version),
                db::text(&now),
            ],
        )?,
    ];
    let result = db.batch(statements).await?;
    if result.first().map(db::changes).unwrap_or(0) == 0 {
        return Err(ApiError::conflict("Command is no longer executable"));
    }
    Ok(())
}

async fn execute_command(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
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
            // These intents are deliberately not reported as successful until
            // their provider/domain adapters exist. A queued command may be
            // retried or end in unknown/failed, but it must never pretend that
            // an external side effect happened.
            Err(ExecutionFailure::Retryable(ApiError::new(
                503,
                "executor_unavailable",
                "The action executor is not configured",
            )))
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
    if matches!(error.status, 408 | 429 | 500..=599) {
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
            "UPDATE commands SET state = 'succeeded', result_json = ?, error_code = NULL, version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'running'",
            vec![
                db::text(&result.to_string()),
                db::number(version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE outbox_events SET state = 'succeeded', next_attempt_at = NULL, last_error = NULL, updated_at = ? WHERE id = ? AND state = 'running'",
            vec![db::text(&now), db::text(&row.id)],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) VALUES (?, ?, ?, 'command.succeeded', ?, ?)",
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(&json!({"command_id": command.id, "version": version}).to_string()),
                db::text(&now),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) VALUES (?, 'command', ?, ?, ?, ?)",
            vec![
                db::text(user_id),
                db::text(&command.id),
                db::optional_text(command.session_id.as_deref()),
                db::number(version),
                db::text(&now),
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
) -> ApiResult<()> {
    let now = db::now_iso();
    let attempt = row.attempts + 1;
    let retry = failure.retryable() && attempt < MAX_ATTEMPTS;
    let command_state = if retry { "unknown" } else { "failed" };
    let outbox_state = if retry { "retrying" } else { "failed" };
    let retry_at = retry.then(|| db::add_seconds_iso(backoff_seconds(row.attempts)));
    let version = command.version + 1;
    let error = failure.error();
    let statements = vec![
        db::prepare(
            db,
            "UPDATE commands SET state = ?, result_json = NULL, error_code = ?, version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'running'",
            vec![
                db::text(command_state),
                db::text(&error.code),
                db::number(version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE outbox_events SET state = ?, next_attempt_at = ?, last_error = ?, updated_at = ? WHERE id = ? AND state = 'running'",
            vec![
                db::text(outbox_state),
                db::optional_text(retry_at.as_deref()),
                db::text(&error.code),
                db::text(&now),
                db::text(&row.id),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) VALUES (?, ?, ?, ?, ?, ?)",
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(if retry { "command.retrying" } else { "command.failed" }),
                db::text(&json!({"command_id": command.id, "error_code": error.code, "retryable": retry, "version": version}).to_string()),
                db::text(&now),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) VALUES (?, 'command', ?, ?, ?, ?)",
            vec![
                db::text(user_id),
                db::text(&command.id),
                db::optional_text(command.session_id.as_deref()),
                db::number(version),
                db::text(&now),
            ],
        )?,
    ];
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
        assert!(!classify_error(ApiError::validation("bad input")).retryable());
    }
}
