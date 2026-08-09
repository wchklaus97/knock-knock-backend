use serde::Deserialize;
use serde_json::Value;
use worker::D1Database;

use crate::db;
use crate::error::{ApiError, ApiResult};

const LEASE_SECONDS: i64 = 300;

#[derive(Debug, Clone, Deserialize)]
struct PhoneOperationRow {
    operation: String,
    request_hash: Option<String>,
    session_id: Option<String>,
    action_id: Option<String>,
    response_json: Option<String>,
    expires_at: String,
}

/// Claims a compatibility reply/confirm idempotency key. A completed request
/// is replayed; an unexpired in-flight request is rejected instead of
/// allowing two side effects to run concurrently.
pub async fn begin(
    db: &D1Database,
    user_id: &str,
    operation: &str,
    idempotency_key: Option<&str>,
    request_hash: &str,
    session_id: &str,
    action_id: Option<&str>,
) -> ApiResult<Option<Value>> {
    let Some(key) = idempotency_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if !["reply", "confirm"].contains(&operation) {
        return Err(ApiError::validation("Unknown phone operation"));
    }
    let now = db::now_iso();
    let expires_at = db::add_seconds_iso(LEASE_SECONDS);
    let inserted = db::run(
        db,
        "INSERT OR IGNORE INTO phone_operations (user_id, idempotency_key, operation, request_hash, session_id, action_id, response_json, expires_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?, ?)",
        vec![
            db::text(user_id),
            db::text(key),
            db::text(operation),
            db::text(request_hash),
            db::text(session_id),
            db::optional_text(action_id),
            db::text(&expires_at),
            db::text(&now),
            db::text(&now),
        ],
    )
    .await?;
    if db::changes(&inserted) > 0 {
        return Ok(None);
    }

    let existing = db::first::<PhoneOperationRow>(
        db,
        "SELECT operation, request_hash, session_id, action_id, response_json, expires_at FROM phone_operations WHERE user_id = ? AND idempotency_key = ?",
        vec![db::text(user_id), db::text(key)],
    )
    .await?
    .ok_or_else(|| ApiError::conflict("Idempotency record is unavailable"))?;
    if existing.operation != operation
        || existing.request_hash.as_deref() != Some(request_hash)
        || existing.session_id.as_deref() != Some(session_id)
        || existing.action_id.as_deref() != action_id
    {
        return Err(ApiError::conflict(
            "Idempotency key was already used for a different phone request",
        ));
    }
    if let Some(response) = existing.response_json {
        return serde_json::from_str(&response).map(Some).map_err(|_| {
            ApiError::new(
                500,
                "idempotency_error",
                "Stored operation response is invalid",
            )
        });
    }
    if db::is_expired(&existing.expires_at) {
        let replaced = db::run(
            db,
            "UPDATE phone_operations SET response_json = NULL, expires_at = ?, updated_at = ? WHERE user_id = ? AND idempotency_key = ? AND operation = ? AND request_hash = ? AND session_id = ? AND response_json IS NULL AND expires_at <= ?",
            vec![
                db::text(&expires_at),
                db::text(&now),
                db::text(user_id),
                db::text(key),
                db::text(operation),
                db::text(request_hash),
                db::text(session_id),
                db::text(&now),
            ],
        )
        .await?;
        if db::changes(&replaced) > 0 {
            return Ok(None);
        }
    }
    Err(ApiError::conflict(
        "The same phone operation is already running",
    ))
}

pub async fn complete(
    db: &D1Database,
    user_id: &str,
    operation: &str,
    idempotency_key: Option<&str>,
    request_hash: &str,
    session_id: &str,
    response: &Value,
) -> ApiResult<()> {
    let Some(key) = idempotency_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    db::run(
        db,
        "UPDATE phone_operations SET response_json = ?, updated_at = ? WHERE user_id = ? AND idempotency_key = ? AND operation = ? AND request_hash = ? AND session_id = ? AND response_json IS NULL",
        vec![
            db::text(&response.to_string()),
            db::text(&db::now_iso()),
            db::text(user_id),
            db::text(key),
            db::text(operation),
            db::text(request_hash),
            db::text(session_id),
        ],
    )
    .await?;
    Ok(())
}

pub async fn release(
    db: &D1Database,
    user_id: &str,
    operation: &str,
    idempotency_key: Option<&str>,
) {
    let Some(key) = idempotency_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let _ = db::run(
        db,
        "DELETE FROM phone_operations WHERE user_id = ? AND idempotency_key = ? AND operation = ? AND response_json IS NULL",
        vec![db::text(user_id), db::text(key), db::text(operation)],
    )
    .await;
}
