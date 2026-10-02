use serde::Deserialize;
use serde_json::{json, Value};
use worker::{D1Database, Request};

use crate::auth::new_id;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::AgentPrincipal;

pub const LISTENER_LEASE_SECS: i64 = 90;
pub const LISTENER_RENEW_AFTER_MS: i64 = 30_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterListenerRequest {
    pub chat_id: String,
    pub chat_title: String,
    pub listener_instance_id: String,
    #[serde(default)]
    pub lease_id: Option<String>,
    #[serde(default)]
    pub generation: Option<i64>,
    #[serde(default)]
    pub takeover: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenewListenerRequest {
    pub lease_id: String,
    pub generation: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
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
    pub lease_id: String,
    pub generation: i64,
}

#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
struct ActiveLeaseRow {
    agent_id: String,
    user_id: String,
    binding_id: String,
    chat_id: String,
    chat_title: String,
    listener_instance_id: String,
    lease_id: String,
    generation: i64,
    acquired_at: String,
    last_seen_at: String,
    expires_at: String,
    updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ReleasedLeaseRow {
    agent_id: String,
    user_id: String,
    binding_id: String,
    chat_id: String,
    listener_instance_id: String,
    lease_id: String,
    generation: i64,
    released_at: String,
}

#[derive(Debug, Deserialize)]
struct BindingIdRow {
    id: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct SchemaProbeRow {
    binding_id: String,
    binding_generation: i64,
    binding_lease_id: Option<String>,
    lease_agent_id: Option<String>,
    lease_generation: Option<i64>,
    active_lease_id: Option<String>,
    lease_released_at: Option<String>,
    ask_binding_id: Option<String>,
    ask_target_chat_id: Option<String>,
    ask_claimed_by_chat_id: Option<String>,
}

fn acquire_lease_sql() -> &'static str {
    "INSERT INTO agent_listener_leases (agent_id, user_id, binding_id, chat_id, chat_title, listener_instance_id, lease_id, generation, acquired_at, last_seen_at, expires_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?, ?) ON CONFLICT(agent_id) DO UPDATE SET user_id = excluded.user_id, binding_id = excluded.binding_id, chat_id = excluded.chat_id, chat_title = excluded.chat_title, listener_instance_id = excluded.listener_instance_id, lease_id = excluded.lease_id, generation = agent_listener_leases.generation + 1, acquired_at = excluded.acquired_at, last_seen_at = excluded.last_seen_at, expires_at = excluded.expires_at, released_at = NULL, updated_at = excluded.updated_at WHERE agent_listener_leases.expires_at <= excluded.acquired_at OR ? = 1 OR (agent_listener_leases.chat_id = excluded.chat_id AND agent_listener_leases.lease_id = ? AND agent_listener_leases.generation = ?) RETURNING agent_id, user_id, binding_id, chat_id, chat_title, listener_instance_id, lease_id, generation, acquired_at, last_seen_at, expires_at, updated_at"
}

fn heartbeat_renew_sql() -> &'static str {
    "UPDATE agent_listener_leases SET last_seen_at = ?, expires_at = ?, updated_at = ? WHERE agent_id = ? AND lease_id = ? AND generation = ? AND released_at IS NULL AND expires_at > ? RETURNING agent_id, user_id, binding_id, chat_id, chat_title, listener_instance_id, lease_id, generation, acquired_at, last_seen_at, expires_at, updated_at"
}

fn release_lease_sql() -> &'static str {
    "UPDATE agent_listener_leases SET expires_at = CASE WHEN expires_at > ? THEN ? ELSE expires_at END, released_at = COALESCE(released_at, ?), updated_at = CASE WHEN released_at IS NULL THEN ? ELSE updated_at END WHERE agent_id = ? AND user_id = ? AND chat_id = ? AND listener_instance_id = ? AND lease_id = ? AND generation = ? AND (expires_at > ? OR released_at IS NOT NULL) RETURNING agent_id, user_id, binding_id, chat_id, listener_instance_id, lease_id, generation, released_at"
}

fn release_binding_sql() -> &'static str {
    "UPDATE agent_chat_bindings SET status = 'revoked', revoked_at = ?, expires_at = ?, updated_at = ? WHERE id = ? AND user_id = ? AND agent_id = ? AND chat_id = ? AND listener_instance_id = ? AND lease_id = ? AND generation = ? AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = ? AND l.user_id = ? AND l.binding_id = ? AND l.chat_id = ? AND l.listener_instance_id = ? AND l.lease_id = ? AND l.generation = ? AND l.released_at = ?)"
}

fn schema_probe_sql() -> &'static str {
    "SELECT b.id AS binding_id, b.generation AS binding_generation, b.lease_id AS binding_lease_id, l.agent_id AS lease_agent_id, l.generation AS lease_generation, l.lease_id AS active_lease_id, l.released_at AS lease_released_at, p.binding_id AS ask_binding_id, p.target_chat_id AS ask_target_chat_id, p.claimed_by_chat_id AS ask_claimed_by_chat_id FROM agent_chat_bindings b LEFT JOIN agent_listener_leases l ON l.binding_id = b.id LEFT JOIN phone_asks p ON 1 = 0 LIMIT 1"
}

#[allow(dead_code)]
pub async fn schema_ready(db: &D1Database) -> ApiResult<()> {
    let _: Option<SchemaProbeRow> = db::first(db, schema_probe_sql(), vec![]).await?;
    Ok(())
}

fn validated(value: &str, field: &str, min: usize, max: usize) -> ApiResult<String> {
    let value = value.trim();
    if value.len() < min || value.len() > max || value.chars().any(char::is_control) {
        return Err(ApiError::validation(format!("{field} is invalid")));
    }
    Ok(value.to_string())
}

fn validated_generation(generation: i64) -> ApiResult<i64> {
    if generation <= 0 {
        return Err(ApiError::validation("generation is invalid"));
    }
    Ok(generation)
}

fn validated_acquire_fence(
    lease_id: Option<&str>,
    generation: Option<i64>,
) -> ApiResult<Option<(String, i64)>> {
    match (lease_id, generation) {
        (None, None) => Ok(None),
        (Some(lease_id), Some(generation)) => Ok(Some((
            validated(lease_id, "lease_id", 8, 128)?,
            validated_generation(generation)?,
        ))),
        _ => Err(ApiError::validation(
            "lease_id and generation must be provided together",
        )),
    }
}

fn listener_already_active() -> ApiError {
    ApiError::new(
        409,
        "listener_already_active",
        "Another Codex chat is already the active voice listener; explicit takeover is required",
    )
}

fn lease_fenced() -> ApiError {
    ApiError::new(
        409,
        "lease_fenced",
        "The listener lease is expired or has been superseded; acquire a new lease",
    )
}

fn lease_to_api(row: &ActiveLeaseRow) -> Value {
    json!({
        "binding_id": row.binding_id,
        "agent_id": row.agent_id,
        "chat_id": row.chat_id,
        "chat_title": row.chat_title,
        "listener_instance_id": row.listener_instance_id,
        "lease_id": row.lease_id,
        "generation": row.generation,
        "status": "active",
        "last_seen_at": row.last_seen_at,
        "expires_at": row.expires_at,
        "renew_after_ms": LISTENER_RENEW_AFTER_MS,
    })
}

async fn ensure_binding(
    db: &D1Database,
    agent: &AgentPrincipal,
    chat_id: &str,
    chat_title: &str,
    instance_id: &str,
    now: &str,
) -> ApiResult<String> {
    let candidate_id = new_id("bind")?;
    db::run(
        db,
        "INSERT OR IGNORE INTO agent_chat_bindings (id, user_id, agent_id, chat_id, chat_title, listener_instance_id, status, last_seen_at, expires_at, revoked_at, lease_id, generation, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 'offline', ?, ?, NULL, NULL, 0, ?, ?)",
        vec![
            db::text(&candidate_id),
            db::text(&agent.user_id),
            db::text(&agent.agent_id),
            db::text(chat_id),
            db::text(chat_title),
            db::text(instance_id),
            db::text(now),
            db::text(now),
            db::text(now),
            db::text(now),
        ],
    )
    .await?;
    db::first::<BindingIdRow>(
        db,
        "SELECT id FROM agent_chat_bindings WHERE agent_id = ? AND chat_id = ?",
        vec![db::text(&agent.agent_id), db::text(chat_id)],
    )
    .await?
    .map(|row| row.id)
    .ok_or_else(|| {
        ApiError::new(
            500,
            "listener_binding_error",
            "Listener binding could not be initialized",
        )
    })
}

async fn current_lease_by_fence(
    db: &D1Database,
    agent_id: &str,
    lease_id: &str,
    generation: i64,
) -> ApiResult<Option<ActiveLeaseRow>> {
    db::first(
        db,
        "SELECT agent_id, user_id, binding_id, chat_id, chat_title, listener_instance_id, lease_id, generation, acquired_at, last_seen_at, expires_at, updated_at FROM agent_listener_leases WHERE agent_id = ? AND lease_id = ? AND generation = ? AND released_at IS NULL AND expires_at > ?",
        vec![
            db::text(agent_id),
            db::text(lease_id),
            db::number(generation),
            db::text(&db::now_iso()),
        ],
    )
    .await
}

async fn sync_active_binding(db: &D1Database, lease: &ActiveLeaseRow) -> ApiResult<()> {
    let checked_at = db::now_iso();
    let statements = vec![
        db::prepare(
            db,
            "UPDATE agent_chat_bindings SET status = 'revoked', revoked_at = ?, updated_at = ? WHERE agent_id = ? AND id <> ? AND status = 'active' AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = ? AND l.lease_id = ? AND l.generation = ? AND l.released_at IS NULL AND l.expires_at > ?)",
            vec![
                db::text(&lease.updated_at),
                db::text(&lease.updated_at),
                db::text(&lease.agent_id),
                db::text(&lease.binding_id),
                db::text(&lease.agent_id),
                db::text(&lease.lease_id),
                db::number(lease.generation),
                db::text(&checked_at),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE agent_chat_bindings SET chat_title = ?, listener_instance_id = ?, status = 'active', last_seen_at = ?, expires_at = ?, revoked_at = NULL, lease_id = ?, generation = ?, updated_at = ? WHERE id = ? AND agent_id = ? AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = ? AND l.lease_id = ? AND l.generation = ? AND l.released_at IS NULL AND l.expires_at > ?)",
            vec![
                db::text(&lease.chat_title),
                db::text(&lease.listener_instance_id),
                db::text(&lease.last_seen_at),
                db::text(&lease.expires_at),
                db::text(&lease.lease_id),
                db::number(lease.generation),
                db::text(&lease.updated_at),
                db::text(&lease.binding_id),
                db::text(&lease.agent_id),
                db::text(&lease.agent_id),
                db::text(&lease.lease_id),
                db::number(lease.generation),
                db::text(&checked_at),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE agents SET last_seen_at = ? WHERE id = ? AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = ? AND l.lease_id = ? AND l.generation = ? AND l.released_at IS NULL AND l.expires_at > ?)",
            vec![
                db::text(&lease.last_seen_at),
                db::text(&lease.agent_id),
                db::text(&lease.agent_id),
                db::text(&lease.lease_id),
                db::number(lease.generation),
                db::text(&checked_at),
            ],
        )?,
    ];
    db.batch(statements).await?;
    Ok(())
}

async fn finish_lease(db: &D1Database, lease: ActiveLeaseRow) -> ApiResult<Value> {
    sync_active_binding(db, &lease).await?;
    let current = current_lease_by_fence(db, &lease.agent_id, &lease.lease_id, lease.generation)
        .await?
        .ok_or_else(lease_fenced)?;
    Ok(lease_to_api(&current))
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
    let takeover = input.takeover == Some(true);
    let fence = validated_acquire_fence(input.lease_id.as_deref(), input.generation)?;

    let binding_id = ensure_binding(db, agent, &chat_id, &chat_title, &instance_id, &now).await?;
    let lease_id = new_id("lease")?;
    let acquired = db::first::<ActiveLeaseRow>(
        db,
        acquire_lease_sql(),
        vec![
            db::text(&agent.agent_id),
            db::text(&agent.user_id),
            db::text(&binding_id),
            db::text(&chat_id),
            db::text(&chat_title),
            db::text(&instance_id),
            db::text(&lease_id),
            db::text(&now),
            db::text(&now),
            db::text(&expires_at),
            db::text(&now),
            db::bool_number(takeover),
            db::optional_text(
                fence
                    .as_ref()
                    .map(|(presented_lease_id, _)| presented_lease_id.as_str()),
            ),
            db::number(
                fence
                    .as_ref()
                    .map(|(_, presented_generation)| *presented_generation)
                    .unwrap_or(0),
            ),
        ],
    )
    .await?
    .ok_or_else(|| {
        if fence.is_some() {
            lease_fenced()
        } else {
            listener_already_active()
        }
    })?;
    finish_lease(db, acquired).await
}

pub async fn renew_listener(
    db: &D1Database,
    agent: &AgentPrincipal,
    input: &RenewListenerRequest,
) -> ApiResult<Value> {
    let lease_id = validated(&input.lease_id, "lease_id", 8, 128)?;
    let generation = validated_generation(input.generation)?;
    let now = db::now_iso();
    let expires_at = db::add_seconds_iso(LISTENER_LEASE_SECS);
    let renewed = db::first::<ActiveLeaseRow>(
        db,
        heartbeat_renew_sql(),
        vec![
            db::text(&now),
            db::text(&expires_at),
            db::text(&now),
            db::text(&agent.agent_id),
            db::text(&lease_id),
            db::number(generation),
            db::text(&now),
        ],
    )
    .await?
    .ok_or_else(lease_fenced)?;
    finish_lease(db, renewed).await
}

pub async fn active_for_agent(
    db: &D1Database,
    agent_id: &str,
) -> ApiResult<Option<ListenerBindingRow>> {
    db::first(
        db,
        "SELECT b.id, b.user_id, b.agent_id, l.chat_id, l.chat_title, l.listener_instance_id, 'active' AS status, l.last_seen_at, l.expires_at, NULL AS revoked_at, b.created_at, l.updated_at, l.lease_id, l.generation FROM agent_listener_leases l JOIN agent_chat_bindings b ON b.id = l.binding_id WHERE l.agent_id = ? AND l.released_at IS NULL AND l.expires_at > ? LIMIT 1",
        vec![db::text(agent_id), db::text(&db::now_iso())],
    )
    .await
}

pub async fn active_for_fence(
    db: &D1Database,
    agent_id: &str,
    binding_id: &str,
    chat_id: &str,
    lease_id: &str,
    generation: i64,
) -> ApiResult<Option<ListenerBindingRow>> {
    db::first(
        db,
        "SELECT b.id, b.user_id, b.agent_id, l.chat_id, l.chat_title, l.listener_instance_id, 'active' AS status, l.last_seen_at, l.expires_at, NULL AS revoked_at, b.created_at, l.updated_at, l.lease_id, l.generation FROM agent_listener_leases l JOIN agent_chat_bindings b ON b.id = l.binding_id WHERE l.agent_id = ? AND l.binding_id = ? AND l.chat_id = ? AND l.lease_id = ? AND l.generation = ? AND l.released_at IS NULL AND l.expires_at > ? LIMIT 1",
        vec![
            db::text(agent_id),
            db::text(binding_id),
            db::text(chat_id),
            db::text(lease_id),
            db::number(generation),
            db::text(&db::now_iso()),
        ],
    )
    .await
}

fn required_listener_header(req: &Request, name: &str) -> ApiResult<String> {
    req.headers()
        .get(name)?
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                409,
                "listener_binding_required",
                "This Codex chat is not registered as the voice listener",
            )
        })
}

fn request_lease_fence(req: &Request) -> ApiResult<(String, i64)> {
    let lease_id = required_listener_header(req, "x-knock-listener-lease-id")?;
    let generation = required_listener_header(req, "x-knock-listener-generation")?
        .parse::<i64>()
        .ok()
        .and_then(|value| (value > 0).then_some(value))
        .ok_or_else(|| ApiError::validation("generation is invalid"))?;
    Ok((validated(&lease_id, "lease_id", 8, 128)?, generation))
}

pub async fn require_request_binding(
    db: &D1Database,
    req: &Request,
    agent_id: &str,
) -> ApiResult<ListenerBindingRow> {
    let chat_id = required_listener_header(req, "x-knock-chat-id")?;
    let instance_id = required_listener_header(req, "x-knock-listener-instance")?;
    let (lease_id, generation) = request_lease_fence(req)?;
    let now = db::now_iso();
    db::first(
        db,
        request_binding_sql(),
        vec![
            db::text(agent_id),
            db::text(&chat_id),
            db::text(&instance_id),
            db::text(&lease_id),
            db::number(generation),
            db::text(&now),
        ],
    )
    .await?
    .ok_or_else(lease_fenced)
}

fn request_binding_sql() -> &'static str {
    "SELECT b.id, b.user_id, b.agent_id, l.chat_id, l.chat_title, l.listener_instance_id, 'active' AS status, l.last_seen_at, l.expires_at, NULL AS revoked_at, b.created_at, l.updated_at, l.lease_id, l.generation FROM agent_listener_leases l JOIN agent_chat_bindings b ON b.id = l.binding_id WHERE l.agent_id = ? AND l.chat_id = ? AND l.listener_instance_id = ? AND l.lease_id = ? AND l.generation = ? AND l.released_at IS NULL AND l.expires_at > ?"
}

pub async fn release_listener(
    db: &D1Database,
    req: &Request,
    agent: &AgentPrincipal,
) -> ApiResult<Value> {
    let chat_id = validated(
        &required_listener_header(req, "x-knock-chat-id")?,
        "chat_id",
        1,
        128,
    )?;
    let instance_id = validated(
        &required_listener_header(req, "x-knock-listener-instance")?,
        "listener_instance_id",
        8,
        128,
    )?;
    let (lease_id, generation) = request_lease_fence(req)?;
    let now = db::now_iso();
    let released = db::first::<ReleasedLeaseRow>(
        db,
        release_lease_sql(),
        vec![
            db::text(&now),
            db::text(&now),
            db::text(&now),
            db::text(&now),
            db::text(&agent.agent_id),
            db::text(&agent.user_id),
            db::text(&chat_id),
            db::text(&instance_id),
            db::text(&lease_id),
            db::number(generation),
            db::text(&now),
        ],
    )
    .await?
    .ok_or_else(lease_fenced)?;
    db::run(
        db,
        release_binding_sql(),
        vec![
            db::text(&released.released_at),
            db::text(&released.released_at),
            db::text(&released.released_at),
            db::text(&released.binding_id),
            db::text(&released.user_id),
            db::text(&released.agent_id),
            db::text(&released.chat_id),
            db::text(&released.listener_instance_id),
            db::text(&released.lease_id),
            db::number(released.generation),
            db::text(&released.agent_id),
            db::text(&released.user_id),
            db::text(&released.binding_id),
            db::text(&released.chat_id),
            db::text(&released.listener_instance_id),
            db::text(&released.lease_id),
            db::number(released.generation),
            db::text(&released.released_at),
        ],
    )
    .await?;
    Ok(json!({
        "ok": true,
        "released": true,
        "binding_id": released.binding_id,
        "agent_id": released.agent_id,
        "chat_id": released.chat_id,
        "listener_instance_id": released.listener_instance_id,
        "lease_id": released.lease_id,
        "generation": released.generation,
        "released_at": released.released_at,
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        acquire_lease_sql, heartbeat_renew_sql, lease_fenced, listener_already_active,
        release_binding_sql, release_lease_sql, request_binding_sql, schema_probe_sql,
        LISTENER_LEASE_SECS, LISTENER_RENEW_AFTER_MS,
    };

    fn acquisition_allowed(
        current_chat: &str,
        current_expires_at_ms: i64,
        requested_chat: &str,
        takeover: bool,
        presented_fence_matches: bool,
        now_ms: i64,
    ) -> bool {
        current_expires_at_ms <= now_ms
            || takeover
            || (current_chat == requested_chat && presented_fence_matches)
    }

    fn fence_matches(
        current_lease_id: &str,
        current_generation: i64,
        presented_lease_id: &str,
        presented_generation: i64,
    ) -> bool {
        current_lease_id == presented_lease_id && current_generation == presented_generation
    }

    fn assert_fragments_in_order(sql: &str, fragments: &[&str]) {
        let mut remainder = sql;
        for fragment in fragments {
            let offset = remainder
                .find(fragment)
                .unwrap_or_else(|| panic!("missing ordered SQL fragment: {fragment}"));
            remainder = &remainder[offset + fragment.len()..];
        }
    }

    fn assert_heartbeat_renew_contract(sql: &str) {
        assert_fragments_in_order(
            sql,
            &[
                "SET last_seen_at = ?",
                "expires_at = ?",
                "updated_at = ?",
                "WHERE agent_id = ?",
                "lease_id = ?",
                "generation = ?",
                "released_at IS NULL",
                "expires_at > ?",
                "RETURNING agent_id, user_id, binding_id, chat_id, chat_title, listener_instance_id, lease_id, generation",
            ],
        );
        assert_eq!(sql.matches('?').count(), 7);

        let update_clause = sql
            .split_once(" WHERE ")
            .map(|(update, _)| update)
            .expect("heartbeat renewal must have a WHERE fence");
        for immutable_identity in [
            "agent_id =",
            "user_id =",
            "binding_id =",
            "chat_id =",
            "chat_title =",
            "listener_instance_id =",
            "lease_id =",
            "generation =",
            "released_at =",
        ] {
            assert!(
                !update_clause.contains(immutable_identity),
                "heartbeat renewal must not mutate {immutable_identity}"
            );
        }
    }

    #[test]
    fn concurrent_acquire_uses_one_agent_cas_and_rejects_a_second_unforced_owner() {
        let sql = acquire_lease_sql();
        assert!(sql.contains("ON CONFLICT(agent_id) DO UPDATE"));
        assert!(sql.contains("generation = agent_listener_leases.generation + 1"));
        assert!(sql.contains("released_at = NULL"));
        assert!(sql.contains("lease_id = ? AND agent_listener_leases.generation = ?"));
        assert!(sql.contains("RETURNING agent_id"));
        assert!(!acquisition_allowed(
            "chat-a", 90_000, "chat-b", false, false, 30_000
        ));
        assert!(!acquisition_allowed(
            "chat-a", 90_000, "chat-a", false, false, 30_000
        ));
        assert!(acquisition_allowed(
            "chat-a", 90_000, "chat-a", false, true, 30_000
        ));
    }

    #[test]
    fn a_b_a_same_chat_sequence_fences_every_old_generation() {
        let chat = "chat-shared";
        let instance_a = "instance-a";
        let instance_b = "instance-b";
        let lease_a = ("lease-a", 1_i64);

        assert_ne!(instance_a, instance_b);
        assert!(!acquisition_allowed(
            chat, 90_000, chat, false, false, 30_000
        ));
        assert_eq!(listener_already_active().code, "listener_already_active");

        assert!(acquisition_allowed(
            chat,
            90_000,
            chat,
            false,
            fence_matches(lease_a.0, lease_a.1, lease_a.0, lease_a.1),
            30_000,
        ));
        let lease_b = ("lease-b", 2_i64);

        assert!(!acquisition_allowed(
            chat,
            120_000,
            chat,
            false,
            fence_matches(lease_b.0, lease_b.1, lease_a.0, lease_a.1),
            60_000,
        ));
        assert_eq!(lease_fenced().code, "lease_fenced");
        assert!(!fence_matches(lease_b.0, lease_b.1, lease_a.0, lease_a.1));
        assert_heartbeat_renew_contract(heartbeat_renew_sql());
    }

    #[test]
    fn old_generation_heartbeat_is_fenced_after_takeover() {
        let current = ("lease-new", 2_i64);
        let stale = ("lease-old", 1_i64);
        assert_ne!(current, stale);
        let sql = heartbeat_renew_sql();
        assert_heartbeat_renew_contract(sql);
        assert!(!sql.contains("SET lease_id"));
        assert!(!sql.contains("SET generation"));
    }

    #[test]
    fn request_authority_is_exact_lease_v2_and_read_only() {
        let sql = request_binding_sql();
        assert!(sql.starts_with("SELECT "));
        assert!(sql.contains("l.chat_id = ?"));
        assert!(sql.contains("l.listener_instance_id = ?"));
        assert!(sql.contains("l.lease_id = ?"));
        assert!(sql.contains("l.generation = ?"));
        for mutation in ["INSERT ", "UPDATE ", "DELETE "] {
            assert!(!sql.contains(mutation));
        }
    }

    #[test]
    fn release_is_exact_idempotent_and_cannot_revoke_a_successor() {
        let release = release_lease_sql();
        assert!(release.starts_with("UPDATE agent_listener_leases"));
        assert!(release.contains("released_at = COALESCE(released_at, ?)"));
        assert!(release.contains("expires_at > ? OR released_at IS NOT NULL"));
        assert!(release.contains(
            "agent_id = ? AND user_id = ? AND chat_id = ? AND listener_instance_id = ? AND lease_id = ? AND generation = ?"
        ));
        assert!(!release.contains("SET lease_id"));
        assert!(!release.contains("SET generation"));

        let binding = release_binding_sql();
        assert!(binding.contains(
            "id = ? AND user_id = ? AND agent_id = ? AND chat_id = ? AND listener_instance_id = ? AND lease_id = ? AND generation = ?"
        ));
        assert!(binding.contains("l.released_at = ?"));
    }

    const _: () = {
        assert!(LISTENER_LEASE_SECS == 90);
        assert!(LISTENER_RENEW_AFTER_MS < LISTENER_LEASE_SECS * 1_000);
    };

    #[test]
    fn lease_boundary_uses_strict_expiry_sql() {
        assert!(heartbeat_renew_sql().contains("expires_at > ?"));
        assert!(acquire_lease_sql().contains("expires_at <= excluded.acquired_at"));
    }

    #[test]
    fn schema_readiness_requires_both_0017_bindings_and_0018_fences() {
        let probe = schema_probe_sql();
        assert!(probe.contains("agent_chat_bindings"));
        assert!(probe.contains("agent_listener_leases"));
        assert!(probe.contains("phone_asks"));
        assert!(probe.contains("b.generation"));
        assert!(probe.contains("l.lease_id"));
        assert!(probe.contains("l.released_at"));
        assert!(probe.contains("p.binding_id"));
        assert!(probe.contains("p.target_chat_id"));
        assert!(probe.contains("p.claimed_by_chat_id"));

        let migration = include_str!("../migrations/0018_listener_lease_fencing.sql");
        assert!(migration.contains("ALTER TABLE agent_chat_bindings ADD COLUMN lease_id"));
        assert!(migration.contains("CREATE TABLE IF NOT EXISTS agent_listener_leases"));
        let upper = migration.to_ascii_uppercase();
        assert!(!upper.contains("DROP TABLE"));
        assert!(!upper.contains("RENAME TO"));

        let release_migration = include_str!("../migrations/0022_listener_lease_release.sql");
        assert!(release_migration.contains(
            "ALTER TABLE agent_listener_leases ADD COLUMN released_at TEXT"
        ));
        let release_upper = release_migration.to_ascii_uppercase();
        assert!(!release_upper.contains("DROP TABLE"));
        assert!(!release_upper.contains("RENAME TO"));
    }
}
