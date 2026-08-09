use serde_json::Value;
use worker::{D1Database, Env};

use crate::apns;
use crate::auth::config_value;
use crate::auth::new_id;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::PushRow;

#[derive(Debug, Clone, serde::Deserialize)]
struct DeviceTokenRow {
    platform: String,
    push_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PushDelivery {
    pub inbox: bool,
    pub apns_sent: usize,
    pub apns_errors: Vec<String>,
}

pub struct PushRequest<'a> {
    pub user_id: &'a str,
    pub session_id: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub voice_script: Option<&'a str>,
    pub payload: Value,
}

pub async fn enqueue_push(
    db: &D1Database,
    user_id: &str,
    session_id: &str,
    title: &str,
    body: &str,
    voice_script: Option<&str>,
    payload: Value,
) -> ApiResult<Value> {
    let push_id = new_id("push")?;
    let created_at = db::now_iso();
    db::run(
        db,
        "INSERT INTO pushes (id, user_id, session_id, title, body, voice_script, payload_json, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            db::text(&push_id),
            db::text(user_id),
            db::text(session_id),
            db::text(title),
            db::text(body),
            db::optional_text(voice_script),
            db::text(&payload.to_string()),
            db::text(&created_at),
            db::text(&created_at),
        ],
    )
    .await?;
    Ok(serde_json::json!({
        "push_id": push_id,
        "session_id": session_id,
        "title": title,
        "body": body,
        "voice_script": voice_script,
        "created_at": created_at,
    }))
}

pub async fn list_pushes(db: &D1Database, user_id: &str, limit: i32) -> ApiResult<Vec<Value>> {
    let rows: Vec<PushRow> = db::all(
        db,
        "SELECT id, session_id, title, body, voice_script, created_at, read_at, dismissed_at FROM pushes WHERE user_id = ? ORDER BY created_at DESC LIMIT ?",
        vec![db::text(user_id), db::number(limit.clamp(1, 200) as i64)],
    )
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| {
            serde_json::json!({
                "push_id": row.id,
                "session_id": row.session_id,
                "title": row.title,
                "body": row.body,
                "voice_script": row.voice_script,
                "created_at": row.created_at,
                "read_at": row.read_at,
                "dismissed_at": row.dismissed_at,
            })
        })
        .collect())
}

pub async fn mark_read(db: &D1Database, user_id: &str, push_id: &str) -> ApiResult<Value> {
    let now = db::now_iso();
    db::run(
        db,
        "UPDATE pushes SET read_at = COALESCE(read_at, ?), updated_at = ? WHERE id = ? AND user_id = ?",
        vec![
            db::text(&now),
            db::text(&now),
            db::text(push_id),
            db::text(user_id),
        ],
    )
    .await?;
    let row: Option<PushRow> = db::first(
        db,
        "SELECT id, session_id, title, body, voice_script, created_at, read_at, dismissed_at FROM pushes WHERE id = ? AND user_id = ?",
        vec![db::text(push_id), db::text(user_id)],
    )
    .await?;
    let row = row.ok_or_else(|| ApiError::not_found("Push not found"))?;
    Ok(push_value(row))
}

pub async fn mark_all_read(db: &D1Database, user_id: &str) -> ApiResult<Value> {
    let now = db::now_iso();
    let result = db::run(
        db,
        "UPDATE pushes SET read_at = COALESCE(read_at, ?), updated_at = ? WHERE user_id = ? AND read_at IS NULL",
        vec![db::text(&now), db::text(&now), db::text(user_id)],
    )
    .await?;
    Ok(serde_json::json!({
        "ok": true,
        "updated": db::changes(&result),
        "read_at": now,
    }))
}

pub async fn dismiss(db: &D1Database, user_id: &str, push_id: &str) -> ApiResult<Value> {
    let now = db::now_iso();
    db::run(
        db,
        "UPDATE pushes SET dismissed_at = COALESCE(dismissed_at, ?), updated_at = ? WHERE id = ? AND user_id = ?",
        vec![
            db::text(&now),
            db::text(&now),
            db::text(push_id),
            db::text(user_id),
        ],
    )
    .await?;
    let row: Option<PushRow> = db::first(
        db,
        "SELECT id, session_id, title, body, voice_script, created_at, read_at, dismissed_at FROM pushes WHERE id = ? AND user_id = ?",
        vec![db::text(push_id), db::text(user_id)],
    )
    .await?;
    let row = row.ok_or_else(|| ApiError::not_found("Push not found"))?;
    Ok(push_value(row))
}

fn push_value(row: PushRow) -> Value {
    serde_json::json!({
        "ok": true,
        "push_id": row.id,
        "session_id": row.session_id,
        "title": row.title,
        "body": row.body,
        "voice_script": row.voice_script,
        "created_at": row.created_at,
        "read_at": row.read_at,
        "dismissed_at": row.dismissed_at,
    })
}

async fn user_apns_tokens(db: &D1Database, user_id: &str) -> ApiResult<Vec<String>> {
    let rows: Vec<DeviceTokenRow> = db::all(
        db,
        "SELECT platform, push_token FROM devices WHERE user_id = ? AND push_token IS NOT NULL AND push_token != ''",
        vec![db::text(user_id)],
    )
    .await?;
    let mut tokens = rows
        .into_iter()
        .filter(|row| row.platform == "ios")
        .filter_map(|row| row.push_token)
        .filter(|token| apns::looks_like_token(token))
        .collect::<Vec<_>>();
    tokens.sort();
    tokens.dedup();
    Ok(tokens)
}

pub async fn notify_user(
    db: &D1Database,
    env: &Env,
    request: PushRequest<'_>,
) -> ApiResult<PushDelivery> {
    let mode = config_value(env, "PUSH_MODE", "dev");
    if !["dev", "apns", "both"].contains(&mode.as_str()) {
        return Err(ApiError::new(
            500,
            "configuration_error",
            "PUSH_MODE must be dev, apns, or both",
        ));
    }
    let apns_ready = apns::is_ready(env);
    let mut inbox = false;
    if mode == "dev" || mode == "both" || !apns_ready {
        enqueue_push(
            db,
            request.user_id,
            request.session_id,
            request.title,
            request.body,
            request.voice_script,
            request.payload.clone(),
        )
        .await?;
        inbox = true;
    }

    let mut apns_sent = 0;
    let mut apns_errors = Vec::new();
    if (mode == "apns" || mode == "both") && apns_ready {
        for token in user_apns_tokens(db, request.user_id).await? {
            match apns::send_alert(
                env,
                &token,
                request.title,
                request.body,
                request.session_id,
                request.voice_script,
            )
            .await
            {
                Ok(()) => apns_sent += 1,
                Err(error) => apns_errors.push(error.message),
            }
        }
    }

    // Keep the existing development phone polling loop alive if APNs is
    // configured but no physical device token was available or delivery failed.
    if !inbox && apns_sent == 0 {
        enqueue_push(
            db,
            request.user_id,
            request.session_id,
            request.title,
            request.body,
            request.voice_script,
            request.payload,
        )
        .await?;
        inbox = true;
    }
    Ok(PushDelivery {
        inbox,
        apns_sent,
        apns_errors,
    })
}
