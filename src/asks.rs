use serde::Deserialize;
use serde_json::{json, Map, Value};
use worker::D1Database;

use crate::auth::new_id;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::listener_bindings::{self, ListenerBindingRow};
use crate::models::{SessionMessageRow, SessionRequest};
use crate::sessions;

/// Exclusive Ask listening window. Exact last_seen age 90.000s is not listening
/// (`seen_ms + 90_000 > now_ms`), matching iOS `age < 90` / HTTP 409.
pub const LISTENING_WINDOW_SECS: i64 = 90;
const ASK_TTL_SECS: i64 = 86_400;
const ASK_CLAIM_TTL_SECS: i64 = listener_bindings::LISTENER_LEASE_SECS;
const MAX_TRANSCRIPT_CHARS: usize = 2_000;
const ASK_CONTEXT_MESSAGE_LIMIT: i64 = 12;
const MESSAGE_RETENTION_SECS: i64 = 90 * 24 * 60 * 60;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAskRequest {
    pub transcript: String,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub client_turn_id: Option<String>,
    #[serde(default)]
    pub idempotency_key: Option<String>,
    #[serde(default)]
    pub binding_id: Option<String>,
    #[serde(default)]
    pub lease_id: Option<String>,
    #[serde(default)]
    pub generation: Option<i64>,
    #[serde(default)]
    pub target_chat_id: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AskMessageSequenceRow {
    sequence: i64,
}

#[derive(Debug, Clone, Deserialize)]
struct AskRow {
    id: String,
    user_id: String,
    agent_id: String,
    transcript: String,
    locale: Option<String>,
    idempotency_key: String,
    client_turn_id: Option<String>,
    conversation_id: Option<String>,
    session_id: Option<String>,
    binding_id: Option<String>,
    lease_id: Option<String>,
    listener_generation: Option<i64>,
    target_chat_id: Option<String>,
    claimed_by_chat_id: Option<String>,
    status: String,
    claimed_at: Option<String>,
    claim_token: Option<String>,
    claim_deadline: Option<String>,
    claim_generation: Option<i64>,
    answered_at: Option<String>,
    reply_event_id: Option<String>,
    attempt_count: i64,
    expires_at: String,
    created_at: String,
}

#[derive(Debug, Deserialize)]
struct AgentSeenRow {
    id: String,
    #[allow(dead_code)]
    user_id: String,
    label: String,
    last_seen_at: Option<String>,
}

#[derive(Debug, Clone)]
struct FrozenAskFence {
    binding_id: String,
    lease_id: String,
    generation: i64,
    target_chat_id: String,
}

fn ask_listener_fence_mismatch(message: impl Into<String>) -> ApiError {
    ApiError::new(409, "ask_listener_fence_mismatch", message)
}

fn validate_fence_value(value: &str, field: &str) -> ApiResult<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() > 128 || trimmed.chars().any(char::is_control) {
        return Err(ask_listener_fence_mismatch(format!(
            "{field} is missing or invalid"
        )));
    }
    Ok(trimmed.to_string())
}

fn frozen_ask_fence(input: &CreateAskRequest) -> ApiResult<Option<FrozenAskFence>> {
    let supplied = [
        input.binding_id.is_some(),
        input.lease_id.is_some(),
        input.generation.is_some(),
        input.target_chat_id.is_some(),
    ];
    if supplied.iter().all(|value| !value) {
        return Ok(None);
    }
    if supplied.iter().any(|value| !value) {
        return Err(ask_listener_fence_mismatch(
            "binding_id, lease_id, generation, and target_chat_id must be supplied together",
        ));
    }
    let generation = input
        .generation
        .filter(|generation| *generation > 0)
        .ok_or_else(|| ask_listener_fence_mismatch("generation must be positive"))?;
    Ok(Some(FrozenAskFence {
        binding_id: validate_fence_value(
            input.binding_id.as_deref().expect("checked above"),
            "binding_id",
        )?,
        lease_id: validate_fence_value(
            input.lease_id.as_deref().expect("checked above"),
            "lease_id",
        )?,
        generation,
        target_chat_id: validate_fence_value(
            input.target_chat_id.as_deref().expect("checked above"),
            "target_chat_id",
        )?,
    }))
}

pub fn validate_transcript(transcript: &str) -> ApiResult<String> {
    let trimmed = transcript.trim();
    if trimmed.is_empty() {
        return Err(ApiError::validation("transcript is required"));
    }
    if trimmed.chars().count() > MAX_TRANSCRIPT_CHARS {
        return Err(ApiError::validation("transcript is too long"));
    }
    Ok(trimmed.to_string())
}

pub fn validate_idempotency_key(key: &str) -> ApiResult<String> {
    let trimmed = key.trim();
    if trimmed.len() < 8 || trimmed.len() > 128 {
        return Err(ApiError::validation(
            "idempotency_key must be 8 to 128 characters",
        ));
    }
    Ok(trimmed.to_string())
}

pub fn validate_client_turn_id(value: &str) -> ApiResult<String> {
    let trimmed = value.trim();
    if trimmed.len() < 8 || trimmed.len() > 128 || trimmed.chars().any(char::is_control) {
        return Err(ApiError::validation(
            "client_turn_id must be 8 to 128 characters",
        ));
    }
    Ok(trimmed.to_string())
}

pub fn validate_locale(locale: Option<&str>) -> ApiResult<Option<String>> {
    let Some(raw) = locale.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if raw.len() < 2 || raw.len() > 35 {
        return Err(ApiError::validation("locale is invalid"));
    }
    Ok(Some(raw.to_string()))
}

pub fn validate_session_id(session_id: Option<&str>) -> ApiResult<Option<String>> {
    let Some(raw) = session_id.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if raw.len() > 128 {
        return Err(ApiError::validation("session_id is invalid"));
    }
    Ok(Some(raw.to_string()))
}

pub async fn touch_agent_seen(db: &D1Database, agent_id: &str) -> ApiResult<()> {
    db::run(
        db,
        "UPDATE agents SET last_seen_at = ? WHERE id = ?",
        vec![db::text(&db::now_iso()), db::text(agent_id)],
    )
    .await?;
    Ok(())
}

const ASK_COLUMNS: &str = "id, user_id, agent_id, transcript, locale, idempotency_key, client_turn_id, conversation_id, session_id, binding_id, lease_id, listener_generation, target_chat_id, claimed_by_chat_id, status, claimed_at, claim_token, claim_deadline, claim_generation, answered_at, reply_event_id, attempt_count, expires_at, created_at";

async fn get_ask_by_id(db: &D1Database, ask_id: &str) -> ApiResult<Option<AskRow>> {
    let sql = format!("SELECT {ASK_COLUMNS} FROM phone_asks WHERE id = ?");
    db::first(db, &sql, vec![db::text(ask_id)]).await
}

async fn find_existing_ask(
    db: &D1Database,
    user_id: &str,
    agent_id: &str,
    client_turn_id: Option<&str>,
    idempotency_key: &str,
) -> ApiResult<Option<AskRow>> {
    let by_turn: Option<AskRow> = if let Some(client_turn_id) = client_turn_id {
        let sql = format!(
            "SELECT {ASK_COLUMNS} FROM phone_asks WHERE user_id = ? AND agent_id = ? AND client_turn_id = ?"
        );
        db::first(
            db,
            &sql,
            vec![
                db::text(user_id),
                db::text(agent_id),
                db::text(client_turn_id),
            ],
        )
        .await?
    } else {
        None
    };
    let sql = format!(
        "SELECT {ASK_COLUMNS} FROM phone_asks WHERE user_id = ? AND agent_id = ? AND idempotency_key = ?"
    );
    let by_idempotency: Option<AskRow> = db::first(
        db,
        &sql,
        vec![
            db::text(user_id),
            db::text(agent_id),
            db::text(idempotency_key),
        ],
    )
    .await?;
    if by_turn
        .as_ref()
        .zip(by_idempotency.as_ref())
        .is_some_and(|(turn, legacy)| turn.id != legacy.id)
    {
        return Err(ApiError::conflict(
            "client_turn_id and idempotency_key identify different Asks",
        ));
    }
    Ok(by_turn.or(by_idempotency))
}

fn validate_existing_ask(
    existing: &AskRow,
    transcript: &str,
    locale: Option<&str>,
    requested_session_id: Option<&str>,
    client_turn_id: Option<&str>,
    requested_fence: Option<&FrozenAskFence>,
) -> ApiResult<()> {
    if existing.transcript != transcript {
        return Err(ApiError::conflict(
            "Ask identity was already used with a different transcript",
        ));
    }
    if existing.locale.as_deref() != locale {
        return Err(ApiError::conflict(
            "Ask identity was already used with a different locale",
        ));
    }
    if requested_session_id.is_some() && existing.session_id.as_deref() != requested_session_id {
        return Err(ApiError::conflict(
            "Ask identity was already used with a different session_id",
        ));
    }
    if existing.client_turn_id.as_deref() != client_turn_id {
        return Err(ApiError::conflict(
            "idempotency_key belongs to a different client_turn_id",
        ));
    }
    let persisted_fence_is_complete = existing.binding_id.is_some()
        && existing.lease_id.is_some()
        && existing.listener_generation.is_some()
        && existing.target_chat_id.is_some();
    if !persisted_fence_is_complete {
        if requested_fence.is_some() {
            return Err(ApiError::new(
                409,
                "legacy_ask_fence_unavailable",
                "Legacy Ask has no complete frozen listener fence and will not be rebound implicitly",
            ));
        }
        return Ok(());
    }
    let requested_fence = requested_fence.ok_or_else(|| {
        ask_listener_fence_mismatch(
            "This client turn already has a frozen listener fence; resend the original binding_id, lease_id, generation, and target_chat_id",
        )
    })?;
    if existing.binding_id.as_deref() != Some(requested_fence.binding_id.as_str())
        || existing.lease_id.as_deref() != Some(requested_fence.lease_id.as_str())
        || existing.listener_generation != Some(requested_fence.generation)
        || existing.target_chat_id.as_deref() != Some(requested_fence.target_chat_id.as_str())
    {
        return Err(ask_listener_fence_mismatch(
            "Ask fence does not match the frozen listener target for this client turn",
        ));
    }
    Ok(())
}

async fn validate_ask_session_binding(
    db: &D1Database,
    session_id: &str,
    user_id: &str,
    agent_id: &str,
    target_chat_id: &str,
) -> ApiResult<()> {
    let session = sessions::get_session(db, session_id)
        .await?
        .ok_or_else(|| ApiError::conflict("Ask session no longer exists"))?;
    validate_ask_session_row(&session, user_id, agent_id, target_chat_id)
}

fn ask_session_is_expired(state: &str, expires_at: &str) -> bool {
    state == "expired" || db::is_expired(expires_at)
}

fn validate_ask_session_row(
    session: &crate::models::SessionRow,
    user_id: &str,
    agent_id: &str,
    target_chat_id: &str,
) -> ApiResult<()> {
    if session.user_id != user_id
        || session.agent_id != agent_id
        || session.skill_id != "phone.ask"
        || session.chat_id.as_deref() != Some(target_chat_id)
        || session.deleted_at.is_some()
    {
        return Err(ApiError::conflict(
            "Ask identity is bound to a different user, agent, skill, or target chat",
        ));
    }
    if ask_session_is_expired(&session.state, &session.expires_at) {
        return Err(ApiError::session("Session expired", 410));
    }
    Ok(())
}

fn insert_atomic_ask_session_sql() -> &'static str {
    "INSERT INTO sessions (id, agent_id, user_id, skill_id, state, progress_status, progress_message, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, idempotency_key, expires_at, retention_expires_at, created_at, updated_at) SELECT ?, ?, ?, 'phone.ask', 'open', NULL, NULL, ?, ?, NULL, NULL, ?, NULL, ?, ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = ? AND l.binding_id = ? AND l.chat_id = ? AND l.lease_id = ? AND l.generation = ? AND l.expires_at > ?)"
}

fn insert_atomic_ask_sql() -> &'static str {
    "INSERT INTO phone_asks (id, user_id, agent_id, transcript, locale, idempotency_key, client_turn_id, conversation_id, session_id, binding_id, lease_id, listener_generation, target_chat_id, claimed_by_chat_id, status, claimed_at, claim_token, claim_deadline, claim_generation, answered_at, reply_event_id, attempt_count, expires_at, created_at, updated_at) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 'queued', NULL, NULL, NULL, NULL, NULL, NULL, 0, ?, ?, ? WHERE EXISTS (SELECT 1 FROM sessions s WHERE s.id = ? AND s.user_id = ? AND s.agent_id = ? AND s.skill_id = 'phone.ask' AND s.chat_id = ? AND s.deleted_at IS NULL AND s.state != 'expired' AND s.expires_at > ?) AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = ? AND l.binding_id = ? AND l.chat_id = ? AND l.lease_id = ? AND l.generation = ? AND l.expires_at > ?)"
}

fn insert_atomic_ask_message_sql() -> &'static str {
    "INSERT INTO session_messages (id, user_id, session_id, role, content, metadata_json, command_id, sequence, retention_expires_at, created_at) SELECT ?, ?, ?, 'user', ?, ?, NULL, COALESCE((SELECT MAX(sequence) + 1 FROM session_messages WHERE user_id = ? AND session_id = ?), 1), ?, ? WHERE EXISTS (SELECT 1 FROM phone_asks a WHERE a.id = ? AND a.user_id = ? AND a.agent_id = ? AND a.session_id = ? AND a.binding_id = ? AND a.target_chat_id = ? AND a.lease_id = ? AND a.listener_generation = ?)"
}

fn activate_atomic_ask_session_sql() -> &'static str {
    "UPDATE sessions SET state = CASE WHEN state IN ('open', 'closed') THEN 'running' ELSE state END, updated_at = ? WHERE id = ? AND user_id = ? AND agent_id = ? AND skill_id = 'phone.ask' AND chat_id = ? AND deleted_at IS NULL AND EXISTS (SELECT 1 FROM phone_asks a WHERE a.session_id = sessions.id AND a.id = ? AND a.binding_id = ? AND a.lease_id = ? AND a.listener_generation = ? AND a.target_chat_id = ?)"
}

fn insert_ask_sql() -> &'static str {
    "INSERT INTO phone_asks (id, user_id, agent_id, transcript, locale, idempotency_key, client_turn_id, conversation_id, session_id, binding_id, lease_id, listener_generation, target_chat_id, claimed_by_chat_id, status, claimed_at, claim_token, claim_deadline, claim_generation, answered_at, reply_event_id, attempt_count, expires_at, created_at, updated_at) SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 'queued', NULL, NULL, NULL, NULL, NULL, NULL, 0, ?, ?, ? WHERE EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = ? AND l.binding_id = ? AND l.chat_id = ? AND l.lease_id = ? AND l.generation = ? AND l.expires_at > ?) ON CONFLICT DO NOTHING"
}

#[allow(dead_code)]
async fn create_ask_legacy(
    db: &D1Database,
    user_id: &str,
    agent_id: &str,
    input: &CreateAskRequest,
) -> ApiResult<Value> {
    let transcript = validate_transcript(&input.transcript)?;
    let client_turn_id = input
        .client_turn_id
        .as_deref()
        .map(validate_client_turn_id)
        .transpose()?;
    let provided_idempotency_key = input
        .idempotency_key
        .as_deref()
        .map(validate_idempotency_key)
        .transpose()?;
    if client_turn_id.is_none() && provided_idempotency_key.is_none() {
        return Err(ApiError::validation(
            "client_turn_id or idempotency_key is required",
        ));
    }
    let idempotency_key = provided_idempotency_key
        .clone()
        .or_else(|| client_turn_id.clone())
        .expect("validated above");
    let locale = validate_locale(input.locale.as_deref())?;
    let requested_session_id = validate_session_id(input.session_id.as_deref())?;
    let requested_fence = frozen_ask_fence(input)?;
    let agent = db::first::<AgentSeenRow>(
        db,
        "SELECT id, user_id, label, last_seen_at FROM agents WHERE id = ? AND user_id = ?",
        vec![db::text(agent_id), db::text(user_id)],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("Agent not found"))?;
    if let Some(mut existing) = find_existing_ask(
        db,
        user_id,
        agent_id,
        client_turn_id.as_deref(),
        &idempotency_key,
    )
    .await?
    {
        validate_existing_ask(
            &existing,
            &transcript,
            locale.as_deref(),
            requested_session_id.as_deref(),
            client_turn_id.as_deref(),
            requested_fence.as_ref(),
        )?;
        if existing.client_turn_id.is_none() {
            if let Some(client_turn_id) = client_turn_id.as_deref() {
                db::run(
                    db,
                    "UPDATE OR IGNORE phone_asks SET client_turn_id = ?, updated_at = ? WHERE id = ? AND client_turn_id IS NULL",
                    vec![
                        db::text(client_turn_id),
                        db::text(&db::now_iso()),
                        db::text(&existing.id),
                    ],
                )
                .await?;
                existing = get_ask_by_id(db, &existing.id)
                    .await?
                    .ok_or_else(|| ApiError::conflict("Ask could not be resumed"))?;
                if existing.client_turn_id.as_deref() != Some(client_turn_id) {
                    return Err(ApiError::conflict(
                        "client_turn_id is already bound to another Ask",
                    ));
                }
            }
        }
        if let Some(session_id) = existing.session_id.as_deref() {
            ensure_ask_message(
                db,
                user_id,
                session_id,
                &existing.id,
                &transcript,
                locale.as_deref(),
            )
            .await?;
        }
        let mut value = ask_to_api(db, &existing, Some(&agent.label), false).await?;
        value["deduped"] = Value::Bool(true);
        return Ok(value);
    }

    let binding = listener_bindings::active_for_agent(db, agent_id).await?;
    if !agent_is_listening(agent.last_seen_at.as_deref()) || binding.is_none() {
        return Err(ApiError::new(
            409,
            "agent_not_listening",
            "The selected agent is not listening. Open the Mac host and keep Knock Knock MCP polling.",
        ));
    }
    let binding = binding.expect("checked above");
    let requested_fence = requested_fence.as_ref().ok_or_else(|| {
        ask_listener_fence_mismatch(
            "binding_id, lease_id, generation, and target_chat_id are required to create an Ask",
        )
    })?;
    if binding.id != requested_fence.binding_id
        || binding.lease_id != requested_fence.lease_id
        || binding.generation != requested_fence.generation
        || binding.chat_id != requested_fence.target_chat_id
    {
        return Err(ask_listener_fence_mismatch(
            "The supplied listener fence is no longer active; refresh agents and retry with a new client turn",
        ));
    }

    let ask_id = new_id("ask")?;
    let now = db::now_iso();
    let expires_at = db::add_seconds_iso(ASK_TTL_SECS);
    if let Some(session_id) = requested_session_id.as_deref() {
        let existing = sessions::get_session(db, session_id)
            .await?
            .ok_or_else(|| ApiError::session("Session not found", 404))?;
        if existing.user_id != user_id
            || existing.agent_id != agent_id
            || existing.skill_id != "phone.ask"
            || existing.chat_id.as_deref() != Some(requested_fence.target_chat_id.as_str())
            || existing.deleted_at.is_some()
        {
            return Err(ApiError::session(
                "Session belongs to an expired Codex chat binding",
                410,
            ));
        }
    }

    let title = session_title(&transcript);
    let mut facts = Map::new();
    facts.insert("transcript".into(), Value::String(transcript.clone()));
    facts.insert("ask_id".into(), Value::String(ask_id.clone()));
    facts.insert(
        "conversation_id".into(),
        Value::String(requested_fence.target_chat_id.clone()),
    );
    facts.insert(
        "binding_id".into(),
        Value::String(requested_fence.binding_id.clone()),
    );
    facts.insert(
        "lease_id".into(),
        Value::String(requested_fence.lease_id.clone()),
    );
    facts.insert(
        "listener_generation".into(),
        Value::Number(requested_fence.generation.into()),
    );
    if let Some(client_turn_id) = client_turn_id.clone() {
        facts.insert("client_turn_id".into(), Value::String(client_turn_id));
    }
    if let Some(locale) = locale.clone() {
        facts.insert("locale".into(), Value::String(locale));
    }
    let session_turn_key = client_turn_id.as_deref().unwrap_or(&idempotency_key);
    let session = sessions::create_or_resume_session(
        db,
        &agent.id,
        user_id,
        &SessionRequest {
            skill_id: "phone.ask".into(),
            session_id: requested_session_id,
            idempotency_key: Some(format!("ask:{session_turn_key}")),
            title: Some(title),
            chat_id: Some(requested_fence.target_chat_id.clone()),
            facts: Some(facts),
            metadata: None,
        },
    )
    .await?;
    let session_id = session
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| ApiError::new(500, "ask_error", "Ask session was not created"))?;

    let inserted = db::run(
        db,
        insert_ask_sql(),
        vec![
            db::text(&ask_id),
            db::text(user_id),
            db::text(agent_id),
            db::text(&transcript),
            db::optional_text(locale.as_deref()),
            db::text(&idempotency_key),
            db::optional_text(client_turn_id.as_deref()),
            db::text(&requested_fence.target_chat_id),
            db::text(&session_id),
            db::text(&requested_fence.binding_id),
            db::text(&requested_fence.lease_id),
            db::number(requested_fence.generation),
            db::text(&requested_fence.target_chat_id),
            db::text(&expires_at),
            db::text(&now),
            db::text(&now),
            db::text(agent_id),
            db::text(&requested_fence.binding_id),
            db::text(&requested_fence.target_chat_id),
            db::text(&requested_fence.lease_id),
            db::number(requested_fence.generation),
            db::text(&now),
        ],
    )
    .await?;
    if db::changes(&inserted) == 0 {
        let Some(mut existing) = find_existing_ask(
            db,
            user_id,
            agent_id,
            client_turn_id.as_deref(),
            &idempotency_key,
        )
        .await?
        else {
            return Err(ask_listener_fence_mismatch(
                "The supplied listener fence changed before the Ask could be persisted",
            ));
        };
        validate_existing_ask(
            &existing,
            &transcript,
            locale.as_deref(),
            input.session_id.as_deref(),
            client_turn_id.as_deref(),
            Some(requested_fence),
        )?;
        if existing.client_turn_id.is_none() {
            if let Some(client_turn_id) = client_turn_id.as_deref() {
                db::run(
                    db,
                    "UPDATE OR IGNORE phone_asks SET client_turn_id = ?, updated_at = ? WHERE id = ? AND client_turn_id IS NULL",
                    vec![
                        db::text(client_turn_id),
                        db::text(&db::now_iso()),
                        db::text(&existing.id),
                    ],
                )
                .await?;
                existing = get_ask_by_id(db, &existing.id)
                    .await?
                    .ok_or_else(|| ApiError::conflict("Ask could not be resumed"))?;
            }
        }
        ensure_ask_message(
            db,
            &existing.user_id,
            existing.session_id.as_deref().unwrap_or(&session_id),
            &existing.id,
            &transcript,
            locale.as_deref(),
        )
        .await?;
        let mut value = ask_to_api(db, &existing, Some(&agent.label), false).await?;
        value["deduped"] = Value::Bool(true);
        return Ok(value);
    }

    ensure_ask_message(
        db,
        user_id,
        &session_id,
        &ask_id,
        &transcript,
        locale.as_deref(),
    )
    .await?;
    let created = AskRow {
        id: ask_id,
        user_id: user_id.to_string(),
        agent_id: agent_id.to_string(),
        transcript,
        locale,
        idempotency_key,
        client_turn_id,
        conversation_id: Some(requested_fence.target_chat_id.clone()),
        session_id: Some(session_id),
        binding_id: Some(requested_fence.binding_id.clone()),
        lease_id: Some(requested_fence.lease_id.clone()),
        listener_generation: Some(requested_fence.generation),
        target_chat_id: Some(requested_fence.target_chat_id.clone()),
        claimed_by_chat_id: None,
        status: "queued".into(),
        claimed_at: None,
        claim_token: None,
        claim_deadline: None,
        claim_generation: None,
        answered_at: None,
        reply_event_id: None,
        attempt_count: 0,
        expires_at,
        created_at: now,
    };
    let mut value = ask_to_api(db, &created, Some(&agent.label), false).await?;
    value["deduped"] = Value::Bool(false);
    Ok(value)
}

pub async fn create_ask(
    db: &D1Database,
    user_id: &str,
    agent_id: &str,
    input: &CreateAskRequest,
) -> ApiResult<Value> {
    let transcript = validate_transcript(&input.transcript)?;
    let client_turn_id = input
        .client_turn_id
        .as_deref()
        .map(validate_client_turn_id)
        .transpose()?
        .ok_or_else(|| ApiError::validation("client_turn_id is required"))?;
    let idempotency_key = input
        .idempotency_key
        .as_deref()
        .map(validate_idempotency_key)
        .transpose()?
        .unwrap_or_else(|| client_turn_id.clone());
    let locale = validate_locale(input.locale.as_deref())?;
    let requested_session_id = validate_session_id(input.session_id.as_deref())?;
    let fence = frozen_ask_fence(input)?.ok_or_else(|| {
        ask_listener_fence_mismatch(
            "binding_id, lease_id, generation, and target_chat_id are required to create an Ask",
        )
    })?;
    let agent = db::first::<AgentSeenRow>(
        db,
        "SELECT id, user_id, label, last_seen_at FROM agents WHERE id = ? AND user_id = ?",
        vec![db::text(agent_id), db::text(user_id)],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("Agent not found"))?;

    let requested_session = if let Some(session_id) = requested_session_id.as_deref() {
        let session = sessions::get_session(db, session_id)
            .await?
            .ok_or_else(|| ApiError::session("Session not found", 404))?;
        validate_ask_session_row(&session, user_id, agent_id, &fence.target_chat_id)?;
        Some(session)
    } else {
        None
    };

    if let Some(existing) = find_existing_ask(
        db,
        user_id,
        agent_id,
        Some(&client_turn_id),
        &idempotency_key,
    )
    .await?
    {
        validate_existing_ask(
            &existing,
            &transcript,
            locale.as_deref(),
            requested_session_id.as_deref(),
            Some(&client_turn_id),
            Some(&fence),
        )?;
        let session_id = existing
            .session_id
            .as_deref()
            .ok_or_else(|| ApiError::conflict("Ask identity has no bound session"))?;
        if requested_session.is_none() {
            validate_ask_session_binding(db, session_id, user_id, agent_id, &fence.target_chat_id)
                .await?;
        }
        let mut value = ask_to_api(db, &existing, Some(&agent.label), false).await?;
        value["deduped"] = Value::Bool(true);
        return Ok(value);
    }

    listener_bindings::active_for_fence(
        db,
        agent_id,
        &fence.binding_id,
        &fence.target_chat_id,
        &fence.lease_id,
        fence.generation,
    )
    .await?
    .ok_or_else(|| {
        ask_listener_fence_mismatch(
            "The supplied listener fence is no longer active; refresh agents and retry with a new client turn",
        )
    })?;

    let session_idempotency_key = format!("ask:{client_turn_id}");
    let existing_session = if requested_session_id.is_some() {
        requested_session
    } else {
        sessions::get_session_by_agent_idempotency(db, agent_id, &session_idempotency_key).await?
    };
    let (session_id, create_session) = if let Some(session) = existing_session {
        if requested_session_id
            .as_deref()
            .is_some_and(|requested| requested != session.id)
            || session.user_id != user_id
            || session.agent_id != agent_id
            || session.skill_id != "phone.ask"
            || session.chat_id.as_deref() != Some(fence.target_chat_id.as_str())
            || session.deleted_at.is_some()
        {
            return Err(ApiError::conflict(
                "Ask session is bound to a different user, agent, skill, or target chat",
            ));
        }
        (session.id, false)
    } else if requested_session_id.is_some() {
        return Err(ApiError::session("Session not found", 404));
    } else {
        (new_id("ses")?, true)
    };

    let ask_id = new_id("ask")?;
    let now = db::now_iso();
    let ask_expires_at = db::add_seconds_iso(ASK_TTL_SECS);
    let session_expires_at = db::add_seconds_iso(ASK_TTL_SECS);
    let retention_expires_at = db::add_seconds_iso(MESSAGE_RETENTION_SECS);
    let title = session_title(&transcript);
    let facts = json!({
        "transcript": transcript,
        "ask_id": ask_id,
        "client_turn_id": client_turn_id,
        "conversation_id": fence.target_chat_id,
        "binding_id": fence.binding_id,
        "lease_id": fence.lease_id,
        "listener_generation": fence.generation,
        "locale": locale,
    })
    .to_string();
    let mut statements = Vec::with_capacity(if create_session { 4 } else { 3 });
    if create_session {
        statements.push(db::prepare(
            db,
            insert_atomic_ask_session_sql(),
            vec![
                db::text(&session_id),
                db::text(agent_id),
                db::text(user_id),
                db::text(&title),
                db::text(&fence.target_chat_id),
                db::text(&facts),
                db::text(&session_idempotency_key),
                db::text(&session_expires_at),
                db::text(&retention_expires_at),
                db::text(&now),
                db::text(&now),
                db::text(agent_id),
                db::text(&fence.binding_id),
                db::text(&fence.target_chat_id),
                db::text(&fence.lease_id),
                db::number(fence.generation),
                db::text(&now),
            ],
        )?);
    }
    let ask_result_index = statements.len();
    statements.push(db::prepare(
        db,
        insert_atomic_ask_sql(),
        vec![
            db::text(&ask_id),
            db::text(user_id),
            db::text(agent_id),
            db::text(&transcript),
            db::optional_text(locale.as_deref()),
            db::text(&idempotency_key),
            db::text(&client_turn_id),
            db::text(&fence.target_chat_id),
            db::text(&session_id),
            db::text(&fence.binding_id),
            db::text(&fence.lease_id),
            db::number(fence.generation),
            db::text(&fence.target_chat_id),
            db::text(&ask_expires_at),
            db::text(&now),
            db::text(&now),
            db::text(&session_id),
            db::text(user_id),
            db::text(agent_id),
            db::text(&fence.target_chat_id),
            db::text(&now),
            db::text(agent_id),
            db::text(&fence.binding_id),
            db::text(&fence.target_chat_id),
            db::text(&fence.lease_id),
            db::number(fence.generation),
            db::text(&now),
        ],
    )?);
    statements.push(db::prepare(
        db,
        insert_atomic_ask_message_sql(),
        vec![
            db::text(&ask_message_id(&ask_id)),
            db::text(user_id),
            db::text(&session_id),
            db::text(&transcript),
            db::text(&json!({"ask_id": ask_id, "locale": locale}).to_string()),
            db::text(user_id),
            db::text(&session_id),
            db::text(&retention_expires_at),
            db::text(&now),
            db::text(&ask_id),
            db::text(user_id),
            db::text(agent_id),
            db::text(&session_id),
            db::text(&fence.binding_id),
            db::text(&fence.target_chat_id),
            db::text(&fence.lease_id),
            db::number(fence.generation),
        ],
    )?);
    statements.push(db::prepare(
        db,
        activate_atomic_ask_session_sql(),
        vec![
            db::text(&now),
            db::text(&session_id),
            db::text(user_id),
            db::text(agent_id),
            db::text(&fence.target_chat_id),
            db::text(&ask_id),
            db::text(&fence.binding_id),
            db::text(&fence.lease_id),
            db::number(fence.generation),
            db::text(&fence.target_chat_id),
        ],
    )?);

    let results = match db.batch(statements).await {
        Ok(results) => results,
        Err(error) => {
            if let Some(existing) = find_existing_ask(
                db,
                user_id,
                agent_id,
                Some(&client_turn_id),
                &idempotency_key,
            )
            .await?
            {
                validate_existing_ask(
                    &existing,
                    &transcript,
                    locale.as_deref(),
                    requested_session_id.as_deref(),
                    Some(&client_turn_id),
                    Some(&fence),
                )?;
                let persisted_session_id = existing
                    .session_id
                    .as_deref()
                    .ok_or_else(|| ApiError::conflict("Ask identity has no bound session"))?;
                validate_ask_session_binding(
                    db,
                    persisted_session_id,
                    user_id,
                    agent_id,
                    &fence.target_chat_id,
                )
                .await?;
                let mut value = ask_to_api(db, &existing, Some(&agent.label), false).await?;
                value["deduped"] = Value::Bool(true);
                return Ok(value);
            }
            return Err(error.into());
        }
    };
    if results
        .get(ask_result_index)
        .map(db::changes)
        .unwrap_or_default()
        != 1
    {
        if let Some(session) = sessions::get_session(db, &session_id).await? {
            if ask_session_is_expired(&session.state, &session.expires_at) {
                return Err(ApiError::session("Session expired", 410));
            }
        }
        return Err(ask_listener_fence_mismatch(
            "The supplied listener fence changed before the Ask could be persisted",
        ));
    }
    let created = get_ask_by_id(db, &ask_id)
        .await?
        .ok_or_else(|| ApiError::new(500, "ask_error", "Ask was not persisted"))?;
    let mut value = ask_to_api(db, &created, Some(&agent.label), false).await?;
    value["deduped"] = Value::Bool(false);
    Ok(value)
}

fn list_agent_asks_sql() -> String {
    format!(
        "SELECT {ASK_COLUMNS} FROM phone_asks WHERE agent_id = ? AND binding_id = ? AND target_chat_id = ? AND COALESCE(conversation_id, target_chat_id) = ? AND answered_at IS NULL AND expires_at > ? AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = ? AND l.binding_id = ? AND l.chat_id = ? AND l.lease_id = ? AND l.generation = ? AND l.expires_at > ?) AND (status = 'queued' OR (status = 'claimed' AND claimed_by_chat_id = ? AND ((claim_token IS NOT NULL AND claim_generation = ? AND claim_deadline > ?) OR (claim_token IS NOT NULL AND claim_generation IS NOT NULL AND claim_generation <> ?) OR (claim_token IS NOT NULL AND claim_deadline IS NOT NULL AND claim_deadline <= ?) OR (claim_token IS NULL AND claim_generation IS NULL AND claim_deadline IS NULL)))) ORDER BY created_at ASC LIMIT 20"
    )
}

async fn load_agent_ask_rows(
    db: &D1Database,
    agent_id: &str,
    binding: &ListenerBindingRow,
) -> ApiResult<Vec<AskRow>> {
    let now = db::now_iso();
    let sql = list_agent_asks_sql();
    let rows: Vec<AskRow> = db::all(
        db,
        &sql,
        vec![
            db::text(agent_id),
            db::text(&binding.id),
            db::text(&binding.chat_id),
            db::text(&binding.chat_id),
            db::text(&now),
            db::text(agent_id),
            db::text(&binding.id),
            db::text(&binding.chat_id),
            db::text(&binding.lease_id),
            db::number(binding.generation),
            db::text(&now),
            db::text(&binding.chat_id),
            db::number(binding.generation),
            db::text(&now),
            db::number(binding.generation),
            db::text(&now),
        ],
    )
    .await?;
    Ok(rows)
}

async fn asks_to_api(
    db: &D1Database,
    rows: Vec<AskRow>,
    include_claim_credentials: bool,
) -> ApiResult<Vec<Value>> {
    let mut asks = Vec::with_capacity(rows.len());
    for row in rows {
        asks.push(ask_to_api(db, &row, None, include_claim_credentials).await?);
    }
    Ok(asks)
}

pub async fn list_agent_asks(
    db: &D1Database,
    agent_id: &str,
    binding: &ListenerBindingRow,
) -> ApiResult<Vec<Value>> {
    asks_to_api(db, load_agent_ask_rows(db, agent_id, binding).await?, false).await
}

pub async fn claim_agent_asks(
    db: &D1Database,
    agent_id: &str,
    binding: &ListenerBindingRow,
) -> ApiResult<Vec<Value>> {
    let rows = load_agent_ask_rows(db, agent_id, binding).await?;
    asks_to_api(db, claim_asks(db, &rows, binding).await?, true).await
}

fn agent_is_listening(last_seen_at: Option<&str>) -> bool {
    agent_is_listening_at(last_seen_at, db::now_ms())
}

fn agent_is_listening_at(last_seen_at: Option<&str>, now_ms: i64) -> bool {
    let Some(seen) = last_seen_at.filter(|value| !value.is_empty()) else {
        return false;
    };
    let Some(seen_ms) = crate::commands::parse_rfc3339_millis(seen) else {
        return false;
    };
    // Exclusive 90s edge: listening iff seen_ms + 90_000 > now_ms (not >=).
    seen_ms.saturating_add(LISTENING_WINDOW_SECS.saturating_mul(1_000)) > now_ms
}

fn session_title(transcript: &str) -> String {
    let mut title = String::new();
    for character in transcript.chars().take(80) {
        title.push(character);
    }
    if transcript.chars().count() > 80 {
        title.push('…');
    }
    title
}

fn ask_message_id(ask_id: &str) -> String {
    format!("msg_phone_{ask_id}")
}

async fn ensure_ask_message(
    db: &D1Database,
    user_id: &str,
    session_id: &str,
    ask_id: &str,
    transcript: &str,
    locale: Option<&str>,
) -> ApiResult<i64> {
    let session = sessions::get_session(db, session_id)
        .await?
        .ok_or_else(|| ApiError::session("Session not found", 404))?;
    let message_id = ask_message_id(ask_id);
    let now = db::now_iso();
    let retention_expires_at = session
        .retention_expires_at
        .unwrap_or_else(|| db::add_seconds_iso(MESSAGE_RETENTION_SECS));
    let inserted = db::run(
        db,
        "INSERT OR IGNORE INTO session_messages (id, user_id, session_id, role, content, metadata_json, command_id, sequence, retention_expires_at, created_at) SELECT ?, ?, ?, 'user', ?, ?, NULL, COALESCE((SELECT MAX(sequence) + 1 FROM session_messages WHERE user_id = ? AND session_id = ?), 1), ?, ? WHERE EXISTS (SELECT 1 FROM sessions WHERE id = ? AND user_id = ? AND deleted_at IS NULL)",
        vec![
            db::text(&message_id),
            db::text(user_id),
            db::text(session_id),
            db::text(transcript),
            db::text(&json!({"ask_id": ask_id, "locale": locale}).to_string()),
            db::text(user_id),
            db::text(session_id),
            db::text(&retention_expires_at),
            db::text(&now),
            db::text(session_id),
            db::text(user_id),
        ],
    )
    .await?;
    if db::changes(&inserted) > 0 {
        db::run(
            db,
            "UPDATE sessions SET state = CASE WHEN state IN ('open', 'closed') THEN 'running' ELSE state END, updated_at = ? WHERE id = ? AND user_id = ? AND deleted_at IS NULL",
            vec![db::text(&now), db::text(session_id), db::text(user_id)],
        )
        .await?;
    }
    db::first::<AskMessageSequenceRow>(
        db,
        "SELECT sequence FROM session_messages WHERE id = ? AND user_id = ? AND session_id = ?",
        vec![
            db::text(&message_id),
            db::text(user_id),
            db::text(session_id),
        ],
    )
    .await?
    .map(|row| row.sequence)
    .ok_or_else(|| ApiError::new(500, "ask_error", "Ask message was not persisted"))
}

async fn ask_context_messages(
    db: &D1Database,
    user_id: &str,
    session_id: &str,
) -> ApiResult<Vec<Value>> {
    let mut rows: Vec<SessionMessageRow> = db::all(
        db,
        "SELECT id, user_id, session_id, role, content, metadata_json, command_id, sequence, retention_expires_at, created_at FROM session_messages WHERE user_id = ? AND session_id = ? AND (retention_expires_at IS NULL OR retention_expires_at > ?) ORDER BY sequence DESC LIMIT ?",
        vec![
            db::text(user_id),
            db::text(session_id),
            db::text(&db::now_iso()),
            db::number(ASK_CONTEXT_MESSAGE_LIMIT),
        ],
    )
    .await?;
    rows.reverse();
    Ok(rows
        .into_iter()
        .map(|row| {
            json!({
                "message_id": row.id,
                "role": row.role,
                "content": row.content,
                "metadata": serde_json::from_str::<Value>(&row.metadata_json)
                    .unwrap_or_else(|_| json!({})),
                "sequence": row.sequence,
                "created_at": row.created_at,
            })
        })
        .collect())
}

async fn ask_to_api(
    db: &D1Database,
    row: &AskRow,
    agent_label: Option<&str>,
    include_claim_credentials: bool,
) -> ApiResult<Value> {
    let (turn_sequence, context_messages) = if let Some(session_id) = row.session_id.as_deref() {
        let turn_sequence = db::first::<AskMessageSequenceRow>(
            db,
            "SELECT sequence FROM session_messages WHERE id = ? AND user_id = ? AND session_id = ?",
            vec![
                db::text(&ask_message_id(&row.id)),
                db::text(&row.user_id),
                db::text(session_id),
            ],
        )
        .await?
        .map(|message| message.sequence);
        (
            turn_sequence,
            ask_context_messages(db, &row.user_id, session_id).await?,
        )
    } else {
        (None, Vec::new())
    };
    let logical_status = if row.answered_at.is_some() {
        "answered"
    } else {
        row.status.as_str()
    };
    let legacy_drain = row.status == "claimed"
        && row.answered_at.is_none()
        && (row.claim_token.is_none()
            || row.claim_deadline.is_none()
            || row.claim_generation.is_none());
    let answerable = include_claim_credentials
        && row.status == "claimed"
        && row.answered_at.is_none()
        && row.claim_token.is_some()
        && row.claim_generation.is_some()
        && row
            .claim_deadline
            .as_deref()
            .is_some_and(|deadline| !db::is_expired(deadline));
    let mut value = json!({
        "ask_id": row.id,
        "agent_id": row.agent_id,
        "user_id": row.user_id,
        "transcript": row.transcript,
        "locale": row.locale,
        "session_id": row.session_id,
        "turn_sequence": turn_sequence,
        "context_messages": context_messages,
        "status": logical_status,
        "claimed_at": row.claimed_at,
        "expires_at": row.expires_at,
        "created_at": row.created_at,
        "binding_id": row.binding_id,
        "target_chat_id": row.target_chat_id,
        "claimed_by_chat_id": row.claimed_by_chat_id,
    });
    value["client_turn_id"] = json!(row.client_turn_id);
    value["conversation_id"] = json!(row.conversation_id);
    value["lease_id"] = json!(row.lease_id);
    value["listener_generation"] = json!(row.listener_generation);
    value["resolved_generation"] = json!(row.listener_generation);
    value["claim_token"] = json!(include_claim_credentials
        .then_some(row.claim_token.as_deref())
        .flatten());
    value["claim_deadline"] = json!(row.claim_deadline);
    value["claim_generation"] = json!(include_claim_credentials
        .then_some(row.claim_generation)
        .flatten());
    value["generation"] = json!(include_claim_credentials
        .then_some(row.claim_generation)
        .flatten());
    value["answered_at"] = json!(row.answered_at);
    value["reply_event_id"] = json!(row.reply_event_id);
    value["attempt_count"] = json!(row.attempt_count);
    value["answerable"] = Value::Bool(answerable);
    value["legacy"] = Value::Bool(row.client_turn_id.is_none());
    value["legacy_fence"] =
        Value::Bool(row.lease_id.is_none() || row.listener_generation.is_none());
    value["legacy_drain"] = Value::Bool(legacy_drain);
    value["receipt"] = json!({
        "ask_id": row.id,
        "client_turn_id": row.client_turn_id,
        "conversation_id": row.conversation_id,
        "binding_id": row.binding_id,
        "lease_id": row.lease_id,
        "listener_generation": row.listener_generation,
        "target_chat_id": row.target_chat_id,
        "idempotency_key": row.idempotency_key,
        "status": logical_status,
        "created_at": row.created_at,
    });
    if let Some(label) = agent_label {
        value["agent_label"] = Value::String(label.to_string());
    }
    Ok(value)
}

fn claim_ask_sql() -> &'static str {
    "UPDATE phone_asks SET status = 'claimed', claimed_at = ?, claimed_by_chat_id = ?, claim_token = ?, claim_deadline = ?, claim_generation = ?, attempt_count = attempt_count + 1, updated_at = ? WHERE id = ? AND binding_id = ? AND target_chat_id = ? AND COALESCE(conversation_id, target_chat_id) = ? AND answered_at IS NULL AND expires_at > ? AND (status = 'queued' OR (status = 'claimed' AND claim_token IS NOT NULL AND claim_deadline IS NOT NULL AND ((claim_generation IS NOT NULL AND claim_generation <> ?) OR claim_deadline <= ?))) AND EXISTS (SELECT 1 FROM agent_listener_leases l WHERE l.agent_id = ? AND l.binding_id = ? AND l.chat_id = ? AND l.lease_id = ? AND l.generation = ? AND l.expires_at > ?)"
}

async fn claim_asks(
    db: &D1Database,
    rows: &[AskRow],
    binding: &ListenerBindingRow,
) -> ApiResult<Vec<AskRow>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let now = db::now_iso();
    let mut claimed = Vec::new();
    for row in rows {
        let legacy_drain = row.status == "claimed"
            && row.claim_token.is_none()
            && row.claim_deadline.is_none()
            && row.claim_generation.is_none();
        let active_recovery = row.status == "claimed"
            && row.claim_token.is_some()
            && row.claim_generation == Some(binding.generation)
            && row
                .claim_deadline
                .as_deref()
                .is_some_and(|deadline| deadline > now.as_str());
        if legacy_drain || active_recovery {
            claimed.push(row.clone());
            continue;
        }
        let claim_token = new_id("claim")?;
        let claim_deadline = db::add_seconds_iso(ASK_CLAIM_TTL_SECS);
        let updated = db::run(
            db,
            claim_ask_sql(),
            vec![
                db::text(&now),
                db::text(&binding.chat_id),
                db::text(&claim_token),
                db::text(&claim_deadline),
                db::number(binding.generation),
                db::text(&now),
                db::text(&row.id),
                db::text(&binding.id),
                db::text(&binding.chat_id),
                db::text(&binding.chat_id),
                db::text(&now),
                db::number(binding.generation),
                db::text(&now),
                db::text(&binding.agent_id),
                db::text(&binding.id),
                db::text(&binding.chat_id),
                db::text(&binding.lease_id),
                db::number(binding.generation),
                db::text(&now),
            ],
        )
        .await?;
        if db::changes(&updated) > 0 {
            let mut next = row.clone();
            next.status = "claimed".into();
            next.claimed_at = Some(now.clone());
            next.claimed_by_chat_id = Some(binding.chat_id.clone());
            next.claim_token = Some(claim_token);
            next.claim_deadline = Some(claim_deadline);
            next.claim_generation = Some(binding.generation);
            next.attempt_count = next.attempt_count.saturating_add(1);
            claimed.push(next);
        } else if let Some(fresh) = get_ask_by_id(db, &row.id).await? {
            let concurrent_recovery = fresh.status == "claimed"
                && fresh.answered_at.is_none()
                && fresh.claim_token.is_some()
                && fresh.claim_generation == Some(binding.generation)
                && fresh.claimed_by_chat_id.as_deref() == Some(binding.chat_id.as_str())
                && fresh
                    .claim_deadline
                    .as_deref()
                    .is_some_and(|deadline| deadline > now.as_str());
            if concurrent_recovery {
                claimed.push(fresh);
            }
        }
    }
    Ok(claimed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_rejects_empty_and_overlong_values() {
        assert!(validate_transcript("  ").is_err());
        assert!(validate_transcript("Help with APNs").is_ok());
        let too_long = "a".repeat(MAX_TRANSCRIPT_CHARS + 1);
        assert!(validate_transcript(&too_long).is_err());
    }

    #[test]
    fn idempotency_key_bounds_are_enforced() {
        assert!(validate_idempotency_key("short").is_err());
        assert!(validate_idempotency_key("ask-key-01").is_ok());
    }

    #[test]
    fn client_turn_id_bounds_are_enforced() {
        assert!(validate_client_turn_id("short").is_err());
        assert_eq!(
            validate_client_turn_id(" turn-phone-0001 ").unwrap(),
            "turn-phone-0001"
        );
        assert!(validate_client_turn_id(&"t".repeat(129)).is_err());
    }

    fn ask_request_with_fence() -> CreateAskRequest {
        CreateAskRequest {
            transcript: "Help with APNs".into(),
            locale: None,
            client_turn_id: Some("turn-phone-0001".into()),
            idempotency_key: None,
            binding_id: Some("binding-phone-0001".into()),
            lease_id: Some("lease-phone-0001".into()),
            generation: Some(7),
            target_chat_id: Some("chat-phone-0001".into()),
            session_id: None,
        }
    }

    #[test]
    fn phone_ask_fence_is_all_or_nothing_and_positive() {
        let request = ask_request_with_fence();
        let fence = frozen_ask_fence(&request).unwrap().unwrap();
        assert_eq!(fence.binding_id, "binding-phone-0001");
        assert_eq!(fence.lease_id, "lease-phone-0001");
        assert_eq!(fence.generation, 7);
        assert_eq!(fence.target_chat_id, "chat-phone-0001");

        let mut partial = ask_request_with_fence();
        partial.lease_id = None;
        let error = frozen_ask_fence(&partial).unwrap_err();
        assert_eq!(error.status, 409);
        assert_eq!(error.code, "ask_listener_fence_mismatch");

        let mut invalid_generation = ask_request_with_fence();
        invalid_generation.generation = Some(0);
        assert!(frozen_ask_fence(&invalid_generation).is_err());
    }

    #[test]
    fn ask_insert_is_gated_by_the_exact_frozen_listener_fence() {
        let sql = insert_ask_sql();
        assert!(sql.contains("l.binding_id = ?"));
        assert!(sql.contains("l.chat_id = ?"));
        assert!(sql.contains("l.lease_id = ?"));
        assert!(sql.contains("l.generation = ?"));
        assert!(sql.contains("l.expires_at > ?"));
        assert!(!sql.contains("UPDATE agent_chat_bindings"));
    }

    #[test]
    fn atomic_session_and_ask_share_one_exact_fence() {
        let session_sql = insert_atomic_ask_session_sql();
        let ask_sql = insert_atomic_ask_sql();
        for predicate in [
            "l.agent_id = ?",
            "l.binding_id = ?",
            "l.chat_id = ?",
            "l.lease_id = ?",
            "l.generation = ?",
            "l.expires_at > ?",
        ] {
            assert!(session_sql.contains(predicate));
            assert!(ask_sql.contains(predicate));
        }
        assert!(!ask_sql.contains("ON CONFLICT"));
        assert!(ask_sql.contains("s.user_id = ?"));
        assert!(ask_sql.contains("s.agent_id = ?"));
        assert!(ask_sql.contains("s.skill_id = 'phone.ask'"));
        assert!(ask_sql.contains("s.chat_id = ?"));
        assert!(ask_sql.contains("s.state != 'expired'"));
        assert!(ask_sql.contains("s.expires_at > ?"));
    }

    #[test]
    fn ask_poll_query_is_read_only_and_surfaces_only_explicit_claim_states() {
        let sql = list_agent_asks_sql();
        assert!(sql.starts_with("SELECT "));
        assert!(!sql.contains("UPDATE phone_asks"));
        assert!(!sql.contains("INSERT "));
        assert!(!sql.contains("DELETE "));
        assert!(sql.contains("claim_generation = ?"));
        assert!(sql.contains("claim_generation <> ?"));
        assert!(sql.contains("claim_deadline <= ?"));
        assert!(sql.contains("claim_generation <> ?"));
        assert!(sql.contains("claim_deadline IS NULL"));
    }

    #[test]
    fn claim_cas_requires_current_lease_generation_and_real_expiry() {
        let sql = claim_ask_sql();
        assert!(sql.starts_with("UPDATE phone_asks"));
        assert!(sql.contains("attempt_count = attempt_count + 1"));
        assert!(sql.contains("claim_deadline <= ?"));
        assert!(!sql.contains("claim_deadline IS NULL OR"));
        assert!(sql.contains("l.lease_id = ?"));
        assert!(sql.contains("l.generation = ?"));
        assert!(sql.contains("answered_at IS NULL"));
    }

    #[test]
    fn expired_ask_sessions_are_rejected_before_persistence() {
        assert!(ask_session_is_expired(
            "expired",
            "2999-01-01T00:00:00.000Z"
        ));
    }

    #[test]
    fn optional_session_id_is_trimmed_and_bounded() {
        assert_eq!(
            validate_session_id(Some("  ses_voice_1  ")).unwrap(),
            Some("ses_voice_1".into())
        );
        assert_eq!(validate_session_id(Some("  ")).unwrap(), None);
        assert!(validate_session_id(Some(&"s".repeat(129))).is_err());
    }

    #[test]
    fn session_title_truncates_without_inventing_words() {
        assert_eq!(session_title("Help with APNs"), "Help with APNs");
        let long = "n".repeat(90);
        let title = session_title(&long);
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), 81);
        assert_eq!(crate::skills::phone_ask_skill().skill_id, "phone.ask");
    }

    const LAST_SEEN_FIXTURE: &str = "2026-08-18T12:00:00.000Z";

    fn fixture_seen_ms() -> i64 {
        crate::commands::parse_rfc3339_millis(LAST_SEEN_FIXTURE)
            .expect("listening fixture is RFC3339")
    }

    #[test]
    fn agent_without_last_seen_is_not_listening() {
        let none_listening = agent_is_listening(None);
        let empty_listening = agent_is_listening(Some(""));
        println!("agent_is_listening None => {none_listening}");
        println!("agent_is_listening empty => {empty_listening}");
        assert!(!none_listening);
        assert!(!empty_listening);
    }

    #[test]
    fn agent_seen_just_inside_90s_is_listening() {
        assert_eq!(LISTENING_WINDOW_SECS, 90);
        let now_ms = fixture_seen_ms() + 89_000;
        let listening = agent_is_listening_at(Some(LAST_SEEN_FIXTURE), now_ms);
        println!("agent_is_listening now-89s (just inside exclusive 90s window) => {listening}");
        assert!(listening);
    }

    #[test]
    fn agent_seen_just_past_90s_is_not_listening() {
        assert_eq!(LISTENING_WINDOW_SECS, 90);
        let now_ms = fixture_seen_ms() + 91_000;
        let listening = agent_is_listening_at(Some(LAST_SEEN_FIXTURE), now_ms);
        println!("agent_is_listening now-91s (just stale) => {listening}");
        assert!(!listening);
    }

    #[test]
    fn agent_seen_at_exact_90s_is_not_listening() {
        assert_eq!(LISTENING_WINDOW_SECS, 90);
        let now_ms = fixture_seen_ms() + 90_000;
        let listening = agent_is_listening_at(Some(LAST_SEEN_FIXTURE), now_ms);
        println!("agent_is_listening now-90.000s (exclusive window edge) => {listening}");
        assert!(!listening);
    }
}
