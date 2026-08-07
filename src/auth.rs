use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use bcrypt::{hash, verify, DEFAULT_COST};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use worker::{D1Database, Env, Request};

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
    let raw = u32::from_le_bytes(random_bytes::<4>()?);
    Ok(format!("{:06}", raw % 1_000_000))
}

fn env_value(env: &Env, name: &str) -> Option<String> {
    env.var(name)
        .ok()
        .map(|value| value.to_string())
        .or_else(|| env.secret(name).ok().map(|value| value.to_string()))
}

pub fn config_value(env: &Env, name: &str, default: &str) -> String {
    env_value(env, name).unwrap_or_else(|| default.to_string())
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
    if !matches!(node_env.as_str(), "development" | "test" | "production") {
        return Err(ApiError::new(
            500,
            "configuration_error",
            "NODE_ENV must be development, test, or production",
        ));
    }
    if node_env != "production" {
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

    for (name, value) in [
        ("APNS_KEY", config_value(env, "APNS_KEY", "")),
        ("APNS_KEY_ID", config_value(env, "APNS_KEY_ID", "")),
        ("APNS_TEAM_ID", config_value(env, "APNS_TEAM_ID", "")),
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

pub fn jwt_secret(env: &Env) -> ApiResult<String> {
    let value = config_value(env, "JWT_SECRET", "dev-change-me");
    let node_env = config_value(env, "NODE_ENV", "development");
    if node_env.trim().eq_ignore_ascii_case("production")
        && (value == "dev-change-me" || value.len() < 32)
    {
        return Err(ApiError::new(
            500,
            "configuration_error",
            "JWT_SECRET must be a random value of at least 32 characters in production",
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
    Ok(AgentPrincipal {
        agent_id: row.id,
        user_id: row.user_id,
    })
}

pub async fn require_user_or_agent(
    request: &Request,
    env: &Env,
    db: &D1Database,
) -> ApiResult<(Option<UserPrincipal>, Option<AgentPrincipal>)> {
    if let Some(value) = authorization_header(request)? {
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
        assert_eq!(pairing_code.len(), 6);
        assert!(pairing_code
            .chars()
            .all(|character| character.is_ascii_digit()));
        assert_ne!(api_key, mint_api_key().unwrap());
    }
}
