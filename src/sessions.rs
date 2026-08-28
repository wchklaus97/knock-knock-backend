use serde::Deserialize;
use serde_json::{Map, Value};
use worker::{D1Database, D1PreparedStatement, Env};

use crate::audit::record_audit;
use crate::auth::new_id;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::history;
use crate::models::{
    ActionInput, ActionRow, EventRow, ProgressRequest, RetrievalInput, SessionRequest, SessionRow,
    SkillAction,
};
use crate::skills::{self, action_needs_confirm, resolve_actions};

const MESSAGE_RETENTION_SECONDS: i64 = 90 * 24 * 60 * 60;
const RETRIEVAL_RETENTION_SECONDS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReportEventRequest {
    pub status: String,
    pub summary: Option<String>,
    #[serde(default, alias = "in_reply_to_ask_id")]
    pub ask_id: Option<String>,
    #[serde(default)]
    pub claim_token: Option<String>,
    #[serde(default, alias = "claim_generation")]
    pub generation: Option<i64>,
    #[serde(default)]
    pub facts: Option<Map<String, Value>>,
    #[serde(default)]
    pub actions: Option<Vec<ActionInput>>,
    pub idempotency_key: String,
    pub force_push: Option<bool>,
    #[serde(default)]
    pub retrievals: Option<Vec<RetrievalInput>>,
}

#[derive(Debug)]
struct PhoneAskAnswerClaim {
    ask_id: String,
    claim_token: String,
    generation: i64,
}

#[derive(Debug, Deserialize)]
struct PhoneAskClaimRow {
    status: String,
    claim_token: Option<String>,
    claim_deadline: Option<String>,
    claim_generation: Option<i64>,
    answered_at: Option<String>,
    reply_event_id: Option<String>,
}

fn phone_ask_answer_claim(input: &ReportEventRequest) -> ApiResult<PhoneAskAnswerClaim> {
    let ask_id = input
        .ask_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 128)
        .ok_or_else(|| {
            ApiError::new(
                400,
                "phone_ask_claim_required",
                "phone.ask requires ask_id, claim_token, and generation; legacy listeners must drain without resetting the Ask",
            )
        })?;
    let claim_token = input
        .claim_token
        .as_deref()
        .map(str::trim)
        .filter(|value| {
            (8..=128).contains(&value.len()) && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| {
            ApiError::new(
                400,
                "phone_ask_claim_required",
                "phone.ask requires ask_id, claim_token, and generation; legacy listeners must drain without resetting the Ask",
            )
        })?;
    let generation = input.generation.filter(|generation| *generation > 0).ok_or_else(|| {
        ApiError::new(
            400,
            "phone_ask_claim_required",
            "phone.ask requires ask_id, claim_token, and generation; legacy listeners must drain without resetting the Ask",
        )
    })?;
    Ok(PhoneAskAnswerClaim {
        ask_id: ask_id.to_string(),
        claim_token: claim_token.to_string(),
        generation,
    })
}

fn parse_object(raw: &str) -> Map<String, Value> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(value)) => value,
        _ => Map::new(),
    }
}

fn parse_result(raw: Option<&str>) -> Value {
    raw.and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or(Value::Null)
}

fn action_descriptor(action: &SkillAction) -> Value {
    serde_json::json!({
        "action_key": action.id,
        "title": action.title,
        "risk": action.risk,
        "confirm_required": action_needs_confirm(action),
        "payload": action.payload,
    })
}

fn row_action_descriptor(row: &ActionRow) -> Value {
    row.descriptor_json
        .as_deref()
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .unwrap_or_else(|| {
            serde_json::json!({
                "action_key": row.action_key,
                "title": row.title,
                "risk": row.risk,
                "confirm_required": row.confirm_required != 0,
                "payload": Value::Null,
            })
        })
}

fn descriptors_for_actions(actions: &[SkillAction]) -> String {
    serde_json::to_string(&actions.iter().map(action_descriptor).collect::<Vec<_>>())
        .unwrap_or_else(|_| "[]".into())
}

pub async fn get_session(db: &D1Database, id: &str) -> ApiResult<Option<SessionRow>> {
    db::first(
        db,
        "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, available_action_descriptors_json, expires_at, created_at, updated_at, archived_at, deleted_at, retention_expires_at FROM sessions WHERE id = ?",
        vec![db::text(id)],
    )
    .await
}

pub async fn get_action(db: &D1Database, id: &str) -> ApiResult<Option<ActionRow>> {
    db::first(
        db,
        "SELECT id, session_id, agent_id, action_key, title, risk, confirm_required, descriptor_json, status, result_json, claimed_at, expires_at, created_at, updated_at FROM actions WHERE id = ?",
        vec![db::text(id)],
    )
    .await
}

async fn expire_session_if_needed(db: &D1Database, row: &SessionRow) -> ApiResult<()> {
    if ["expired", "closed", "completed", "failed"]
        .iter()
        .any(|state| *state == row.state)
        || !db::is_expired(&row.expires_at)
    {
        return Ok(());
    }
    db::run(
        db,
        "UPDATE sessions SET state = 'expired', updated_at = ? WHERE id = ? AND deleted_at IS NULL AND state NOT IN ('expired', 'closed')",
        vec![db::text(&db::now_iso()), db::text(&row.id)],
    )
    .await?;
    Ok(())
}

async fn expire_action_if_needed(db: &D1Database, row: &ActionRow) -> ApiResult<()> {
    if ["completed", "done", "failed", "expired", "cancelled"]
        .iter()
        .any(|status| *status == row.status)
        || !db::is_expired(&row.expires_at)
    {
        return Ok(());
    }
    db::run(
        db,
        "UPDATE actions SET status = 'expired', updated_at = ? WHERE id = ? AND status NOT IN ('done', 'failed', 'expired', 'cancelled')",
        vec![db::text(&db::now_iso()), db::text(&row.id)],
    )
    .await?;
    Ok(())
}

pub fn session_to_api(row: &SessionRow) -> Value {
    let available_actions = db::parse_json_array(row.available_actions_json.as_deref());
    let available_action_descriptors = row
        .available_action_descriptors_json
        .as_deref()
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .unwrap_or_else(|| Value::Array(Vec::new()));
    let facts = Value::Object(parse_object(&row.facts_json));
    serde_json::json!({
        "session_id": row.id,
        "agent_id": row.agent_id,
        "user_id": row.user_id,
        "skill_id": row.skill_id,
        "state": row.state,
        "progress_status": row.progress_status,
        "progress_message": row.progress_message,
        "progress_percent": row.progress_percent,
        "chat_id": row.chat_id,
        "title": row.title,
        "summary_text": row.summary_text,
        "voice_script": row.voice_script,
        "available_actions": available_actions,
        "available_action_descriptors": available_action_descriptors,
        "facts": facts,
        "expires_at": row.expires_at,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
        "archived_at": row.archived_at,
        "deleted_at": row.deleted_at,
        "retention_expires_at": row.retention_expires_at,
    })
}

pub fn action_to_api(row: &ActionRow) -> Value {
    let result = parse_result(row.result_json.as_deref());
    let cancelled = result
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    serde_json::json!({
        "action_id": row.id,
        "session_id": row.session_id,
        "action_key": row.action_key,
        "title": row.title,
        "risk": row.risk,
        "confirm_required": row.confirm_required != 0,
        "status": row.status,
        "expires_at": row.expires_at,
        "claimed_at": row.claimed_at,
        "descriptor": row_action_descriptor(row),
        "result": result,
        "cancelled_by_user": cancelled,
    })
}

pub async fn session_api(db: &D1Database, row: SessionRow) -> ApiResult<Value> {
    expire_session_if_needed(db, &row).await?;
    let fresh = get_session(db, &row.id).await?.unwrap_or(row);
    Ok(session_to_api(&fresh))
}

pub async fn action_api(db: &D1Database, row: ActionRow) -> ApiResult<Value> {
    expire_action_if_needed(db, &row).await?;
    let fresh = get_action(db, &row.id).await?.unwrap_or(row);
    Ok(action_to_api(&fresh))
}

pub async fn get_session_by_agent_idempotency(
    db: &D1Database,
    agent_id: &str,
    idempotency_key: &str,
) -> ApiResult<Option<SessionRow>> {
    db::first(
        db,
        "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, available_action_descriptors_json, expires_at, created_at, updated_at, archived_at, deleted_at, retention_expires_at FROM sessions WHERE agent_id = ? AND idempotency_key = ?",
        vec![db::text(agent_id), db::text(idempotency_key)],
    )
    .await
}

pub async fn create_or_resume_session(
    db: &D1Database,
    agent_id: &str,
    user_id: &str,
    input: &SessionRequest,
) -> ApiResult<Value> {
    let skill = skills::get_skill(db, &input.skill_id)
        .await?
        .ok_or_else(|| ApiError::session("Unknown skill_id", 404))?;

    let chat_id = input.chat_id.clone().or_else(|| {
        input
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("chat_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });

    if let Some(session_id) = input.session_id.as_deref() {
        let existing = get_session(db, session_id)
            .await?
            .ok_or_else(|| ApiError::session("Session not found", 404))?;
        if existing.agent_id != agent_id
            || existing.user_id != user_id
            || existing.skill_id != input.skill_id
            || chat_id
                .as_deref()
                .is_some_and(|requested| existing.chat_id.as_deref() != Some(requested))
        {
            return Err(ApiError::session("Session not found", 404));
        }
        expire_session_if_needed(db, &existing).await?;
        let fresh = get_session(db, session_id).await?.unwrap_or(existing);
        if fresh.state == "expired" {
            return Err(ApiError::session("Session expired", 410));
        }
        return Ok(session_to_api(&fresh));
    }

    if let Some(idempotency_key) = input.idempotency_key.as_deref() {
        if let Some(existing) =
            get_session_by_agent_idempotency(db, agent_id, idempotency_key).await?
        {
            if existing.user_id != user_id
                || existing.skill_id != input.skill_id
                || existing.chat_id.as_deref() != chat_id.as_deref()
                || existing.deleted_at.is_some()
            {
                return Err(ApiError::conflict(
                    "Session idempotency key is bound to a different user, skill, or chat",
                ));
            }
            expire_session_if_needed(db, &existing).await?;
            return Ok(session_to_api(
                &get_session(db, &existing.id).await?.unwrap_or(existing),
            ));
        }
    }

    let session_id = new_id("ses")?;
    let now = db::now_iso();
    let expires_at = db::add_seconds_iso(skill.ttl.default_sec);
    let retention_expires_at = db::add_seconds_iso(MESSAGE_RETENTION_SECONDS);
    let facts = input.facts.clone().unwrap_or_default();
    db::run(
        db,
        "INSERT INTO sessions (id, agent_id, user_id, skill_id, state, progress_status, progress_message, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, idempotency_key, expires_at, retention_expires_at, created_at, updated_at) VALUES (?, ?, ?, ?, 'open', NULL, NULL, ?, ?, NULL, NULL, ?, NULL, ?, ?, ?, ?, ?) ON CONFLICT(agent_id, idempotency_key) DO NOTHING",
        vec![
            db::text(&session_id),
            db::text(agent_id),
            db::text(user_id),
            db::text(&input.skill_id),
            db::optional_text(input.title.as_deref()),
            db::optional_text(chat_id.as_deref()),
            db::text(&Value::Object(facts).to_string()),
            db::optional_text(input.idempotency_key.as_deref()),
            db::text(&expires_at),
            db::text(&retention_expires_at),
            db::text(&now),
            db::text(&now),
        ],
    )
    .await?;
    let created = get_session(db, &session_id).await?;
    let created = match created {
        Some(value) => value,
        None => {
            let key = input
                .idempotency_key
                .as_deref()
                .ok_or_else(|| ApiError::new(500, "session_error", "Session insert failed"))?;
            db::first::<SessionRow>(
                db,
                "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, available_action_descriptors_json, expires_at, created_at, updated_at, archived_at, deleted_at, retention_expires_at FROM sessions WHERE agent_id = ? AND idempotency_key = ?",
                vec![db::text(agent_id), db::text(key)],
            )
            .await?
            .ok_or_else(|| ApiError::new(500, "session_error", "Session insert failed"))?
        }
    };
    record_audit(
        db,
        "session.create",
        Some(user_id),
        Some(agent_id),
        Some(&created.id),
        serde_json::json!({"skill_id": input.skill_id, "title": input.title}),
    )
    .await;
    Ok(session_to_api(&created))
}

fn merge_facts(current: &str, incoming: Option<&Map<String, Value>>) -> Value {
    let mut facts = parse_object(current);
    if let Some(incoming) = incoming {
        facts.extend(incoming.clone());
    }
    Value::Object(facts)
}

fn valid_progress_status(status: &str) -> bool {
    [
        "started",
        "running",
        "blocked",
        "succeeded",
        "failed",
        "cancelled",
    ]
    .contains(&status)
}

fn progress_lease_fenced() -> ApiError {
    ApiError::new(
        409,
        "lease_fenced",
        "The Ask claim or listener generation is stale or has been superseded",
    )
}

pub(crate) fn progress_requires_ask_authority(
    session: &SessionRow,
    input: &ProgressRequest,
) -> bool {
    session.skill_id == "phone.ask"
        || input.ask_id.is_some()
        || input.claim_token.is_some()
        || input.listener_generation.is_some()
}

fn progress_claim(input: &ProgressRequest) -> ApiResult<(&str, &str, i64)> {
    match (
        input
            .ask_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        input
            .claim_token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty()),
        input
            .listener_generation
            .filter(|generation| *generation > 0),
    ) {
        (Some(ask_id), Some(claim_token), Some(generation)) => {
            Ok((ask_id, claim_token, generation))
        }
        _ => Err(progress_lease_fenced()),
    }
}

fn fenced_phone_ask_progress_sql() -> &'static str {
    "UPDATE sessions SET state = ?, progress_status = ?, progress_message = ?, progress_percent = ?, facts_json = ?, updated_at = ? WHERE id = ? AND agent_id = ? AND user_id = ? AND deleted_at IS NULL AND state NOT IN ('expired', 'closed', 'completed', 'failed') AND expires_at > ? AND EXISTS (SELECT 1 FROM phone_asks a WHERE a.id = ? AND a.session_id = sessions.id AND a.agent_id = sessions.agent_id AND a.user_id = sessions.user_id AND a.status = 'claimed' AND a.answered_at IS NULL AND a.reply_event_id IS NULL AND a.claim_token = ? AND a.claim_generation = ? AND a.claim_deadline > ? AND a.binding_id = ? AND a.target_chat_id = ? AND a.claimed_by_chat_id = ? AND COALESCE(a.conversation_id, a.target_chat_id) = ? AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = sessions.agent_id AND l.binding_id = a.binding_id AND l.chat_id = a.target_chat_id AND l.lease_id = ? AND l.generation = ? AND l.expires_at > ?))"
}

pub async fn update_progress(
    db: &D1Database,
    session: &SessionRow,
    input: &ProgressRequest,
    binding: Option<&crate::listener_bindings::ListenerBindingRow>,
) -> ApiResult<Value> {
    if !valid_progress_status(&input.status) {
        return Err(ApiError::validation("Invalid progress status"));
    }
    let ask_authority_required = progress_requires_ask_authority(session, input);
    if !ask_authority_required {
        expire_session_if_needed(db, session).await?;
    }
    let current = get_session(db, &session.id)
        .await?
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    if current.deleted_at.is_some() {
        return Err(ApiError::not_found("Session not found"));
    }
    if ask_authority_required && (current.state == "expired" || db::is_expired(&current.expires_at))
    {
        return Err(ApiError::session("Session expired", 410));
    }
    if ["expired", "closed", "completed", "failed"]
        .iter()
        .any(|state| *state == current.state)
    {
        return Err(ApiError::session(
            format!("Session is {}", current.state),
            409,
        ));
    }
    let next_state = if ["needs_user", "awaiting_confirm", "queued", "claimed"]
        .iter()
        .any(|state| *state == current.state)
    {
        current.state.clone()
    } else {
        "running".into()
    };
    let facts = merge_facts(&current.facts_json, input.facts.as_ref());
    let now = db::now_iso();
    let mut values = vec![
        db::text(&next_state),
        db::text(&input.status),
        db::optional_text(
            input
                .message
                .as_deref()
                .or(current.progress_message.as_deref()),
        ),
        input
            .percent
            .map(db::decimal)
            .unwrap_or_else(|| db::optional_decimal(current.progress_percent)),
        db::text(&facts.to_string()),
        db::text(&now),
        db::text(&current.id),
    ];
    let updated_result = if ask_authority_required {
        let binding = binding.ok_or_else(progress_lease_fenced)?;
        let (ask_id, claim_token, generation) = progress_claim(input)?;
        if generation != binding.generation
            || current.chat_id.as_deref() != Some(binding.chat_id.as_str())
        {
            return Err(progress_lease_fenced());
        }
        values.extend([
            db::text(&current.agent_id),
            db::text(&current.user_id),
            db::text(&now),
            db::text(ask_id),
            db::text(claim_token),
            db::number(generation),
            db::text(&now),
            db::text(&binding.id),
            db::text(&binding.chat_id),
            db::text(&binding.chat_id),
            db::text(&binding.chat_id),
            db::text(&binding.lease_id),
            db::number(binding.generation),
            db::text(&now),
        ]);
        db::run(db, fenced_phone_ask_progress_sql(), values).await?
    } else {
        db::run(
            db,
            "UPDATE sessions SET state = ?, progress_status = ?, progress_message = ?, progress_percent = ?, facts_json = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
            values,
        )
        .await?
    };
    if ask_authority_required && db::changes(&updated_result) != 1 {
        return Err(progress_lease_fenced());
    }
    let updated = get_session(db, &current.id)
        .await?
        .ok_or_else(|| ApiError::new(500, "progress_error", "Session update failed"))?;
    Ok(session_to_api(&updated))
}

fn should_push(
    status: &str,
    actions: &[crate::models::SkillAction],
    force_push: bool,
    phone_ask_answer: bool,
) -> bool {
    (phone_ask_answer && matches!(status, "info" | "succeeded" | "failed"))
        || status == "needs_user"
        || force_push
        || ((status == "succeeded" || status == "failed") && !actions.is_empty())
}

fn next_event_state<'a>(
    current_state: &'a str,
    status: &str,
    has_actions: bool,
    phone_ask_answer: bool,
) -> &'a str {
    match status {
        "needs_user" => "needs_user",
        "succeeded" | "failed" if has_actions => "needs_user",
        "succeeded" | "failed" => "closed",
        "info" if phone_ask_answer && has_actions => "needs_user",
        "info" if phone_ask_answer => "closed",
        "info" if current_state == "needs_user" => "needs_user",
        "info" => "running",
        _ => current_state,
    }
}

fn event_result(event: EventRow, session: Value) -> Value {
    let delivery_queued = event.pushed != 0;
    serde_json::json!({
        "event_id": event.id,
        "session": session,
        "pushed": delivery_queued,
        "delivery_queued": delivery_queued,
        "push_delivery": if delivery_queued {
            crate::push::durable_delivery_queued_value()
        } else {
            Value::Null
        },
        "summary_text": event.summary_text.unwrap_or_default(),
        "voice_script": event.voice_script.unwrap_or_default(),
        "deduped": true,
    })
}

fn reserve_phone_ask_answer_sql() -> &'static str {
    "UPDATE phone_asks SET answered_at = ?, reply_event_id = ?, updated_at = ? WHERE id = ? AND session_id = ? AND agent_id = ? AND user_id = ? AND status = 'claimed' AND answered_at IS NULL AND reply_event_id IS NULL AND claim_token = ? AND claim_generation = ? AND claim_deadline > ? AND target_chat_id = ? AND claimed_by_chat_id = ? AND COALESCE(conversation_id, target_chat_id) = ? AND NOT EXISTS (SELECT 1 FROM events WHERE session_id = ? AND idempotency_key = ?) AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = phone_asks.agent_id AND l.binding_id = phone_asks.binding_id AND l.chat_id = phone_asks.target_chat_id AND l.generation = ? AND l.expires_at > ?)"
}

fn event_insert_sql(phone_ask_answer: bool) -> &'static str {
    if phone_ask_answer {
        "INSERT INTO events (id, session_id, status, idempotency_key, payload_json, pushed, summary_text, voice_script, created_at) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ? AND user_id = ? AND deleted_at IS NULL) AND EXISTS (SELECT 1 FROM phone_asks WHERE id = ? AND reply_event_id = ? AND answered_at = ?) ON CONFLICT(session_id, idempotency_key) DO NOTHING"
    } else {
        "INSERT INTO events (id, session_id, status, idempotency_key, payload_json, pushed, summary_text, voice_script, created_at) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ? AND user_id = ? AND deleted_at IS NULL) ON CONFLICT(session_id, idempotency_key) DO NOTHING"
    }
}

async fn existing_event(
    db: &D1Database,
    session_id: &str,
    idempotency_key: &str,
) -> ApiResult<Option<EventRow>> {
    db::first(
        db,
        "SELECT id, pushed, summary_text, voice_script FROM events WHERE session_id = ? AND idempotency_key = ?",
        vec![db::text(session_id), db::text(idempotency_key)],
    )
    .await
}

async fn require_claim_links_event(
    db: &D1Database,
    current: &SessionRow,
    claim: &PhoneAskAnswerClaim,
    event_id: &str,
) -> ApiResult<()> {
    let chat_id = current
        .chat_id
        .as_deref()
        .ok_or_else(|| ApiError::conflict("phone.ask has no resolved conversation"))?;
    let linked = db::first::<PhoneAskClaimRow>(
        db,
        "SELECT id, status, claim_token, claim_deadline, claim_generation, answered_at, reply_event_id FROM phone_asks WHERE id = ? AND session_id = ? AND agent_id = ? AND user_id = ? AND target_chat_id = ? AND claimed_by_chat_id = ? AND claim_token = ? AND claim_generation = ? AND answered_at IS NOT NULL AND reply_event_id = ?",
        vec![
            db::text(&claim.ask_id),
            db::text(&current.id),
            db::text(&current.agent_id),
            db::text(&current.user_id),
            db::text(chat_id),
            db::text(chat_id),
            db::text(&claim.claim_token),
            db::number(claim.generation),
            db::text(event_id),
        ],
    )
    .await?;
    if linked.is_none() {
        return Err(ApiError::conflict(
            "Event idempotency key does not belong to this Ask claim",
        ));
    }
    Ok(())
}

pub async fn report_event(
    db: &D1Database,
    _env: &Env,
    session: &SessionRow,
    input: &ReportEventRequest,
) -> ApiResult<Value> {
    if !["info", "needs_user", "succeeded", "failed"].contains(&input.status.as_str()) {
        return Err(ApiError::validation("Invalid event status"));
    }
    expire_session_if_needed(db, session).await?;
    let current = get_session(db, &session.id)
        .await?
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    if current.deleted_at.is_some() {
        return Err(ApiError::not_found("Session not found"));
    }
    if current.state == "expired" {
        return Err(ApiError::session("Session expired", 410));
    }
    let reply_ask_id = input
        .ask_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let phone_claim = if current.skill_id == "phone.ask" {
        Some(phone_ask_answer_claim(input)?)
    } else {
        None
    };
    if let Some(previous) = existing_event(db, &current.id, &input.idempotency_key).await? {
        if let Some(claim) = phone_claim.as_ref() {
            require_claim_links_event(db, &current, claim, &previous.id).await?;
        }
        if previous.pushed != 0 {
            let summary = previous.summary_text.as_deref().unwrap_or_default();
            let request = crate::push::PushRequest {
                user_id: &current.user_id,
                session_id: Some(&current.id),
                title: current.title.as_deref().unwrap_or(&current.skill_id),
                body: summary,
                voice_script: previous.voice_script.as_deref(),
                dedupe_key: None,
                payload: serde_json::json!({
                    "event_id": previous.id.clone(),
                    "delivery_replay": true,
                }),
            };
            crate::push::ensure_session_event_notification(
                db,
                &request,
                &previous.id,
                &input.idempotency_key,
            )
            .await?;
        }
        return Ok(event_result(previous, session_api(db, current).await?));
    }
    if let Some(claim) = phone_claim.as_ref() {
        let chat_id = current
            .chat_id
            .as_deref()
            .ok_or_else(|| ApiError::conflict("phone.ask has no resolved conversation"))?;
        let matching_ask = db::first::<PhoneAskClaimRow>(
            db,
            "SELECT id, status, claim_token, claim_deadline, claim_generation, answered_at, reply_event_id FROM phone_asks WHERE id = ? AND session_id = ? AND agent_id = ? AND user_id = ? AND target_chat_id = ? AND claimed_by_chat_id = ? AND COALESCE(conversation_id, target_chat_id) = ?",
            vec![
                db::text(&claim.ask_id),
                db::text(&current.id),
                db::text(&current.agent_id),
                db::text(&current.user_id),
                db::text(chat_id),
                db::text(chat_id),
                db::text(chat_id),
            ],
        )
        .await?
        .ok_or_else(|| ApiError::conflict("Ask does not belong to this phone.ask session"))?;
        if matching_ask.answered_at.is_some() || matching_ask.reply_event_id.is_some() {
            return Err(ApiError::conflict("Ask already has an answer"));
        }
        if matching_ask.claim_token.is_none()
            || matching_ask.claim_deadline.is_none()
            || matching_ask.claim_generation.is_none()
        {
            return Err(ApiError::new(
                409,
                "legacy_ask_drain",
                "Legacy Ask claim has no fencing credentials and will not be reset implicitly",
            ));
        }
        if matching_ask.status != "claimed"
            || matching_ask.claim_token.as_deref() != Some(claim.claim_token.as_str())
            || matching_ask.claim_generation != Some(claim.generation)
            || matching_ask
                .claim_deadline
                .as_deref()
                .is_some_and(db::is_expired)
        {
            return Err(ApiError::conflict(
                "Ask claim is stale, expired, or no longer answerable",
            ));
        }
    } else if let Some(ask_id) = reply_ask_id.as_deref() {
        let matching_ask = db::first::<crate::models::PhoneAskReplyRow>(
            db,
            "SELECT id FROM phone_asks WHERE id = ? AND session_id = ? AND agent_id = ? AND user_id = ? AND status = 'claimed' AND target_chat_id = ? AND claimed_by_chat_id = ?",
            vec![
                db::text(ask_id),
                db::text(&current.id),
                db::text(&current.agent_id),
                db::text(&current.user_id),
                db::optional_text(current.chat_id.as_deref()),
                db::optional_text(current.chat_id.as_deref()),
            ],
        )
        .await?;
        if matching_ask.is_none() {
            return Err(ApiError::validation(
                "in_reply_to_ask_id does not belong to this session",
            ));
        }
    }
    let skill = skills::get_skill(db, &current.skill_id)
        .await?
        .ok_or_else(|| ApiError::new(500, "event_error", "Skill missing"))?;
    let resolved = resolve_actions(&skill, input.actions.as_deref());
    if input.status == "needs_user" && resolved.is_empty() {
        return Err(ApiError::new(
            400,
            "event_error",
            "needs_user requires actions",
        ));
    }
    let facts = merge_facts(&current.facts_json, input.facts.as_ref());
    let facts_object = facts.as_object().cloned().unwrap_or_default();
    let summary = input
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| skills::render_summary(&skill.template, &facts_object));
    let voice = skills::to_voice_script(&summary);
    let pushed = should_push(
        &input.status,
        &resolved,
        input.force_push.unwrap_or(false),
        phone_claim.is_some(),
    );
    let next_state = next_event_state(
        &current.state,
        &input.status,
        !resolved.is_empty(),
        phone_claim.is_some(),
    );
    let event_id = new_id("evt")?;
    let now = db::now_iso();
    // The event insert is the idempotency claim. Every later statement in the
    // same D1 batch is gated on this newly generated event ID. If another
    // request already owns the (session, idempotency_key) pair, its event ID
    // differs and no business mutation can run a second time.
    let mut statements: Vec<D1PreparedStatement> = Vec::new();
    let answer_reservation_index = if let Some(claim) = phone_claim.as_ref() {
        let chat_id = current
            .chat_id
            .as_deref()
            .ok_or_else(|| ApiError::conflict("phone.ask has no resolved conversation"))?;
        let index = statements.len();
        statements.push(db::prepare(
            db,
            reserve_phone_ask_answer_sql(),
            vec![
                db::text(&now),
                db::text(&event_id),
                db::text(&now),
                db::text(&claim.ask_id),
                db::text(&current.id),
                db::text(&current.agent_id),
                db::text(&current.user_id),
                db::text(&claim.claim_token),
                db::number(claim.generation),
                db::text(&now),
                db::text(chat_id),
                db::text(chat_id),
                db::text(chat_id),
                db::text(&current.id),
                db::text(&input.idempotency_key),
                db::number(claim.generation),
                db::text(&now),
            ],
        )?);
        Some(index)
    } else {
        None
    };
    let event_insert_index = statements.len();
    let event_payload = serde_json::json!({
        "facts": facts,
        "actions": resolved,
        "force_push": input.force_push.unwrap_or(false),
        "in_reply_to_ask_id": reply_ask_id.clone(),
        "claim_generation": phone_claim.as_ref().map(|claim| claim.generation),
    })
    .to_string();
    let mut event_values = vec![
        db::text(&event_id),
        db::text(&current.id),
        db::text(&input.status),
        db::text(&input.idempotency_key),
        db::text(&event_payload),
        db::number(if pushed { 1 } else { 0 }),
        db::text(&summary),
        db::text(&voice),
        db::text(&now),
        db::text(&current.id),
        db::text(&current.user_id),
    ];
    if let Some(claim) = phone_claim.as_ref() {
        event_values.push(db::text(&claim.ask_id));
        event_values.push(db::text(&event_id));
        event_values.push(db::text(&now));
    }
    statements.push(db::prepare(
        db,
        event_insert_sql(phone_claim.is_some()),
        event_values,
    )?);
    if !resolved.is_empty() {
        statements.push(db::prepare(
            db,
            "UPDATE actions SET status = 'cancelled', updated_at = ? WHERE session_id = ? AND status IN ('offered', 'pending_confirm', 'awaiting_confirm') AND EXISTS (SELECT 1 FROM sessions WHERE id = ? AND user_id = ? AND deleted_at IS NULL) AND EXISTS (SELECT 1 FROM events WHERE id = ? AND session_id = ? AND idempotency_key = ?)",
            vec![
                db::text(&now),
                db::text(&current.id),
                db::text(&current.id),
                db::text(&current.user_id),
                db::text(&event_id),
                db::text(&current.id),
                db::text(&input.idempotency_key),
            ],
        )?);
    }
    let action_keys: Vec<String> = resolved.iter().map(|action| action.id.clone()).collect();
    let action_descriptors = descriptors_for_actions(&resolved);
    let message_id = new_id("msg")?;
    let message_retention = current
        .retention_expires_at
        .clone()
        .unwrap_or_else(|| db::add_seconds_iso(MESSAGE_RETENTION_SECONDS));
    for action in &resolved {
        let ttl = if action_needs_confirm(action) {
            skill.ttl.destructive_sec
        } else {
            skill.ttl.default_sec
        };
        let action_id = new_id("act")?;
        statements.push(db::prepare(
            db,
            "INSERT INTO actions (id, session_id, agent_id, action_key, title, risk, confirm_required, descriptor_json, status, result_json, claimed_at, expires_at, created_at, updated_at) SELECT ?, ?, ?, ?, ?, ?, ?, ?, 'offered', NULL, NULL, ?, ?, ? WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ? AND user_id = ? AND deleted_at IS NULL) AND EXISTS (SELECT 1 FROM events WHERE id = ? AND session_id = ? AND idempotency_key = ?)",
            vec![
                db::text(&action_id),
                db::text(&current.id),
                db::text(&current.agent_id),
                db::text(&action.id),
                db::text(&action.title),
                db::text(&action.risk),
                db::number(if action_needs_confirm(action) { 1 } else { 0 }),
                db::text(&action_descriptor(action).to_string()),
                db::text(&db::add_seconds_iso(ttl)),
                db::text(&now),
                db::text(&now),
                db::text(&current.id),
                db::text(&current.user_id),
                db::text(&event_id),
                db::text(&current.id),
                db::text(&input.idempotency_key),
            ],
        )?);
    }
    statements.push(db::prepare(
        db,
        "UPDATE sessions SET state = ?, summary_text = ?, voice_script = ?, facts_json = ?, available_actions_json = ?, available_action_descriptors_json = ?, retention_expires_at = COALESCE(retention_expires_at, ?), updated_at = ? WHERE id = ? AND deleted_at IS NULL AND EXISTS (SELECT 1 FROM events WHERE id = ? AND session_id = ? AND idempotency_key = ?)",
        vec![
            db::text(next_state),
            db::text(&summary),
            db::text(&voice),
            db::text(&facts.to_string()),
            if action_keys.is_empty() {
                db::optional_text(None)
            } else {
                db::text(&serde_json::to_string(&action_keys)?)
            },
            if action_descriptors == "[]" {
                db::optional_text(None)
            } else {
                db::text(&action_descriptors)
            },
            db::text(&message_retention),
            db::text(&now),
            db::text(&current.id),
            db::text(&event_id),
            db::text(&current.id),
            db::text(&input.idempotency_key),
        ],
    )?);
    statements.push(db::prepare(
        db,
        "INSERT INTO session_messages (id, user_id, session_id, role, content, metadata_json, command_id, sequence, retention_expires_at, created_at) SELECT ?, ?, ?, 'agent', ?, ?, NULL, COALESCE((SELECT MAX(sequence) + 1 FROM session_messages WHERE user_id = ? AND session_id = ?), 1), ?, ? WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ? AND user_id = ? AND deleted_at IS NULL) AND EXISTS (SELECT 1 FROM events WHERE id = ? AND session_id = ? AND idempotency_key = ?)",
        vec![
            db::text(&message_id),
            db::text(&current.user_id),
            db::text(&current.id),
            db::text(&summary),
            db::text(
                &serde_json::json!({
                    "event_id": event_id.clone(),
                    "status": input.status.clone(),
                    "actions": action_keys.clone(),
                    "ask_id": reply_ask_id.clone(),
                    "claim_generation": phone_claim.as_ref().map(|claim| claim.generation),
                })
                .to_string(),
            ),
            db::text(&current.user_id),
            db::text(&current.id),
            db::text(&message_retention),
            db::text(&now),
            db::text(&current.id),
            db::text(&current.user_id),
            db::text(&event_id),
            db::text(&current.id),
            db::text(&input.idempotency_key),
        ],
    )?);
    if pushed {
        statements.extend(crate::push::prepare_session_event_notification(
            db,
            &crate::push::PushRequest {
                user_id: &current.user_id,
                session_id: Some(&current.id),
                title: current.title.as_deref().unwrap_or(&skill.skill_id),
                body: &summary,
                voice_script: Some(&voice),
                dedupe_key: None,
                payload: serde_json::json!({
                    "event_id": event_id.clone(),
                    "status": input.status.clone(),
                    "actions": action_keys.clone(),
                }),
            },
            &event_id,
            &input.idempotency_key,
        )?);
    }
    for retrieval in input.retrievals.as_deref().unwrap_or_default() {
        let title = retrieval.title.trim();
        let url = retrieval.url.trim();
        let content_hash = retrieval.content_hash.trim();
        if title.is_empty() || title.len() > 500 {
            return Err(ApiError::validation(
                "retrieval title must contain 1-500 characters",
            ));
        }
        if !(url.starts_with("https://") || url.starts_with("http://")) || url.len() > 2_048 {
            return Err(ApiError::validation("retrieval url must be an HTTP(S) URL"));
        }
        if content_hash.is_empty() || content_hash.len() > 256 {
            return Err(ApiError::validation("retrieval content_hash is required"));
        }
        if retrieval.r2_key.as_deref().is_some_and(|key| {
            let trimmed = key.trim();
            trimmed.is_empty()
                || trimmed.len() > 1_024
                || trimmed.chars().any(char::is_control)
                || !history::is_user_r2_key(&current.user_id, trimmed)
        }) {
            return Err(ApiError::validation(
                "retrieval r2_key must be a user-scoped path under users/{user_id}/retrievals/",
            ));
        }
        let retrieval_id = new_id("ret")?;
        statements.push(db::prepare(
            db,
            "INSERT OR IGNORE INTO retrieval_items (id, user_id, session_id, message_id, title, url, snippet, score, content_hash, r2_key, retention_expires_at, created_at) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM events WHERE id = ? AND session_id = ? AND idempotency_key = ?)",
            vec![
                db::text(&retrieval_id),
                db::text(&current.user_id),
                db::text(&current.id),
                db::text(&message_id),
                db::text(title),
                db::text(url),
                db::optional_text(retrieval.snippet.as_deref()),
                db::optional_decimal(retrieval.score),
                db::text(content_hash),
                db::optional_text(retrieval.r2_key.as_deref()),
                db::text(&db::add_seconds_iso(RETRIEVAL_RETENTION_SECONDS)),
                db::text(&now),
                db::text(&event_id),
                db::text(&current.id),
                db::text(&input.idempotency_key),
            ],
        )?);
    }
    statements.push(db::prepare(
        db,
        "INSERT INTO audit_logs (id, user_id, agent_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM events WHERE id = ? AND session_id = ? AND idempotency_key = ?)",
        vec![
            db::text(&new_id("aud")?),
            db::text(&current.user_id),
            db::text(&current.agent_id),
            db::text(&current.id),
            db::text(&format!("session.event.{}", input.status)),
            db::text(&serde_json::json!({"event_id": event_id, "actions": action_keys, "retrieval_count": input.retrievals.as_ref().map_or(0, Vec::len), "pushed": pushed}).to_string()),
            db::text(&now),
            db::text(&event_id),
            db::text(&current.id),
            db::text(&input.idempotency_key),
        ],
    )?);
    let results = db.batch(statements).await?;
    let reservation_failed = answer_reservation_index
        .and_then(|index| results.get(index))
        .map(db::changes)
        .is_some_and(|changes| changes == 0);
    let event_insert_failed = results
        .get(event_insert_index)
        .map(db::changes)
        .unwrap_or(0)
        == 0;
    if reservation_failed || event_insert_failed {
        if let Some(previous) = existing_event(db, &current.id, &input.idempotency_key).await? {
            if let Some(claim) = phone_claim.as_ref() {
                require_claim_links_event(db, &current, claim, &previous.id).await?;
            }
            return Ok(event_result(previous, session_api(db, current).await?));
        }
        return Err(ApiError::conflict(
            "Ask claim is stale, expired, or already answered",
        ));
    }
    let push_delivery = if pushed {
        crate::push::durable_delivery_queued_value()
    } else {
        Value::Null
    };
    let fresh = get_session(db, &current.id)
        .await?
        .ok_or_else(|| ApiError::new(500, "event_error", "Session update failed"))?;
    Ok(serde_json::json!({
        "event_id": event_id,
        "session": session_to_api(&fresh),
        "pushed": pushed,
        "delivery_queued": pushed,
        "push_delivery": push_delivery,
        "summary_text": summary,
        "voice_script": voice,
        "deduped": false,
    }))
}

pub async fn list_queued_actions(
    db: &D1Database,
    agent_id: &str,
    session_id: Option<&str>,
) -> ApiResult<Vec<ActionRow>> {
    let mut rows: Vec<ActionRow> = if let Some(session_id) = session_id {
        db::all(
            db,
            "SELECT a.id, a.session_id, a.agent_id, a.action_key, a.title, a.risk, a.confirm_required, a.descriptor_json, a.status, a.result_json, a.claimed_at, a.expires_at, a.created_at, a.updated_at FROM actions AS a JOIN sessions AS s ON s.id = a.session_id AND s.deleted_at IS NULL WHERE a.agent_id = ? AND a.session_id = ? AND a.status = 'queued' ORDER BY a.created_at ASC",
            vec![db::text(agent_id), db::text(session_id)],
        )
        .await?
    } else {
        db::all(
            db,
            "SELECT a.id, a.session_id, a.agent_id, a.action_key, a.title, a.risk, a.confirm_required, a.descriptor_json, a.status, a.result_json, a.claimed_at, a.expires_at, a.created_at, a.updated_at FROM actions AS a JOIN sessions AS s ON s.id = a.session_id AND s.deleted_at IS NULL WHERE a.agent_id = ? AND a.status = 'queued' ORDER BY a.created_at ASC",
            vec![db::text(agent_id)],
        )
        .await?
    };
    for row in &rows {
        expire_action_if_needed(db, row).await?;
    }
    rows = rows
        .into_iter()
        .filter_map(|row| row.status.eq("queued").then_some(row))
        .collect();
    let mut fresh = Vec::new();
    for row in rows {
        if let Some(row) = get_action(db, &row.id).await? {
            if row.status == "queued" {
                fresh.push(row);
            }
        }
    }
    Ok(fresh)
}

pub async fn claim_actions(db: &D1Database, actions: &[ActionRow]) -> ApiResult<Vec<ActionRow>> {
    if actions.is_empty() {
        return Ok(Vec::new());
    }
    let now = db::now_iso();
    let mut statements = Vec::new();
    for action in actions {
        statements.push(db::prepare(
            db,
            "UPDATE actions SET status = 'claimed', claimed_at = ?, updated_at = ? WHERE id = ? AND status = 'queued' AND EXISTS (SELECT 1 FROM sessions WHERE id = actions.session_id AND deleted_at IS NULL)",
            vec![db::text(&now), db::text(&now), db::text(&action.id)],
        )?);
    }
    db.batch(statements).await?;
    let mut claimed = Vec::new();
    let mut sessions = Vec::new();
    for action in actions {
        if let Some(fresh) = get_action(db, &action.id).await? {
            if fresh.status == "claimed" {
                sessions.push(fresh.session_id.clone());
                claimed.push(fresh);
            }
        }
    }
    sessions.sort();
    sessions.dedup();
    if !sessions.is_empty() {
        let statements = sessions
            .iter()
            .map(|session_id| {
                db::prepare(
                    db,
                    "UPDATE sessions SET state = 'claimed', updated_at = ? WHERE id = ? AND state = 'queued' AND deleted_at IS NULL",
                    vec![db::text(&now), db::text(session_id)],
                )
            })
            .collect::<ApiResult<Vec<_>>>()?;
        db.batch(statements).await?;
    }
    Ok(claimed)
}

pub async fn pending_actions(
    db: &D1Database,
    agent_id: &str,
    session_id: Option<&str>,
    claim: bool,
) -> ApiResult<Vec<Value>> {
    // A Worker invocation is intentionally stateless. wait_ms from the Node
    // API is accepted by the route but does not hold an edge isolate open.
    let actions = list_queued_actions(db, agent_id, session_id).await?;
    let actions = if claim {
        claim_actions(db, &actions).await?
    } else {
        actions
    };
    let mut output = Vec::new();
    for row in actions {
        output.push(action_api(db, row).await?);
    }
    Ok(output)
}

async fn active_actions(db: &D1Database, session_id: &str) -> ApiResult<Vec<ActionRow>> {
    db::all(
        db,
        "SELECT a.id, a.session_id, a.agent_id, a.action_key, a.title, a.risk, a.confirm_required, a.descriptor_json, a.status, a.result_json, a.claimed_at, a.expires_at, a.created_at, a.updated_at FROM actions AS a JOIN sessions AS s ON s.id = a.session_id AND s.deleted_at IS NULL WHERE a.session_id = ? AND a.status IN ('offered', 'pending_confirm', 'awaiting_confirm') ORDER BY a.created_at ASC",
        vec![db::text(session_id)],
    )
    .await
}

pub async fn reconcile_waiting_session(db: &D1Database, row: SessionRow) -> ApiResult<SessionRow> {
    if row.deleted_at.is_some() {
        return Ok(row);
    }
    expire_session_if_needed(db, &row).await?;
    let current = get_session(db, &row.id).await?.unwrap_or(row);
    if !["needs_user", "awaiting_confirm"].contains(&current.state.as_str()) {
        return Ok(current);
    }
    for action in active_actions(db, &current.id).await? {
        expire_action_if_needed(db, &action).await?;
    }
    let active = active_actions(db, &current.id)
        .await?
        .into_iter()
        .filter(|action| {
            ["offered", "pending_confirm", "awaiting_confirm"].contains(&action.status.as_str())
        })
        .collect::<Vec<_>>();
    let active_keys = active
        .iter()
        .map(|action| action.action_key.clone())
        .collect::<Vec<_>>();
    let active_descriptors = active.iter().map(row_action_descriptor).collect::<Vec<_>>();
    let active_descriptor_json = serde_json::to_string(&active_descriptors)?;
    let stored_keys = db::parse_json_array(current.available_actions_json.as_deref());
    if active_keys.is_empty() {
        db::run(
            db,
            "UPDATE sessions SET state = 'running', available_actions_json = NULL, available_action_descriptors_json = NULL, progress_message = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL AND state IN ('needs_user', 'awaiting_confirm') AND NOT EXISTS (SELECT 1 FROM actions WHERE session_id = sessions.id AND status IN ('offered', 'pending_confirm', 'awaiting_confirm'))",
            vec![
                db::text("No actionable phone decision remains; the agent must emit a new decision."),
                db::text(&db::now_iso()),
                db::text(&current.id),
            ],
        )
        .await?;
        return Ok(get_session(db, &current.id).await?.unwrap_or(current));
    }
    let stored_descriptors = current
        .available_action_descriptors_json
        .as_deref()
        .unwrap_or("[]");
    if active_keys != stored_keys || active_descriptor_json != stored_descriptors {
        db::run(
            db,
            "UPDATE sessions SET available_actions_json = ?, available_action_descriptors_json = ?, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
            vec![
                db::text(&serde_json::to_string(&active_keys)?),
                db::text(&active_descriptor_json),
                db::text(&db::now_iso()),
                db::text(&current.id),
            ],
        )
        .await?;
        return Ok(get_session(db, &current.id).await?.unwrap_or(current));
    }
    Ok(current)
}

#[allow(dead_code)]
pub async fn list_phone_sessions(
    db: &D1Database,
    user_id: &str,
    limit: i32,
) -> ApiResult<Vec<Value>> {
    let rows: Vec<SessionRow> = db::all(
        db,
        "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, available_action_descriptors_json, expires_at, created_at, updated_at, archived_at, deleted_at, retention_expires_at FROM sessions WHERE user_id = ? AND deleted_at IS NULL ORDER BY updated_at DESC LIMIT ?",
        vec![db::text(user_id), db::number(limit.clamp(1, 200) as i64)],
    )
    .await?;
    let mut result = Vec::new();
    for row in rows {
        result.push(session_to_api(&reconcile_waiting_session(db, row).await?));
    }
    Ok(result)
}

pub async fn phone_reply(
    db: &D1Database,
    user_id: &str,
    session_id: &str,
    action_key: &str,
    utterance: Option<&str>,
) -> ApiResult<Value> {
    let session = get_session(db, session_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    if session.user_id != user_id {
        return Err(ApiError::not_found("Session not found"));
    }
    if session.deleted_at.is_some() {
        return Err(ApiError::not_found("Session not found"));
    }
    expire_session_if_needed(db, &session).await?;
    let current = get_session(db, session_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    if current.state == "expired" {
        return Err(ApiError::gone("Session expired"));
    }
    if current.deleted_at.is_some() {
        return Err(ApiError::not_found("Session not found"));
    }
    let action = db::first::<ActionRow>(
        db,
        "SELECT id, session_id, agent_id, action_key, title, risk, confirm_required, descriptor_json, status, result_json, claimed_at, expires_at, created_at, updated_at FROM actions WHERE session_id = ? AND action_key = ? AND status IN ('offered', 'pending_confirm', 'awaiting_confirm') ORDER BY created_at DESC LIMIT 1",
        vec![db::text(session_id), db::text(action_key)],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("Action not available"))?;
    expire_action_if_needed(db, &action).await?;
    let fresh = get_action(db, &action.id)
        .await?
        .ok_or_else(|| ApiError::not_found("Action not available"))?;
    if fresh.status == "expired" {
        return Err(ApiError::gone("Action expired"));
    }
    let needs_confirm = fresh.confirm_required != 0 || fresh.risk == "destructive";
    if ["pending_confirm", "awaiting_confirm"].contains(&fresh.status.as_str()) {
        return Ok(serde_json::json!({
            "session": session_api(db, current).await?,
            "action": action_to_api(&fresh),
            "needs_confirm": true,
        }));
    }
    if fresh.status != "offered" {
        return Err(ApiError::action(
            format!("Action status is {}", fresh.status),
            409,
        ));
    }
    let now = db::now_iso();
    let next_action_status = if needs_confirm {
        "pending_confirm"
    } else {
        "queued"
    };
    let next_session_state = if needs_confirm {
        "awaiting_confirm"
    } else {
        "queued"
    };
    let message_id = new_id("msg")?;
    let message_retention = current
        .retention_expires_at
        .clone()
        .unwrap_or_else(|| db::add_seconds_iso(MESSAGE_RETENTION_SECONDS));
    let mut statements = vec![db::prepare(
        db,
        "UPDATE actions SET status = ?, updated_at = ? WHERE id = ? AND status = 'offered' AND EXISTS (SELECT 1 FROM sessions WHERE id = ? AND state IN ('needs_user', 'awaiting_confirm') AND deleted_at IS NULL)",
        vec![
            db::text(next_action_status),
            db::text(&now),
            db::text(&fresh.id),
            db::text(&current.id),
        ],
    )?];
    if !needs_confirm {
        statements.push(db::prepare(
            db,
            "UPDATE actions SET status = 'cancelled', updated_at = ? WHERE session_id = ? AND id <> ? AND status IN ('offered', 'pending_confirm', 'awaiting_confirm') AND EXISTS (SELECT 1 FROM sessions WHERE id = ? AND deleted_at IS NULL)",
            vec![
                db::text(&now),
                db::text(&current.id),
                db::text(&fresh.id),
                db::text(&current.id),
            ],
        )?);
    }
    statements.push(db::prepare(
        db,
        "UPDATE sessions SET state = ?, available_actions_json = CASE WHEN ? = 1 THEN available_actions_json ELSE NULL END, available_action_descriptors_json = CASE WHEN ? = 1 THEN available_action_descriptors_json ELSE NULL END, retention_expires_at = COALESCE(retention_expires_at, ?), updated_at = ? WHERE id = ? AND state IN ('needs_user', 'awaiting_confirm') AND deleted_at IS NULL",
        vec![
            db::text(next_session_state),
            db::number(if needs_confirm { 1 } else { 0 }),
            db::number(if needs_confirm { 1 } else { 0 }),
            db::text(&message_retention),
            db::text(&now),
            db::text(&current.id),
        ],
    )?);
    statements.push(db::prepare(
        db,
        "INSERT INTO session_messages (id, user_id, session_id, role, content, metadata_json, command_id, sequence, retention_expires_at, created_at) SELECT ?, ?, ?, 'user', ?, ?, NULL, COALESCE((SELECT MAX(sequence) + 1 FROM session_messages WHERE user_id = ? AND session_id = ?), 1), ?, ? WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ? AND user_id = ? AND deleted_at IS NULL)",
        vec![
            db::text(&message_id),
            db::text(user_id),
            db::text(&current.id),
            db::text(utterance.unwrap_or(action_key)),
            db::text(
                &serde_json::json!({
                    "action_id": fresh.id.clone(),
                    "action_key": action_key,
                    "needs_confirm": needs_confirm,
                })
                .to_string(),
            ),
            db::text(user_id),
            db::text(&current.id),
            db::text(&message_retention),
            db::text(&now),
            db::text(&current.id),
            db::text(user_id),
        ],
    )?);
    let results = db.batch(statements).await?;
    if results.first().map(db::changes).unwrap_or(0) == 0 {
        return Err(ApiError::action("Action is no longer available", 409));
    }
    record_audit(
        db,
        "phone.reply",
        Some(user_id),
        Some(&current.agent_id),
        Some(&current.id),
        serde_json::json!({"action_id": fresh.id, "action_key": action_key, "needs_confirm": needs_confirm}),
    )
    .await;
    let updated_session = get_session(db, &current.id)
        .await?
        .ok_or_else(|| ApiError::new(500, "reply_error", "Session update failed"))?;
    let updated_action = get_action(db, &fresh.id)
        .await?
        .ok_or_else(|| ApiError::new(500, "reply_error", "Action update failed"))?;
    Ok(serde_json::json!({
        "session": session_to_api(&updated_session),
        "action": action_to_api(&updated_action),
        "needs_confirm": needs_confirm,
    }))
}

fn action_was_cancelled(row: &ActionRow) -> bool {
    parse_result(row.result_json.as_deref())
        .get("cancelled")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

pub async fn phone_confirm(
    db: &D1Database,
    user_id: &str,
    session_id: &str,
    action_id: &str,
    confirm: bool,
) -> ApiResult<Value> {
    let session = get_session(db, session_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    if session.user_id != user_id {
        return Err(ApiError::not_found("Session not found"));
    }
    if session.deleted_at.is_some() {
        return Err(ApiError::not_found("Session not found"));
    }
    expire_session_if_needed(db, &session).await?;
    let current = get_session(db, session_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    if current.deleted_at.is_some() {
        return Err(ApiError::not_found("Session not found"));
    }
    let action = get_action(db, action_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Action not found"))?;
    if action.session_id != current.id {
        return Err(ApiError::not_found("Action not found"));
    }
    expire_action_if_needed(db, &action).await?;
    let fresh = get_action(db, action_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Action not found"))?;
    if fresh.status == "expired" {
        return Err(ApiError::gone("Action expired"));
    }
    if !confirm
        && ["queued", "cancelled"].contains(&fresh.status.as_str())
        && action_was_cancelled(&fresh)
    {
        return Ok(serde_json::json!({
            "session": session_api(db, current).await?,
            "action": action_to_api(&fresh),
            "needs_confirm": false,
        }));
    }
    if confirm
        && ["queued", "cancelled"].contains(&fresh.status.as_str())
        && action_was_cancelled(&fresh)
    {
        return Err(ApiError::conflict("Action was cancelled on the phone"));
    }
    if confirm && fresh.status == "queued" {
        return Ok(serde_json::json!({
            "session": session_api(db, current).await?,
            "action": action_to_api(&fresh),
            "needs_confirm": false,
        }));
    }
    if !confirm && fresh.status == "cancelled" {
        return Ok(serde_json::json!({
            "session": session_api(db, current).await?,
            "action": action_to_api(&fresh),
            "needs_confirm": false,
        }));
    }
    if !["pending_confirm", "awaiting_confirm"].contains(&fresh.status.as_str()) {
        return Err(ApiError::conflict("Action is not awaiting confirm"));
    }
    let available = db::parse_json_array(current.available_actions_json.as_deref());
    let remaining: Vec<String> = available
        .into_iter()
        .filter(|key| key != &fresh.action_key)
        .collect();
    let queue_cancellation = !confirm && remaining.is_empty();
    let next_action_status = if confirm || queue_cancellation {
        "queued"
    } else {
        "cancelled"
    };
    let cancellation_result = serde_json::json!({
        "ok": false,
        "cancelled": true,
        "message": "User cancelled this action on the phone",
        "output": Value::Null,
    });
    let now = db::now_iso();
    let message_id = new_id("msg")?;
    let mut statements = vec![db::prepare(
        db,
        "UPDATE actions SET status = ?, result_json = CASE WHEN ? = 1 THEN ? ELSE result_json END, updated_at = ? WHERE id = ? AND status IN ('pending_confirm', 'awaiting_confirm') AND EXISTS (SELECT 1 FROM sessions WHERE id = ? AND state = 'awaiting_confirm' AND deleted_at IS NULL)",
        vec![
            db::text(next_action_status),
            db::number(if confirm { 0 } else { 1 }),
            db::text(&cancellation_result.to_string()),
            db::text(&now),
            db::text(&fresh.id),
            db::text(&current.id),
        ],
    )?];
    if confirm {
        statements.push(db::prepare(
            db,
            "UPDATE actions SET status = 'cancelled', updated_at = ? WHERE session_id = ? AND id <> ? AND status IN ('offered', 'pending_confirm', 'awaiting_confirm') AND EXISTS (SELECT 1 FROM sessions WHERE id = ? AND deleted_at IS NULL)",
            vec![
                db::text(&now),
                db::text(&current.id),
                db::text(&fresh.id),
                db::text(&current.id),
            ],
        )?);
    }
    let (next_state, available_actions, message) = if confirm {
        ("queued", None, None)
    } else if queue_cancellation {
        (
            "queued",
            None,
            Some("User cancelled this action on the phone."),
        )
    } else {
        (
            "needs_user",
            Some(serde_json::to_string(&remaining)?),
            Some("That action was cancelled. Choose another option or wait for the agent."),
        )
    };
    let available_descriptor_json = if confirm || queue_cancellation {
        None
    } else {
        let descriptors = current
            .available_action_descriptors_json
            .as_deref()
            .and_then(|value| serde_json::from_str::<Vec<Value>>(value).ok())
            .unwrap_or_default()
            .into_iter()
            .filter(|descriptor| {
                descriptor
                    .get("action_key")
                    .and_then(Value::as_str)
                    .is_some_and(|key| key != fresh.action_key)
            })
            .collect::<Vec<_>>();
        Some(serde_json::to_string(&descriptors)?)
    };
    let message_retention = current
        .retention_expires_at
        .clone()
        .unwrap_or_else(|| db::add_seconds_iso(MESSAGE_RETENTION_SECONDS));
    statements.push(db::prepare(
        db,
        "UPDATE sessions SET state = ?, available_actions_json = ?, available_action_descriptors_json = ?, progress_message = COALESCE(?, progress_message), retention_expires_at = COALESCE(retention_expires_at, ?), updated_at = ? WHERE id = ? AND state = 'awaiting_confirm' AND deleted_at IS NULL",
        vec![
            db::text(next_state),
            db::optional_text(available_actions.as_deref()),
            db::optional_text(available_descriptor_json.as_deref()),
            db::optional_text(message),
            db::text(&message_retention),
            db::text(&now),
            db::text(&current.id),
        ],
    )?);
    statements.push(db::prepare(
        db,
        "INSERT INTO session_messages (id, user_id, session_id, role, content, metadata_json, command_id, sequence, retention_expires_at, created_at) SELECT ?, ?, ?, 'user', ?, ?, NULL, COALESCE(MAX(sequence) + 1, 1), ?, ? FROM session_messages WHERE user_id = ? AND session_id = ?",
        vec![
            db::text(&message_id),
            db::text(user_id),
            db::text(&current.id),
            db::text(if confirm { "confirm" } else { "cancel" }),
            db::text(
                &serde_json::json!({
                    "action_id": fresh.id.clone(),
                    "action_key": fresh.action_key.clone(),
                    "confirm": confirm,
                })
                .to_string(),
            ),
            db::text(&message_retention),
            db::text(&now),
            db::text(user_id),
            db::text(&current.id),
        ],
    )?);
    let results = db.batch(statements).await?;
    if results.first().map(db::changes).unwrap_or(0) == 0 {
        return Err(ApiError::conflict("Action is no longer awaiting confirm"));
    }
    record_audit(
        db,
        if confirm {
            "phone.confirm"
        } else {
            "phone.cancel"
        },
        Some(user_id),
        Some(&current.agent_id),
        Some(&current.id),
        serde_json::json!({"action_id": fresh.id, "action_key": fresh.action_key}),
    )
    .await;
    let updated_session = get_session(db, &current.id)
        .await?
        .ok_or_else(|| ApiError::new(500, "confirm_error", "Session update failed"))?;
    let updated_action = get_action(db, &fresh.id)
        .await?
        .ok_or_else(|| ApiError::new(500, "confirm_error", "Action update failed"))?;
    Ok(serde_json::json!({
        "session": session_to_api(&updated_session),
        "action": action_to_api(&updated_action),
        "needs_confirm": false,
    }))
}

pub async fn submit_action_result(
    db: &D1Database,
    agent_id: &str,
    action_id: &str,
    ok: bool,
    message: Option<&str>,
    output: Option<&Map<String, Value>>,
) -> ApiResult<Value> {
    let action = get_action(db, action_id)
        .await?
        .ok_or_else(|| ApiError::action("Action not found", 404))?;
    if action.agent_id != agent_id {
        return Err(ApiError::action("Action not found", 404));
    }
    let session = get_session(db, &action.session_id)
        .await?
        .filter(|row| row.deleted_at.is_none())
        .ok_or_else(|| ApiError::action("Action not found", 404))?;
    expire_action_if_needed(db, &action).await?;
    let fresh = get_action(db, action_id)
        .await?
        .ok_or_else(|| ApiError::action("Action not found", 404))?;
    if fresh.status == "expired" {
        return Err(ApiError::action("Action expired", 410));
    }
    if ["done", "failed"].contains(&fresh.status.as_str()) {
        return Ok(action_to_api(&fresh));
    }
    if !["claimed", "queued"].contains(&fresh.status.as_str()) {
        return Err(ApiError::action(
            format!("Action status is {}", fresh.status),
            409,
        ));
    }
    if ok && action_was_cancelled(&fresh) {
        return Err(ApiError::conflict(
            "Action was cancelled by the user; do not execute it",
        ));
    }
    let previous = parse_result(fresh.result_json.as_deref());
    let result = serde_json::json!({
        "ok": ok,
        "cancelled": previous.get("cancelled").and_then(Value::as_bool).unwrap_or(false),
        "message": message.or_else(|| previous.get("message").and_then(Value::as_str)),
        "output": output,
    });
    let status = if ok { "done" } else { "failed" };
    let now = db::now_iso();
    let statements = vec![
        db::prepare(
            db,
        "UPDATE actions SET status = ?, result_json = ?, updated_at = ? WHERE id = ? AND status IN ('claimed', 'queued') AND EXISTS (SELECT 1 FROM sessions WHERE id = actions.session_id AND deleted_at IS NULL)",
            vec![
                db::text(status),
                db::text(&result.to_string()),
                db::text(&now),
                db::text(action_id),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE sessions SET state = ?, available_actions_json = NULL, available_action_descriptors_json = NULL, updated_at = ? WHERE id = ? AND deleted_at IS NULL",
            vec![
                db::text(if ok { "running" } else { "closed" }),
                db::text(&now),
                db::text(&session.id),
            ],
        )?,
    ];
    let results = db.batch(statements).await?;
    if results.first().map(db::changes).unwrap_or(0) == 0 {
        let after = get_action(db, action_id)
            .await?
            .ok_or_else(|| ApiError::action("Action not found", 404))?;
        if ["done", "failed"].contains(&after.status.as_str()) {
            return Ok(action_to_api(&after));
        }
        return Err(ApiError::action(
            format!("Action status is {}", after.status),
            409,
        ));
    }
    record_audit(
        db,
        &format!("agent.action_result.{status}"),
        None,
        Some(agent_id),
        Some(&fresh.session_id),
        serde_json::json!({"action_id": action_id, "ok": ok}),
    )
    .await;
    let result = get_action(db, action_id)
        .await?
        .ok_or_else(|| ApiError::action("Action not found", 404))?;
    Ok(action_to_api(&result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn progress_status_allowlist_rejects_unknown_values() {
        assert!(valid_progress_status("started"));
        assert!(valid_progress_status("running"));
        assert!(valid_progress_status("cancelled"));
        assert!(!valid_progress_status("needs_user"));
        assert!(!valid_progress_status("finished"));
    }

    #[test]
    fn progress_request_is_fail_closed_and_ask_claim_is_all_or_nothing() {
        let request: ProgressRequest = serde_json::from_value(json!({
            "status": "running",
            "ask_id": "ask_phone_0001",
            "claim_token": "claim_phone_0001",
            "listener_generation": 7
        }))
        .unwrap();
        assert_eq!(
            progress_claim(&request).unwrap(),
            ("ask_phone_0001", "claim_phone_0001", 7)
        );

        let partial: ProgressRequest = serde_json::from_value(json!({
            "status": "running",
            "ask_id": "ask_phone_0001"
        }))
        .unwrap();
        let error = progress_claim(&partial).unwrap_err();
        assert_eq!(error.status, 409);
        assert_eq!(error.code, "lease_fenced");

        assert!(serde_json::from_value::<ProgressRequest>(json!({
            "status": "running",
            "unknown": true
        }))
        .is_err());
    }

    #[test]
    fn phone_ask_progress_update_is_one_exact_claim_and_listener_cas() {
        let sql = fenced_phone_ask_progress_sql();
        for predicate in [
            "a.claim_token = ?",
            "a.claim_generation = ?",
            "a.claim_deadline > ?",
            "a.answered_at IS NULL",
            "a.binding_id = ?",
            "a.target_chat_id = ?",
            "a.claimed_by_chat_id = ?",
            "l.lease_id = ?",
            "l.generation = ?",
            "l.expires_at > ?",
        ] {
            assert!(sql.contains(predicate));
        }
    }

    #[test]
    fn terminal_phone_ask_answers_push_even_without_actions() {
        assert!(should_push("succeeded", &[], false, true));
        assert!(should_push("failed", &[], false, true));
        assert!(should_push("info", &[], false, true));
        assert!(!should_push("succeeded", &[], false, false));
        assert!(!should_push("info", &[], false, false));
        assert_eq!(next_event_state("running", "info", false, true), "closed");
        assert_eq!(
            next_event_state("running", "info", true, true),
            "needs_user"
        );
        assert_eq!(
            next_event_state("needs_user", "info", false, false),
            "needs_user"
        );
    }

    #[test]
    fn facts_merge_preserves_existing_values_and_applies_new_values() {
        let incoming = serde_json::from_value(json!({
            "status": "running",
            "percent": 25
        }))
        .unwrap();
        let merged = merge_facts(r#"{"service":"api","status":"started"}"#, Some(&incoming));
        assert_eq!(merged["service"], "api");
        assert_eq!(merged["status"], "running");
        assert_eq!(merged["percent"], 25);
    }

    #[test]
    fn malformed_facts_are_safe_empty_objects() {
        assert!(parse_object("not-json").is_empty());
        assert_eq!(parse_result(Some("null")), Value::Null);
        assert_eq!(parse_result(Some("not-json")), Value::Null);
    }

    #[test]
    fn phone_ask_event_accepts_canonical_claim_fields_and_legacy_ask_id_alias() {
        let canonical: ReportEventRequest = serde_json::from_value(json!({
            "status": "succeeded",
            "ask_id": "ask_phone_0001",
            "claim_token": "claim_phone_0001",
            "generation": 7,
            "idempotency_key": "event-phone-0001"
        }))
        .unwrap();
        let claim = phone_ask_answer_claim(&canonical).unwrap();
        assert_eq!(claim.ask_id, "ask_phone_0001");
        assert_eq!(claim.generation, 7);

        let legacy_alias: ReportEventRequest = serde_json::from_value(json!({
            "status": "succeeded",
            "in_reply_to_ask_id": "ask_phone_0001",
            "claim_token": "claim_phone_0001",
            "claim_generation": 7,
            "idempotency_key": "event-phone-0001"
        }))
        .unwrap();
        assert_eq!(
            phone_ask_answer_claim(&legacy_alias).unwrap().ask_id,
            "ask_phone_0001"
        );
    }

    #[test]
    fn legacy_phone_ask_event_without_fencing_credentials_is_rejected() {
        let legacy: ReportEventRequest = serde_json::from_value(json!({
            "status": "succeeded",
            "in_reply_to_ask_id": "ask_phone_0001",
            "idempotency_key": "event-phone-0001"
        }))
        .unwrap();
        let error = phone_ask_answer_claim(&legacy).unwrap_err();
        assert_eq!(error.status, 400);
        assert_eq!(error.code, "phone_ask_claim_required");
    }

    #[test]
    fn answer_reservation_is_single_use_and_generation_fenced() {
        let sql = reserve_phone_ask_answer_sql();
        assert!(sql.contains("answered_at IS NULL"));
        assert!(sql.contains("reply_event_id IS NULL"));
        assert!(sql.contains("claim_token = ?"));
        assert!(sql.contains("claim_generation = ?"));
        assert!(sql.contains("claim_deadline > ?"));
        assert!(sql.contains("agent_listener_leases"));
        assert!(sql.contains("l.generation = ?"));
        assert!(event_insert_sql(true).contains("reply_event_id = ?"));
        assert!(!event_insert_sql(false).contains("phone_asks"));
    }

    #[test]
    fn event_and_ask_answer_notification_intent_share_the_event_commit_gate() {
        assert!(event_insert_sql(true).contains("reply_event_id = ?"));
        assert!(
            crate::push::session_event_push_insert_sql().contains("EXISTS (SELECT 1 FROM events")
        );
        assert!(crate::push::session_event_notification_insert_sql()
            .contains("EXISTS (SELECT 1 FROM events"));
        assert!(crate::push::session_event_notification_insert_sql()
            .contains("ON CONFLICT(topic, idempotency_key) DO NOTHING"));
        assert_eq!(
            crate::push::durable_delivery_queued_value()["delivery_queued"],
            true
        );
    }
}
