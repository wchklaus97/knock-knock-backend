use base64::{
    engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD, Engine as _,
};
use futures_util::TryStreamExt;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::SigningKey;
use p256::pkcs8::DecodePrivateKey;
use serde_json::json;
use std::cell::RefCell;
use worker::{Fetch, Headers, Method, Request, RequestInit, Response, ResponseBody};

use crate::auth::{config_value, secret_value};
use crate::error::{ApiError, ApiResult};

const PROVIDER_TOKEN_REFRESH_SECONDS: i64 = 50 * 60;
const APNS_ERROR_BODY_LIMIT: usize = 1_024;
const MAX_RETRY_AFTER_SECONDS: u64 = 24 * 60 * 60;
const DEFAULT_TRANSIENT_BACKOFF_SECONDS: u64 = 5;

#[derive(Debug, Clone)]
struct CachedProviderToken {
    token: String,
    issued_at: i64,
    key_id: String,
    team_id: String,
}

thread_local! {
    // Cloudflare may reuse an isolate and its APNs HTTP/2 connection across
    // requests. Reuse the matching provider token in that isolate too: APNs
    // rejects a connection that sees newly signed tokens too frequently.
    static PROVIDER_TOKEN_CACHE: RefCell<Option<CachedProviderToken>> = const { RefCell::new(None) };
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum DeliveryClass {
    Accepted,
    InvalidToken,
    Permanent,
    Retryable,
    Unknown,
}

impl DeliveryClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::InvalidToken => "invalid_token",
            Self::Permanent => "permanent",
            Self::Retryable => "retryable",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub(crate) struct DeliveryOutcome {
    pub(crate) class: DeliveryClass,
    pub(crate) code: &'static str,
    pub(crate) request_id: Option<String>,
    pub(crate) retry_after_seconds: Option<u64>,
    pub(crate) http_status: Option<u16>,
}

impl DeliveryOutcome {
    pub(crate) fn invalidates_token(&self) -> bool {
        self.class == DeliveryClass::InvalidToken
    }

    pub(crate) fn from_local_error(error: &ApiError) -> Self {
        let class = if error.retryable {
            DeliveryClass::Retryable
        } else {
            DeliveryClass::Permanent
        };
        let code = match error.code.as_str() {
            "apns_configuration_error" => "apns_configuration_error",
            "apns_error" => "apns_request_error",
            _ => "apns_local_error",
        };
        let retry_after_seconds = (class == DeliveryClass::Retryable).then(|| {
            error
                .retry_after
                .map(|seconds| seconds.clamp(1, MAX_RETRY_AFTER_SECONDS))
                .unwrap_or_else(|| default_retry_after(error.status))
        });
        Self {
            class,
            code,
            request_id: safe_request_id(error.request_id.as_deref()),
            retry_after_seconds,
            http_status: Some(error.status),
        }
    }

    fn into_api_error(self) -> ApiError {
        let status = match self.class {
            DeliveryClass::Accepted => 200,
            DeliveryClass::InvalidToken => 410,
            DeliveryClass::Permanent => self
                .http_status
                .filter(|status| (400..500).contains(status))
                .unwrap_or(400),
            DeliveryClass::Retryable => self
                .http_status
                .filter(|status| matches!(status, 408 | 425 | 429) || *status >= 500)
                .unwrap_or(503),
            DeliveryClass::Unknown => 502,
        };
        let message = match self.class {
            DeliveryClass::Accepted => "APNs accepted the notification",
            DeliveryClass::InvalidToken => "The APNs device token is no longer valid",
            DeliveryClass::Permanent => "APNs permanently rejected the notification",
            DeliveryClass::Retryable => "APNs temporarily rejected the notification",
            DeliveryClass::Unknown => "APNs returned an unrecognized response",
        };
        let mut error = ApiError::new(status, self.code, message);
        error.retryable = self.class == DeliveryClass::Retryable;
        error.retry_after = self.retry_after_seconds;
        error.request_id = self.request_id;
        error
    }
}

#[derive(Debug)]
struct BodyPrefix {
    bytes: Vec<u8>,
    truncated: bool,
}

#[derive(serde::Deserialize)]
struct AppleErrorBody {
    reason: String,
}

#[derive(Debug)]
pub(crate) enum CommandWakeFailure {
    Known(ApiError),
    Unknown(ApiError),
}

impl CommandWakeFailure {
    pub(crate) fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown(_))
    }

    pub(crate) fn into_error(self) -> ApiError {
        match self {
            Self::Known(error) | Self::Unknown(error) => error,
        }
    }
}

pub(crate) const CANONICAL_APNS_BUNDLE_ID: &str = "hk.knockknock.app";

pub(crate) fn bundle_id(env: &worker::Env) -> String {
    // Staging and Production must provide this identity explicitly. An empty
    // default keeps readiness fail-closed instead of making a missing binding
    // appear canonical.
    config_value(env, "APNS_BUNDLE_ID", "")
}

pub(crate) fn is_canonical_bundle_id(value: &str) -> bool {
    value == CANONICAL_APNS_BUNDLE_ID
}

pub(crate) fn bundle_id_is_canonical(env: &worker::Env) -> bool {
    is_canonical_bundle_id(&bundle_id(env))
}

#[cfg(test)]
mod bundle_identity_tests {
    use super::{is_canonical_bundle_id, CANONICAL_APNS_BUNDLE_ID};

    #[test]
    fn canonical_bundle_identity_is_exact() {
        assert!(is_canonical_bundle_id(CANONICAL_APNS_BUNDLE_ID));
        assert!(!is_canonical_bundle_id(""));
        assert!(!is_canonical_bundle_id(" hk.knockknock.app"));
        assert!(!is_canonical_bundle_id("hk.knockknock.app "));
        assert!(!is_canonical_bundle_id("hk.knockknock.wrong"));
    }
}

pub fn is_ready(env: &worker::Env) -> bool {
    let key = secret_value(env, "APNS_KEY").unwrap_or_default();
    let key_is_valid = decode_private_key(&key)
        .ok()
        .and_then(|der| SigningKey::from_pkcs8_der(&der).ok())
        .is_some();
    key_is_valid
        && !secret_value(env, "APNS_KEY_ID")
            .unwrap_or_default()
            .trim()
            .is_empty()
        && !secret_value(env, "APNS_TEAM_ID")
            .unwrap_or_default()
            .trim()
            .is_empty()
        && bundle_id_is_canonical(env)
}

pub fn looks_like_token(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn decode_private_key(value: &str) -> ApiResult<Vec<u8>> {
    let encoded = value
        .lines()
        .filter(|line| !line.trim_start().starts_with("-----"))
        .map(str::trim)
        .collect::<String>();
    STANDARD
        .decode(encoded)
        .map_err(|error| ApiError::new(500, "apns_configuration_error", error.to_string()))
}

fn signing_key(env: &worker::Env) -> ApiResult<SigningKey> {
    let key = secret_value(env, "APNS_KEY").unwrap_or_default();
    let der = decode_private_key(&key)?;
    SigningKey::from_pkcs8_der(&der)
        .map_err(|error| ApiError::new(500, "apns_configuration_error", error.to_string()))
}

fn signed_token(env: &worker::Env, now: i64, key_id: &str, team_id: &str) -> ApiResult<String> {
    let header = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({ "alg": "ES256", "kid": key_id }))
            .map_err(|error| ApiError::new(500, "apns_configuration_error", error.to_string()))?,
    );
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "iss": team_id,
            "iat": now,
        }))
        .map_err(|error| ApiError::new(500, "apns_configuration_error", error.to_string()))?,
    );
    let message = format!("{header}.{payload}");
    let signature: p256::ecdsa::Signature = signing_key(env)?.sign(message.as_bytes());
    Ok(format!(
        "{message}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    ))
}

fn reusable_provider_token(
    cached: &CachedProviderToken,
    now: i64,
    key_id: &str,
    team_id: &str,
) -> Option<String> {
    let age = now - cached.issued_at;
    ((0..PROVIDER_TOKEN_REFRESH_SECONDS).contains(&age)
        && cached.key_id == key_id
        && cached.team_id == team_id)
        .then(|| cached.token.clone())
}

pub(crate) fn provider_authorization(env: &worker::Env) -> ApiResult<String> {
    let now = worker::Date::now().as_millis() as i64 / 1000;
    let key_id = secret_value(env, "APNS_KEY_ID").unwrap_or_default();
    let team_id = secret_value(env, "APNS_TEAM_ID").unwrap_or_default();
    if let Some(token) = PROVIDER_TOKEN_CACHE.with(|cache| {
        cache
            .borrow()
            .as_ref()
            .and_then(|cached| reusable_provider_token(cached, now, &key_id, &team_id))
    }) {
        return Ok(token);
    }

    let token = signed_token(env, now, &key_id, &team_id)?;
    PROVIDER_TOKEN_CACHE.with(|cache| {
        *cache.borrow_mut() = Some(CachedProviderToken {
            token: token.clone(),
            issued_at: now,
            key_id,
            team_id,
        });
    });
    Ok(token)
}

pub async fn send_alert(
    env: &worker::Env,
    provider_authorization: &str,
    token: &str,
    title: &str,
    body: &str,
    session_id: Option<&str>,
    _voice_script: Option<&str>,
) -> ApiResult<DeliveryOutcome> {
    let payload = json!({
        "aps": {
            "alert": { "title": title, "body": body },
            "sound": "default",
        },
        "session_id": session_id,
    })
    .to_string();
    send_payload(env, provider_authorization, token, &payload, "alert", None).await
}

fn command_wakeup_payload() -> String {
    json!({
        "aps": { "content-available": 1 },
        "wake_hint": "command",
    })
    .to_string()
}

pub async fn send_command_wakeup(env: &worker::Env, token: &str) -> Result<(), CommandWakeFailure> {
    let payload = command_wakeup_payload();
    let provider_authorization = provider_authorization(env).map_err(CommandWakeFailure::Known)?;
    let request = payload_request(
        env,
        &provider_authorization,
        token,
        &payload,
        "background",
        Some("5"),
    )
    .map_err(CommandWakeFailure::Known)?;
    let outcome = match Fetch::Request(request).send().await {
        Ok(response) => response_outcome(response).await,
        Err(error) => classify_transport_error(&format!("{error:?}")),
    };
    match outcome.class {
        DeliveryClass::Accepted => Ok(()),
        DeliveryClass::Unknown => Err(CommandWakeFailure::Unknown(outcome.into_api_error())),
        DeliveryClass::InvalidToken | DeliveryClass::Permanent | DeliveryClass::Retryable => {
            Err(CommandWakeFailure::Known(outcome.into_api_error()))
        }
    }
}

async fn send_payload(
    env: &worker::Env,
    provider_authorization: &str,
    token: &str,
    payload: &str,
    push_type: &str,
    priority: Option<&str>,
) -> ApiResult<DeliveryOutcome> {
    let request = payload_request(
        env,
        provider_authorization,
        token,
        payload,
        push_type,
        priority,
    )?;
    Ok(match Fetch::Request(request).send().await {
        Ok(response) => response_outcome(response).await,
        Err(error) => classify_transport_error(&format!("{error:?}")),
    })
}

fn payload_request(
    env: &worker::Env,
    provider_authorization: &str,
    token: &str,
    payload: &str,
    push_type: &str,
    priority: Option<&str>,
) -> ApiResult<Request> {
    if !looks_like_token(token) {
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
    let url = worker::Url::parse(&format!("https://{host}/3/device/{token}"))
        .map_err(|error| ApiError::new(500, "apns_error", error.to_string()))?;
    let headers = Headers::new();
    headers.set("authorization", &format!("bearer {provider_authorization}"))?;
    headers.set("apns-topic", &bundle_id(env))?;
    headers.set("apns-push-type", push_type)?;
    if let Some(priority) = priority {
        headers.set("apns-priority", priority)?;
    }
    headers.set("content-type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from_str(payload)));
    Ok(Request::new_with_init(url.as_str(), &init)?)
}

async fn response_outcome(mut response: Response) -> DeliveryOutcome {
    let status = response.status_code();
    let request_id = response.headers().get("apns-id").ok().flatten();
    let retry_after = response.headers().get("retry-after").ok().flatten();
    if (200..300).contains(&status) {
        return classify_apns_response(
            status,
            &[],
            false,
            request_id.as_deref(),
            retry_after.as_deref(),
        );
    }

    let body = read_error_body_prefix(&mut response).await;
    classify_apns_response(
        status,
        &body.bytes,
        body.truncated,
        request_id.as_deref(),
        retry_after.as_deref(),
    )
}

async fn read_error_body_prefix(response: &mut Response) -> BodyPrefix {
    match response.body() {
        ResponseBody::Empty => {
            return BodyPrefix {
                bytes: Vec::new(),
                truncated: false,
            };
        }
        ResponseBody::Body(bytes) => {
            return BodyPrefix {
                bytes: bytes[..bytes.len().min(APNS_ERROR_BODY_LIMIT)].to_vec(),
                truncated: bytes.len() > APNS_ERROR_BODY_LIMIT,
            };
        }
        ResponseBody::Stream(_) => {}
    }

    let mut stream = match response.stream() {
        Ok(stream) => stream,
        Err(_) => {
            return BodyPrefix {
                bytes: Vec::new(),
                truncated: true,
            };
        }
    };
    let mut bytes = Vec::with_capacity(APNS_ERROR_BODY_LIMIT);
    let mut truncated = false;
    while bytes.len() < APNS_ERROR_BODY_LIMIT {
        match stream.try_next().await {
            Ok(Some(chunk)) => {
                let remaining = APNS_ERROR_BODY_LIMIT - bytes.len();
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
    BodyPrefix { bytes, truncated }
}

fn classify_apns_response(
    status: u16,
    body: &[u8],
    body_truncated: bool,
    request_id: Option<&str>,
    retry_after: Option<&str>,
) -> DeliveryOutcome {
    let request_id = safe_request_id(request_id);
    if (200..300).contains(&status) {
        return DeliveryOutcome {
            class: DeliveryClass::Accepted,
            code: "apns_accepted",
            request_id,
            retry_after_seconds: None,
            http_status: Some(status),
        };
    }

    let body_truncated = body_truncated || body.len() > APNS_ERROR_BODY_LIMIT;
    let body = &body[..body.len().min(APNS_ERROR_BODY_LIMIT)];
    let parsed = if body_truncated {
        None
    } else {
        serde_json::from_slice::<AppleErrorBody>(body).ok()
    };
    let reason = parsed.as_ref().map(|body| body.reason.as_str());
    let (class, code) = match (status, reason) {
        (400, Some("BadDeviceToken")) => (DeliveryClass::InvalidToken, "apns_bad_device_token"),
        (400, Some("DeviceTokenNotForTopic")) => (
            DeliveryClass::InvalidToken,
            "apns_device_token_not_for_topic",
        ),
        (410, Some("Unregistered")) => (DeliveryClass::InvalidToken, "apns_unregistered"),
        (408, _) => (DeliveryClass::Retryable, "apns_timeout"),
        (429, Some("TooManyRequests")) => (DeliveryClass::Retryable, "apns_too_many_requests"),
        (429, Some("TooManyProviderTokenUpdates")) => (
            DeliveryClass::Retryable,
            "apns_too_many_provider_token_updates",
        ),
        (429, _) => (DeliveryClass::Retryable, "apns_rate_limited"),
        (500, Some("InternalServerError")) => {
            (DeliveryClass::Retryable, "apns_internal_server_error")
        }
        (500, _) => (DeliveryClass::Retryable, "apns_server_error"),
        (503, Some("Shutdown")) => (DeliveryClass::Retryable, "apns_shutdown"),
        (503, Some("ServiceUnavailable")) => (DeliveryClass::Retryable, "apns_service_unavailable"),
        (503, _) => (DeliveryClass::Retryable, "apns_service_unavailable"),
        (_, Some("IdleTimeout")) => (DeliveryClass::Retryable, "apns_idle_timeout"),
        (_, Some(reason)) => match permanent_reason_code(reason) {
            Some(code) => (DeliveryClass::Permanent, code),
            None => (DeliveryClass::Unknown, "apns_unknown_response"),
        },
        (_, None) => (DeliveryClass::Unknown, "apns_unknown_response"),
    };
    let retry_after_seconds = (class == DeliveryClass::Retryable)
        .then(|| parse_retry_after(retry_after).unwrap_or_else(|| default_retry_after(status)));
    DeliveryOutcome {
        class,
        code,
        request_id,
        retry_after_seconds,
        http_status: Some(status),
    }
}

fn permanent_reason_code(reason: &str) -> Option<&'static str> {
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

fn classify_transport_error(details: &str) -> DeliveryOutcome {
    let timeout = details.to_ascii_lowercase();
    if timeout.contains("timeout") || timeout.contains("timed out") {
        DeliveryOutcome {
            class: DeliveryClass::Retryable,
            code: "apns_timeout",
            request_id: None,
            retry_after_seconds: Some(DEFAULT_TRANSIENT_BACKOFF_SECONDS),
            http_status: Some(408),
        }
    } else {
        DeliveryOutcome {
            class: DeliveryClass::Unknown,
            code: "apns_delivery_unknown",
            request_id: None,
            retry_after_seconds: None,
            http_status: None,
        }
    }
}

fn safe_request_id(value: Option<&str>) -> Option<String> {
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

fn parse_retry_after(value: Option<&str>) -> Option<u64> {
    value
        .map(str::trim)
        .and_then(|value| value.parse::<u64>().ok())
        .map(|seconds| seconds.clamp(1, MAX_RETRY_AFTER_SECONDS))
}

fn default_retry_after(status: u16) -> u64 {
    match status {
        429 => 60,
        503 => 30,
        _ => DEFAULT_TRANSIENT_BACKOFF_SECONDS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_wakeup_payload_is_a_data_free_rest_refresh_hint() {
        let payload: serde_json::Value = serde_json::from_str(&command_wakeup_payload()).unwrap();

        assert_eq!(
            payload,
            json!({
                "aps": { "content-available": 1 },
                "wake_hint": "command",
            })
        );
        let encoded = payload.to_string();
        for sensitive_key in [
            "args",
            "result",
            "error",
            "title",
            "body",
            "session_id",
            "user_id",
            "command_id",
        ] {
            assert!(!encoded.contains(sensitive_key));
        }
    }

    #[test]
    fn command_wakeup_failure_preserves_unknown_delivery_certainty() {
        let known = CommandWakeFailure::Known(ApiError::new(503, "known", "retry"));
        let unknown = CommandWakeFailure::Unknown(ApiError::new(502, "unknown", "reconcile"));

        assert!(!known.is_unknown());
        assert!(known.into_error().retryable);
        assert!(unknown.is_unknown());
        assert_eq!(unknown.into_error().code, "unknown");
    }

    #[test]
    fn status_200_is_accepted_without_reading_a_body() {
        let outcome = classify_apns_response(
            200,
            b"<html>ignored upstream body</html>",
            false,
            Some("apns-accepted-1"),
            None,
        );

        assert_eq!(outcome.class, DeliveryClass::Accepted);
        assert_eq!(outcome.code, "apns_accepted");
        assert_eq!(outcome.request_id.as_deref(), Some("apns-accepted-1"));
        assert_eq!(outcome.retry_after_seconds, None);
    }

    #[test]
    fn status_400_invalid_token_reasons_are_permanent() {
        for (reason, code) in [
            ("BadDeviceToken", "apns_bad_device_token"),
            ("DeviceTokenNotForTopic", "apns_device_token_not_for_topic"),
        ] {
            let body = json!({ "reason": reason }).to_string();
            let outcome = classify_apns_response(400, body.as_bytes(), false, None, None);

            assert_eq!(outcome.class, DeliveryClass::InvalidToken);
            assert_eq!(outcome.code, code);
            assert!(outcome.invalidates_token());
            assert!(!outcome.into_api_error().retryable);
        }
    }

    #[test]
    fn status_410_unregistered_invalidates_the_token_without_retry() {
        let outcome = classify_apns_response(
            410,
            br#"{"reason":"Unregistered"}"#,
            false,
            Some("apns-unregistered-1"),
            None,
        );

        assert_eq!(outcome.class, DeliveryClass::InvalidToken);
        assert!(outcome.invalidates_token());
        assert!(!outcome.into_api_error().retryable);
    }

    #[test]
    fn status_429_preserves_bounded_retry_after() {
        let outcome = classify_apns_response(
            429,
            br#"{"reason":"TooManyRequests"}"#,
            false,
            Some("apns-rate-limit-1"),
            Some("120"),
        );

        assert_eq!(outcome.class, DeliveryClass::Retryable);
        assert_eq!(outcome.code, "apns_too_many_requests");
        assert_eq!(outcome.retry_after_seconds, Some(120));
        let error = outcome.into_api_error();
        assert!(error.retryable);
        assert_eq!(error.retry_after, Some(120));
    }

    #[test]
    fn status_500_uses_safe_retry_backoff() {
        let outcome = classify_apns_response(
            500,
            br#"{"reason":"InternalServerError"}"#,
            false,
            None,
            None,
        );

        assert_eq!(outcome.class, DeliveryClass::Retryable);
        assert_eq!(outcome.code, "apns_internal_server_error");
        assert_eq!(outcome.retry_after_seconds, Some(5));
    }

    #[test]
    fn status_503_uses_safe_backoff_for_invalid_retry_after() {
        let outcome = classify_apns_response(
            503,
            br#"{"reason":"ServiceUnavailable"}"#,
            false,
            None,
            Some("not-a-safe-delay"),
        );

        assert_eq!(outcome.class, DeliveryClass::Retryable);
        assert_eq!(outcome.code, "apns_service_unavailable");
        assert_eq!(outcome.retry_after_seconds, Some(30));
    }

    #[test]
    fn oversized_html_is_unknown_and_never_reaches_the_error() {
        let html = format!("<html>{}</html>", "private upstream text".repeat(100));
        assert!(html.len() > APNS_ERROR_BODY_LIMIT);
        let outcome =
            classify_apns_response(400, html.as_bytes(), false, Some("bad\nrequest-id"), None);

        assert_eq!(outcome.class, DeliveryClass::Unknown);
        assert_eq!(outcome.code, "apns_unknown_response");
        assert_eq!(outcome.request_id, None);
        let error = outcome.into_api_error();
        assert!(!error.message.contains("private upstream text"));
        assert!(!error.message.contains("html"));
    }

    #[test]
    fn transport_timeout_is_retryable_without_exposing_transport_details() {
        let outcome = classify_transport_error("Network request timed out: private endpoint");

        assert_eq!(outcome.class, DeliveryClass::Retryable);
        assert_eq!(outcome.code, "apns_timeout");
        assert_eq!(outcome.retry_after_seconds, Some(5));
        assert!(!outcome
            .into_api_error()
            .message
            .contains("private endpoint"));
    }

    #[test]
    fn provider_token_is_reused_until_the_safe_refresh_window() {
        let cached = CachedProviderToken {
            token: "signed-token".into(),
            issued_at: 1_000,
            key_id: "KEY123".into(),
            team_id: "TEAM123".into(),
        };

        assert_eq!(
            reusable_provider_token(
                &cached,
                1_000 + PROVIDER_TOKEN_REFRESH_SECONDS - 1,
                "KEY123",
                "TEAM123",
            )
            .as_deref(),
            Some("signed-token")
        );
        assert!(reusable_provider_token(
            &cached,
            1_000 + PROVIDER_TOKEN_REFRESH_SECONDS,
            "KEY123",
            "TEAM123",
        )
        .is_none());
    }

    #[test]
    fn provider_token_is_not_reused_after_identity_or_clock_change() {
        let cached = CachedProviderToken {
            token: "signed-token".into(),
            issued_at: 1_000,
            key_id: "KEY123".into(),
            team_id: "TEAM123".into(),
        };

        assert!(reusable_provider_token(&cached, 999, "KEY123", "TEAM123").is_none());
        assert!(reusable_provider_token(&cached, 1_001, "KEY999", "TEAM123").is_none());
        assert!(reusable_provider_token(&cached, 1_001, "KEY123", "TEAM999").is_none());
    }
}
