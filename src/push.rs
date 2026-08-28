use futures_util::TryStreamExt;
use serde_json::Value;
use worker::{
    D1Database, D1PreparedStatement, Env, Fetch, Headers, Method, Request, RequestInit, Response,
    ResponseBody,
};

use crate::apns;
use crate::auth::config_value;
use crate::auth::new_id;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::PushRow;

const DURABLE_APNS_ERROR_BODY_LIMIT: usize = 1_024;
const DURABLE_APNS_MAX_RETRY_AFTER_SECONDS: u64 = 24 * 60 * 60;
const DURABLE_APNS_DEFAULT_RETRY_SECONDS: u64 = 5;
const DURABLE_APNS_DELIVERY_PREFIX: &str = "apns_delivery_";
pub(crate) const MAX_ACTIVE_DEVICE_REGISTRATIONS_PER_USER: i64 = 8;

#[derive(Debug, Clone, serde::Deserialize)]
struct DeviceTokenRow {
    platform: String,
    push_token: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PushDelivery;

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

#[derive(Debug)]
struct DurableBodyPrefix {
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(Debug, serde::Deserialize)]
struct DurableAppleErrorBody {
    reason: String,
}

pub(crate) fn session_event_notification_key(user_id: &str, event_id: &str) -> String {
    format!("session.event.notification:{user_id}:{event_id}")
}

pub(crate) fn session_event_push_insert_sql() -> &'static str {
    "INSERT OR IGNORE INTO pushes (id, user_id, session_id, title, body, voice_script, payload_json, dedupe_key, created_at, updated_at) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM events WHERE id = ? AND session_id = ? AND idempotency_key = ?)"
}

pub(crate) fn session_event_notification_insert_sql() -> &'static str {
    "INSERT INTO outbox_events (id, user_id, topic, aggregate_id, payload_json, idempotency_key, state, attempts, next_attempt_at, last_error, created_at, updated_at, lease_token, lease_expires_at) SELECT ?, ?, 'session.event.notification', ?, json_object('event_id', ?, 'push_id', pushes.id, 'session_id', ?, 'title', ?, 'body', ?, 'voice_script', ?, 'payload', json(?)), ?, 'queued', 0, NULL, NULL, ?, ?, NULL, NULL FROM pushes WHERE pushes.user_id = ? AND pushes.dedupe_key = ? AND EXISTS (SELECT 1 FROM events WHERE id = ? AND session_id = ? AND idempotency_key = ?) ON CONFLICT(topic, idempotency_key) DO NOTHING"
}

pub(crate) fn prepare_session_event_notification(
    db: &D1Database,
    request: &PushRequest<'_>,
    event_id: &str,
    event_idempotency_key: &str,
) -> ApiResult<Vec<D1PreparedStatement>> {
    let session_id = request
        .session_id
        .ok_or_else(|| ApiError::validation("Session event notification requires a session"))?;
    let push_id = new_id("push")?;
    let outbox_id = new_id("out")?;
    let now = db::now_iso();
    let delivery_key = session_event_notification_key(request.user_id, event_id);
    let payload_json = request.payload.to_string();
    Ok(vec![
        db::prepare(
            db,
            session_event_push_insert_sql(),
            vec![
                db::text(&push_id),
                db::text(request.user_id),
                db::text(session_id),
                db::text(request.title),
                db::text(request.body),
                db::optional_text(request.voice_script),
                db::text(&payload_json),
                db::text(&delivery_key),
                db::text(&now),
                db::text(&now),
                db::text(event_id),
                db::text(session_id),
                db::text(event_idempotency_key),
            ],
        )?,
        db::prepare(
            db,
            session_event_notification_insert_sql(),
            vec![
                db::text(&outbox_id),
                db::text(request.user_id),
                db::text(event_id),
                db::text(event_id),
                db::text(session_id),
                db::text(request.title),
                db::text(request.body),
                db::optional_text(request.voice_script),
                db::text(&payload_json),
                db::text(&delivery_key),
                db::text(&now),
                db::text(&now),
                db::text(request.user_id),
                db::text(&delivery_key),
                db::text(event_id),
                db::text(session_id),
                db::text(event_idempotency_key),
            ],
        )?,
    ])
}

pub(crate) async fn ensure_session_event_notification(
    db: &D1Database,
    request: &PushRequest<'_>,
    event_id: &str,
    event_idempotency_key: &str,
) -> ApiResult<()> {
    db.batch(prepare_session_event_notification(
        db,
        request,
        event_id,
        event_idempotency_key,
    )?)
    .await?;
    Ok(())
}

pub(crate) fn durable_delivery_queued_value() -> Value {
    serde_json::json!({
        "inbox": true,
        "delivery_queued": true,
        "apns_attempted": 0,
        "apns_sent": 0,
        "apns_accepted": 0,
        "apns_invalid_token": 0,
        "apns_permanent": 0,
        "apns_retryable": 0,
        "apns_unknown": 0,
        "apns_errors": [],
    })
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
        "SELECT platform, push_token FROM devices WHERE user_id = ? AND platform = 'ios' AND push_token IS NOT NULL AND push_token != '' ORDER BY updated_at DESC, created_at DESC, id DESC LIMIT ?",
        vec![
            db::text(user_id),
            db::number(MAX_ACTIVE_DEVICE_REGISTRATIONS_PER_USER),
        ],
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

fn invalidate_apns_token_sql() -> &'static str {
    "UPDATE devices SET push_token = NULL, updated_at = ? WHERE push_token = ?"
}

async fn invalidate_apns_token(db: &D1Database, token: &str) -> ApiResult<()> {
    db::run(
        db,
        invalidate_apns_token_sql(),
        vec![db::text(&db::now_iso()), db::text(token)],
    )
    .await?;
    Ok(())
}

pub(crate) fn stable_apns_id(delivery_id: &str) -> ApiResult<String> {
    let hex = delivery_id
        .strip_prefix(DURABLE_APNS_DELIVERY_PREFIX)
        .filter(|value| value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .ok_or_else(|| {
            ApiError::new(
                500,
                "apns_delivery_identity_error",
                "Durable APNs delivery identity is invalid",
            )
        })?
        .to_ascii_lowercase();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32],
    ))
}

pub(crate) struct DurableAlertInput<'a> {
    pub(crate) provider_authorization: &'a str,
    pub(crate) token: &'a str,
    pub(crate) apns_id: &'a str,
    pub(crate) title: &'a str,
    pub(crate) body: &'a str,
    pub(crate) session_id: Option<&'a str>,
    pub(crate) event_id: &'a str,
}

fn durable_alert_payload(input: &DurableAlertInput<'_>) -> String {
    serde_json::json!({
        "aps": {
            "alert": { "title": input.title, "body": input.body },
            "sound": "default",
            "content-available": 1,
        },
        "session_id": input.session_id,
        "event_id": input.event_id,
    })
    .to_string()
}

pub(crate) fn durable_alert_request(env: &Env, input: DurableAlertInput<'_>) -> ApiResult<Request> {
    if !apns::looks_like_token(input.token) {
        return Err(ApiError::new(
            400,
            "apns_error",
            "Invalid APNs device token",
        ));
    }
    let host = if config_value(env, "APNS_PRODUCTION", "false") == "true" {
        "api.push.apple.com"
    } else {
        "api.sandbox.push.apple.com"
    };
    let url = worker::Url::parse(&format!("https://{host}/3/device/{}", input.token))
        .map_err(|error| ApiError::new(500, "apns_error", error.to_string()))?;
    let headers = Headers::new();
    headers.set(
        "authorization",
        &format!("bearer {}", input.provider_authorization),
    )?;
    headers.set("apns-topic", &apns::bundle_id(env))?;
    headers.set("apns-push-type", "alert")?;
    headers.set("apns-priority", "10")?;
    headers.set("apns-id", input.apns_id)?;
    headers.set("content-type", "application/json")?;
    let payload = durable_alert_payload(&input);
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&payload)));
    Ok(Request::new_with_init(url.as_str(), &init)?)
}

pub(crate) async fn send_durable_alert(request: Request) -> apns::DeliveryOutcome {
    match Fetch::Request(request).send().await {
        Ok(response) => durable_response_outcome(response).await,
        Err(_) => durable_transport_unknown(),
    }
}

async fn durable_response_outcome(mut response: Response) -> apns::DeliveryOutcome {
    let status = response.status_code();
    let request_id = response.headers().get("apns-id").ok().flatten();
    let retry_after = response.headers().get("retry-after").ok().flatten();
    if (200..300).contains(&status) {
        return classify_durable_apns_response(
            status,
            &[],
            false,
            request_id.as_deref(),
            retry_after.as_deref(),
        );
    }
    let body = read_durable_error_body_prefix(&mut response).await;
    classify_durable_apns_response(
        status,
        &body.bytes,
        body.truncated,
        request_id.as_deref(),
        retry_after.as_deref(),
    )
}

async fn read_durable_error_body_prefix(response: &mut Response) -> DurableBodyPrefix {
    match response.body() {
        ResponseBody::Empty => {
            return DurableBodyPrefix {
                bytes: Vec::new(),
                truncated: false,
            };
        }
        ResponseBody::Body(bytes) => {
            return DurableBodyPrefix {
                bytes: bytes[..bytes.len().min(DURABLE_APNS_ERROR_BODY_LIMIT)].to_vec(),
                truncated: bytes.len() > DURABLE_APNS_ERROR_BODY_LIMIT,
            };
        }
        ResponseBody::Stream(_) => {}
    }
    let mut stream = match response.stream() {
        Ok(stream) => stream,
        Err(_) => {
            return DurableBodyPrefix {
                bytes: Vec::new(),
                truncated: true,
            };
        }
    };
    let mut bytes = Vec::with_capacity(DURABLE_APNS_ERROR_BODY_LIMIT);
    let mut truncated = false;
    while bytes.len() < DURABLE_APNS_ERROR_BODY_LIMIT {
        match stream.try_next().await {
            Ok(Some(chunk)) => {
                let remaining = DURABLE_APNS_ERROR_BODY_LIMIT - bytes.len();
                if chunk.len() >= remaining {
                    bytes.extend_from_slice(&chunk[..remaining]);
                    truncated = true;
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => {
                truncated = true;
                break;
            }
        }
    }
    DurableBodyPrefix { bytes, truncated }
}

fn classify_durable_apns_response(
    status: u16,
    body: &[u8],
    body_truncated: bool,
    request_id: Option<&str>,
    retry_after: Option<&str>,
) -> apns::DeliveryOutcome {
    let request_id = durable_safe_request_id(request_id);
    if (200..300).contains(&status) {
        return apns::DeliveryOutcome {
            class: apns::DeliveryClass::Accepted,
            code: "apns_accepted",
            request_id,
            retry_after_seconds: None,
            http_status: Some(status),
        };
    }
    let body_truncated = body_truncated || body.len() > DURABLE_APNS_ERROR_BODY_LIMIT;
    let body = &body[..body.len().min(DURABLE_APNS_ERROR_BODY_LIMIT)];
    let parsed = if body_truncated {
        None
    } else {
        serde_json::from_slice::<DurableAppleErrorBody>(body).ok()
    };
    let reason = parsed.as_ref().map(|body| body.reason.as_str());
    let (class, code) = match (status, reason) {
        (400, Some("BadDeviceToken")) => {
            (apns::DeliveryClass::InvalidToken, "apns_bad_device_token")
        }
        (400, Some("DeviceTokenNotForTopic")) => (
            apns::DeliveryClass::InvalidToken,
            "apns_device_token_not_for_topic",
        ),
        (410, Some("Unregistered")) => (apns::DeliveryClass::InvalidToken, "apns_unregistered"),
        (408 | 425, _) => (apns::DeliveryClass::Retryable, "apns_timeout"),
        (429, Some("TooManyRequests")) => {
            (apns::DeliveryClass::Retryable, "apns_too_many_requests")
        }
        (429, Some("TooManyProviderTokenUpdates")) => (
            apns::DeliveryClass::Retryable,
            "apns_too_many_provider_token_updates",
        ),
        (429, _) => (apns::DeliveryClass::Retryable, "apns_rate_limited"),
        (500..=599, Some("Shutdown")) => (apns::DeliveryClass::Retryable, "apns_shutdown"),
        (500..=599, Some("ServiceUnavailable")) => {
            (apns::DeliveryClass::Retryable, "apns_service_unavailable")
        }
        (500..=599, _) => (apns::DeliveryClass::Retryable, "apns_server_error"),
        (_, Some("IdleTimeout")) => (apns::DeliveryClass::Retryable, "apns_idle_timeout"),
        (_, Some(reason)) => match durable_permanent_reason_code(reason) {
            Some(code) => (apns::DeliveryClass::Permanent, code),
            None => (apns::DeliveryClass::Unknown, "apns_unknown_response"),
        },
        (_, None) => (apns::DeliveryClass::Unknown, "apns_unknown_response"),
    };
    let retry_after_seconds = (class == apns::DeliveryClass::Retryable).then(|| {
        retry_after
            .map(str::trim)
            .and_then(|value| value.parse::<u64>().ok())
            .map(|seconds| seconds.clamp(1, DURABLE_APNS_MAX_RETRY_AFTER_SECONDS))
            .unwrap_or_else(|| match status {
                429 => 60,
                503 => 30,
                _ => DURABLE_APNS_DEFAULT_RETRY_SECONDS,
            })
    });
    apns::DeliveryOutcome {
        class,
        code,
        request_id,
        retry_after_seconds,
        http_status: Some(status),
    }
}

fn durable_permanent_reason_code(reason: &str) -> Option<&'static str> {
    match reason {
        "BadCollapseId" => Some("apns_bad_collapse_id"),
        "BadExpirationDate" => Some("apns_bad_expiration_date"),
        "BadMessageId" => Some("apns_bad_message_id"),
        "BadPriority" => Some("apns_bad_priority"),
        "BadTopic" => Some("apns_bad_topic"),
        "DuplicateHeaders" => Some("apns_duplicate_headers"),
        "InvalidPushType" => Some("apns_invalid_push_type"),
        "MissingDeviceToken" => Some("apns_missing_device_token"),
        "MissingTopic" => Some("apns_missing_topic"),
        "PayloadEmpty" => Some("apns_payload_empty"),
        "TopicDisallowed" => Some("apns_topic_disallowed"),
        "BadCertificate" => Some("apns_bad_certificate"),
        "BadCertificateEnvironment" => Some("apns_bad_certificate_environment"),
        "ExpiredProviderToken" => Some("apns_expired_provider_token"),
        "Forbidden" => Some("apns_forbidden"),
        "InvalidProviderToken" => Some("apns_invalid_provider_token"),
        "MissingProviderToken" => Some("apns_missing_provider_token"),
        "BadPath" => Some("apns_bad_path"),
        "MethodNotAllowed" => Some("apns_method_not_allowed"),
        "PayloadTooLarge" => Some("apns_payload_too_large"),
        _ => None,
    }
}

fn durable_safe_request_id(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 128
                && value.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')
                })
        })
        .map(str::to_owned)
}

fn durable_transport_unknown() -> apns::DeliveryOutcome {
    apns::DeliveryOutcome {
        class: apns::DeliveryClass::Unknown,
        code: "apns_delivery_unknown",
        request_id: None,
        retry_after_seconds: None,
        http_status: None,
    }
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

    let mut apns_results = Vec::new();
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
                            Ok(outcome) => {
                                if outcome.invalidates_token() {
                                    invalidate_apns_token(db, &token).await?;
                                }
                                apns_results.push(outcome);
                            }
                            Err(error) => {
                                apns_results.push(apns::DeliveryOutcome::from_local_error(&error));
                            }
                        }
                    }
                }
                Err(error) => {
                    apns_results.resize(
                        tokens.len(),
                        apns::DeliveryOutcome::from_local_error(&error),
                    );
                }
            }
        }
    }

    // Keep the existing development phone polling loop alive if APNs is
    // configured but no physical device token was available or delivery failed.
    if !inbox
        && !apns_results
            .iter()
            .any(|result| result.class == apns::DeliveryClass::Accepted)
    {
        enqueue_push(db, &request).await?;
    }
    Ok(PushDelivery)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_alert_payload_supports_visible_and_background_refresh_delivery() {
        let input = DurableAlertInput {
            provider_authorization: "provider-secret",
            token: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            apns_id: "00000000-0000-0000-0000-000000000001",
            title: "Answer ready",
            body: "Open Knock Knock to refresh",
            session_id: Some("ses_voice"),
            event_id: "evt_answer",
        };
        let encoded = durable_alert_payload(&input);
        let payload: serde_json::Value = serde_json::from_str(&encoded).unwrap();

        assert_eq!(payload["aps"]["content-available"], serde_json::json!(1));
        assert_eq!(payload["aps"]["sound"], serde_json::json!("default"));
        assert_eq!(
            payload["aps"]["alert"],
            serde_json::json!({
                "title": "Answer ready",
                "body": "Open Knock Knock to refresh",
            })
        );
        assert_eq!(payload["session_id"], serde_json::json!("ses_voice"));
        assert_eq!(payload["event_id"], serde_json::json!("evt_answer"));
        assert!(!encoded.contains(input.provider_authorization));
        assert!(!encoded.contains(input.token));
        assert!(!encoded.contains(input.apns_id));
    }

    #[test]
    fn invalid_apns_registration_is_retired_by_exact_token() {
        assert_eq!(
            invalidate_apns_token_sql(),
            "UPDATE devices SET push_token = NULL, updated_at = ? WHERE push_token = ?"
        );
    }

    #[test]
    fn session_event_intent_is_commit_gated_and_replay_safe() {
        let push = session_event_push_insert_sql();
        let intent = session_event_notification_insert_sql();
        assert!(push.contains("EXISTS (SELECT 1 FROM events"));
        assert!(intent.contains("EXISTS (SELECT 1 FROM events"));
        assert!(intent.contains("'session.event.notification'"));
        assert!(intent.contains("ON CONFLICT(topic, idempotency_key) DO NOTHING"));
        assert_eq!(
            session_event_notification_key("usr_1", "evt_1"),
            session_event_notification_key("usr_1", "evt_1")
        );
        assert_ne!(
            session_event_notification_key("usr_1", "evt_1"),
            session_event_notification_key("usr_1", "evt_2")
        );
    }

    #[test]
    fn stable_delivery_id_produces_one_stable_apns_id() {
        let delivery = "apns_delivery_00112233445566778899aabbccddeeff";
        let first = stable_apns_id(delivery).unwrap();
        let second = stable_apns_id(delivery).unwrap();
        assert_eq!(first, "00112233-4455-6677-8899-aabbccddeeff");
        assert_eq!(first, second);
        assert!(stable_apns_id("out_unstable").is_err());
    }

    #[test]
    fn durable_alert_classifies_410_as_invalid_token() {
        let outcome = classify_durable_apns_response(
            410,
            br#"{"reason":"Unregistered"}"#,
            false,
            Some("00112233-4455-6677-8899-aabbccddeeff"),
            None,
        );
        assert_eq!(outcome.class, apns::DeliveryClass::InvalidToken);
        assert!(outcome.invalidates_token());
        assert_eq!(outcome.code, "apns_unregistered");
    }

    #[test]
    fn durable_alert_classifies_429_and_5xx_as_retryable() {
        let limited = classify_durable_apns_response(
            429,
            br#"{"reason":"TooManyRequests"}"#,
            false,
            None,
            Some("120"),
        );
        assert_eq!(limited.class, apns::DeliveryClass::Retryable);
        assert_eq!(limited.retry_after_seconds, Some(120));

        for status in [500, 502, 503] {
            let outcome = classify_durable_apns_response(status, b"{}", false, None, None);
            assert_eq!(outcome.class, apns::DeliveryClass::Retryable);
        }
    }

    #[test]
    fn durable_alert_unknown_is_not_reclassified_as_retryable() {
        let response =
            classify_durable_apns_response(400, b"<html>unknown</html>", false, None, None);
        assert_eq!(response.class, apns::DeliveryClass::Unknown);
        assert_eq!(
            durable_transport_unknown().class,
            apns::DeliveryClass::Unknown
        );
        assert_eq!(response.retry_after_seconds, None);
    }
}
