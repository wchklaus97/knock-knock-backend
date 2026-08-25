use serde::Deserialize;
use serde_json::{json, Map, Value};
use worker::D1Database;

use crate::auth::new_id;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::{SessionMessageRow, SessionRequest};
use crate::listener_bindings::{self, ListenerBindingRow};
use crate::sessions;

/// Exclusive Ask listening window. Exact last_seen age 90.000s is not listening
/// (`seen_ms + 90_000 > now_ms`), matching iOS `age < 90` / HTTP 409.
pub const LISTENING_WINDOW_SECS: i64 = 90;
const ASK_TTL_SECS: i64 = 86_400;
const MAX_TRANSCRIPT_CHARS: usize = 2_000;
const ASK_CONTEXT_MESSAGE_LIMIT: i64 = 12;
const MESSAGE_RETENTION_SECS: i64 = 90 * 24 * 60 * 60;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAskRequest {
    pub transcript: String,
    #[serde(default)]
    pub locale: Option<String>,
    pub idempotency_key: String,
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
    session_id: Option<String>,
    binding_id: Option<String>,
    target_chat_id: Option<String>,
    claimed_by_chat_id: Option<String>,
    status: String,
    claimed_at: Option<String>,
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

pub async fn create_ask(
    db: &D1Database,
    user_id: &str,
    agent_id: &str,
    input: &CreateAskRequest,
) -> ApiResult<Value> {
    let transcript = validate_transcript(&input.transcript)?;
    let idempotency_key = validate_idempotency_key(&input.idempotency_key)?;
    let locale = validate_locale(input.locale.as_deref())?;
    let requested_session_id = validate_session_id(input.session_id.as_deref())?;
    let agent = db::first::<AgentSeenRow>(
        db,
        "SELECT id, user_id, label, last_seen_at FROM agents WHERE id = ? AND user_id = ?",
        vec![db::text(agent_id), db::text(user_id)],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("Agent not found"))?;
    let binding = listener_bindings::active_for_agent(db, agent_id).await?;
    if !agent_is_listening(agent.last_seen_at.as_deref()) || binding.is_none() {
        return Err(ApiError::new(
            409,
            "agent_not_listening",
            "The selected agent is not listening. Open the Mac host and keep Knock Knock MCP polling.",
        ));
    }
    let binding = binding.expect("checked above");

    if let Some(existing) = db::first::<AskRow>(
        db,
        "SELECT id, user_id, agent_id, transcript, locale, session_id, binding_id, target_chat_id, claimed_by_chat_id, status, claimed_at, expires_at, created_at FROM phone_asks WHERE user_id = ? AND agent_id = ? AND idempotency_key = ?",
        vec![
            db::text(user_id),
            db::text(agent_id),
            db::text(&idempotency_key),
        ],
    )
    .await?
    {
        if existing.transcript != transcript {
            return Err(ApiError::conflict(
                "idempotency_key was already used with a different transcript",
            ));
        }
        if requested_session_id.is_some()
            && existing.session_id.as_deref() != requested_session_id.as_deref()
        {
            return Err(ApiError::conflict(
                "idempotency_key was already used with a different session_id",
            ));
        }
        if existing.binding_id.as_deref() != Some(binding.id.as_str())
            || existing.target_chat_id.as_deref() != Some(binding.chat_id.as_str())
        {
            return Err(ApiError::conflict("idempotency_key belongs to another Codex chat"));
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
        return ask_to_api(db, &existing, Some(&agent.label)).await;
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
            || existing.chat_id.as_deref() != Some(binding.chat_id.as_str())
            || existing.deleted_at.is_some()
        {
            return Err(ApiError::session("Session belongs to an expired Codex chat binding", 410));
        }
    }

    let title = session_title(&transcript);
    let mut facts = Map::new();
    facts.insert("transcript".into(), Value::String(transcript.clone()));
    facts.insert("ask_id".into(), Value::String(ask_id.clone()));
    if let Some(locale) = locale.clone() {
        facts.insert("locale".into(), Value::String(locale));
    }
    let session = sessions::create_or_resume_session(
        db,
        &agent.id,
        user_id,
        &SessionRequest {
            skill_id: "phone.ask".into(),
            session_id: requested_session_id,
            idempotency_key: Some(format!("ask:{idempotency_key}")),
            title: Some(title),
            chat_id: Some(binding.chat_id.clone()),
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
        "INSERT INTO phone_asks (id, user_id, agent_id, transcript, locale, idempotency_key, session_id, binding_id, target_chat_id, claimed_by_chat_id, status, claimed_at, expires_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, 'queued', NULL, ?, ?, ?) ON CONFLICT(user_id, agent_id, idempotency_key) DO NOTHING",
        vec![
            db::text(&ask_id),
            db::text(user_id),
            db::text(agent_id),
            db::text(&transcript),
            db::optional_text(locale.as_deref()),
            db::text(&idempotency_key),
            db::text(&session_id),
            db::text(&binding.id),
            db::text(&binding.chat_id),
            db::text(&expires_at),
            db::text(&now),
            db::text(&now),
        ],
    )
    .await?;
    if db::changes(&inserted) == 0 {
        let existing = db::first::<AskRow>(
            db,
            "SELECT id, user_id, agent_id, transcript, locale, session_id, binding_id, target_chat_id, claimed_by_chat_id, status, claimed_at, expires_at, created_at FROM phone_asks WHERE user_id = ? AND agent_id = ? AND idempotency_key = ?",
            vec![
                db::text(user_id),
                db::text(agent_id),
                db::text(&idempotency_key),
            ],
        )
        .await?
        .ok_or_else(|| ApiError::conflict("Ask could not be created"))?;
        if existing.transcript != transcript
            || existing.session_id.as_deref() != Some(session_id.as_str())
        {
            return Err(ApiError::conflict(
                "idempotency_key was already used with different Ask data",
            ));
        }
        ensure_ask_message(
            db,
            user_id,
            &session_id,
            &existing.id,
            &transcript,
            locale.as_deref(),
        )
        .await?;
        return ask_to_api(db, &existing, Some(&agent.label)).await;
    }

    let turn_sequence = ensure_ask_message(
        db,
        user_id,
        &session_id,
        &ask_id,
        &transcript,
        locale.as_deref(),
    )
    .await?;
    Ok(json!({
        "ask_id": ask_id,
        "agent_id": agent_id,
        "agent_label": agent.label,
        "user_id": user_id,
        "transcript": transcript,
        "locale": locale,
        "session_id": session_id,
        "turn_sequence": turn_sequence,
        "context_messages": [{
            "message_id": ask_message_id(&ask_id),
            "role": "user",
            "content": transcript,
            "metadata": {"ask_id": ask_id, "locale": locale},
            "sequence": turn_sequence,
            "created_at": now,
        }],
        "status": "queued",
        "claimed_at": Value::Null,
        "expires_at": expires_at,
        "created_at": now,
        "binding_id": binding.id,
        "target_chat_id": binding.chat_id,
        "claimed_by_chat_id": Value::Null,
    }))
}

pub async fn list_agent_asks(
    db: &D1Database,
    agent_id: &str,
    binding: &ListenerBindingRow,
    claim: bool,
) -> ApiResult<Vec<Value>> {
    expire_asks(db, agent_id).await?;
    let rows: Vec<AskRow> = db::all(
        db,
        "SELECT id, user_id, agent_id, transcript, locale, session_id, binding_id, target_chat_id, claimed_by_chat_id, status, claimed_at, expires_at, created_at FROM phone_asks WHERE agent_id = ? AND binding_id = ? AND target_chat_id = ? AND status = 'queued' ORDER BY created_at ASC LIMIT 20",
        vec![db::text(agent_id), db::text(&binding.id), db::text(&binding.chat_id)],
    )
    .await?;
    let rows = if claim {
        claim_asks(db, &rows, binding).await?
    } else {
        rows
    };
    let mut asks = Vec::with_capacity(rows.len());
    for row in rows {
        asks.push(ask_to_api(db, &row, None).await?);
    }
    Ok(asks)
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
    let mut value = json!({
        "ask_id": row.id,
        "agent_id": row.agent_id,
        "user_id": row.user_id,
        "transcript": row.transcript,
        "locale": row.locale,
        "session_id": row.session_id,
        "turn_sequence": turn_sequence,
        "context_messages": context_messages,
        "status": row.status,
        "claimed_at": row.claimed_at,
        "expires_at": row.expires_at,
        "created_at": row.created_at,
        "binding_id": row.binding_id,
        "target_chat_id": row.target_chat_id,
        "claimed_by_chat_id": row.claimed_by_chat_id,
    });
    if let Some(label) = agent_label {
        value["agent_label"] = Value::String(label.to_string());
    }
    Ok(value)
}

async fn expire_asks(db: &D1Database, agent_id: &str) -> ApiResult<()> {
    db::run(
        db,
        "UPDATE phone_asks SET status = 'expired', updated_at = ? WHERE agent_id = ? AND status = 'queued' AND expires_at <= ?",
        vec![
            db::text(&db::now_iso()),
            db::text(agent_id),
            db::text(&db::now_iso()),
        ],
    )
    .await?;
    Ok(())
}

async fn claim_asks(db: &D1Database, rows: &[AskRow], binding: &ListenerBindingRow) -> ApiResult<Vec<AskRow>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let now = db::now_iso();
    let mut claimed = Vec::new();
    for row in rows {
        let updated = db::run(
            db,
            "UPDATE phone_asks SET status = 'claimed', claimed_at = ?, claimed_by_chat_id = ?, updated_at = ? WHERE id = ? AND binding_id = ? AND target_chat_id = ? AND status = 'queued'",
            vec![db::text(&now), db::text(&binding.chat_id), db::text(&now), db::text(&row.id), db::text(&binding.id), db::text(&binding.chat_id)],
        )
        .await?;
        if db::changes(&updated) > 0 {
            let mut next = row.clone();
            next.status = "claimed".into();
            next.claimed_at = Some(now.clone());
            next.claimed_by_chat_id = Some(binding.chat_id.clone());
            claimed.push(next);
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
