use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeSet;
use worker::{D1Database, Env};

use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::{RetrievalItemRow, SessionMessageRow, SessionRow};
use crate::pagination;
use crate::sessions;

const RETENTION_SWEEP_BATCH: i64 = 500;
const EXPORT_MAX_ITEMS: usize = 10_000;
const EXPORT_QUERY_LIMIT: i64 = EXPORT_MAX_ITEMS as i64 + 1;
pub(crate) const SEARCH_QUERY_MAX_CHARACTERS: usize = 200;

#[derive(Debug, Clone, Deserialize)]
struct ExpiredRetrievalRow {
    id: String,
    r2_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct R2ReferenceRow {
    r2_key: String,
    reference_count: i64,
}

/// R2 objects are private user data. New retrieval references must be stored
/// below this backend-owned namespace so a client cannot point at another
/// user's object key and then read it through the authenticated download
/// route.
pub fn user_r2_key_prefix(user_id: &str) -> String {
    format!("users/{user_id}/retrievals/")
}

pub fn is_user_r2_key(user_id: &str, key: &str) -> bool {
    let prefix = user_r2_key_prefix(user_id);
    key.starts_with(&prefix)
        && key.len() > prefix.len()
        && !key.contains('\\')
        && !key
            .split('/')
            .any(|segment| segment == "." || segment == "..")
}

pub async fn purge_expired(db: &D1Database, env: &Env, user_id: &str) -> ApiResult<()> {
    let now = db::now_iso();
    // Retrievals reference messages, so remove source metadata before the
    // message row. This is an opportunistic sweep; a scheduled worker can
    // later compact old rows for users who are never active.
    let retrievals = expired_retrievals(db, Some(user_id), &now, RETENTION_SWEEP_BATCH).await?;
    delete_r2_objects(db, env, &retrievals).await?;
    expire_retrieval_rows(db, &retrievals, &now).await?;
    db::run(
        db,
        "DELETE FROM session_messages WHERE user_id = ? AND retention_expires_at IS NOT NULL AND retention_expires_at <= ?",
        vec![db::text(user_id), db::text(&now)],
    )
    .await?;
    Ok(())
}

/// Scheduled retention sweep for users who do not open the app frequently.
/// Delete triggers emit tombstone/change records, so an offline device still
/// converges instead of resurrecting expired messages or retrieval snapshots.
pub async fn purge_expired_all(db: &D1Database, env: &Env) -> ApiResult<usize> {
    let now = db::now_iso();
    let expired = expired_retrievals(db, None, &now, RETENTION_SWEEP_BATCH).await?;
    delete_r2_objects(db, env, &expired).await?;
    let retrievals = expire_retrieval_rows(db, &expired, &now).await?;
    let messages = db::run(
        db,
        "DELETE FROM session_messages WHERE id IN (SELECT id FROM session_messages WHERE retention_expires_at IS NOT NULL AND retention_expires_at <= ? ORDER BY created_at ASC, id ASC LIMIT ?)",
        vec![db::text(&now), db::number(RETENTION_SWEEP_BATCH)],
    )
    .await?;
    Ok(retrievals + db::changes(&messages))
}

async fn expired_retrievals(
    db: &D1Database,
    user_id: Option<&str>,
    now: &str,
    limit: i64,
) -> ApiResult<Vec<ExpiredRetrievalRow>> {
    match user_id {
        Some(user_id) => {
            db::all(
                db,
                "SELECT id, r2_key FROM retrieval_items WHERE user_id = ? AND r2_delete_status = 'active' AND retention_expires_at IS NOT NULL AND retention_expires_at <= ? ORDER BY created_at ASC, id ASC LIMIT ?",
                vec![
                    db::text(user_id),
                    db::text(now),
                    db::number(limit),
                ],
            )
            .await
        }
        None => {
            db::all(
                db,
                "SELECT id, r2_key FROM retrieval_items WHERE r2_delete_status = 'active' AND retention_expires_at IS NOT NULL AND retention_expires_at <= ? ORDER BY created_at ASC, id ASC LIMIT ?",
                vec![db::text(now), db::number(limit)],
            )
            .await
        }
    }
}

async fn delete_r2_objects(
    db: &D1Database,
    env: &Env,
    retrievals: &[ExpiredRetrievalRow],
) -> ApiResult<()> {
    let keys = retrievals
        .iter()
        .filter_map(|row| row.r2_key.as_deref())
        .filter(|key| !key.trim().is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    if keys.is_empty() {
        return Ok(());
    }

    // Several retrieval rows may point to one immutable snapshot. Delete the
    // object only when no non-expired/out-of-batch row still references it.
    // Otherwise retention of one row could break another row's download.
    let key_placeholders = std::iter::repeat_n("?", keys.len())
        .collect::<Vec<_>>()
        .join(", ");
    let id_placeholders = std::iter::repeat_n("?", retrievals.len())
        .collect::<Vec<_>>()
        .join(", ");
    let mut reference_params = keys.iter().map(|key| db::text(key)).collect::<Vec<_>>();
    reference_params.extend(retrievals.iter().map(|row| db::text(&row.id)));
    let protected: BTreeSet<String> = db::all::<R2ReferenceRow>(
        db,
        &format!(
            "SELECT r2_key, COUNT(*) AS reference_count FROM retrieval_items WHERE r2_key IN ({key_placeholders}) AND id NOT IN ({id_placeholders}) GROUP BY r2_key"
        ),
        reference_params,
    )
    .await?
    .into_iter()
    .filter(|row| row.reference_count > 0)
    .map(|row| row.r2_key)
    .collect();

    let bucket = env.bucket("R2").map_err(|_| {
        ApiError::new(
            503,
            "retrieval_storage_unavailable",
            "Retrieval storage is not configured for retention cleanup",
        )
    })?;
    for key in keys {
        if protected.contains(&key) {
            continue;
        }
        bucket.delete(&key).await.map_err(|_| {
            ApiError::new(
                502,
                "retrieval_storage_error",
                "Retrieval retention cleanup could not delete an object",
            )
        })?;
    }
    Ok(())
}

async fn expire_retrieval_rows(
    db: &D1Database,
    retrievals: &[ExpiredRetrievalRow],
    now: &str,
) -> ApiResult<usize> {
    if retrievals.is_empty() {
        return Ok(0);
    }
    let placeholders = std::iter::repeat_n("?", retrievals.len())
        .collect::<Vec<_>>()
        .join(", ");
    let result = db::run(
        db,
        &format!("UPDATE retrieval_items SET r2_delete_status = 'deleted', expired_at = COALESCE(expired_at, ?), r2_deleted_at = COALESCE(r2_deleted_at, ?), r2_key = NULL, message_id = NULL WHERE id IN ({placeholders}) AND r2_delete_status <> 'deleted'"),
        std::iter::once(db::text(now))
            .chain(std::iter::once(db::text(now)))
            .chain(retrievals.iter().map(|row| db::text(&row.id)))
            .collect(),
    )
    .await?;
    for row in retrievals {
        db::run(
            db,
            "INSERT OR IGNORE INTO sync_tombstones (id, user_id, entity_type, entity_id, deleted_at) SELECT ?, user_id, 'retrieval', id, ? FROM retrieval_items WHERE id = ?",
            vec![
                db::text(&format!("retention-tombstone-{}", row.id)),
                db::text(now),
                db::text(&row.id),
            ],
        )
        .await?;
        db::run(
            db,
            "INSERT OR IGNORE INTO phone_changes (user_id, entity_type, entity_id, session_id, version, deleted_at, created_at) SELECT user_id, 'retrieval', id, session_id, 2, ?, ? FROM retrieval_items WHERE id = ?",
            vec![db::text(now), db::text(now), db::text(&row.id)],
        )
        .await?;
    }
    Ok(db::changes(&result))
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
    // reference; the authenticated download endpoint performs the R2 lookup
    // after authorization and retention checks.
    serde_json::json!({
        "retrieval_id": row.id,
        "session_id": row.session_id,
        "message_id": row.message_id,
        "title": row.title,
        "url": row.url,
        "snippet": row.snippet,
        "score": row.score,
        "content_hash": row.content_hash,
        "download_path": format!("/v1/phone/retrievals/{}/download", row.id),
        "retention_expires_at": row.retention_expires_at,
        "expired_at": row.expired_at,
        "r2_delete_status": row.r2_delete_status,
        "r2_deleted_at": row.r2_deleted_at,
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

#[allow(dead_code)]
pub async fn list_retrieval(
    db: &D1Database,
    user_id: &str,
    session_id: &str,
    limit: i32,
) -> ApiResult<Vec<Value>> {
    let page = list_retrieval_page(db, user_id, session_id, None, limit).await?;
    Ok(page
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

/// Cursor-paginated retrieval summaries. The `(created_at, id)` tie-breaker
/// makes equal timestamps deterministic and prevents duplicate/omitted rows
/// when a client walks the history while new snapshots arrive.
pub async fn list_retrieval_page(
    db: &D1Database,
    user_id: &str,
    session_id: &str,
    before: Option<&str>,
    limit: i32,
) -> ApiResult<Value> {
    let before = pagination::decode(before)?;
    let now = db::now_iso();
    let rows: Vec<RetrievalItemRow> = db::all(
        db,
        if before.is_some() {
            "SELECT id, user_id, session_id, message_id, title, url, snippet, score, content_hash, r2_key, retention_expires_at, r2_delete_status, r2_deleted_at, expired_at, created_at FROM retrieval_items WHERE user_id = ? AND session_id = ? AND r2_delete_status = 'active' AND (retention_expires_at IS NULL OR retention_expires_at > ?) AND (created_at < ? OR (created_at = ? AND id < ?)) ORDER BY created_at DESC, id DESC LIMIT ?"
        } else {
            "SELECT id, user_id, session_id, message_id, title, url, snippet, score, content_hash, r2_key, retention_expires_at, r2_delete_status, r2_deleted_at, expired_at, created_at FROM retrieval_items WHERE user_id = ? AND session_id = ? AND r2_delete_status = 'active' AND (retention_expires_at IS NULL OR retention_expires_at > ?) ORDER BY created_at DESC, id DESC LIMIT ?"
        },
        match before {
            Some(cursor) => vec![
                db::text(user_id), db::text(session_id), db::text(&now),
                db::text(&cursor.sort_key), db::text(&cursor.sort_key),
                db::text(&cursor.id), db::number(limit.clamp(1, 50) as i64 + 1),
            ],
            None => vec![
                db::text(user_id), db::text(session_id), db::text(&now),
                db::number(limit.clamp(1, 50) as i64 + 1),
            ],
        },
    )
    .await?;
    let safe_limit = limit.clamp(1, 50) as usize;
    let has_more = rows.len() > safe_limit;
    let rows = rows.into_iter().take(safe_limit).collect::<Vec<_>>();
    let next_cursor = rows
        .last()
        .map(|row| pagination::encode(&row.created_at, &row.id));
    Ok(serde_json::json!({
        "items": rows.into_iter().map(retrieval_value).collect::<Vec<_>>(),
        "next_cursor": has_more.then_some(next_cursor).flatten(),
        "has_more": has_more,
    }))
}

// Keep export retrieval reads independent from the bounded summary page. The
// additive retention columns introduced by migration 0013 are intentionally
// included via SELECT * so this query remains compatible with both the
// pre-migration model and the tombstone-aware model; serde ignores columns
// unknown to the older model. The response still passes through
// retrieval_value, which never exposes the internal R2 key.
async fn list_retrieval_for_export(
    db: &D1Database,
    user_id: &str,
    session_id: &str,
) -> ApiResult<Vec<Value>> {
    let now = db::now_iso();
    let rows: Vec<RetrievalItemRow> = db::all(
        db,
        "SELECT * FROM retrieval_items WHERE user_id = ? AND session_id = ? AND (retention_expires_at IS NULL OR retention_expires_at > ?) ORDER BY created_at DESC, id DESC LIMIT ?",
        vec![
            db::text(user_id),
            db::text(session_id),
            db::text(&now),
            db::number(EXPORT_QUERY_LIMIT),
        ],
    )
    .await?;
    Ok(rows.into_iter().map(retrieval_value).collect())
}

/// Resolve one retrieval snapshot for an authenticated user without exposing
/// its internal R2 object key. Deleted sessions and expired snapshots are
/// intentionally indistinguishable from a missing retrieval to avoid leaking
/// resource existence across lifecycle boundaries.
pub async fn get_retrieval(
    db: &D1Database,
    user_id: &str,
    retrieval_id: &str,
) -> ApiResult<Option<RetrievalItemRow>> {
    let now = db::now_iso();
    db::first(
        db,
        "SELECT r.id, r.user_id, r.session_id, r.message_id, r.title, r.url, r.snippet, r.score, r.content_hash, r.r2_key, r.retention_expires_at, r.r2_delete_status, r.r2_deleted_at, r.expired_at, r.created_at FROM retrieval_items AS r JOIN sessions AS s ON s.id = r.session_id AND s.user_id = r.user_id AND s.deleted_at IS NULL WHERE r.id = ? AND r.user_id = ? AND r.r2_delete_status = 'active' AND (r.retention_expires_at IS NULL OR r.retention_expires_at > ?)",
        vec![
            db::text(retrieval_id),
            db::text(user_id),
            db::text(&now),
        ],
    )
    .await
}

pub async fn search(db: &D1Database, user_id: &str, query: &str, limit: i32) -> ApiResult<Value> {
    let query = validate_search_query(query)?;
    let safe_limit = limit.clamp(1, 50) as i64;
    let needle = search_like_pattern(query);
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
        "SELECT r.id, r.user_id, r.session_id, r.message_id, r.title, r.url, r.snippet, r.score, r.content_hash, r.r2_key, r.retention_expires_at, r.r2_delete_status, r.r2_deleted_at, r.expired_at, r.created_at FROM retrieval_items AS r JOIN sessions AS s ON s.id = r.session_id AND s.user_id = r.user_id AND s.deleted_at IS NULL WHERE r.user_id = ? AND r.r2_delete_status = 'active' AND (r.retention_expires_at IS NULL OR r.retention_expires_at > ?) AND (r.title LIKE ? ESCAPE '\\' OR r.snippet LIKE ? ESCAPE '\\') ORDER BY r.created_at DESC, r.id DESC LIMIT ?",
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

fn validate_search_query(query: &str) -> ApiResult<&str> {
    let query = query.trim();
    let length = query.chars().count();
    if length == 0 || length > SEARCH_QUERY_MAX_CHARACTERS {
        return Err(ApiError::validation(
            "Search query must contain between 1 and 200 characters",
        ));
    }
    Ok(query)
}

fn search_like_pattern(query: &str) -> String {
    format!(
        "%{}%",
        query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    )
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
    let retrieval_items = list_retrieval_for_export(db, user_id, &session.id).await?;
    if retrieval_items.len() > EXPORT_MAX_ITEMS {
        truncated = true;
    }
    let retrieval_items = retrieval_items
        .into_iter()
        .take(EXPORT_MAX_ITEMS)
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "schema_version": 1,
        "exported_at": db::now_iso(),
        "session": sessions::session_to_api(session),
        "messages": messages,
        "retrieval_items": retrieval_items,
        "truncated": truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r2_namespace_is_strictly_user_scoped() {
        assert!(is_user_r2_key("user_a", "users/user_a/retrievals/item-1"));
        assert!(!is_user_r2_key("user_a", "users/user_b/retrievals/item-1"));
        assert!(!is_user_r2_key(
            "user_a",
            "users/user_a/retrievals/../user_b/item-1"
        ));
        assert!(!is_user_r2_key("user_a", "users/user_a/retrievals/"));
    }

    #[test]
    fn retrieval_summary_never_exposes_the_r2_reference() {
        let row = RetrievalItemRow {
            id: "ret-1".into(),
            user_id: "user_a".into(),
            session_id: "session-1".into(),
            message_id: None,
            title: "Source".into(),
            url: "https://example.test".into(),
            snippet: Some("summary".into()),
            score: Some(0.9),
            content_hash: "hash".into(),
            r2_key: Some("users/user_a/retrievals/ret-1".into()),
            retention_expires_at: None,
            r2_delete_status: "active".into(),
            r2_deleted_at: None,
            expired_at: None,
            created_at: "2026-08-11T00:00:00Z".into(),
        };
        let value = retrieval_value(row);
        assert!(value.get("r2_key").is_none());
        assert_eq!(
            value.get("r2_delete_status").and_then(Value::as_str),
            Some("active")
        );
    }

    #[test]
    fn export_query_keeps_a_truncation_sentinel() {
        assert_eq!(EXPORT_MAX_ITEMS, 10_000);
        assert_eq!(EXPORT_QUERY_LIMIT, 10_001);
    }

    #[test]
    fn search_like_pattern_escapes_sql_like_controls() {
        assert_eq!(search_like_pattern(r"a\b%c_d"), r"%a\\b\%c\_d%");
    }

    #[test]
    fn search_query_contract_accepts_one_character_and_rejects_blank_or_oversized() {
        assert_eq!(validate_search_query(" x ").unwrap(), "x");
        assert_eq!(validate_search_query(" 家 ").unwrap(), "家");
        assert!(validate_search_query("   ").is_err());
        assert!(validate_search_query(&"x".repeat(SEARCH_QUERY_MAX_CHARACTERS + 1)).is_err());
    }
}
