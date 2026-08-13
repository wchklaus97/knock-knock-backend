use base64::{
    engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD, Engine as _,
};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::SigningKey;
use p256::pkcs8::DecodePrivateKey;
use serde_json::json;
use std::cell::RefCell;
use worker::{Fetch, Headers, Method, Request, RequestInit};

use crate::auth::{config_value, secret_value};
use crate::error::{ApiError, ApiResult};

const PROVIDER_TOKEN_REFRESH_SECONDS: i64 = 50 * 60;

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
        && !config_value(env, "APNS_BUNDLE_ID", "hk.knockknock.app")
            .trim()
            .is_empty()
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
) -> ApiResult<()> {
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
    let response = Fetch::Request(request).send().await.map_err(|error| {
        CommandWakeFailure::Unknown(ApiError::new(
            502,
            "apns_delivery_unknown",
            format!("APNs request outcome is unknown: {error:?}"),
        ))
    })?;
    require_success(response, true)
        .await
        .map_err(CommandWakeFailure::Known)
}

async fn send_payload(
    env: &worker::Env,
    provider_authorization: &str,
    token: &str,
    payload: &str,
    push_type: &str,
    priority: Option<&str>,
) -> ApiResult<()> {
    let request = payload_request(
        env,
        provider_authorization,
        token,
        payload,
        push_type,
        priority,
    )?;
    let response = Fetch::Request(request).send().await?;
    require_success(response, false).await
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
    headers.set(
        "apns-topic",
        &config_value(env, "APNS_BUNDLE_ID", "hk.knockknock.app"),
    )?;
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

async fn require_success(mut response: worker::Response, preserve_status: bool) -> ApiResult<()> {
    let status = response.status_code();
    if status != 200 {
        let details = response
            .text()
            .await
            .unwrap_or_else(|_| "APNs rejected the request".into());
        return Err(ApiError::new(
            if preserve_status { status } else { 502 },
            "apns_delivery_error",
            format!("APNs returned {status}: {details}"),
        ));
    }
    Ok(())
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
