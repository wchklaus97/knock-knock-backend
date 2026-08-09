use serde::Deserialize;
use sha2::{Digest, Sha256};
use worker::D1Database;

use crate::db;
use crate::error::{ApiError, ApiResult};

#[derive(Debug, Deserialize)]
struct RateLimitRow {
    request_count: i64,
}

fn digest(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn category(path: &str) -> (&'static str, i64) {
    if path.starts_with("/v1/auth/") {
        ("auth", 20)
    } else if path.starts_with("/v1/phone/commands") {
        ("command", 60)
    } else if path.starts_with("/v1/sessions/") || path.starts_with("/v1/actions/") {
        ("agent_event", 120)
    } else {
        ("api", 240)
    }
}

pub async fn enforce(db: &D1Database, path: &str, identity: &str) -> ApiResult<()> {
    let (kind, limit) = category(path);
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
