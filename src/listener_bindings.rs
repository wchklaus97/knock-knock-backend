use serde::Deserialize;
use serde_json::{json, Value};
use worker::{D1Database, Request};

use crate::auth::new_id;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::AgentPrincipal;

pub const LISTENER_LEASE_SECS: i64 = 90;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterListenerRequest {
    pub chat_id: String,
    pub chat_title: String,
    pub listener_instance_id: String,
    #[serde(default)]
    pub takeover: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListenerBindingRow {
    pub id: String,
    pub user_id: String,
    pub agent_id: String,
    pub chat_id: String,
    pub chat_title: String,
    pub listener_instance_id: String,
    pub status: String,
    pub last_seen_at: String,
    pub expires_at: String,
    pub revoked_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn validated(value: &str, field: &str, min: usize, max: usize) -> ApiResult<String> {
    let value = value.trim();
    if value.len() < min || value.len() > max || value.chars().any(char::is_control) {
        return Err(ApiError::validation(format!("{field} is invalid")));
    }
    Ok(value.to_string())
}

fn binding_to_api(row: &ListenerBindingRow) -> Value {
    json!({
        "binding_id": row.id,
        "agent_id": row.agent_id,
        "chat_id": row.chat_id,
        "chat_title": row.chat_title,
        "listener_instance_id": row.listener_instance_id,
        "status": row.status,
        "last_seen_at": row.last_seen_at,
        "expires_at": row.expires_at,
    })
}

async fn expire_stale(db: &D1Database, agent_id: &str) -> ApiResult<()> {
    let now = db::now_iso();
    db::run(
        db,
        "UPDATE agent_chat_bindings SET status = 'expired', updated_at = ? WHERE agent_id = ? AND status = 'active' AND expires_at <= ?",
        vec![db::text(&now), db::text(agent_id), db::text(&now)],
    )
    .await?;
    Ok(())
}

pub async fn register_listener(
    db: &D1Database,
    agent: &AgentPrincipal,
    input: &RegisterListenerRequest,
) -> ApiResult<Value> {
    let chat_id = validated(&input.chat_id, "chat_id", 1, 128)?;
    let chat_title = validated(&input.chat_title, "chat_title", 1, 160)?;
    let instance_id = validated(&input.listener_instance_id, "listener_instance_id", 8, 128)?;
    let now = db::now_iso();
    let expires_at = db::add_seconds_iso(LISTENER_LEASE_SECS);

    if let Some(active) = active_for_agent(db, &agent.agent_id).await? {
        if active.chat_id != chat_id && input.takeover != Some(true) {
            return Err(ApiError::new(
                409,
                "listener_already_active",
                "Another Codex chat is already the active voice listener; explicit takeover is required",
            ));
        }
    }

    db::run(
        db,
        "UPDATE agent_chat_bindings SET status = 'revoked', revoked_at = ?, updated_at = ? WHERE agent_id = ? AND status = 'active' AND chat_id <> ?",
        vec![db::text(&now), db::text(&now), db::text(&agent.agent_id), db::text(&chat_id)],
    )
    .await?;

    let existing = db::first::<ListenerBindingRow>(
        db,
        "SELECT id, user_id, agent_id, chat_id, chat_title, listener_instance_id, status, last_seen_at, expires_at, revoked_at, created_at, updated_at FROM agent_chat_bindings WHERE agent_id = ? AND chat_id = ?",
        vec![db::text(&agent.agent_id), db::text(&chat_id)],
    )
    .await?;
    let binding_id = existing
        .as_ref()
        .map(|row| row.id.clone())
        .unwrap_or(new_id("bind")?);
    if existing.is_some() {
        db::run(
            db,
            "UPDATE agent_chat_bindings SET chat_title = ?, listener_instance_id = ?, status = 'active', last_seen_at = ?, expires_at = ?, revoked_at = NULL, updated_at = ? WHERE id = ? AND agent_id = ?",
            vec![
                db::text(&chat_title), db::text(&instance_id), db::text(&now),
                db::text(&expires_at), db::text(&now), db::text(&binding_id),
                db::text(&agent.agent_id),
            ],
        )
        .await?;
    } else {
        db::run(
            db,
            "INSERT INTO agent_chat_bindings (id, user_id, agent_id, chat_id, chat_title, listener_instance_id, status, last_seen_at, expires_at, revoked_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 'active', ?, ?, NULL, ?, ?)",
            vec![
                db::text(&binding_id), db::text(&agent.user_id), db::text(&agent.agent_id),
                db::text(&chat_id), db::text(&chat_title), db::text(&instance_id),
                db::text(&now), db::text(&expires_at), db::text(&now), db::text(&now),
            ],
        )
        .await?;
    }
    db::run(
        db,
        "UPDATE agents SET last_seen_at = ? WHERE id = ?",
        vec![db::text(&now), db::text(&agent.agent_id)],
    )
    .await?;
    let row = db::first::<ListenerBindingRow>(
        db,
        "SELECT id, user_id, agent_id, chat_id, chat_title, listener_instance_id, status, last_seen_at, expires_at, revoked_at, created_at, updated_at FROM agent_chat_bindings WHERE id = ?",
        vec![db::text(&binding_id)],
    )
    .await?
    .ok_or_else(|| ApiError::new(500, "listener_binding_error", "Listener binding was not saved"))?;
    Ok(binding_to_api(&row))
}

pub async fn active_for_agent(
    db: &D1Database,
    agent_id: &str,
) -> ApiResult<Option<ListenerBindingRow>> {
    expire_stale(db, agent_id).await?;
    db::first(
        db,
        "SELECT id, user_id, agent_id, chat_id, chat_title, listener_instance_id, status, last_seen_at, expires_at, revoked_at, created_at, updated_at FROM agent_chat_bindings WHERE agent_id = ? AND status = 'active' AND expires_at > ? ORDER BY updated_at DESC LIMIT 1",
        vec![db::text(agent_id), db::text(&db::now_iso())],
    )
    .await
}

pub async fn require_request_binding(
    db: &D1Database,
    req: &Request,
    agent_id: &str,
) -> ApiResult<ListenerBindingRow> {
    let chat_id = req
        .headers()
        .get("x-knock-chat-id")?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::new(409, "listener_binding_required", "This Codex chat is not registered as the voice listener"))?;
    let instance_id = req
        .headers()
        .get("x-knock-listener-instance")?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::new(409, "listener_binding_required", "This Codex chat is not registered as the voice listener"))?;
    expire_stale(db, agent_id).await?;
    db::first(
        db,
        "SELECT id, user_id, agent_id, chat_id, chat_title, listener_instance_id, status, last_seen_at, expires_at, revoked_at, created_at, updated_at FROM agent_chat_bindings WHERE agent_id = ? AND chat_id = ? AND listener_instance_id = ? AND status = 'active' AND expires_at > ?",
        vec![db::text(agent_id), db::text(&chat_id), db::text(&instance_id), db::text(&db::now_iso())],
    )
    .await?
    .ok_or_else(|| ApiError::new(409, "listener_binding_mismatch", "This Codex chat is not the active voice listener"))
}

pub async fn disconnect_listener(
    db: &D1Database,
    req: &Request,
    agent_id: &str,
) -> ApiResult<Value> {
    let binding = require_request_binding(db, req, agent_id).await?;
    let now = db::now_iso();
    db::run(
        db,
        "UPDATE agent_chat_bindings SET status = 'revoked', revoked_at = ?, updated_at = ? WHERE id = ? AND status = 'active'",
        vec![db::text(&now), db::text(&now), db::text(&binding.id)],
    )
    .await?;
    Ok(json!({"ok": true, "binding_id": binding.id}))
}
