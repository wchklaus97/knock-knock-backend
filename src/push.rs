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

#[derive(Debug, Clone, serde::Serialize)]
pub struct PushDelivery {
    pub inbox: bool,
    pub apns_sent: usize,
    pub apns_errors: Vec<String>,
}

impl PushDelivery {
    pub fn diagnostic_value(&self) -> Value {
        serde_json::json!({
            "inbox": self.inbox,
            "apns_attempted": self.apns_sent + self.apns_errors.len(),
            "apns_sent": self.apns_sent,
            "apns_errors": self.apns_errors,
        })
    }
}

pub struct PushRequest<'a> {
    pub user_id: &'a str,
    pub session_id: Option<&'a str>,
    pub title: &'a str,
    pub body: &'a str,
    pub voice_script: Option<&'a str>,
    pub dedupe_key: Option<&'a str>,
    pub payload: Value,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct PushIdRow {
    id: String,
}

async fn enqueue_push(db: &D1Database, request: &PushRequest<'_>) -> ApiResult<Value> {
    let push_id = new_id("push")?;
    let created_at = db::now_iso();
    db::run(
        db,
        "INSERT OR IGNORE INTO pushes (id, user_id, session_id, title, body, voice_script, payload_json, dedupe_key, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            db::text(&push_id),
            db::text(request.user_id),
            db::optional_text(request.session_id),
            db::text(request.title),
            db::text(request.body),
            db::optional_text(request.voice_script),
            db::text(&request.payload.to_string()),
            db::optional_text(request.dedupe_key),
            db::text(&created_at),
            db::text(&created_at),
        ],
    )
    .await?;
    let stored_id = if let Some(dedupe_key) = request.dedupe_key {
        db::first::<PushIdRow>(
            db,
            "SELECT id FROM pushes WHERE user_id = ? AND dedupe_key = ?",
            vec![db::text(request.user_id), db::text(dedupe_key)],
        )
        .await?
        .map(|row| row.id)
        .ok_or_else(|| ApiError::new(500, "push_error", "Deduplicated push was not persisted"))?
    } else {
        push_id
    };
    Ok(serde_json::json!({
        "push_id": stored_id,
        "session_id": request.session_id,
        "title": request.title,
        "body": request.body,
        "voice_script": request.voice_script,
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
        enqueue_push(db, &request).await?;
        inbox = true;
    }

    let mut apns_sent = 0;
    let mut apns_errors = Vec::new();
    if (mode == "apns" || mode == "both") && apns_ready {
        let tokens = user_apns_tokens(db, request.user_id).await?;
        if !tokens.is_empty() {
            match apns::provider_authorization(env) {
                Ok(provider_authorization) => {
                    for token in tokens {
                        match apns::send_alert(
                            env,
                            &provider_authorization,
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
                Err(error) => {
                    apns_errors.resize(tokens.len(), error.message);
                }
            }
        }
    }

    // Keep the existing development phone polling loop alive if APNs is
    // configured but no physical device token was available or delivery failed.
    if !inbox && apns_sent == 0 {
        enqueue_push(db, &request).await?;
        inbox = true;
    }
    Ok(PushDelivery {
        inbox,
        apns_sent,
        apns_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_diagnostic_reports_attempts_without_device_tokens() {
        let delivery = PushDelivery {
            inbox: true,
            apns_sent: 1,
            apns_errors: vec!["APNs returned 400: {\"reason\":\"BadDeviceToken\"}".into()],
        };

        let value = delivery.diagnostic_value();
        assert_eq!(value["inbox"], true);
        assert_eq!(value["apns_attempted"], 2);
        assert_eq!(value["apns_sent"], 1);
        assert_eq!(value["apns_errors"].as_array().map(Vec::len), Some(1));
        assert!(!value.to_string().contains("device_token"));
    }
}
