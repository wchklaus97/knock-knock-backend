use serde_json::{Map, Value};
use worker::D1Database;

use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::{RetrievalItemRow, SessionMessageRow, SessionRow};
use crate::pagination;
use crate::sessions;

pub async fn purge_expired(db: &D1Database, user_id: &str) -> ApiResult<()> {
    let now = db::now_iso();
    // Retrievals reference messages, so remove source metadata before the
    // message row. This is an opportunistic sweep; a scheduled worker can
    // later compact old rows for users who are never active.
    db::run(
        db,
        "DELETE FROM retrieval_items WHERE user_id = ? AND retention_expires_at IS NOT NULL AND retention_expires_at <= ?",
        vec![db::text(user_id), db::text(&now)],
    )
    .await?;
    db::run(
        db,
        "DELETE FROM session_messages WHERE user_id = ? AND retention_expires_at IS NOT NULL AND retention_expires_at <= ?",
        vec![db::text(user_id), db::text(&now)],
    )
    .await?;
    Ok(())
}

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
    let available_action_descriptors = row
        .available_action_descriptors_json
        .as_deref()
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .unwrap_or_else(|| Value::Array(Vec::new()));
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
        "available_action_descriptors": available_action_descriptors,
        "expires_at": row.expires_at,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
        "archived_at": row.archived_at,
        "deleted_at": row.deleted_at,
        "retention_expires_at": row.retention_expires_at,
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
            "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, available_action_descriptors_json, expires_at, created_at, updated_at, archived_at, deleted_at, retention_expires_at FROM sessions WHERE user_id = ? AND deleted_at IS NULL AND (updated_at < ? OR (updated_at = ? AND id < ?)) ORDER BY updated_at DESC, id DESC LIMIT ?",
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
            "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, available_action_descriptors_json, expires_at, created_at, updated_at, archived_at, deleted_at, retention_expires_at FROM sessions WHERE user_id = ? AND deleted_at IS NULL ORDER BY updated_at DESC, id DESC LIMIT ?",
            vec![db::text(user_id), db::number(safe_limit + 1)],
        )
        .await?
    };
    let has_more = rows.len() as i64 > safe_limit;
    let next_cursor = rows
        .get(safe_limit as usize - 1)
        .map(|row| pagination::encode(&row.updated_at, &row.id));
    let mut result = Vec::new();
    for row in rows.into_iter().take(safe_limit as usize) {
        let fresh = sessions::reconcile_waiting_session(db, row).await?;
        result.push(session_summary(&fresh));
    }
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
    let messages = rows.into_iter().map(message_value).collect::<Vec<_>>();
    Ok(serde_json::json!({
        // `items` is retained for clients generated from the Phase 0 schema;
        // `messages` is the canonical v1 field.
        "items": messages.clone(),
        "messages": messages,
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
            db::number(limit.clamp(1, 10_001) as i64),
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
        "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, available_action_descriptors_json, expires_at, created_at, updated_at, archived_at, deleted_at, retention_expires_at FROM sessions WHERE user_id = ? AND deleted_at IS NULL AND (title LIKE ? ESCAPE '\\' OR summary_text LIKE ? ESCAPE '\\' OR facts_json LIKE ? ESCAPE '\\') ORDER BY updated_at DESC, id DESC LIMIT ?",
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
        "SELECT m.id, m.user_id, m.session_id, m.role, m.content, m.metadata_json, m.command_id, m.sequence, m.retention_expires_at, m.created_at FROM session_messages AS m JOIN sessions AS s ON s.id = m.session_id AND s.user_id = m.user_id AND s.deleted_at IS NULL WHERE m.user_id = ? AND (m.retention_expires_at IS NULL OR m.retention_expires_at > ?) AND m.content LIKE ? ESCAPE '\\' ORDER BY m.created_at DESC, m.id DESC LIMIT ?",
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
        "SELECT r.id, r.user_id, r.session_id, r.message_id, r.title, r.url, r.snippet, r.score, r.content_hash, r.r2_key, r.retention_expires_at, r.created_at FROM retrieval_items AS r JOIN sessions AS s ON s.id = r.session_id AND s.user_id = r.user_id AND s.deleted_at IS NULL WHERE r.user_id = ? AND (r.retention_expires_at IS NULL OR r.retention_expires_at > ?) AND (r.title LIKE ? ESCAPE '\\' OR r.snippet LIKE ? ESCAPE '\\') ORDER BY r.created_at DESC, r.id DESC LIMIT ?",
        vec![
            db::text(user_id),
            db::text(&db::now_iso()),
            db::text(&needle),
            db::text(&needle),
            db::number(safe_limit),
        ],
    )
    .await?;
    let session_values = sessions.iter().map(session_summary).collect::<Vec<_>>();
    let message_values = messages.into_iter().map(message_value).collect::<Vec<_>>();
    let retrieval_values = retrievals
        .into_iter()
        .map(retrieval_value)
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "query": query,
        "items": message_values.clone(),
        "next_cursor": Value::Null,
        "has_more": false,
        "sessions": session_values,
        "messages": message_values,
        "retrieval_items": retrieval_values,
    }))
}

pub async fn export_session(
    db: &D1Database,
    user_id: &str,
    session: &SessionRow,
) -> ApiResult<Value> {
    let mut messages = Vec::new();
    let mut before = None;
    let mut truncated = false;
    loop {
        let page = list_messages(db, user_id, &session.id, before.as_deref(), 100).await?;
        let page_messages = page
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        messages.extend(page_messages);
        let has_more = page
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !has_more {
            break;
        }
        before = page
            .get("next_cursor")
            .and_then(Value::as_str)
            .map(str::to_string);
        if before.is_none() || messages.len() >= 10_000 {
            truncated = true;
            break;
        }
    }
    let retrieval_items = list_retrieval(db, user_id, &session.id, 10_001).await?;
    if retrieval_items.len() > 10_000 {
        truncated = true;
    }
    let retrieval_items = retrieval_items.into_iter().take(10_000).collect::<Vec<_>>();
    Ok(serde_json::json!({
        "schema_version": 1,
        "exported_at": db::now_iso(),
        "session": sessions::session_to_api(session),
        "messages": messages,
        "retrieval_items": retrieval_items,
        "truncated": truncated,
    }))
}
