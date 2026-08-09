use serde_json::{Map, Value};
use worker::{D1Database, D1PreparedStatement, Env};

use crate::audit::record_audit;
use crate::auth::new_id;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::{
    ActionRow, EventRequest, EventRow, ProgressRequest, SessionRequest, SessionRow,
};
use crate::skills::{self, action_needs_confirm, resolve_actions};

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

pub async fn get_session(db: &D1Database, id: &str) -> ApiResult<Option<SessionRow>> {
    db::first(
        db,
        "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, idempotency_key, expires_at, created_at, updated_at FROM sessions WHERE id = ?",
        vec![db::text(id)],
    )
    .await
}

pub async fn get_action(db: &D1Database, id: &str) -> ApiResult<Option<ActionRow>> {
    db::first(
        db,
        "SELECT id, session_id, agent_id, action_key, title, risk, confirm_required, status, result_json, claimed_at, expires_at, created_at, updated_at FROM actions WHERE id = ?",
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
        "UPDATE sessions SET state = 'expired', updated_at = ? WHERE id = ? AND state NOT IN ('expired', 'closed')",
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
        "facts": facts,
        "expires_at": row.expires_at,
        "created_at": row.created_at,
        "updated_at": row.updated_at,
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

pub async fn create_or_resume_session(
    db: &D1Database,
    agent_id: &str,
    user_id: &str,
    input: &SessionRequest,
) -> ApiResult<Value> {
    let skill = skills::get_skill(db, &input.skill_id)
        .await?
        .ok_or_else(|| ApiError::session("Unknown skill_id", 404))?;

    if let Some(session_id) = input.session_id.as_deref() {
        let existing = get_session(db, session_id)
            .await?
            .ok_or_else(|| ApiError::session("Session not found", 404))?;
        if existing.agent_id != agent_id {
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
        if let Some(existing) = db::first::<SessionRow>(
            db,
            "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, idempotency_key, expires_at, created_at, updated_at FROM sessions WHERE agent_id = ? AND idempotency_key = ?",
            vec![db::text(agent_id), db::text(idempotency_key)],
        )
        .await?
        {
            expire_session_if_needed(db, &existing).await?;
            return Ok(session_to_api(
                &get_session(db, &existing.id).await?.unwrap_or(existing),
            ));
        }
    }

    let session_id = new_id("ses")?;
    let now = db::now_iso();
    let expires_at = db::add_seconds_iso(skill.ttl.default_sec);
    let facts = input.facts.clone().unwrap_or_default();
    let chat_id = input.chat_id.clone().or_else(|| {
        input
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("chat_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    });
    db::run(
        db,
        "INSERT INTO sessions (id, agent_id, user_id, skill_id, state, progress_status, progress_message, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, idempotency_key, expires_at, created_at, updated_at) VALUES (?, ?, ?, ?, 'open', NULL, NULL, ?, ?, NULL, NULL, ?, NULL, ?, ?, ?, ?) ON CONFLICT(agent_id, idempotency_key) DO NOTHING",
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
                "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, idempotency_key, expires_at, created_at, updated_at FROM sessions WHERE agent_id = ? AND idempotency_key = ?",
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

pub async fn update_progress(
    db: &D1Database,
    session: &SessionRow,
    input: &ProgressRequest,
) -> ApiResult<Value> {
    if !valid_progress_status(&input.status) {
        return Err(ApiError::validation("Invalid progress status"));
    }
    expire_session_if_needed(db, session).await?;
    let current = get_session(db, &session.id)
        .await?
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
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
    db::run(
        db,
        "UPDATE sessions SET state = ?, progress_status = ?, progress_message = ?, progress_percent = ?, facts_json = ?, updated_at = ? WHERE id = ?",
        vec![
            db::text(&next_state),
            db::text(&input.status),
            db::optional_text(input.message.as_deref().or(current.progress_message.as_deref())),
            input
                .percent
                .map(db::decimal)
                .unwrap_or_else(|| db::optional_decimal(current.progress_percent)),
            db::text(&facts.to_string()),
            db::text(&db::now_iso()),
            db::text(&current.id),
        ],
    )
    .await?;
    let updated = get_session(db, &current.id)
        .await?
        .ok_or_else(|| ApiError::new(500, "progress_error", "Session update failed"))?;
    Ok(session_to_api(&updated))
}

fn should_push(status: &str, actions: &[crate::models::SkillAction], force_push: bool) -> bool {
    status == "needs_user"
        || force_push
        || ((status == "succeeded" || status == "failed") && !actions.is_empty())
}

fn event_result(event: EventRow, session: Value) -> Value {
    serde_json::json!({
        "event_id": event.id,
        "session": session,
        "pushed": event.pushed != 0,
        "summary_text": event.summary_text.unwrap_or_default(),
        "voice_script": event.voice_script.unwrap_or_default(),
        "deduped": true,
    })
}

pub async fn report_event(
    db: &D1Database,
    env: &Env,
    session: &SessionRow,
    input: &EventRequest,
) -> ApiResult<Value> {
    if !["info", "needs_user", "succeeded", "failed"].contains(&input.status.as_str()) {
        return Err(ApiError::validation("Invalid event status"));
    }
    expire_session_if_needed(db, session).await?;
    let current = get_session(db, &session.id)
        .await?
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    if current.state == "expired" {
        return Err(ApiError::session("Session expired", 410));
    }
    if let Some(previous) = db::first::<EventRow>(
        db,
        "SELECT id, pushed, summary_text, voice_script FROM events WHERE session_id = ? AND idempotency_key = ?",
        vec![db::text(&current.id), db::text(&input.idempotency_key)],
    )
    .await?
    {
        return Ok(event_result(previous, session_api(db, current).await?));
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
    let pushed = should_push(&input.status, &resolved, input.force_push.unwrap_or(false));
    let next_state = match input.status.as_str() {
        "needs_user" => "needs_user",
        "succeeded" | "failed" if !resolved.is_empty() => "needs_user",
        "succeeded" | "failed" => "closed",
        "info" if current.state == "needs_user" => "needs_user",
        "info" => "running",
        _ => &current.state,
    };
    let event_id = new_id("evt")?;
    let now = db::now_iso();
    let insert_result = db::run(
        db,
        "INSERT INTO events (id, session_id, status, idempotency_key, payload_json, pushed, summary_text, voice_script, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(session_id, idempotency_key) DO NOTHING",
        vec![
            db::text(&event_id),
            db::text(&current.id),
            db::text(&input.status),
            db::text(&input.idempotency_key),
            db::text(&serde_json::json!({"facts": facts, "actions": resolved, "force_push": input.force_push.unwrap_or(false)}).to_string()),
            db::number(if pushed { 1 } else { 0 }),
            db::text(&summary),
            db::text(&voice),
            db::text(&now),
        ],
    )
    .await?;
    if db::changes(&insert_result) == 0 {
        let previous: EventRow = db::first(
            db,
            "SELECT id, pushed, summary_text, voice_script FROM events WHERE session_id = ? AND idempotency_key = ?",
            vec![db::text(&current.id), db::text(&input.idempotency_key)],
        )
        .await?
        .ok_or_else(|| ApiError::conflict("Event idempotency conflict"))?;
        return Ok(event_result(previous, session_api(db, current).await?));
    }

    let mut statements: Vec<D1PreparedStatement> = Vec::new();
    if !resolved.is_empty() {
        statements.push(db::prepare(
            db,
            "UPDATE actions SET status = 'cancelled', updated_at = ? WHERE session_id = ? AND status IN ('offered', 'pending_confirm', 'awaiting_confirm')",
            vec![db::text(&now), db::text(&current.id)],
        )?);
    }
    let action_keys: Vec<String> = resolved.iter().map(|action| action.id.clone()).collect();
    let message_id = new_id("msg")?;
    for action in &resolved {
        let ttl = if action_needs_confirm(action) {
            skill.ttl.destructive_sec
        } else {
            skill.ttl.default_sec
        };
        let action_id = new_id("act")?;
        statements.push(db::prepare(
            db,
            "INSERT INTO actions (id, session_id, agent_id, action_key, title, risk, confirm_required, status, result_json, claimed_at, expires_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, 'offered', NULL, NULL, ?, ?, ?)",
            vec![
                db::text(&action_id),
                db::text(&current.id),
                db::text(&current.agent_id),
                db::text(&action.id),
                db::text(&action.title),
                db::text(&action.risk),
                db::number(if action_needs_confirm(action) { 1 } else { 0 }),
                db::text(&db::add_seconds_iso(ttl)),
                db::text(&now),
                db::text(&now),
            ],
        )?);
    }
    statements.push(db::prepare(
        db,
        "UPDATE sessions SET state = ?, summary_text = ?, voice_script = ?, facts_json = ?, available_actions_json = ?, updated_at = ? WHERE id = ?",
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
            db::text(&now),
            db::text(&current.id),
        ],
    )?);
    statements.push(db::prepare(
        db,
        "INSERT INTO session_messages (id, user_id, session_id, role, content, metadata_json, command_id, sequence, retention_expires_at, created_at) SELECT ?, ?, ?, 'agent', ?, ?, NULL, COALESCE(MAX(sequence) + 1, 1), NULL, ? FROM session_messages WHERE user_id = ? AND session_id = ?",
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
                })
                .to_string(),
            ),
            db::text(&now),
            db::text(&current.user_id),
            db::text(&current.id),
        ],
    )?);
    db.batch(statements).await?;

    record_audit(
        db,
        &format!("session.event.{}", input.status),
        Some(&current.user_id),
        Some(&current.agent_id),
        Some(&current.id),
        serde_json::json!({"event_id": event_id, "actions": action_keys, "pushed": pushed}),
    )
    .await;
    if pushed {
        if let Ok(delivery) = crate::push::notify_user(
            db,
            env,
            crate::push::PushRequest {
                user_id: &current.user_id,
                session_id: &current.id,
                title: current.title.as_deref().unwrap_or(&skill.skill_id),
                body: &summary,
                voice_script: Some(&voice),
                payload: serde_json::json!({
                "event_id": event_id,
                "status": input.status,
                "actions": action_keys,
                }),
            },
        )
        .await
        {
            let _ = (
                delivery.inbox,
                delivery.apns_sent,
                delivery.apns_errors.len(),
            );
        }
    }
    let fresh = get_session(db, &current.id)
        .await?
        .ok_or_else(|| ApiError::new(500, "event_error", "Session update failed"))?;
    Ok(serde_json::json!({
        "event_id": event_id,
        "session": session_to_api(&fresh),
        "pushed": pushed,
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
            "SELECT id, session_id, agent_id, action_key, title, risk, confirm_required, status, result_json, claimed_at, expires_at, created_at, updated_at FROM actions WHERE agent_id = ? AND session_id = ? AND status = 'queued' ORDER BY created_at ASC",
            vec![db::text(agent_id), db::text(session_id)],
        )
        .await?
    } else {
        db::all(
            db,
            "SELECT id, session_id, agent_id, action_key, title, risk, confirm_required, status, result_json, claimed_at, expires_at, created_at, updated_at FROM actions WHERE agent_id = ? AND status = 'queued' ORDER BY created_at ASC",
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
            "UPDATE actions SET status = 'claimed', claimed_at = ?, updated_at = ? WHERE id = ? AND status = 'queued'",
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
                    "UPDATE sessions SET state = 'claimed', updated_at = ? WHERE id = ? AND state = 'queued'",
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
        "SELECT id, session_id, agent_id, action_key, title, risk, confirm_required, status, result_json, claimed_at, expires_at, created_at, updated_at FROM actions WHERE session_id = ? AND status IN ('offered', 'pending_confirm', 'awaiting_confirm') ORDER BY created_at ASC",
        vec![db::text(session_id)],
    )
    .await
}

pub async fn reconcile_waiting_session(db: &D1Database, row: SessionRow) -> ApiResult<SessionRow> {
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
    let stored_keys = db::parse_json_array(current.available_actions_json.as_deref());
    if active_keys.is_empty() {
        db::run(
            db,
            "UPDATE sessions SET state = 'running', available_actions_json = NULL, progress_message = ?, updated_at = ? WHERE id = ? AND state IN ('needs_user', 'awaiting_confirm') AND NOT EXISTS (SELECT 1 FROM actions WHERE session_id = sessions.id AND status IN ('offered', 'pending_confirm', 'awaiting_confirm'))",
            vec![
                db::text("No actionable phone decision remains; the agent must emit a new decision."),
                db::text(&db::now_iso()),
                db::text(&current.id),
            ],
        )
        .await?;
        return Ok(get_session(db, &current.id).await?.unwrap_or(current));
    }
    if active_keys != stored_keys {
        db::run(
            db,
            "UPDATE sessions SET available_actions_json = ?, updated_at = ? WHERE id = ?",
            vec![
                db::text(&serde_json::to_string(&active_keys)?),
                db::text(&db::now_iso()),
                db::text(&current.id),
            ],
        )
        .await?;
        return Ok(get_session(db, &current.id).await?.unwrap_or(current));
    }
    Ok(current)
}

pub async fn list_phone_sessions(
    db: &D1Database,
    user_id: &str,
    limit: i32,
) -> ApiResult<Vec<Value>> {
    let rows: Vec<SessionRow> = db::all(
        db,
        "SELECT id, agent_id, user_id, skill_id, state, progress_status, progress_message, progress_percent, title, chat_id, summary_text, voice_script, facts_json, available_actions_json, idempotency_key, expires_at, created_at, updated_at FROM sessions WHERE user_id = ? ORDER BY updated_at DESC LIMIT ?",
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
    expire_session_if_needed(db, &session).await?;
    let current = get_session(db, session_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    if current.state == "expired" {
        return Err(ApiError::gone("Session expired"));
    }
    let action = db::first::<ActionRow>(
        db,
        "SELECT id, session_id, agent_id, action_key, title, risk, confirm_required, status, result_json, claimed_at, expires_at, created_at, updated_at FROM actions WHERE session_id = ? AND action_key = ? AND status IN ('offered', 'pending_confirm', 'awaiting_confirm') ORDER BY created_at DESC LIMIT 1",
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
    let mut statements = vec![db::prepare(
        db,
        "UPDATE actions SET status = ?, updated_at = ? WHERE id = ? AND status = 'offered' AND EXISTS (SELECT 1 FROM sessions WHERE id = ? AND state IN ('needs_user', 'awaiting_confirm'))",
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
            "UPDATE actions SET status = 'cancelled', updated_at = ? WHERE session_id = ? AND id <> ? AND status IN ('offered', 'pending_confirm', 'awaiting_confirm')",
            vec![db::text(&now), db::text(&current.id), db::text(&fresh.id)],
        )?);
    }
    statements.push(db::prepare(
        db,
        "UPDATE sessions SET state = ?, available_actions_json = CASE WHEN ? = 1 THEN available_actions_json ELSE NULL END, updated_at = ? WHERE id = ? AND state IN ('needs_user', 'awaiting_confirm')",
        vec![
            db::text(next_session_state),
            db::number(if needs_confirm { 1 } else { 0 }),
            db::text(&now),
            db::text(&current.id),
        ],
    )?);
    statements.push(db::prepare(
        db,
        "INSERT INTO session_messages (id, user_id, session_id, role, content, metadata_json, command_id, sequence, retention_expires_at, created_at) SELECT ?, ?, ?, 'user', ?, ?, NULL, COALESCE(MAX(sequence) + 1, 1), NULL, ? FROM session_messages WHERE user_id = ? AND session_id = ?",
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
            db::text(&now),
            db::text(user_id),
            db::text(&current.id),
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
    expire_session_if_needed(db, &session).await?;
    let current = get_session(db, session_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
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
        "UPDATE actions SET status = ?, result_json = CASE WHEN ? = 1 THEN ? ELSE result_json END, updated_at = ? WHERE id = ? AND status IN ('pending_confirm', 'awaiting_confirm') AND EXISTS (SELECT 1 FROM sessions WHERE id = ? AND state = 'awaiting_confirm')",
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
            "UPDATE actions SET status = 'cancelled', updated_at = ? WHERE session_id = ? AND id <> ? AND status IN ('offered', 'pending_confirm', 'awaiting_confirm')",
            vec![db::text(&now), db::text(&current.id), db::text(&fresh.id)],
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
    statements.push(db::prepare(
        db,
        "UPDATE sessions SET state = ?, available_actions_json = ?, progress_message = COALESCE(?, progress_message), updated_at = ? WHERE id = ? AND state = 'awaiting_confirm'",
        vec![
            db::text(next_state),
            db::optional_text(available_actions.as_deref()),
            db::optional_text(message),
            db::text(&now),
            db::text(&current.id),
        ],
    )?);
    statements.push(db::prepare(
        db,
        "INSERT INTO session_messages (id, user_id, session_id, role, content, metadata_json, command_id, sequence, retention_expires_at, created_at) SELECT ?, ?, ?, 'user', ?, ?, NULL, COALESCE(MAX(sequence) + 1, 1), NULL, ? FROM session_messages WHERE user_id = ? AND session_id = ?",
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
            "UPDATE actions SET status = ?, result_json = ?, updated_at = ? WHERE id = ? AND status IN ('claimed', 'queued')",
            vec![
                db::text(status),
                db::text(&result.to_string()),
                db::text(&now),
                db::text(action_id),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE sessions SET state = ?, available_actions_json = NULL, updated_at = ? WHERE id = ?",
            vec![
                db::text(if ok { "running" } else { "closed" }),
                db::text(&now),
                db::text(&fresh.session_id),
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
}
