use serde_json::{Map, Value};
use worker::D1Database;

use crate::auth::new_id;
use crate::db;
use crate::error::ApiResult;
use crate::models::AuditRow;
use crate::pagination;

/// Records compatibility-path audit data with the project's established
/// best-effort semantics: an audit write failure never rewinds an already
/// durable business mutation. Callers must keep metadata explicitly redacted.
pub async fn record_audit(
    db: &D1Database,
    action: &str,
    user_id: Option<&str>,
    agent_id: Option<&str>,
    session_id: Option<&str>,
    metadata: Value,
) {
    let metadata = if metadata.is_object() {
        metadata
    } else {
        Value::Object(Map::new())
    };
    let _ = db::run(
        db,
        "INSERT INTO audit_logs (id, user_id, agent_id, session_id, action, metadata_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        vec![
            db::text(&new_id("aud").unwrap_or_else(|_| "aud_fallback".into())),
            db::optional_text(user_id),
            db::optional_text(agent_id),
            db::optional_text(session_id),
            db::text(action),
            db::text(&metadata.to_string()),
            db::text(&db::now_iso()),
        ],
    )
    .await;
}

pub async fn list_audit_for_session(
    db: &D1Database,
    user_id: &str,
    session_id: &str,
    before: Option<&str>,
    limit: i32,
) -> ApiResult<Value> {
    let cursor = pagination::decode(before)?;
    let safe_limit = limit.clamp(1, 200) as i64;
    let (sql, values) = if let Some(cursor) = cursor {
        (
            "SELECT id, action, session_id, agent_id, metadata_json, created_at FROM audit_logs WHERE user_id = ? AND session_id = ? AND (created_at > ? OR (created_at = ? AND id > ?)) ORDER BY created_at ASC, id ASC LIMIT ?",
            vec![
                db::text(user_id),
                db::text(session_id),
                db::text(&cursor.sort_key),
                db::text(&cursor.sort_key),
                db::text(&cursor.id),
                db::number(safe_limit + 1),
            ],
        )
    } else {
        (
            "SELECT id, action, session_id, agent_id, metadata_json, created_at FROM audit_logs WHERE user_id = ? AND session_id = ? ORDER BY created_at ASC, id ASC LIMIT ?",
            vec![
                db::text(user_id),
                db::text(session_id),
                db::number(safe_limit + 1),
            ],
        )
    };
    let rows: Vec<AuditRow> = db::all(db, sql, values).await?;
    let has_more = rows.len() as i64 > safe_limit;
    let rows = rows
        .into_iter()
        .take(safe_limit as usize)
        .collect::<Vec<_>>();
    let next_cursor = has_more
        .then(|| {
            rows.last()
                .map(|row| pagination::encode(&row.created_at, &row.id))
        })
        .flatten();
    Ok(serde_json::json!({
        "entries": rows
            .into_iter()
            .map(|row| {
                json_object(
                    row.id,
                    row.action,
                    row.session_id,
                    row.agent_id,
                    row.metadata_json,
                    row.created_at,
                )
            })
            .collect::<Vec<_>>(),
        "next_cursor": next_cursor,
        "has_more": has_more,
    }))
}

fn json_object(
    id: String,
    action: String,
    session_id: Option<String>,
    agent_id: Option<String>,
    metadata_json: String,
    created_at: String,
) -> Value {
    let metadata =
        serde_json::from_str::<Value>(&metadata_json).unwrap_or_else(|_| Value::Object(Map::new()));
    serde_json::json!({
        "audit_id": id,
        "action": action,
        "session_id": session_id,
        "agent_id": agent_id,
        "metadata": metadata,
        "created_at": created_at,
    })
}
