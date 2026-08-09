use serde::Deserialize;
use serde_json::Value;
use worker::D1Database;

use crate::auth::new_id;
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

#[derive(Debug, Default)]
pub struct BeginResult {
    pub replay: Option<Value>,
    pub claim_token: Option<String>,
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
) -> ApiResult<BeginResult> {
    let Some(key) = idempotency_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(BeginResult::default());
    };
    if !["reply", "confirm"].contains(&operation) {
        return Err(ApiError::validation("Unknown phone operation"));
    }
    let now = db::now_iso();
    let expires_at = db::add_seconds_iso(LEASE_SECONDS);
    let claim_token = new_id("opclaim")?;
    let inserted = db::run(
        db,
        "INSERT OR IGNORE INTO phone_operations (user_id, idempotency_key, operation, request_hash, session_id, action_id, response_json, expires_at, claim_token, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?, ?, ?)",
        vec![
            db::text(user_id),
            db::text(key),
            db::text(operation),
            db::text(request_hash),
            db::text(session_id),
            db::optional_text(action_id),
            db::text(&expires_at),
            db::text(&claim_token),
            db::text(&now),
            db::text(&now),
        ],
    )
    .await?;
    if db::changes(&inserted) > 0 {
        return Ok(BeginResult {
            replay: None,
            claim_token: Some(claim_token),
        });
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
        return serde_json::from_str(&response)
            .map(|replay| BeginResult {
                replay: Some(replay),
                claim_token: None,
            })
            .map_err(|_| {
                ApiError::new(
                    500,
                    "idempotency_error",
                    "Stored operation response is invalid",
                )
            });
    }
    if db::is_expired(&existing.expires_at) {
        let replacement_claim = new_id("opclaim")?;
        let replaced = db::run(
            db,
            "UPDATE phone_operations SET response_json = NULL, expires_at = ?, claim_token = ?, updated_at = ? WHERE user_id = ? AND idempotency_key = ? AND operation = ? AND request_hash = ? AND session_id = ? AND response_json IS NULL AND expires_at <= ?",
            vec![
                db::text(&expires_at),
                db::text(&replacement_claim),
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
            return Ok(BeginResult {
                replay: None,
                claim_token: Some(replacement_claim),
            });
        }
    }
    Err(ApiError::conflict(
        "The same phone operation is already running",
    ))
}

#[allow(clippy::too_many_arguments)]
pub async fn complete(
    db: &D1Database,
    user_id: &str,
    operation: &str,
    idempotency_key: Option<&str>,
    request_hash: &str,
    session_id: &str,
    claim_token: Option<&str>,
    response: &Value,
) -> ApiResult<()> {
    let Some(key) = idempotency_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(());
    };
    let claim_token = claim_token
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::conflict("Phone operation claim is missing"))?;
    let result = db::run(
        db,
        "UPDATE phone_operations SET response_json = ?, claim_token = NULL, updated_at = ? WHERE user_id = ? AND idempotency_key = ? AND operation = ? AND request_hash = ? AND session_id = ? AND claim_token = ? AND response_json IS NULL",
        vec![
            db::text(&response.to_string()),
            db::text(&db::now_iso()),
            db::text(user_id),
            db::text(key),
            db::text(operation),
            db::text(request_hash),
            db::text(session_id),
            db::text(claim_token),
        ],
    )
    .await?;
    if db::changes(&result) == 0 {
        return Err(ApiError::conflict(
            "Phone operation claim expired or was replaced",
        ));
    }
    Ok(())
}

pub async fn release(
    db: &D1Database,
    user_id: &str,
    operation: &str,
    idempotency_key: Option<&str>,
    claim_token: Option<&str>,
) {
    let Some(key) = idempotency_key
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };
    let Some(claim_token) = claim_token.map(str::trim).filter(|value| !value.is_empty()) else {
        return;
    };
    let _ = db::run(
        db,
        "DELETE FROM phone_operations WHERE user_id = ? AND idempotency_key = ? AND operation = ? AND claim_token = ? AND response_json IS NULL",
        vec![
            db::text(user_id),
            db::text(key),
            db::text(operation),
            db::text(claim_token),
        ],
    )
    .await;
}
