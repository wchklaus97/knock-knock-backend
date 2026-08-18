use serde::Deserialize;
use serde_json::{json, Map, Value};
use worker::D1Database;

use crate::auth::new_id;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::SessionRequest;
use crate::sessions;
use crate::skills;

pub const LISTENING_WINDOW_SECS: i64 = 90;
const ASK_TTL_SECS: i64 = 86_400;
const MAX_TRANSCRIPT_CHARS: usize = 2_000;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateAskRequest {
    pub transcript: String,
    #[serde(default)]
    pub locale: Option<String>,
    pub idempotency_key: String,
}

#[derive(Debug, Clone, Deserialize)]
struct AskRow {
    id: String,
    user_id: String,
    agent_id: String,
    transcript: String,
    locale: Option<String>,
    session_id: Option<String>,
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
    let agent = db::first::<AgentSeenRow>(
        db,
        "SELECT id, user_id, label, last_seen_at FROM agents WHERE id = ? AND user_id = ?",
        vec![db::text(agent_id), db::text(user_id)],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("Agent not found"))?;
    if !agent_is_listening(agent.last_seen_at.as_deref()) {
        return Err(ApiError::new(
            409,
            "agent_not_listening",
            "The selected agent is not listening. Open the Mac host and keep Knock Knock MCP polling.",
        ));
    }

    if let Some(existing) = db::first::<AskRow>(
        db,
        "SELECT id, user_id, agent_id, transcript, locale, session_id, status, claimed_at, expires_at, created_at FROM phone_asks WHERE user_id = ? AND agent_id = ? AND idempotency_key = ?",
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
        return Ok(ask_to_api(&existing, Some(&agent.label)));
    }

    skills::seed_skill(db).await?;
    let ask_id = new_id("ask")?;
    let now = db::now_iso();
    let expires_at = db::add_seconds_iso(ASK_TTL_SECS);
    let inserted = db::run(
        db,
        "INSERT INTO phone_asks (id, user_id, agent_id, transcript, locale, idempotency_key, session_id, status, claimed_at, expires_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, NULL, 'queued', NULL, ?, ?, ?) ON CONFLICT(user_id, agent_id, idempotency_key) DO NOTHING",
        vec![
            db::text(&ask_id),
            db::text(user_id),
            db::text(agent_id),
            db::text(&transcript),
            db::optional_text(locale.as_deref()),
            db::text(&idempotency_key),
            db::text(&expires_at),
            db::text(&now),
            db::text(&now),
        ],
    )
    .await?;
    if db::changes(&inserted) == 0 {
        let existing = db::first::<AskRow>(
            db,
            "SELECT id, user_id, agent_id, transcript, locale, session_id, status, claimed_at, expires_at, created_at FROM phone_asks WHERE user_id = ? AND agent_id = ? AND idempotency_key = ?",
            vec![
                db::text(user_id),
                db::text(agent_id),
                db::text(&idempotency_key),
            ],
        )
        .await?
        .ok_or_else(|| ApiError::conflict("Ask could not be created"))?;
        return Ok(ask_to_api(&existing, Some(&agent.label)));
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
            session_id: None,
            idempotency_key: Some(format!("ask:{idempotency_key}")),
            title: Some(title),
            chat_id: None,
            facts: Some(facts),
            metadata: None,
        },
    )
    .await?;
    let session_id = session
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Some(session_id) = session_id.as_deref() {
        db::run(
            db,
            "UPDATE phone_asks SET session_id = ?, updated_at = ? WHERE id = ?",
            vec![db::text(session_id), db::text(&now), db::text(&ask_id)],
        )
        .await?;
    }

    let stored = db::first::<AskRow>(
        db,
        "SELECT id, user_id, agent_id, transcript, locale, session_id, status, claimed_at, expires_at, created_at FROM phone_asks WHERE id = ?",
        vec![db::text(&ask_id)],
    )
    .await?
    .ok_or_else(|| ApiError::new(500, "ask_error", "Ask disappeared after insert"))?;
    Ok(ask_to_api(&stored, Some(&agent.label)))
}

pub async fn list_agent_asks(
    db: &D1Database,
    agent_id: &str,
    claim: bool,
) -> ApiResult<Vec<Value>> {
    expire_asks(db, agent_id).await?;
    let rows: Vec<AskRow> = db::all(
        db,
        "SELECT id, user_id, agent_id, transcript, locale, session_id, status, claimed_at, expires_at, created_at FROM phone_asks WHERE agent_id = ? AND status = 'queued' ORDER BY created_at ASC LIMIT 20",
        vec![db::text(agent_id)],
    )
    .await?;
    let rows = if claim {
        claim_asks(db, &rows).await?
    } else {
        rows
    };
    Ok(rows.into_iter().map(|row| ask_to_api(&row, None)).collect())
}

fn agent_is_listening(last_seen_at: Option<&str>) -> bool {
    let Some(seen) = last_seen_at.filter(|value| !value.is_empty()) else {
        return false;
    };
    !db::is_older_than(seen, LISTENING_WINDOW_SECS)
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

fn ask_to_api(row: &AskRow, agent_label: Option<&str>) -> Value {
    let mut value = json!({
        "ask_id": row.id,
        "agent_id": row.agent_id,
        "user_id": row.user_id,
        "transcript": row.transcript,
        "locale": row.locale,
        "session_id": row.session_id,
        "status": row.status,
        "claimed_at": row.claimed_at,
        "expires_at": row.expires_at,
        "created_at": row.created_at,
    });
    if let Some(label) = agent_label {
        value["agent_label"] = Value::String(label.to_string());
    }
    value
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

async fn claim_asks(db: &D1Database, rows: &[AskRow]) -> ApiResult<Vec<AskRow>> {
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let now = db::now_iso();
    let mut claimed = Vec::new();
    for row in rows {
        let updated = db::run(
            db,
            "UPDATE phone_asks SET status = 'claimed', claimed_at = ?, updated_at = ? WHERE id = ? AND status = 'queued'",
            vec![db::text(&now), db::text(&now), db::text(&row.id)],
        )
        .await?;
        if db::changes(&updated) > 0 {
            let mut next = row.clone();
            next.status = "claimed".into();
            next.claimed_at = Some(now.clone());
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
    fn session_title_truncates_without_inventing_words() {
        assert_eq!(session_title("Help with APNs"), "Help with APNs");
        let long = "n".repeat(90);
        let title = session_title(&long);
        assert!(title.ends_with('…'));
        assert_eq!(title.chars().count(), 81);
    }

    #[test]
    fn agent_without_last_seen_is_not_listening() {
        assert!(!agent_is_listening(None));
        assert!(!agent_is_listening(Some("")));
    }
}
