use serde_json::{Map, Value};
use worker::D1Database;

use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::{RetrievalItemRow, SessionMessageRow, SessionRow};
use crate::pagination;
use crate::sessions;

fn parse_json(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|_| Value::Object(Map::new()))
}

fn message_value(row: SessionMessageRow) -> Value {
    serde_json::json!({
        "message_id": row.id,
        "session_id": row.session_id,
        "role": row.role,
        "content": row.content,
        "metadata": parse_json(&row.metadata_json),
        "command_id": row.command_id,
        "sequence": row.sequence,
        "created_at": row.created_at,
    })
}

fn retrieval_value(row: RetrievalItemRow) -> Value {
    // r2_key is intentionally not returned. It is an internal storage
    // reference; a future signed-download endpoint can expose a short-lived
    // URL after authorization and retention checks.
    serde_json::json!({
        "retrieval_id": row.id,
        "session_id": row.session_id,
        "message_id": row.message_id,
        "title": row.title,
        "url": row.url,
        "snippet": row.snippet,
        "score": row.score,
        "content_hash": row.content_hash,
        "created_at": row.created_at,
    })
}

pub fn session_summary(row: &SessionRow) -> Value {
    let available_actions = db::parse_json_array(row.available_actions_json.as_deref());
    serde_json::json!({
        "session_id": row.id,
        "agent_id": row.agent_id,
        "skill_id": row.skill_id,
        "state": row.state,
        "progress_status": row.progress_status,
        "progress_message": row.progress_message,
        "progress_percent": row.progress_percent,
        "title": row.title,
        "chat_id": row.chat_id,
        "summary_text": row.summary_text,
        "available_actions": available_actions,
        "expires_at": row.expires_at,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
    })
}

pub async fn list_sessions(
    db: &D1Database,
    user_id: &str,
    before: Option<&str>,
    limit: i32,
) -> ApiResult<Value> {
    let before = pagination::decode(before)?;
    let safe_limit = limit.clamp(1, 50) as i64;
    let rows: Vec<SessionRow> = if let Some(cursor) = before {
        db::all(
            db,
            "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, expires_at, created_at, updated_at FROM sessions WHERE user_id = ? AND deleted_at IS NULL AND (updated_at < ? OR (updated_at = ? AND id < ?)) ORDER BY updated_at DESC, id DESC LIMIT ?",
            vec![
                db::text(user_id),
                db::text(&cursor.sort_key),
                db::text(&cursor.sort_key),
                db::text(&cursor.id),
                db::number(safe_limit + 1),
            ],
        )
        .await?
    } else {
        db::all(
            db,
            "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, expires_at, created_at, updated_at FROM sessions WHERE user_id = ? AND deleted_at IS NULL ORDER BY updated_at DESC, id DESC LIMIT ?",
            vec![db::text(user_id), db::number(safe_limit + 1)],
        )
        .await?
    };
    let has_more = rows.len() as i64 > safe_limit;
    let mut result = Vec::new();
    for row in rows.into_iter().take(safe_limit as usize) {
        let fresh = sessions::reconcile_waiting_session(db, row).await?;
        result.push(session_summary(&fresh));
    }
    let next_cursor = result.last().and_then(|item| {
        Some(pagination::encode(
            item.get("updated_at")?.as_str()?,
            item.get("session_id")?.as_str()?,
        ))
    });
    Ok(serde_json::json!({
        "sessions": result,
        "next_cursor": has_more.then_some(next_cursor).flatten(),
        "has_more": has_more,
    }))
}

pub async fn list_messages(
    db: &D1Database,
    user_id: &str,
    session_id: &str,
    before: Option<&str>,
    limit: i32,
) -> ApiResult<Value> {
    let before = pagination::decode(before)?;
    let safe_limit = limit.clamp(1, 100) as i64;
    let now = db::now_iso();
    let rows: Vec<SessionMessageRow> = if let Some(cursor) = before {
        db::all(
            db,
            "SELECT id, user_id, session_id, role, content, metadata_json, command_id, sequence, retention_expires_at, created_at FROM session_messages WHERE user_id = ? AND session_id = ? AND (retention_expires_at IS NULL OR retention_expires_at > ?) AND (created_at < ? OR (created_at = ? AND id < ?)) ORDER BY created_at DESC, id DESC LIMIT ?",
            vec![
                db::text(user_id),
                db::text(session_id),
                db::text(&now),
                db::text(&cursor.sort_key),
                db::text(&cursor.sort_key),
                db::text(&cursor.id),
                db::number(safe_limit + 1),
            ],
        )
        .await?
    } else {
        db::all(
            db,
            "SELECT id, user_id, session_id, role, content, metadata_json, command_id, sequence, retention_expires_at, created_at FROM session_messages WHERE user_id = ? AND session_id = ? AND (retention_expires_at IS NULL OR retention_expires_at > ?) ORDER BY created_at DESC, id DESC LIMIT ?",
            vec![
                db::text(user_id),
                db::text(session_id),
                db::text(&now),
                db::number(safe_limit + 1),
            ],
        )
        .await?
    };
    let has_more = rows.len() as i64 > safe_limit;
    let rows = rows
        .into_iter()
        .take(safe_limit as usize)
        .collect::<Vec<_>>();
    let next_cursor = rows
        .last()
        .map(|row| pagination::encode(&row.created_at, &row.id));
    Ok(serde_json::json!({
        "messages": rows.into_iter().map(message_value).collect::<Vec<_>>(),
        "next_cursor": has_more.then_some(next_cursor).flatten(),
        "has_more": has_more,
    }))
}

pub async fn list_retrieval(
    db: &D1Database,
    user_id: &str,
    session_id: &str,
    limit: i32,
) -> ApiResult<Vec<Value>> {
    let now = db::now_iso();
    let rows: Vec<RetrievalItemRow> = db::all(
        db,
        "SELECT id, user_id, session_id, message_id, title, url, snippet, score, content_hash, r2_key, retention_expires_at, created_at FROM retrieval_items WHERE user_id = ? AND session_id = ? AND (retention_expires_at IS NULL OR retention_expires_at > ?) ORDER BY created_at DESC, id DESC LIMIT ?",
        vec![
            db::text(user_id),
            db::text(session_id),
            db::text(&now),
            db::number(limit.clamp(1, 100) as i64),
        ],
    )
    .await?;
    Ok(rows.into_iter().map(retrieval_value).collect())
}

pub async fn search(db: &D1Database, user_id: &str, query: &str, limit: i32) -> ApiResult<Value> {
    let query = query.trim();
    if query.len() < 2 {
        return Err(ApiError::validation(
            "Search query must contain at least 2 characters",
        ));
    }
    let safe_limit = limit.clamp(1, 50) as i64;
    let needle = format!("%{}%", query.replace('%', "\\%").replace('_', "\\_"));
    let sessions: Vec<SessionRow> = db::all(
        db,
        "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, expires_at, created_at, updated_at FROM sessions WHERE user_id = ? AND deleted_at IS NULL AND (title LIKE ? ESCAPE '\\' OR summary_text LIKE ? ESCAPE '\\' OR facts_json LIKE ? ESCAPE '\\') ORDER BY updated_at DESC, id DESC LIMIT ?",
        vec![
            db::text(user_id),
            db::text(&needle),
            db::text(&needle),
            db::text(&needle),
            db::number(safe_limit),
        ],
    )
    .await?;
    let messages: Vec<SessionMessageRow> = db::all(
        db,
        "SELECT id, user_id, session_id, role, content, metadata_json, command_id, sequence, retention_expires_at, created_at FROM session_messages WHERE user_id = ? AND (retention_expires_at IS NULL OR retention_expires_at > ?) AND content LIKE ? ESCAPE '\\' ORDER BY created_at DESC, id DESC LIMIT ?",
        vec![
            db::text(user_id),
            db::text(&db::now_iso()),
            db::text(&needle),
            db::number(safe_limit),
        ],
    )
    .await?;
    let retrievals: Vec<RetrievalItemRow> = db::all(
        db,
        "SELECT id, user_id, session_id, message_id, title, url, snippet, score, content_hash, r2_key, retention_expires_at, created_at FROM retrieval_items WHERE user_id = ? AND (retention_expires_at IS NULL OR retention_expires_at > ?) AND (title LIKE ? ESCAPE '\\' OR snippet LIKE ? ESCAPE '\\') ORDER BY created_at DESC, id DESC LIMIT ?",
        vec![
            db::text(user_id),
            db::text(&db::now_iso()),
            db::text(&needle),
            db::text(&needle),
            db::number(safe_limit),
        ],
    )
    .await?;
    Ok(serde_json::json!({
        "query": query,
        "sessions": sessions.iter().map(session_summary).collect::<Vec<_>>(),
        "messages": messages.into_iter().map(message_value).collect::<Vec<_>>(),
        "retrieval_items": retrievals.into_iter().map(retrieval_value).collect::<Vec<_>>(),
    }))
}

pub async fn export_session(
    db: &D1Database,
    user_id: &str,
    session: &SessionRow,
) -> ApiResult<Value> {
    let messages = list_messages(db, user_id, &session.id, None, 100).await?;
    let retrieval_items = list_retrieval(db, user_id, &session.id, 100).await?;
    Ok(serde_json::json!({
        "schema_version": 1,
        "exported_at": db::now_iso(),
        "session": sessions::session_to_api(session),
        "messages": messages.get("messages").cloned().unwrap_or(Value::Array(Vec::new())),
        "retrieval_items": retrieval_items,
    }))
}
