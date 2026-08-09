use serde::Deserialize;
use serde_json::{json, Map, Value};
use worker::D1Database;

use crate::auth::new_id;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::CommandRow;
use crate::providers::{self, ActionProviderConfig};

const MAX_TITLE: usize = 200;
const MAX_RECIPIENT: usize = 320;
const MAX_BODY: usize = 8_000;

#[derive(Debug, Clone, Deserialize)]
struct EffectAttemptRow {
    provider: String,
    state: String,
    response_json: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct EffectRow {
    id: String,
    status: String,
}

/// Execute an action whose effect is fully represented in D1.
///
/// The caller is the Outbox worker, so this function must be safe to invoke
/// repeatedly. Each effect table has a `(user_id, command_id)` uniqueness
/// constraint and `action_attempts` records the provider-level result.
pub async fn execute(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    args: &Map<String, Value>,
    provider_config: ActionProviderConfig,
) -> ApiResult<Value> {
    if let Some(previous) = previous_attempt(db, command).await? {
        match previous.state.as_str() {
            "succeeded" => return parse_response(previous.response_json.as_deref()),
            "running" | "unknown" | "retrying"
                if !provider_config.mode().local_effects_allowed() =>
            {
                return Err(ApiError::new(
                    503,
                    "effect_in_progress",
                    format!(
                        "The provider effect is still being reconciled by {}",
                        previous.provider
                    ),
                ));
            }
            _ => {}
        }
    }

    match command.intent.as_str() {
        "create_reminder" => create_reminder(db, user_id, command, args, provider_config).await,
        "create_draft" => create_draft(db, user_id, command, args).await,
        "send_message" => queue_message(db, user_id, command, args, provider_config).await,
        _ => Err(ApiError::new(
            422,
            "unsupported_intent",
            format!(
                "No durable effect executor is registered for {}",
                command.intent
            ),
        )),
    }
}

/// Cancel a reversible materialized effect. This is intentionally not a
/// generic SQL delete: the cancelled state remains queryable and auditable.
pub async fn undo(db: &D1Database, user_id: &str, command: &CommandRow) -> ApiResult<Value> {
    let (table, provider) = match command.intent.as_str() {
        "create_reminder" => ("reminders", "local.reminder"),
        "create_draft" => ("drafts", "local.draft"),
        _ => return Err(ApiError::conflict("Command is not currently undoable")),
    };

    let now = db::now_iso();
    let id_sql = format!("SELECT id, status FROM {table} WHERE user_id = ? AND command_id = ?");
    let effect =
        db::first::<EffectRow>(db, &id_sql, vec![db::text(user_id), db::text(&command.id)])
            .await?
            .ok_or_else(|| ApiError::not_found("Command effect was not found"))?;
    if effect.status == "cancelled" {
        return Ok(json!({
            "kind": "undo",
            "provider": provider,
            "effect_id": effect.id,
            "status": "cancelled",
            "already_cancelled": true,
        }));
    }
    if !matches!(effect.status.as_str(), "scheduled" | "draft") {
        return Err(ApiError::conflict("Command effect is no longer undoable"));
    }

    let result = json!({
        "kind": "undo",
        "provider": provider,
        "effect_id": effect.id,
        "status": "cancelled",
        "already_cancelled": false,
    });
    let mut command_result = command
        .result_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .unwrap_or_else(|| json!({}));
    if let Some(object) = command_result.as_object_mut() {
        object.insert("undo".to_string(), result.clone());
    } else {
        command_result = json!({"value": command_result, "undo": result});
    }
    let next_version = command.version + 1;
    let statements = vec![
        db::prepare(
            db,
            &format!(
                "UPDATE {table} SET status = 'cancelled', updated_at = ? WHERE user_id = ? AND command_id = ? AND status IN ('scheduled', 'draft') AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'succeeded' AND version = ?)"
            ),
            vec![
                db::text(&now),
                db::text(user_id),
                db::text(&command.id),
                db::text(&command.id),
                db::text(user_id),
                db::number(command.version),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE commands SET result_json = ?, version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'succeeded' AND version = ?",
            vec![
                db::text(&command_result.to_string()),
                db::number(next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(command.version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.undo', ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'succeeded' AND version = ?)",
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(&json!({"command_id": command.id, "effect_id": effect.id, "version": next_version}).to_string()),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(next_version),
            ],
        )?,
        db::prepare(
            db,
            "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'succeeded' AND version = ?)",
            vec![
                db::text(user_id),
                db::text(&command.id),
                db::optional_text(command.session_id.as_deref()),
                db::number(next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(next_version),
            ],
        )?,
    ];
    let results = db.batch(statements).await?;
    if results.get(1).map(db::changes).unwrap_or(0) == 0 {
        let current =
            db::first::<EffectRow>(db, &id_sql, vec![db::text(user_id), db::text(&command.id)])
                .await?;
        if current
            .as_ref()
            .is_some_and(|row| row.status == "cancelled")
        {
            return Ok(json!({
                "kind": "undo",
                "provider": provider,
                "effect_id": effect.id,
                "status": "cancelled",
                "already_cancelled": true,
            }));
        }
        return Err(ApiError::conflict("Command was already undone or changed"));
    }
    Ok(result)
}

async fn create_reminder(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    args: &Map<String, Value>,
    provider_config: ActionProviderConfig,
) -> ApiResult<Value> {
    if !provider_config.enabled("create_reminder") {
        return Err(providers::disabled("create_reminder"));
    }
    if !provider_config.mode().local_effects_allowed() {
        return Err(providers::unavailable(
            provider_config.mode(),
            "create_reminder",
        ));
    }
    let title = required_string(args, &["title", "text", "message"], MAX_TITLE, "title")?;
    let due_at = required_string(args, &["due_at", "time", "datetime"], 64, "due_at")?;
    if db::is_expired(due_at) {
        return Err(ApiError::validation("due_at must be in the future"));
    }
    let now = db::now_iso();
    let id = new_id("rem")?;
    db::run(
        db,
        "INSERT OR IGNORE INTO reminders (id, user_id, command_id, session_id, title, due_at, timezone, status, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, 'scheduled', ?, ?)",
        vec![
            db::text(&id),
            db::text(user_id),
            db::text(&command.id),
            db::optional_text(command.session_id.as_deref()),
            db::text(title),
            db::text(due_at),
            db::text(&command.timezone),
            db::text(&now),
            db::text(&now),
        ],
    )
    .await?;
    let effect: EffectRow = db::first(
        db,
        "SELECT id, status FROM reminders WHERE user_id = ? AND command_id = ?",
        vec![db::text(user_id), db::text(&command.id)],
    )
    .await?
    .ok_or_else(|| ApiError::new(500, "effect_error", "Reminder effect was not persisted"))?;
    let response = json!({
        "kind": "reminder",
        "reminder_id": effect.id,
        "status": effect.status,
        "title": title,
        "due_at": due_at,
        "timezone": command.timezone,
    });
    record_attempt(db, user_id, command, "local.reminder", &response).await
}

async fn create_draft(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    args: &Map<String, Value>,
) -> ApiResult<Value> {
    let body = required_string(args, &["body", "content", "text"], MAX_BODY, "body")?;
    let title = optional_string(args, &["title", "subject"], MAX_TITLE, "title")?;
    let recipient = optional_string(args, &["recipient", "to"], MAX_RECIPIENT, "recipient")?;
    let now = db::now_iso();
    let id = new_id("drf")?;
    db::run(
        db,
        "INSERT OR IGNORE INTO drafts (id, user_id, command_id, session_id, title, recipient, body, status, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, 'draft', ?, ?)",
        vec![
            db::text(&id),
            db::text(user_id),
            db::text(&command.id),
            db::optional_text(command.session_id.as_deref()),
            db::optional_text(title),
            db::optional_text(recipient),
            db::text(body),
            db::text(&now),
            db::text(&now),
        ],
    )
    .await?;
    let effect: EffectRow = db::first(
        db,
        "SELECT id, status FROM drafts WHERE user_id = ? AND command_id = ?",
        vec![db::text(user_id), db::text(&command.id)],
    )
    .await?
    .ok_or_else(|| ApiError::new(500, "effect_error", "Draft effect was not persisted"))?;
    let response = json!({
        "kind": "draft",
        "draft_id": effect.id,
        "status": effect.status,
        "title": title,
        "recipient": recipient,
    });
    record_attempt(db, user_id, command, "local.draft", &response).await
}

async fn queue_message(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    args: &Map<String, Value>,
    provider_config: ActionProviderConfig,
) -> ApiResult<Value> {
    if !provider_config.enabled("send_message") {
        return Err(providers::disabled("send_message"));
    }
    if !provider_config.mode().local_effects_allowed() {
        return Err(providers::unavailable(
            provider_config.mode(),
            "send_message",
        ));
    }
    let body = required_string(args, &["body", "message", "text"], MAX_BODY, "body")?;
    let recipient = required_string(
        args,
        &["recipient", "to", "email", "phone"],
        MAX_RECIPIENT,
        "recipient",
    )?;
    let now = db::now_iso();
    let id = new_id("msgout")?;
    db::run(
        db,
        "INSERT OR IGNORE INTO outbound_messages (id, user_id, command_id, session_id, recipient, body, provider, delivery_state, provider_message_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 'internal.outbox', 'queued', NULL, ?, ?)",
        vec![
            db::text(&id),
            db::text(user_id),
            db::text(&command.id),
            db::optional_text(command.session_id.as_deref()),
            db::text(recipient),
            db::text(body),
            db::text(&now),
            db::text(&now),
        ],
    )
    .await?;
    let effect: EffectRow = db::first(
        db,
        "SELECT id, delivery_state AS status FROM outbound_messages WHERE user_id = ? AND command_id = ?",
        vec![db::text(user_id), db::text(&command.id)],
    )
    .await?
    .ok_or_else(|| ApiError::new(500, "effect_error", "Message queue record was not persisted"))?;
    let response = json!({
        "kind": "message",
        "message_id": effect.id,
        "delivery_state": effect.status,
        "provider": "internal.outbox",
        "external_delivery": "not_configured",
    });
    record_attempt(db, user_id, command, "internal.message", &response).await
}

async fn previous_attempt(
    db: &D1Database,
    command: &CommandRow,
) -> ApiResult<Option<EffectAttemptRow>> {
    db::first(
        db,
        "SELECT provider, state, response_json FROM action_attempts WHERE command_id = ? ORDER BY created_at DESC LIMIT 1",
        vec![db::text(&command.id)],
    )
    .await
}

async fn record_attempt(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    provider: &str,
    response: &Value,
) -> ApiResult<Value> {
    db::run(
        db,
        "INSERT OR IGNORE INTO action_attempts (id, user_id, command_id, action_id, provider, provider_idempotency_key, state, request_hash, response_json, attempts, next_attempt_at, last_error, created_at, updated_at) VALUES (?, ?, ?, NULL, ?, ?, 'succeeded', ?, ?, 1, NULL, NULL, ?, ?)",
        vec![
            db::text(&new_id("attempt")?),
            db::text(user_id),
            db::text(&command.id),
            db::text(provider),
            db::text(&command.id),
            db::text(&command.command_hash),
            db::text(&response.to_string()),
            db::text(&command.created_at),
            db::text(&db::now_iso()),
        ],
    )
    .await?;
    Ok(response.clone())
}

fn parse_response(raw: Option<&str>) -> ApiResult<Value> {
    raw.map(serde_json::from_str)
        .transpose()?
        .ok_or_else(|| ApiError::new(500, "effect_error", "Provider result is missing"))
}

fn required_string<'a>(
    args: &'a Map<String, Value>,
    names: &[&str],
    max_len: usize,
    field: &str,
) -> ApiResult<&'a str> {
    let value = names
        .iter()
        .find_map(|name| args.get(*name).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::validation(format!("{field} is required")))?;
    if value.chars().count() > max_len {
        return Err(ApiError::validation(format!("{field} is too long")));
    }
    Ok(value)
}

fn optional_string<'a>(
    args: &'a Map<String, Value>,
    names: &[&str],
    max_len: usize,
    field: &str,
) -> ApiResult<Option<&'a str>> {
    let Some(value) = names
        .iter()
        .find_map(|name| args.get(*name).and_then(Value::as_str))
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if value.chars().count() > max_len {
        return Err(ApiError::validation(format!("{field} is too long")));
    }
    Ok(Some(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_fields_accept_aliases_and_trim_input() {
        let args = serde_json::from_value::<Map<String, Value>>(json!({
            "message": "  call John  "
        }))
        .unwrap();
        assert_eq!(
            required_string(&args, &["body", "message"], 100, "body").unwrap(),
            "call John"
        );
    }

    #[test]
    fn oversized_effect_fields_are_rejected() {
        let args = serde_json::from_value::<Map<String, Value>>(json!({"body": "x".repeat(9_000)}))
            .unwrap();
        assert!(required_string(&args, &["body"], MAX_BODY, "body").is_err());
    }
}
