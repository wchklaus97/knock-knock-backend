use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use bcrypt::{hash, verify, DEFAULT_COST};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use worker::{D1Database, Env, Fetch, Headers, Method, Request, RequestInit};

use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::{AgentPrincipal, AgentRow, UserPrincipal};

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Deserialize)]
struct JwtClaims {
    sub: Option<String>,
    typ: Option<String>,
    exp: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct JwtHeader {
    alg: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IdRow {
    #[serde(rename = "id")]
    _id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SupabaseUser {
    pub id: String,
    pub email: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SupabaseSession {
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
    pub expires_in: Option<i64>,
    pub user: Option<SupabaseUser>,
}

#[derive(Debug, Clone, Deserialize)]
struct UserIdentityRow {
    id: String,
    email: String,
    supabase_user_id: Option<String>,
}

pub fn sha256_hex(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn hash_api_key(api_key: &str) -> String {
    sha256_hex(api_key)
}

pub fn hash_password(password: &str) -> ApiResult<String> {
    hash(password, DEFAULT_COST).map_err(|error| {
        ApiError::new(
            500,
            "password_error",
            format!("Unable to hash password: {error}"),
        )
    })
}

pub fn verify_password(password: &str, password_hash: &str) -> bool {
    verify(password, password_hash).unwrap_or(false)
}

pub fn random_bytes<const N: usize>() -> ApiResult<[u8; N]> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes)
        .map_err(|error| ApiError::new(500, "random_error", error.to_string()))?;
    Ok(bytes)
}

pub fn random_token(prefix: &str, bytes: usize) -> ApiResult<String> {
    let mut raw = vec![0_u8; bytes];
    getrandom::fill(&mut raw)
        .map_err(|error| ApiError::new(500, "random_error", error.to_string()))?;
    Ok(format!("{prefix}{}", URL_SAFE_NO_PAD.encode(raw)))
}

pub fn new_id(prefix: &str) -> ApiResult<String> {
    let random = random_bytes::<16>()?;
    Ok(format!(
        "{prefix}_{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    ))
}

pub fn mint_api_key() -> ApiResult<String> {
    random_token("vak_", 24)
}

pub fn mint_refresh_token() -> ApiResult<String> {
    random_token("vbr_", 32)
}

pub fn mint_pairing_code() -> ApiResult<String> {
    // Pairing is an unauthenticated claim boundary. Use a high-entropy URL
    // safe token instead of a six-digit value that can be guessed over the
    // code's lifetime. The `pair_` prefix also makes accidental API-key or
    // refresh-token reuse easier to spot in diagnostics.
    random_token("pair_", 12)
}

fn env_value(env: &Env, name: &str) -> Option<String> {
    env.var(name)
        .ok()
        .map(|value| value.to_string())
        .or_else(|| env.secret(name).ok().map(|value| value.to_string()))
}

/// Read a value only from Wrangler's secret store. Provider credentials and
/// signing material must not silently fall back to public Worker vars.
pub fn secret_value(env: &Env, name: &str) -> Option<String> {
    env.secret(name).ok().map(|value| value.to_string())
}

pub fn config_value(env: &Env, name: &str, default: &str) -> String {
    env_value(env, name).unwrap_or_else(|| default.to_string())
}

pub fn supabase_auth_enabled(env: &Env) -> bool {
    config_value(env, "AUTH_PROVIDER", "legacy")
        .trim()
        .eq_ignore_ascii_case("supabase")
}

fn supabase_url(env: &Env) -> ApiResult<String> {
    let value = config_value(env, "SUPABASE_URL", "");
    let value = value.trim().trim_end_matches('/').to_string();
    if value.is_empty() || !value.starts_with("https://") {
        return Err(ApiError::new(
            500,
            "configuration_error",
            "SUPABASE_URL must be an HTTPS URL",
        ));
    }
    Ok(value)
}

fn supabase_api_key(env: &Env) -> ApiResult<String> {
    let value = env_value(env, "SUPABASE_PUBLISHABLE_KEY")
        .or_else(|| env_value(env, "SUPABASE_ANON_KEY"))
        .unwrap_or_default();
    if value.trim().is_empty() {
        return Err(ApiError::new(
            500,
            "configuration_error",
            "SUPABASE_PUBLISHABLE_KEY must be configured",
        ));
    }
    Ok(value)
}

fn validate_supabase_configuration(env: &Env) -> ApiResult<()> {
    let _ = supabase_url(env)?;
    let _ = supabase_api_key(env)?;
    Ok(())
}

/// Validate settings that are unsafe to infer in a production Worker.
///
/// Local development deliberately uses permissive defaults. Production must
/// opt in explicitly so deploying the local Wrangler file cannot silently
/// fall back to a demo JWT secret, wildcard CORS, or development push inbox.
pub fn runtime_configuration(env: &Env) -> ApiResult<()> {
    let node_env = config_value(env, "NODE_ENV", "development")
        .trim()
        .to_ascii_lowercase();
    if !matches!(
        node_env.as_str(),
        "development" | "test" | "staging" | "production"
    ) {
        return Err(ApiError::new(
            500,
            "configuration_error",
            "NODE_ENV must be development, test, staging, or production",
        ));
    }
    let auth_provider = config_value(env, "AUTH_PROVIDER", "legacy")
        .trim()
        .to_ascii_lowercase();
    if !matches!(auth_provider.as_str(), "legacy" | "supabase") {
        return Err(ApiError::new(
            500,
            "configuration_error",
            "AUTH_PROVIDER must be legacy or supabase",
        ));
    }
    if !matches!(node_env.as_str(), "development" | "test") && auth_provider != "supabase" {
        return Err(ApiError::new(
            500,
            "configuration_error",
            "AUTH_PROVIDER must be supabase outside development",
        ));
    }
    if auth_provider == "supabase" {
        validate_supabase_configuration(env)?;
    }
    if matches!(node_env.as_str(), "development" | "test") {
        return Ok(());
    }

    if node_env == "staging" {
        let cors_origin = config_value(env, "CORS_ORIGIN", "");
        if cors_origin.trim().is_empty()
            || cors_origin.trim() == "*"
            || cors_origin.trim().starts_with("REPLACE_")
        {
            return Err(ApiError::new(
                500,
                "configuration_error",
                "CORS_ORIGIN must be an explicit staging origin",
            ));
        }
        let service_version = config_value(env, "SERVICE_VERSION", "");
        if service_version.trim().is_empty() || service_version.trim().starts_with("REPLACE_") {
            return Err(ApiError::new(
                500,
                "configuration_error",
                "SERVICE_VERSION must be supplied for staging",
            ));
        }
        let push_mode = config_value(env, "PUSH_MODE", "")
            .trim()
            .to_ascii_lowercase();
        if !matches!(push_mode.as_str(), "dev" | "apns" | "both") {
            return Err(ApiError::new(
                500,
                "configuration_error",
                "PUSH_MODE must be dev, apns, or both in staging",
            ));
        }
        let apns_production = config_value(env, "APNS_PRODUCTION", "false")
            .trim()
            .to_ascii_lowercase();
        if apns_production != "false" {
            return Err(ApiError::new(
                500,
                "configuration_error",
                "APNS_PRODUCTION must be false in staging",
            ));
        }
        return Ok(());
    }

    // jwt_secret rejects the development fallback and short values.
    let _ = jwt_secret(env)?;

    let cors_origin = config_value(env, "CORS_ORIGIN", "");
    if cors_origin.trim().is_empty()
        || cors_origin.trim() == "*"
        || cors_origin.trim().starts_with("REPLACE_")
    {
        return Err(ApiError::new(
            500,
            "configuration_error",
            "CORS_ORIGIN must be an explicit production origin",
        ));
    }

    let push_mode = config_value(env, "PUSH_MODE", "")
        .trim()
        .to_ascii_lowercase();
    if !matches!(push_mode.as_str(), "apns" | "both") {
        return Err(ApiError::new(
            500,
            "configuration_error",
            "PUSH_MODE must be apns or both in production",
        ));
    }

    let service_version = config_value(env, "SERVICE_VERSION", "");
    if service_version.trim().is_empty() || service_version.trim().starts_with("REPLACE_") {
        return Err(ApiError::new(
            500,
            "configuration_error",
            "SERVICE_VERSION must be supplied for production",
        ));
    }

    if config_value(env, "VOICE_MODEL_ENABLED", "false") == "true" {
        let model_url = config_value(env, "VOICE_MODEL_URL", "");
        let manifest = config_value(env, "VOICE_MODEL_MANIFEST_JSON", "");
        if !model_url.starts_with("https://")
            || model_url.starts_with("https://REPLACE_")
            || manifest.trim().is_empty()
            || manifest.trim().starts_with("REPLACE_")
            || serde_json::from_str::<Value>(&manifest).is_err()
        {
            return Err(ApiError::new(
                500,
                "configuration_error",
                "VOICE_MODEL_URL and VOICE_MODEL_MANIFEST_JSON must be configured for model rollout",
            ));
        }
    }

    for (name, value) in [
        (
            "APNS_KEY",
            secret_value(env, "APNS_KEY").unwrap_or_default(),
        ),
        (
            "APNS_KEY_ID",
            secret_value(env, "APNS_KEY_ID").unwrap_or_default(),
        ),
        (
            "APNS_TEAM_ID",
            secret_value(env, "APNS_TEAM_ID").unwrap_or_default(),
        ),
        ("APNS_BUNDLE_ID", config_value(env, "APNS_BUNDLE_ID", "")),
    ] {
        if value.trim().is_empty() || value.trim().starts_with("REPLACE_") {
            return Err(ApiError::new(
                500,
                "configuration_error",
                format!("{name} must be configured for production APNs"),
            ));
        }
    }

    let apns_production = config_value(env, "APNS_PRODUCTION", "")
        .trim()
        .to_ascii_lowercase();
    if !matches!(apns_production.as_str(), "true" | "false") {
        return Err(ApiError::new(
            500,
            "configuration_error",
            "APNS_PRODUCTION must be true or false in production",
        ));
    }

    Ok(())
}

fn supabase_error_message(raw: &str) -> String {
    let value = serde_json::from_str::<Value>(raw).unwrap_or(Value::Null);
    for key in ["message", "msg", "error_description", "error"] {
        if let Some(message) = value.get(key).and_then(Value::as_str) {
            if !message.trim().is_empty() {
                return message.to_string();
            }
        }
    }
    if raw.trim().is_empty() {
        "Supabase Auth request failed".to_string()
    } else {
        raw.to_string()
    }
}

async fn supabase_request(
    env: &Env,
    method: Method,
    path: &str,
    body: Option<Value>,
    bearer_token: Option<&str>,
) -> ApiResult<Value> {
    let url = format!("{}{path}", supabase_url(env)?);
    let headers = Headers::new();
    headers.set("apikey", &supabase_api_key(env)?)?;
    headers.set("accept", "application/json")?;
    if body.is_some() {
        headers.set("content-type", "application/json")?;
    }
    if let Some(token) = bearer_token {
        headers.set("authorization", &format!("Bearer {token}"))?;
    }

    let mut init = RequestInit::new();
    init.with_method(method).with_headers(headers);
    if let Some(body) = body {
        init.with_body(Some(worker::wasm_bindgen::JsValue::from_str(
            &body.to_string(),
        )));
    }
    let request = Request::new_with_init(&url, &init)?;
    let mut response = Fetch::Request(request).send().await?;
    let status = response.status_code();
    let raw = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        let mapped_status = match status {
            400 | 422 => 400,
            401 | 403 => 401,
            409 => 409,
            status if status >= 500 => 502,
            status => status,
        };
        return Err(ApiError::new(
            mapped_status,
            "supabase_auth_error",
            supabase_error_message(&raw),
        ));
    }
    if raw.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&raw).map_err(|error| {
        ApiError::new(
            502,
            "supabase_auth_error",
            format!("Supabase returned invalid JSON: {error}"),
        )
    })
}

fn parse_supabase_session(value: Value) -> ApiResult<SupabaseSession> {
    serde_json::from_value(value).map_err(|error| {
        ApiError::new(
            502,
            "supabase_auth_error",
            format!("Supabase returned an invalid session: {error}"),
        )
    })
}

pub async fn supabase_sign_up(
    env: &Env,
    email: &str,
    password: &str,
) -> ApiResult<SupabaseSession> {
    parse_supabase_session(
        supabase_request(
            env,
            Method::Post,
            "/auth/v1/signup",
            Some(json!({ "email": email, "password": password })),
            None,
        )
        .await?,
    )
}

pub async fn supabase_sign_in(
    env: &Env,
    email: &str,
    password: &str,
) -> ApiResult<SupabaseSession> {
    parse_supabase_session(
        supabase_request(
            env,
            Method::Post,
            "/auth/v1/token?grant_type=password",
            Some(json!({ "email": email, "password": password })),
            None,
        )
        .await?,
    )
}

pub async fn supabase_refresh(env: &Env, refresh_token: &str) -> ApiResult<SupabaseSession> {
    parse_supabase_session(
        supabase_request(
            env,
            Method::Post,
            "/auth/v1/token?grant_type=refresh_token",
            Some(json!({ "refresh_token": refresh_token })),
            None,
        )
        .await?,
    )
}

pub async fn supabase_get_user(env: &Env, access_token: &str) -> ApiResult<SupabaseUser> {
    serde_json::from_value(
        supabase_request(env, Method::Get, "/auth/v1/user", None, Some(access_token)).await?,
    )
    .map_err(|error| {
        ApiError::new(
            502,
            "supabase_auth_error",
            format!("Supabase returned an invalid user: {error}"),
        )
    })
}

pub async fn supabase_logout(env: &Env, access_token: &str) -> ApiResult<()> {
    let _ = supabase_request(
        env,
        Method::Post,
        "/auth/v1/logout",
        None,
        Some(access_token),
    )
    .await?;
    Ok(())
}

pub fn supabase_auth_response(user_id: &str, session: &SupabaseSession) -> ApiResult<Value> {
    let token = session
        .access_token
        .clone()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ApiError::new(
                409,
                "email_confirmation_required",
                "Please confirm your email before signing in",
            )
        })?;
    let refresh_token = session
        .refresh_token
        .clone()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ApiError::new(
                502,
                "supabase_auth_error",
                "Supabase did not return a refresh token",
            )
        })?;
    Ok(json!({
        "user_id": user_id,
        "token": token,
        "refresh_token": refresh_token,
        "expires_in": session.expires_in.unwrap_or(3600),
    }))
}

pub async fn ensure_supabase_user(
    db: &D1Database,
    supabase_user_id: &str,
    email: &str,
) -> ApiResult<UserPrincipal> {
    let by_external_id: Option<UserIdentityRow> = db::first(
        db,
        "SELECT id, email, supabase_user_id FROM users WHERE supabase_user_id = ?",
        vec![db::text(supabase_user_id)],
    )
    .await?;
    if let Some(row) = by_external_id {
        if row.email != email {
            db::run(
                db,
                "UPDATE users SET email = ? WHERE id = ?",
                vec![db::text(email), db::text(&row.id)],
            )
            .await?;
        }
        return Ok(UserPrincipal { user_id: row.id });
    }

    let by_email: Option<UserIdentityRow> = db::first(
        db,
        "SELECT id, email, supabase_user_id FROM users WHERE email = ?",
        vec![db::text(email)],
    )
    .await?;
    if let Some(row) = by_email {
        if row
            .supabase_user_id
            .as_deref()
            .is_some_and(|value| value != supabase_user_id)
        {
            return Err(ApiError::conflict(
                "Email is already linked to another user",
            ));
        }
        db::run(
            db,
            "UPDATE users SET supabase_user_id = ? WHERE id = ?",
            vec![db::text(supabase_user_id), db::text(&row.id)],
        )
        .await?;
        return Ok(UserPrincipal { user_id: row.id });
    }

    let user_id = new_id("usr")?;
    db::run(
        db,
        "INSERT INTO users (id, email, password_hash, supabase_user_id, created_at) VALUES (?, ?, ?, ?, ?)",
        vec![
            db::text(&user_id),
            db::text(email),
            db::text("supabase-managed"),
            db::text(supabase_user_id),
            db::text(&db::now_iso()),
        ],
    )
    .await?;
    Ok(UserPrincipal { user_id })
}

pub fn bearer_token(request: &Request) -> ApiResult<Option<String>> {
    let value = authorization_header(request)?;
    Ok(value.and_then(|value| {
        value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    }))
}

async fn resolve_supabase_user(
    request: &Request,
    env: &Env,
    db: &D1Database,
) -> ApiResult<UserPrincipal> {
    let token =
        bearer_token(request)?.ok_or_else(|| ApiError::unauthorized("Missing Bearer token"))?;
    let remote = supabase_get_user(env, &token).await?;
    let email = remote
        .email
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ApiError::unauthorized("Supabase user email is missing"))?;
    ensure_supabase_user(db, &remote.id, &email).await
}

pub fn jwt_secret(env: &Env) -> ApiResult<String> {
    let node_env = config_value(env, "NODE_ENV", "development")
        .trim()
        .to_ascii_lowercase();
    let local_environment = matches!(node_env.as_str(), "development" | "test");
    let value = if local_environment {
        config_value(env, "JWT_SECRET", "dev-change-me")
    } else {
        // Staging must not silently inherit the development signing key. A
        // missing secret is a startup/request configuration failure rather
        // than an authentication fallback.
        secret_value(env, "JWT_SECRET").unwrap_or_default()
    };
    if !local_environment && value.len() < 32 {
        return Err(ApiError::new(
            500,
            "configuration_error",
            "JWT_SECRET must be a random value of at least 32 characters outside development",
        ));
    }
    Ok(value)
}

fn sign_hmac(secret: &str, message: &str) -> ApiResult<String> {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .map_err(|_| ApiError::new(500, "configuration_error", "Invalid JWT secret"))?;
    mac.update(message.as_bytes());
    Ok(URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes()))
}

pub fn sign_user_token(env: &Env, user_id: &str, email: &str) -> ApiResult<String> {
    let now = worker::Date::now().as_millis() as i64 / 1000;
    let expires_in = config_value(env, "ACCESS_TOKEN_TTL_SEC", "900")
        .parse::<i64>()
        .unwrap_or(900)
        .clamp(60, 86_400);
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "email": email,
            "typ": "user",
            "sub": user_id,
            "iat": now,
            "exp": now + expires_in,
        }))
        .map_err(|error| ApiError::new(500, "jwt_error", error.to_string()))?,
    );
    let message = format!("{header}.{payload}");
    Ok(format!(
        "{message}.{}",
        sign_hmac(&jwt_secret(env)?, &message)?
    ))
}

pub fn verify_user_token(env: &Env, token: &str) -> ApiResult<UserPrincipal> {
    let mut parts = token.split('.');
    let header = parts
        .next()
        .ok_or_else(|| ApiError::unauthorized("Invalid token"))?;
    let payload = parts
        .next()
        .ok_or_else(|| ApiError::unauthorized("Invalid token"))?;
    let signature = parts
        .next()
        .ok_or_else(|| ApiError::unauthorized("Invalid token"))?;
    if parts.next().is_some() {
        return Err(ApiError::unauthorized("Invalid token"));
    }
    let message = format!("{header}.{payload}");
    let expected = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| ApiError::unauthorized("Invalid token"))?;
    let mut mac = HmacSha256::new_from_slice(jwt_secret(env)?.as_bytes())
        .map_err(|_| ApiError::unauthorized("Invalid token"))?;
    mac.update(message.as_bytes());
    mac.verify_slice(&expected)
        .map_err(|_| ApiError::unauthorized("Invalid token"))?;
    let jwt_header: JwtHeader = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(header)
            .map_err(|_| ApiError::unauthorized("Invalid token"))?,
    )
    .map_err(|_| ApiError::unauthorized("Invalid token"))?;
    if jwt_header.alg.as_deref() != Some("HS256") {
        return Err(ApiError::unauthorized("Invalid token"));
    }
    let claims: JwtClaims = serde_json::from_slice(
        &URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| ApiError::unauthorized("Invalid token"))?,
    )
    .map_err(|_| ApiError::unauthorized("Invalid token"))?;
    let now = worker::Date::now().as_millis() as i64 / 1000;
    if claims.typ.as_deref() != Some("user")
        || claims.sub.as_deref().unwrap_or_default().is_empty()
        || claims.exp.unwrap_or(0) <= now
    {
        return Err(ApiError::unauthorized("Invalid token"));
    }
    Ok(UserPrincipal {
        user_id: claims.sub.unwrap_or_default(),
    })
}

fn authorization_header(request: &Request) -> ApiResult<Option<String>> {
    Ok(request.headers().get("authorization")?)
}

fn agent_key_header(request: &Request) -> ApiResult<Option<String>> {
    Ok(request.headers().get("x-agent-key")?)
}

pub async fn require_user(
    request: &Request,
    env: &Env,
    db: &D1Database,
) -> ApiResult<UserPrincipal> {
    let user = if supabase_auth_enabled(env) {
        resolve_supabase_user(request, env, db).await?
    } else {
        let value = authorization_header(request)?
            .ok_or_else(|| ApiError::unauthorized("Missing Bearer token"))?;
        let token = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| ApiError::unauthorized("Missing Bearer token"))?;
        let user = verify_user_token(env, token)?;
        let existing: Option<IdRow> = db::first(
            db,
            "SELECT id FROM users WHERE id = ?",
            vec![db::text(&user.user_id)],
        )
        .await?;
        if existing.is_none() {
            return Err(ApiError::unauthorized("User not found"));
        }
        user
    };
    let device_id = request.headers().get("x-device-id")?;
    crate::rate_limits::enforce_authenticated(
        db,
        request.path().as_str(),
        &format!("user:{}", user.user_id),
        device_id.as_deref(),
    )
    .await?;
    Ok(user)
}

pub async fn require_agent(request: &Request, db: &D1Database) -> ApiResult<AgentPrincipal> {
    let key =
        agent_key_header(request)?.ok_or_else(|| ApiError::unauthorized("Missing X-Agent-Key"))?;
    let row: Option<AgentRow> = db::first(
        db,
        "SELECT id, user_id, label, host_label, created_at FROM agents WHERE api_key_hash = ?",
        vec![db::text(&hash_api_key(&key))],
    )
    .await?;
    let row = row.ok_or_else(|| ApiError::unauthorized("Invalid agent key"))?;
    let principal = AgentPrincipal {
        agent_id: row.id,
        user_id: row.user_id,
    };
    crate::rate_limits::enforce_authenticated(
        db,
        request.path().as_str(),
        &format!("agent:{}", principal.agent_id),
        None,
    )
    .await?;
    Ok(principal)
}

pub async fn require_user_or_agent(
    request: &Request,
    env: &Env,
    db: &D1Database,
) -> ApiResult<(Option<UserPrincipal>, Option<AgentPrincipal>)> {
    if supabase_auth_enabled(env) {
        if bearer_token(request)?.is_some() {
            if let Ok(user) = resolve_supabase_user(request, env, db).await {
                let device_id = request.headers().get("x-device-id")?;
                crate::rate_limits::enforce_authenticated(
                    db,
                    request.path().as_str(),
                    &format!("user:{}", user.user_id),
                    device_id.as_deref(),
                )
                .await?;
                return Ok((Some(user), None));
            }
        }
    } else if let Some(value) = authorization_header(request)? {
        if let Some(token) = value
            .strip_prefix("Bearer ")
            .or_else(|| value.strip_prefix("bearer "))
            .map(str::trim)
        {
            if !token.is_empty() {
                if let Ok(user) = verify_user_token(env, token) {
                    let existing: Option<IdRow> = db::first(
                        db,
                        "SELECT id FROM users WHERE id = ?",
                        vec![db::text(&user.user_id)],
                    )
                    .await?;
                    if existing.is_some() {
                        let device_id = request.headers().get("x-device-id")?;
                        crate::rate_limits::enforce_authenticated(
                            db,
                            request.path().as_str(),
                            &format!("user:{}", user.user_id),
                            device_id.as_deref(),
                        )
                        .await?;
                        return Ok((Some(user), None));
                    }
                }
            }
        }
    }
    Ok((None, Some(require_agent(request, db).await?)))
}

pub fn auth_response(user_id: &str, token: String, refresh_token: String, env: &Env) -> Value {
    let expires_in = config_value(env, "ACCESS_TOKEN_TTL_SEC", "900")
        .parse::<i64>()
        .unwrap_or(900)
        .clamp(60, 86_400);
    json!({
        "user_id": user_id,
        "token": token,
        "refresh_token": refresh_token,
        "expires_in": expires_in,
    })
}

pub async fn issue_user_auth(
    db: &D1Database,
    env: &Env,
    user_id: &str,
    email: &str,
    user_agent: Option<&str>,
    ip_address: Option<&str>,
) -> ApiResult<Value> {
    let token = sign_user_token(env, user_id, email)?;
    let refresh_token = mint_refresh_token()?;
    let now = db::now_iso();
    let ttl = config_value(env, "REFRESH_TOKEN_TTL_SEC", "2592000")
        .parse::<i64>()
        .unwrap_or(2_592_000)
        .clamp(3_600, 31_536_000);
    db::run(
        db,
        "INSERT INTO refresh_tokens (id, user_id, token_hash, expires_at, revoked_at, last_used_at, user_agent, ip_address, created_at) VALUES (?, ?, ?, ?, NULL, NULL, ?, ?, ?)",
        vec![
            db::text(&new_id("rft")?),
            db::text(user_id),
            db::text(&sha256_hex(&refresh_token)),
            db::text(&db::add_seconds_iso(ttl)),
            db::optional_text(user_agent),
            db::optional_text(ip_address),
            db::text(&now),
        ],
    )
    .await?;
    Ok(auth_response(user_id, token, refresh_token, env))
}

pub async fn create_agent_for_user(
    db: &D1Database,
    user_id: &str,
    label: &str,
    host_label: Option<&str>,
) -> ApiResult<Value> {
    let agent_id = new_id("agt")?;
    let api_key = mint_api_key()?;
    let created_at = db::now_iso();
    db::run(
        db,
        "INSERT INTO agents (id, user_id, label, host_label, api_key_hash, created_at) VALUES (?, ?, ?, ?, ?, ?)",
        vec![
            db::text(&agent_id),
            db::text(user_id),
            db::text(label),
            db::optional_text(host_label),
            db::text(&hash_api_key(&api_key)),
            db::text(&created_at),
        ],
    )
    .await?;
    Ok(json!({
        "agent": {
            "agent_id": agent_id,
            "user_id": user_id,
            "label": label,
            "host_label": host_label,
            "created_at": created_at,
        },
        "api_key": api_key,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_is_stable_and_hex_encoded() {
        assert_eq!(
            sha256_hex("knock-knock"),
            "4af706022fbaae08abb4685d8e70acfb0c4f25143420bd43e67572729cc11abc"
        );
    }

    #[test]
    fn password_hash_verifies_only_the_original_password() {
        let password_hash = hash_password("correct horse battery staple").unwrap();
        assert_ne!(password_hash, "correct horse battery staple");
        assert!(verify_password(
            "correct horse battery staple",
            &password_hash
        ));
        assert!(!verify_password("wrong password", &password_hash));
        assert!(!verify_password("anything", "not-a-bcrypt-hash"));
    }

    #[test]
    fn minted_tokens_have_scoped_prefixes_and_non_empty_entropy() {
        let api_key = mint_api_key().unwrap();
        let refresh_token = mint_refresh_token().unwrap();
        let pairing_code = mint_pairing_code().unwrap();

        assert!(api_key.starts_with("vak_"));
        assert!(refresh_token.starts_with("vbr_"));
        assert!(pairing_code.starts_with("pair_"));
        assert!(pairing_code.len() >= 20);
        assert!(pairing_code
            .chars()
            .all(|character| character.is_ascii_alphanumeric()
                || character == '_'
                || character == '-'));
        assert_ne!(api_key, mint_api_key().unwrap());
    }
}
