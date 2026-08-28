use serde::Deserialize;
use sha2::{Digest, Sha256};
use worker::{D1Database, Method};

use crate::db;
use crate::error::{ApiError, ApiResult};

#[derive(Debug, Deserialize)]
struct RateLimitRow {
    request_count: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateLimitPolicy {
    ReadOnly,
    Persistent,
}

impl RateLimitPolicy {
    pub const fn is_persistent(self) -> bool {
        match self {
            Self::ReadOnly => false,
            Self::Persistent => true,
        }
    }
}

/// High-frequency Ask polls authenticate and validate their listener fence,
/// but must not create D1 write traffic when there is no work to claim.
/// Every other route remains on the persistent abuse-control path.
pub fn route_policy(method: &Method, path: &str) -> RateLimitPolicy {
    let normalized_path = path.trim_end_matches('/');
    if method == &Method::Get && normalized_path == "/v1/agents/me/asks" {
        RateLimitPolicy::ReadOnly
    } else {
        RateLimitPolicy::Persistent
    }
}

fn digest(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn category(path: &str) -> (&'static str, i64) {
    if path.starts_with("/v1/auth/") {
        ("auth", 20)
    } else if path.starts_with("/v1/pairing/") {
        // Pairing claims are intentionally unauthenticated. Keep a tighter
        // edge bucket in addition to the high-entropy token requirement.
        ("pairing", 10)
    } else if path.starts_with("/v1/phone/commands") || path.contains("/asks") {
        ("command", 60)
    } else if path == "/v1/phone/events" {
        ("sse", 30)
    } else if path == "/v1/phone/devices" {
        ("device", 60)
    } else if path.starts_with("/v1/phone/models") || path.starts_with("/v1/phone/model-fallback") {
        ("model", 30)
    } else if path.starts_with("/v1/phone/retrievals/") {
        ("download", 60)
    } else if path.starts_with("/v1/sessions/") || path.starts_with("/v1/actions/") {
        ("agent_event", 120)
    } else {
        ("api", 240)
    }
}

async fn enforce_bucket(db: &D1Database, kind: &str, limit: i64, identity: &str) -> ApiResult<()> {
    let bucket_key = format!("{}:{}", kind, digest(identity));
    let now = db::now_iso();
    let expires_at = db::add_seconds_iso(60);

    // A single SQLite upsert resets an expired window or increments the
    // current one. The follow-up read is only used to decide whether to reject
    // the request; no credential or raw IP is stored in D1.
    db::run(
        db,
        "INSERT INTO rate_limit_buckets (bucket_key, window_started_at, request_count, expires_at) VALUES (?, ?, 1, ?) ON CONFLICT(bucket_key) DO UPDATE SET window_started_at = CASE WHEN rate_limit_buckets.expires_at <= ? THEN excluded.window_started_at ELSE rate_limit_buckets.window_started_at END, request_count = CASE WHEN rate_limit_buckets.expires_at <= ? THEN 1 ELSE rate_limit_buckets.request_count + 1 END, expires_at = CASE WHEN rate_limit_buckets.expires_at <= ? THEN excluded.expires_at ELSE rate_limit_buckets.expires_at END",
        vec![
            db::text(&bucket_key),
            db::text(&now),
            db::text(&expires_at),
            db::text(&now),
            db::text(&now),
            db::text(&now),
        ],
    )
    .await?;

    let row: RateLimitRow = db::first(
        db,
        "SELECT request_count FROM rate_limit_buckets WHERE bucket_key = ?",
        vec![db::text(&bucket_key)],
    )
    .await?
    .ok_or_else(|| ApiError::new(500, "rate_limit_error", "Rate limit state is unavailable"))?;

    if row.request_count > limit {
        return Err(ApiError::rate_limited(60));
    }
    Ok(())
}

/// Applies the pre-authentication bucket. It intentionally receives only an
/// edge-provided network identity; bearer tokens are not stable principals.
pub async fn enforce(db: &D1Database, path: &str, identity: &str) -> ApiResult<()> {
    let (kind, limit) = category(path);
    enforce_bucket(db, kind, limit, identity).await
}

pub async fn enforce_with_policy(
    db: &D1Database,
    path: &str,
    identity: &str,
    policy: RateLimitPolicy,
) -> ApiResult<()> {
    if !policy.is_persistent() {
        return Ok(());
    }
    enforce(db, path, identity).await
}

/// Applies verified user/agent limits and an optional per-device bucket. This
/// is called after auth has resolved the principal, so token rotation cannot
/// bypass the account quota.
pub async fn enforce_authenticated(
    db: &D1Database,
    path: &str,
    principal: &str,
    device_id: Option<&str>,
) -> ApiResult<()> {
    let (kind, limit) = category(path);
    enforce_bucket(db, kind, limit, principal).await?;
    if let Some(device_id) = device_id.map(str::trim).filter(|value| !value.is_empty()) {
        enforce_bucket(db, "device", 60, &format!("{principal}:{device_id}")).await?;
    }
    Ok(())
}

pub async fn enforce_authenticated_with_policy(
    db: &D1Database,
    path: &str,
    principal: &str,
    device_id: Option<&str>,
    policy: RateLimitPolicy,
) -> ApiResult<()> {
    if !policy.is_persistent() {
        return Ok(());
    }
    enforce_authenticated(db, path, principal, device_id).await
}

#[cfg(test)]
mod tests {
    use super::{category, route_policy, RateLimitPolicy};
    use worker::Method;

    #[test]
    fn rate_limit_categories_cover_reconnect_and_model_paths() {
        assert_eq!(category("/v1/auth/login"), ("auth", 20));
        assert_eq!(category("/v1/pairing/claim"), ("pairing", 10));
        assert_eq!(category("/v1/phone/commands"), ("command", 60));
        assert_eq!(category("/v1/phone/agents/agt_1/asks"), ("command", 60));
        assert_eq!(category("/v1/phone/events"), ("sse", 30));
        assert_eq!(category("/v1/phone/devices"), ("device", 60));
        assert_eq!(category("/v1/phone/memories"), ("api", 240));
        assert_eq!(category("/v1/phone/models/gemma"), ("model", 30));
        assert_eq!(
            category("/v1/phone/retrievals/ret_1/download"),
            ("download", 60)
        );
        assert_eq!(
            category("/v1/actions/action_1/result"),
            ("agent_event", 120)
        );
    }

    #[test]
    fn only_high_frequency_ask_get_uses_read_only_rate_policy() {
        assert_eq!(
            route_policy(&Method::Get, "/v1/agents/me/asks"),
            RateLimitPolicy::ReadOnly
        );
        assert_eq!(
            route_policy(&Method::Get, "/v1/agents/me/asks/"),
            RateLimitPolicy::ReadOnly
        );
        assert_eq!(
            route_policy(&Method::Post, "/v1/phone/agents/agt_1/asks"),
            RateLimitPolicy::Persistent
        );
        assert_eq!(
            route_policy(&Method::Post, "/v1/agents/me/asks/claim"),
            RateLimitPolicy::Persistent
        );
        assert_eq!(
            route_policy(&Method::Post, "/v1/pairing/claim"),
            RateLimitPolicy::Persistent
        );
        assert_eq!(
            route_policy(&Method::Post, "/v1/agents/me/listener/heartbeat"),
            RateLimitPolicy::Persistent
        );
        assert_eq!(
            route_policy(&Method::Delete, "/v1/agents/me/listener"),
            RateLimitPolicy::Persistent
        );
    }
}
