use serde::Deserialize;
use serde_json::{json, Map, Value};
use worker::{D1Database, Env};

use crate::auth::new_id;
use crate::commands;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::CommandRow;
use crate::providers::{self, ActionProviderConfig};

const MAX_TITLE: usize = 200;
const MAX_RECIPIENT: usize = 320;
const MAX_BODY: usize = 8_000;
const CANCEL_LEASE_SECONDS: i64 = 300;

#[derive(Debug, Clone, Deserialize)]
struct EffectAttemptRow {
    provider: String,
    provider_idempotency_key: String,
    state: String,
    response_json: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct CancelAttemptRow {
    state: String,
    response_json: Option<String>,
    updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CancelReconciliationRow {
    user_id: String,
    command_id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct EffectRow {
    id: String,
    status: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    provider_reminder_id: Option<String>,
}

/// Execute an action whose effect is fully represented in D1.
///
/// The caller is the Outbox worker, so this function must be safe to invoke
/// repeatedly. Each effect table has a `(user_id, command_id)` uniqueness
/// constraint and `action_attempts` records the provider-level result.
pub async fn execute(
    env: &Env,
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    args: &Map<String, Value>,
    provider_config: ActionProviderConfig,
) -> ApiResult<Value> {
    let mut reconciled_provider_response = None;
    if let Some(previous) = previous_attempt(db, user_id, command).await? {
        match previous.state.as_str() {
            "succeeded" => return parse_response(previous.response_json.as_deref()),
            "running" | "unknown" | "retrying"
                if provider_config.mode() == providers::ActionProviderMode::External =>
            {
                let provider_idempotency_key = previous.provider_idempotency_key.clone();
                let status = providers::status(
                    env,
                    &provider_config,
                    &command.intent,
                    &provider_idempotency_key,
                    json!({
                        "schema_version": 1,
                        "command_id": command.id,
                        "idempotency_key": provider_idempotency_key.clone(),
                        "command_idempotency_key": command.idempotency_key,
                    }),
                )
                .await?;
                match status.state {
                    providers::ProviderDeliveryState::Succeeded => {
                        reconciled_provider_response = Some(providers::ProviderResponse {
                            provider_id: status.provider_id,
                            state: status.state,
                        });
                    }
                    providers::ProviderDeliveryState::Pending => {
                        return Err(ApiError::new(
                            503,
                            "provider_pending",
                            "The provider has not finished processing this request",
                        ));
                    }
                    providers::ProviderDeliveryState::Failed => {
                        if command.intent == "send_message" {
                            mark_message_delivery(
                                db,
                                user_id,
                                &command.id,
                                "failed",
                                status.provider_id.as_deref(),
                            )
                            .await?;
                        }
                        return Err(ApiError::new(
                            424,
                            "provider_failed",
                            "The provider reported that this request failed",
                        ));
                    }
                    providers::ProviderDeliveryState::Unknown => {
                        return Err(ApiError::new(
                            503,
                            "provider_unknown",
                            "The provider returned an unknown delivery state",
                        ));
                    }
                }
            }
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

    // Persist the provider-level running fence before making any external
    // request. If the Worker stops during the request, the next outbox retry
    // can reconcile this durable key through the provider status endpoint
    // instead of blindly issuing a second send.
    if matches!(
        command.intent.as_str(),
        "create_reminder" | "create_draft" | "send_message"
    ) {
        mark_attempt_running(db, user_id, command).await?;
    }

    match command.intent.as_str() {
        "create_reminder" => {
            create_reminder(
                env,
                db,
                user_id,
                command,
                args,
                provider_config,
                reconciled_provider_response,
            )
            .await
        }
        "create_draft" => create_draft(db, user_id, command, args).await,
        "send_message" => {
            queue_message(
                env,
                db,
                user_id,
                command,
                args,
                provider_config,
                reconciled_provider_response,
            )
            .await
        }
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
pub async fn undo(
    env: &Env,
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    provider_config: ActionProviderConfig,
) -> ApiResult<Value> {
    let (table, default_provider, id_sql) = match command.intent.as_str() {
        "create_reminder" => (
            "reminders",
            "local.reminder",
            "SELECT id, status, provider, provider_reminder_id FROM reminders WHERE user_id = ? AND command_id = ?",
        ),
        "create_draft" => (
            "drafts",
            "local.draft",
            "SELECT id, status, NULL AS provider, NULL AS provider_reminder_id FROM drafts WHERE user_id = ? AND command_id = ?",
        ),
        _ => return Err(ApiError::conflict("Command is not currently undoable")),
    };

    let now = db::now_iso();
    let effect = db::first::<EffectRow>(db, id_sql, vec![db::text(user_id), db::text(&command.id)])
        .await?
        .ok_or_else(|| ApiError::not_found("Command effect was not found"))?;
    let provider = effect
        .provider
        .as_deref()
        .unwrap_or(default_provider)
        .to_string();
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

    if command.intent == "create_reminder" && provider != "local.reminder" {
        let provider_id = effect.provider_reminder_id.as_deref().ok_or_else(|| {
            ApiError::conflict("External reminder has no provider identifier to cancel")
        })?;
        let cancel_key = providers::scoped_idempotency_key(
            user_id,
            "action.reminder.cancel",
            &command.idempotency_key,
        );
        let payload = json!({
            "schema_version": 1,
            "kind": "reminder",
            "operation": "cancel",
            "command_id": command.id,
            "command_idempotency_key": command.idempotency_key,
            "provider_id": provider_id,
        });
        let existing = claim_cancel_attempt(db, user_id, command, &cancel_key).await?;
        let response = if let Some(response) = existing {
            response
        } else {
            match providers::cancel(
                env,
                &provider_config,
                "create_reminder",
                &cancel_key,
                payload,
            )
            .await
            {
                Ok(response) => {
                    let state = match response.state {
                        providers::ProviderDeliveryState::Succeeded => "succeeded",
                        providers::ProviderDeliveryState::Pending
                        | providers::ProviderDeliveryState::Unknown => "unknown",
                        providers::ProviderDeliveryState::Failed => "failed",
                    };
                    finish_cancel_attempt(db, &cancel_key, state, Some(&response), None).await?;
                    response
                }
                Err(error) => {
                    let state = if error.retryable { "unknown" } else { "failed" };
                    finish_cancel_attempt(db, &cancel_key, state, None, Some(&error)).await?;
                    return Err(error);
                }
            }
        };
        if let Some(error) = cancel_state_error(response.state) {
            return Err(error);
        }
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
            db::first::<EffectRow>(db, id_sql, vec![db::text(user_id), db::text(&command.id)])
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

/// Reconcile external reminder cancellations after an inline Undo request
/// returned pending/unknown or the Worker stopped while the provider call was
/// in flight. The provider idempotency key is retained in action_attempts;
/// calling undo again reuses that key and only changes local state after the
/// provider reports a terminal cancellation state.
pub async fn reconcile_external_cancellations(
    env: &Env,
    db: &D1Database,
    provider_config: ActionProviderConfig,
) -> ApiResult<usize> {
    if provider_config.mode() != providers::ActionProviderMode::External {
        return Ok(0);
    }
    let cutoff = db::add_seconds_iso(-CANCEL_LEASE_SECONDS);
    let rows: Vec<CancelReconciliationRow> = db::all(
        db,
        "SELECT user_id, command_id FROM action_attempts WHERE provider = 'external.reminder.cancel' AND user_id IS NOT NULL AND command_id IS NOT NULL AND (state IN ('unknown', 'retrying') OR (state = 'running' AND updated_at <= ?)) ORDER BY updated_at ASC LIMIT 20",
        vec![db::text(&cutoff)],
    )
    .await?;

    let mut processed = 0;
    for row in rows {
        let Some(command) = commands::get_for_user(db, &row.user_id, &row.command_id).await? else {
            continue;
        };
        let _ = undo(env, db, &row.user_id, &command, provider_config.clone()).await;
        processed += 1;
    }
    Ok(processed)
}

async fn create_reminder(
    env: &Env,
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    args: &Map<String, Value>,
    provider_config: ActionProviderConfig,
    reconciled_provider_response: Option<providers::ProviderResponse>,
) -> ApiResult<Value> {
    if !provider_config.enabled("create_reminder") {
        return Err(providers::disabled("create_reminder"));
    }
    let title = required_string(args, &["title", "text", "message"], MAX_TITLE, "title")?;
    let due_at = required_string(args, &["due_at", "time", "datetime"], 64, "due_at")?;
    if db::is_expired(due_at) {
        return Err(ApiError::validation("due_at must be in the future"));
    }
    let external = provider_config.mode() == providers::ActionProviderMode::External;
    let provider_response = if external {
        if let Some(response) = reconciled_provider_response {
            Some(response)
        } else {
            Some(
                providers::send(
                    env,
                    &provider_config,
                    "create_reminder",
                    &providers::scoped_action_idempotency_key(
                        user_id,
                        &command.intent,
                        &command.idempotency_key,
                    ),
                    json!({
                        "schema_version": 1,
                        "kind": "reminder",
                        "command_id": command.id,
                        "command_idempotency_key": command.idempotency_key,
                        "user_id": user_id,
                        "title": title,
                        "due_at": due_at,
                        "timezone": command.timezone,
                    }),
                )
                .await?,
            )
        }
    } else {
        None
    };
    if let Some(response) = provider_response.as_ref() {
        if let Some(error) = provider_state_error("reminder", response.state) {
            return Err(error);
        }
    }
    if external
        && provider_response
            .as_ref()
            .and_then(|response| response.provider_id.as_deref())
            .is_none()
    {
        return Err(ApiError::new(
            503,
            "provider_missing_id",
            "External reminder provider did not return a provider identifier",
        ));
    }
    if !external && !provider_config.mode().local_effects_allowed() {
        return Err(providers::unavailable(
            provider_config.mode(),
            "create_reminder",
        ));
    }
    let provider = if external {
        "external.reminder"
    } else {
        "local.reminder"
    };
    let now = db::now_iso();
    let id = new_id("rem")?;
    db::run(
        db,
        "INSERT OR IGNORE INTO reminders (id, user_id, command_id, session_id, title, due_at, timezone, status, created_at, updated_at, provider, provider_reminder_id) VALUES (?, ?, ?, ?, ?, ?, ?, 'scheduled', ?, ?, ?, ?)",
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
            db::text(provider),
            db::optional_text(
                provider_response
                    .as_ref()
                    .and_then(|value| value.provider_id.as_deref()),
            ),
        ],
    )
    .await?;
    let effect: EffectRow = db::first(
        db,
        "SELECT id, status, provider FROM reminders WHERE user_id = ? AND command_id = ?",
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
        "provider": provider,
        "external_delivery": external.then_some("accepted"),
        "provider_id": provider_response.as_ref().and_then(|value| value.provider_id.clone()),
    });
    record_attempt(db, user_id, command, provider, &response).await
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
    env: &Env,
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    args: &Map<String, Value>,
    provider_config: ActionProviderConfig,
    reconciled_provider_response: Option<providers::ProviderResponse>,
) -> ApiResult<Value> {
    if !provider_config.enabled("send_message") {
        return Err(providers::disabled("send_message"));
    }
    let body = required_string(args, &["body", "message", "text"], MAX_BODY, "body")?;
    let recipient = required_string(
        args,
        &["recipient", "to", "email", "phone"],
        MAX_RECIPIENT,
        "recipient",
    )?;
    let external = provider_config.mode() == providers::ActionProviderMode::External;
    let provider_response = if external {
        if let Some(response) = reconciled_provider_response {
            Some(response)
        } else {
            Some(
                providers::send(
                    env,
                    &provider_config,
                    "send_message",
                    &providers::scoped_action_idempotency_key(
                        user_id,
                        &command.intent,
                        &command.idempotency_key,
                    ),
                    json!({
                        "schema_version": 1,
                        "kind": "message",
                        "command_id": command.id,
                        "command_idempotency_key": command.idempotency_key,
                        "user_id": user_id,
                        "recipient": recipient,
                        "body": body,
                    }),
                )
                .await?,
            )
        }
    } else {
        None
    };
    if !external && !provider_config.mode().local_effects_allowed() {
        return Err(providers::unavailable(
            provider_config.mode(),
            "send_message",
        ));
    }
    let now = db::now_iso();
    let id = new_id("msgout")?;
    let provider = if external {
        "external.message"
    } else {
        "internal.outbox"
    };
    let provider_state = provider_response
        .as_ref()
        .map(|response| response.state)
        .unwrap_or(providers::ProviderDeliveryState::Succeeded);
    let delivery_state = match provider_state {
        providers::ProviderDeliveryState::Succeeded => "sent",
        providers::ProviderDeliveryState::Failed => "failed",
        providers::ProviderDeliveryState::Pending | providers::ProviderDeliveryState::Unknown => {
            "queued"
        }
    };
    db::run(
        db,
        "INSERT OR IGNORE INTO outbound_messages (id, user_id, command_id, session_id, recipient, body, provider, delivery_state, provider_message_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        vec![
            db::text(&id),
            db::text(user_id),
            db::text(&command.id),
            db::optional_text(command.session_id.as_deref()),
            db::text(recipient),
            db::text(body),
            db::text(provider),
            db::text(delivery_state),
            db::optional_text(
                provider_response
                    .as_ref()
                    .and_then(|value| value.provider_id.as_deref()),
            ),
            db::text(&now),
            db::text(&now),
        ],
    )
    .await?;
    if external {
        db::run(
            db,
            "UPDATE outbound_messages SET delivery_state = ?, provider_message_id = COALESCE(?, provider_message_id), updated_at = ? WHERE user_id = ? AND command_id = ?",
            vec![
                db::text(delivery_state),
                db::optional_text(
                    provider_response
                        .as_ref()
                        .and_then(|value| value.provider_id.as_deref()),
                ),
                db::text(&now),
                db::text(user_id),
                db::text(&command.id),
            ],
        )
        .await?;
    }
    let effect: EffectRow = db::first(
        db,
        "SELECT id, delivery_state AS status, NULL AS provider FROM outbound_messages WHERE user_id = ? AND command_id = ?",
        vec![db::text(user_id), db::text(&command.id)],
    )
    .await?
    .ok_or_else(|| ApiError::new(500, "effect_error", "Message queue record was not persisted"))?;
    let response = json!({
        "kind": "message",
        "message_id": effect.id,
        "delivery_state": effect.status,
        "provider": provider,
        "external_delivery": if !external {
            "not_configured"
        } else if provider_state == providers::ProviderDeliveryState::Succeeded {
            "sent"
        } else if provider_state == providers::ProviderDeliveryState::Failed {
            "rejected"
        } else {
            "accepted"
        },
        "provider_id": provider_response.as_ref().and_then(|value| value.provider_id.clone()),
    });
    if external && provider_state != providers::ProviderDeliveryState::Succeeded {
        return Err(provider_state_error("message", provider_state)
            .expect("non-success provider state must have an error"));
    }
    record_attempt(db, user_id, command, provider, &response).await
}

fn provider_state_error(effect: &str, state: providers::ProviderDeliveryState) -> Option<ApiError> {
    match (effect, state) {
        (_, providers::ProviderDeliveryState::Succeeded) => None,
        ("message", providers::ProviderDeliveryState::Pending) => Some(ApiError::new(
            503,
            "provider_pending",
            "The provider accepted the message but has not finished processing it",
        )),
        (_, providers::ProviderDeliveryState::Pending) => Some(ApiError::new(
            503,
            "provider_pending",
            "The provider has not finished processing this request",
        )),
        ("message", providers::ProviderDeliveryState::Failed) => Some(ApiError::new(
            424,
            "provider_failed",
            "The provider reported that the message was not sent",
        )),
        (_, providers::ProviderDeliveryState::Failed) => Some(ApiError::new(
            424,
            "provider_failed",
            "The provider reported that this request failed",
        )),
        ("message", providers::ProviderDeliveryState::Unknown) => Some(ApiError::new(
            503,
            "provider_unknown",
            "The provider returned an unknown message delivery state",
        )),
        (_, providers::ProviderDeliveryState::Unknown) => Some(ApiError::new(
            503,
            "provider_unknown",
            "The provider returned an unknown delivery state",
        )),
    }
}

async fn mark_message_delivery(
    db: &D1Database,
    user_id: &str,
    command_id: &str,
    delivery_state: &str,
    provider_id: Option<&str>,
) -> ApiResult<()> {
    db::run(
        db,
        "UPDATE outbound_messages SET delivery_state = ?, provider_message_id = COALESCE(?, provider_message_id), updated_at = ? WHERE user_id = ? AND command_id = ?",
        vec![
            db::text(delivery_state),
            db::optional_text(provider_id),
            db::text(&db::now_iso()),
            db::text(user_id),
            db::text(command_id),
        ],
    )
    .await?;
    Ok(())
}

async fn previous_attempt(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
) -> ApiResult<Option<EffectAttemptRow>> {
    db::first(
        db,
        "SELECT provider, provider_idempotency_key, state, response_json FROM action_attempts WHERE command_id = ? AND user_id = ? ORDER BY created_at DESC LIMIT 1",
        vec![db::text(&command.id), db::text(user_id)],
    )
    .await
}

async fn mark_attempt_running(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
) -> ApiResult<()> {
    let provider =
        providers::action_attempt_provider(&command.intent).unwrap_or(command.intent.as_str());
    let provider_idempotency_key = providers::scoped_action_idempotency_key(
        user_id,
        &command.intent,
        &command.idempotency_key,
    );
    let now = db::now_iso();
    db::run(
        db,
        "INSERT INTO action_attempts (id, user_id, command_id, action_id, provider, provider_idempotency_key, state, request_hash, response_json, attempts, next_attempt_at, last_error, created_at, updated_at) VALUES (?, ?, ?, NULL, ?, ?, 'running', ?, NULL, 1, NULL, NULL, ?, ?) ON CONFLICT(provider, provider_idempotency_key) DO UPDATE SET state = 'running', attempts = action_attempts.attempts + 1, response_json = NULL, next_attempt_at = NULL, last_error = NULL, updated_at = excluded.updated_at",
        vec![
            db::text(&new_id("attempt")?),
            db::text(user_id),
            db::text(&command.id),
            db::text(provider),
            db::text(&provider_idempotency_key),
            db::text(&command.command_hash),
            db::text(&command.created_at),
            db::text(&now),
        ],
    )
    .await?;
    Ok(())
}

async fn record_attempt(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    provider: &str,
    response: &Value,
) -> ApiResult<Value> {
    let attempt_provider = providers::action_attempt_provider(&command.intent).unwrap_or(provider);
    let provider_idempotency_key = providers::scoped_action_idempotency_key(
        user_id,
        &command.intent,
        &command.idempotency_key,
    );
    db::run(
        db,
        "INSERT INTO action_attempts (id, user_id, command_id, action_id, provider, provider_idempotency_key, state, request_hash, response_json, attempts, next_attempt_at, last_error, created_at, updated_at) VALUES (?, ?, ?, NULL, ?, ?, 'succeeded', ?, ?, 1, NULL, NULL, ?, ?) ON CONFLICT(provider, provider_idempotency_key) DO UPDATE SET state = 'succeeded', response_json = excluded.response_json, attempts = MAX(action_attempts.attempts, excluded.attempts), next_attempt_at = NULL, last_error = NULL, updated_at = excluded.updated_at",
        vec![
            db::text(&new_id("attempt")?),
            db::text(user_id),
            db::text(&command.id),
            db::text(attempt_provider),
            db::text(&provider_idempotency_key),
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

fn cancel_state_error(state: providers::ProviderDeliveryState) -> Option<ApiError> {
    match state {
        providers::ProviderDeliveryState::Succeeded => None,
        providers::ProviderDeliveryState::Pending => Some(ApiError::new(
            503,
            "provider_cancel_pending",
            "The provider has not finished cancelling this reminder",
        )),
        providers::ProviderDeliveryState::Failed => Some(ApiError::new(
            424,
            "provider_cancel_failed",
            "The provider reported that cancelling this reminder failed",
        )),
        providers::ProviderDeliveryState::Unknown => Some(ApiError::new(
            503,
            "provider_cancel_unknown",
            "The provider returned no authoritative cancellation state",
        )),
    }
}

async fn claim_cancel_attempt(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    provider_idempotency_key: &str,
) -> ApiResult<Option<providers::ProviderResponse>> {
    let provider = "external.reminder.cancel";
    let now = db::now_iso();
    let inserted = db::run(
        db,
        "INSERT INTO action_attempts (id, user_id, command_id, action_id, provider, provider_idempotency_key, state, request_hash, response_json, attempts, next_attempt_at, last_error, created_at, updated_at) VALUES (?, ?, ?, NULL, ?, ?, 'running', ?, NULL, 1, NULL, NULL, ?, ?) ON CONFLICT(provider, provider_idempotency_key) DO NOTHING",
        vec![
            db::text(&new_id("attempt")?),
            db::text(user_id),
            db::text(&command.id),
            db::text(provider),
            db::text(provider_idempotency_key),
            db::text(&command.command_hash),
            db::text(&now),
            db::text(&now),
        ],
    )
    .await?;
    if db::changes(&inserted) == 1 {
        return Ok(None);
    }

    let existing: CancelAttemptRow = db::first(
        db,
        "SELECT state, response_json, updated_at FROM action_attempts WHERE provider = ? AND provider_idempotency_key = ?",
        vec![db::text(provider), db::text(provider_idempotency_key)],
    )
    .await?
    .ok_or_else(|| ApiError::new(500, "provider_cancel_error", "Cancellation fence disappeared"))?;

    if existing.state == "succeeded" {
        let provider_id = existing
            .response_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| {
                value
                    .get("provider_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            });
        return Ok(Some(providers::ProviderResponse {
            provider_id,
            state: providers::ProviderDeliveryState::Succeeded,
        }));
    }

    if existing.state == "running"
        && existing.updated_at <= db::add_seconds_iso(-CANCEL_LEASE_SECONDS)
    {
        let reclaimed = db::run(
            db,
            "UPDATE action_attempts SET state = 'running', attempts = attempts + 1, response_json = NULL, next_attempt_at = NULL, last_error = 'cancel_lease_expired', updated_at = ? WHERE provider = ? AND provider_idempotency_key = ? AND state = 'running' AND updated_at <= ?",
            vec![
                db::text(&now),
                db::text(provider),
                db::text(provider_idempotency_key),
                db::text(&db::add_seconds_iso(-CANCEL_LEASE_SECONDS)),
            ],
        )
        .await?;
        if db::changes(&reclaimed) == 1 {
            return Ok(None);
        }
    }

    if matches!(existing.state.as_str(), "unknown" | "retrying" | "failed") {
        let reclaimed = db::run(
            db,
            "UPDATE action_attempts SET state = 'running', attempts = attempts + 1, response_json = NULL, next_attempt_at = NULL, last_error = NULL, updated_at = ? WHERE provider = ? AND provider_idempotency_key = ? AND state IN ('unknown', 'retrying', 'failed')",
            vec![
                db::text(&now),
                db::text(provider),
                db::text(provider_idempotency_key),
            ],
        )
        .await?;
        if db::changes(&reclaimed) == 1 {
            return Ok(None);
        }
    }

    Err(ApiError::new(
        503,
        "provider_cancel_in_progress",
        "Another request is already cancelling this reminder",
    ))
}

async fn finish_cancel_attempt(
    db: &D1Database,
    provider_idempotency_key: &str,
    state: &str,
    response: Option<&providers::ProviderResponse>,
    error: Option<&ApiError>,
) -> ApiResult<()> {
    let response_json = response.map(|value| {
        json!({
            "provider_id": value.provider_id,
            "state": state,
        })
        .to_string()
    });
    db::run(
        db,
        "UPDATE action_attempts SET state = ?, response_json = ?, next_attempt_at = NULL, last_error = ?, updated_at = ? WHERE provider = 'external.reminder.cancel' AND provider_idempotency_key = ? AND state = 'running'",
        vec![
            db::text(state),
            db::optional_text(response_json.as_deref()),
            db::optional_text(error.map(|value| value.code.as_str())),
            db::text(&db::now_iso()),
            db::text(provider_idempotency_key),
        ],
    )
    .await?;
    Ok(())
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

    #[test]
    fn external_provider_non_success_states_are_retryable_or_explicit_failure() {
        assert_eq!(
            provider_state_error("reminder", providers::ProviderDeliveryState::Pending)
                .unwrap()
                .code,
            "provider_pending"
        );
        assert_eq!(
            provider_state_error("message", providers::ProviderDeliveryState::Failed)
                .unwrap()
                .status,
            424
        );
        assert!(
            provider_state_error("reminder", providers::ProviderDeliveryState::Succeeded).is_none()
        );
    }
}
