use base64::{
    engine::general_purpose::STANDARD, engine::general_purpose::URL_SAFE_NO_PAD, Engine as _,
};
use p256::ecdsa::signature::Signer;
use p256::ecdsa::SigningKey;
use p256::pkcs8::DecodePrivateKey;
use serde_json::json;
use worker::{Fetch, Headers, Method, Request, RequestInit};

use crate::auth::{config_value, secret_value};
use crate::error::{ApiError, ApiResult};

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

fn signed_token(env: &worker::Env) -> ApiResult<String> {
    let now = worker::Date::now().as_millis() as i64 / 1000;
    let key_id = secret_value(env, "APNS_KEY_ID").unwrap_or_default();
    let header = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({ "alg": "ES256", "kid": key_id }))
            .map_err(|error| ApiError::new(500, "apns_configuration_error", error.to_string()))?,
    );
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "iss": secret_value(env, "APNS_TEAM_ID").unwrap_or_default(),
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

pub async fn send_alert(
    env: &worker::Env,
    token: &str,
    title: &str,
    body: &str,
    session_id: Option<&str>,
    voice_script: Option<&str>,
) -> ApiResult<()> {
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
    let payload = json!({
        "aps": {
            "alert": { "title": title, "body": body },
            "sound": "default",
        },
        "session_id": session_id,
        "voice_script": voice_script,
    })
    .to_string();
    let headers = Headers::new();
    headers.set("authorization", &format!("bearer {}", signed_token(env)?))?;
    headers.set(
        "apns-topic",
        &config_value(env, "APNS_BUNDLE_ID", "hk.knockknock.app"),
    )?;
    headers.set("apns-push-type", "alert")?;
    headers.set("content-type", "application/json")?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(worker::wasm_bindgen::JsValue::from_str(&payload)));
    let request = Request::new_with_init(url.as_str(), &init)?;
    let mut response = Fetch::Request(request).send().await?;
    if response.status_code() != 200 {
        let details = response
            .text()
            .await
            .unwrap_or_else(|_| "APNs rejected the request".into());
        return Err(ApiError::new(
            502,
            "apns_delivery_error",
            format!("APNs returned {}: {details}", response.status_code()),
        ));
    }
    Ok(())
}
