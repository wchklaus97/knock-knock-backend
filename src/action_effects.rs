use serde::Deserialize;
use serde_json::{json, Map, Value};
use worker::{D1Database, Env};

use crate::auth::new_id;
use crate::commands;
use crate::db;
use crate::error::{ApiError, ApiResult};
use crate::models::{CommandRow, OutboxEventRow};
use crate::providers::{self, ActionProviderConfig};

const MAX_TITLE: usize = 200;
const MAX_RECIPIENT: usize = 320;
const MAX_BODY: usize = 8_000;
const CANCEL_LEASE_SECONDS: i64 = 300;
const REMINDER_EFFECT_SQL: &str = "SELECT id, status, provider, provider_reminder_id FROM reminders WHERE user_id = ? AND command_id = ?";

#[derive(Debug, Clone, Deserialize)]
struct EffectAttemptRow {
    provider: String,
    provider_idempotency_key: String,
    state: String,
    response_json: Option<String>,
    attempts: i32,
}

#[derive(Debug, Clone, Deserialize)]
struct MaterializedReminderRow {
    id: String,
    status: String,
    title: String,
    due_at: String,
    timezone: String,
    provider: String,
    provider_reminder_id: Option<String>,
}

fn materialized_reminder_response(row: &MaterializedReminderRow) -> Value {
    let external = row.provider != "local.reminder";
    json!({
        "kind": "reminder",
        "reminder_id": row.id,
        "status": row.status,
        "title": row.title,
        "due_at": row.due_at,
        "timezone": row.timezone,
        "provider": row.provider,
        "external_delivery": external.then_some("accepted"),
        "provider_id": row.provider_reminder_id,
    })
}

async fn recover_materialized_reminder(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
) -> ApiResult<Option<Value>> {
    if command.intent != "create_reminder" {
        return Ok(None);
    }
    let Some(row) = db::first::<MaterializedReminderRow>(
        db,
        "SELECT id, status, title, due_at, timezone, provider, provider_reminder_id FROM reminders WHERE user_id = ? AND command_id = ? LIMIT 1",
        vec![db::text(user_id), db::text(&command.id)],
    )
    .await?
    else {
        return Ok(None);
    };
    let response = materialized_reminder_response(&row);
    record_attempt(db, user_id, command, &row.provider, &response)
        .await
        .map(Some)
}

fn reusable_succeeded_response(attempt: &EffectAttemptRow) -> ApiResult<Option<Value>> {
    if attempt.state == "succeeded" {
        parse_response(attempt.response_json.as_deref()).map(Some)
    } else {
        Ok(None)
    }
}

fn is_execution_permit_only(attempt: &EffectAttemptRow) -> bool {
    attempt.state == "running" && attempt.attempts == 0
}

fn validate_args_for_effect(
    intent: &str,
    args: &Map<String, Value>,
    recovered_provider_success: bool,
) -> Result<(), commands::CommandValidationError> {
    if recovered_provider_success {
        commands::validate_action_args_shape(intent, args)
    } else {
        commands::validate_action_args(intent, args)
    }
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
    attempt_state: String,
}

#[derive(Debug, Clone)]
enum CancelAttemptClaim {
    CallProvider,
    ReuseSucceeded(providers::ProviderResponse),
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

#[derive(Debug, Clone)]
struct UndoFinalization {
    result: Value,
    command_result: Value,
    next_version: i64,
}

fn succeeded_cancel_reconciliation_sql() -> &'static str {
    "SELECT attempt.user_id, attempt.command_id, attempt.state AS attempt_state FROM action_attempts AS attempt INNER JOIN reminders AS reminder ON reminder.user_id = attempt.user_id AND reminder.command_id = attempt.command_id INNER JOIN commands AS command ON command.id = attempt.command_id AND command.user_id = attempt.user_id WHERE attempt.provider = 'external.reminder.cancel' AND attempt.user_id IS NOT NULL AND attempt.command_id IS NOT NULL AND reminder.status = 'scheduled' AND command.state = 'succeeded' AND attempt.state = 'succeeded' ORDER BY attempt.updated_at ASC LIMIT 20"
}

fn pending_cancel_reconciliation_sql() -> &'static str {
    "SELECT attempt.user_id, attempt.command_id, attempt.state AS attempt_state FROM action_attempts AS attempt INNER JOIN reminders AS reminder ON reminder.user_id = attempt.user_id AND reminder.command_id = attempt.command_id INNER JOIN commands AS command ON command.id = attempt.command_id AND command.user_id = attempt.user_id WHERE attempt.provider = 'external.reminder.cancel' AND attempt.user_id IS NOT NULL AND attempt.command_id IS NOT NULL AND reminder.status = 'scheduled' AND command.state = 'succeeded' AND (attempt.state IN ('unknown', 'retrying') OR (attempt.state = 'running' AND attempt.updated_at <= ?)) ORDER BY attempt.updated_at ASC LIMIT 20"
}

fn is_succeeded_cancel_reconciliation_row(row: &CancelReconciliationRow) -> bool {
    row.attempt_state == "succeeded"
}

fn is_pending_cancel_reconciliation_row(row: &CancelReconciliationRow) -> bool {
    matches!(
        row.attempt_state.as_str(),
        "unknown" | "retrying" | "running"
    )
}

fn pending_cancel_reconciliation_enabled(mode: providers::ActionProviderMode) -> bool {
    mode == providers::ActionProviderMode::External
}

fn undo_effect_update_sql(table: &str) -> String {
    format!(
        "UPDATE {table} SET status = 'cancelled', updated_at = ? WHERE user_id = ? AND command_id = ? AND status IN ('scheduled', 'draft') AND EXISTS (SELECT 1 FROM commands WHERE id = ? AND user_id = ? AND state = 'succeeded' AND version = ?)"
    )
}

fn undo_command_update_sql() -> &'static str {
    "UPDATE commands SET result_json = ?, version = ?, updated_at = ? WHERE id = ? AND user_id = ? AND state = 'succeeded' AND version = ? AND changes() = 1"
}

fn undo_audit_insert_sql() -> &'static str {
    "INSERT INTO audit_logs (id, user_id, session_id, action, metadata_json, created_at) SELECT ?, ?, ?, 'command.undo', ?, ? WHERE changes() = 1"
}

fn undo_phone_change_insert_sql() -> &'static str {
    "INSERT INTO phone_changes (user_id, entity_type, entity_id, session_id, version, created_at) SELECT ?, 'command', ?, ?, ?, ? WHERE changes() = 1"
}

fn build_undo_finalization(
    command: &CommandRow,
    effect_id: &str,
    provider: &str,
) -> UndoFinalization {
    let result = json!({
        "kind": "undo",
        "provider": provider,
        "effect_id": effect_id,
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
    UndoFinalization {
        result,
        command_result,
        next_version: command.version + 1,
    }
}

async fn finalize_undo_transaction(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    effect: &EffectRow,
    provider: &str,
    table: &str,
    id_sql: &str,
) -> ApiResult<Value> {
    let now = db::now_iso();
    let finalization = build_undo_finalization(command, &effect.id, provider);
    let command_result_json = finalization.command_result.to_string();
    let statements = vec![
        db::prepare(
            db,
            &undo_effect_update_sql(table),
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
            undo_command_update_sql(),
            vec![
                db::text(&command_result_json),
                db::number(finalization.next_version),
                db::text(&now),
                db::text(&command.id),
                db::text(user_id),
                db::number(command.version),
            ],
        )?,
        db::prepare(
            db,
            undo_audit_insert_sql(),
            vec![
                db::text(&new_id("aud")?),
                db::text(user_id),
                db::optional_text(command.session_id.as_deref()),
                db::text(&json!({"command_id": command.id, "effect_id": effect.id, "version": finalization.next_version}).to_string()),
                db::text(&now),
            ],
        )?,
        db::prepare(
            db,
            undo_phone_change_insert_sql(),
            vec![
                db::text(user_id),
                db::text(&command.id),
                db::optional_text(command.session_id.as_deref()),
                db::number(finalization.next_version),
                db::text(&now),
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
    Ok(finalization.result)
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
    claim: &OutboxEventRow,
    args: &Map<String, Value>,
    provider_config: ActionProviderConfig,
) -> ApiResult<Value> {
    let mut reconciled_provider_response = None;
    if let Some(previous) = previous_attempt(db, user_id, command).await? {
        if let Some(response) = reusable_succeeded_response(&previous)? {
            return Ok(response);
        }
        let execution_permit_only = is_execution_permit_only(&previous);
        if !execution_permit_only
            && matches!(previous.state.as_str(), "running" | "unknown" | "retrying")
        {
            if let Some(response) = recover_materialized_reminder(db, user_id, command).await? {
                return Ok(response);
            }
        }
        match previous.state.as_str() {
            "running" | "unknown" | "retrying"
                if !execution_permit_only
                    && provider_config.mode() == providers::ActionProviderMode::External =>
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
                if !execution_permit_only && !provider_config.mode().local_effects_allowed() =>
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

    let recovered_provider_success = reconciled_provider_response.is_some();
    validate_args_for_effect(&command.intent, args, recovered_provider_success)
        .map_err(|error| ApiError::validation(error.to_string()))?;

    // The outbox permit transaction already persisted an attempts=0 row under
    // the exact command generation and lease. Advancing it to attempts>=1 is
    // the boundary after which recovery must reconcile rather than assume no
    // effect started. It never overwrites a concurrently succeeded attempt.
    if reconciled_provider_response.is_none()
        && matches!(
            command.intent.as_str(),
            "create_reminder" | "create_draft" | "send_message"
        )
    {
        if let Some(response) = begin_effect_attempt(db, user_id, command, claim).await? {
            return Ok(response);
        }
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
            REMINDER_EFFECT_SQL,
        ),
        "create_draft" => (
            "drafts",
            "local.draft",
            "SELECT id, status, NULL AS provider, NULL AS provider_reminder_id FROM drafts WHERE user_id = ? AND command_id = ?",
        ),
        _ => return Err(ApiError::conflict("Command is not currently undoable")),
    };

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
        let claim = claim_cancel_attempt(db, user_id, command, &cancel_key).await?;
        let response = match claim {
            CancelAttemptClaim::ReuseSucceeded(response) => response,
            CancelAttemptClaim::CallProvider => match providers::cancel(
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
            },
        };
        if let Some(error) = cancel_state_error(response.state) {
            return Err(error);
        }
    }

    finalize_undo_transaction(db, user_id, command, &effect, &provider, table, id_sql).await
}

/// Finish the local transaction for provider cancellations that already
/// succeeded. This path deliberately needs neither an Env nor provider
/// configuration: the external side effect is terminal and must be reflected
/// in D1 regardless of its age or the current adapter configuration.
pub async fn reconcile_succeeded_cancellations(db: &D1Database) -> ApiResult<usize> {
    let rows: Vec<CancelReconciliationRow> =
        db::all(db, succeeded_cancel_reconciliation_sql(), vec![]).await?;

    let mut processed = 0;
    for row in rows {
        if !is_succeeded_cancel_reconciliation_row(&row) {
            continue;
        }
        let Some(command) = commands::get_for_user(db, &row.user_id, &row.command_id).await? else {
            continue;
        };
        if command.state != "succeeded" || command.intent != "create_reminder" {
            continue;
        }
        let Some(effect) = db::first::<EffectRow>(
            db,
            REMINDER_EFFECT_SQL,
            vec![db::text(&row.user_id), db::text(&row.command_id)],
        )
        .await?
        else {
            continue;
        };
        if effect.status != "scheduled" {
            continue;
        }
        let provider = effect
            .provider
            .as_deref()
            .unwrap_or("external.reminder")
            .to_string();
        if finalize_undo_transaction(
            db,
            &row.user_id,
            &command,
            &effect,
            &provider,
            "reminders",
            REMINDER_EFFECT_SQL,
        )
        .await
        .is_ok()
        {
            processed += 1;
        }
    }
    Ok(processed)
}

/// Reconcile nonterminal external reminder cancellations after an inline Undo
/// returned pending/unknown or the Worker stopped while the provider call was
/// in flight. This path may call the provider and therefore only runs with an
/// explicitly external provider configuration.
pub async fn reconcile_external_cancellations(
    env: &Env,
    db: &D1Database,
    provider_config: ActionProviderConfig,
) -> ApiResult<usize> {
    if !pending_cancel_reconciliation_enabled(provider_config.mode()) {
        return Ok(0);
    }
    let cutoff = db::add_seconds_iso(-CANCEL_LEASE_SECONDS);
    let rows: Vec<CancelReconciliationRow> = db::all(
        db,
        pending_cancel_reconciliation_sql(),
        vec![db::text(&cutoff)],
    )
    .await?;

    let mut processed = 0;
    for row in rows {
        if !is_pending_cancel_reconciliation_row(&row) {
            continue;
        }
        let Some(command) = commands::get_for_user(db, &row.user_id, &row.command_id).await? else {
            continue;
        };
        if command.state != "succeeded" || command.intent != "create_reminder" {
            continue;
        }
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
    // A fresh effect must never create a past reminder. A reconciled provider
    // success is different: the external effect already happened, so recovery
    // must materialize that exact result without another delivery call.
    if reconciled_provider_response.is_none() && db::is_expired(due_at) {
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
        "SELECT provider, provider_idempotency_key, state, response_json, attempts FROM action_attempts WHERE command_id = ? AND user_id = ? ORDER BY created_at DESC LIMIT 1",
        vec![db::text(&command.id), db::text(user_id)],
    )
    .await
}

fn begin_effect_attempt_sql() -> &'static str {
    "UPDATE action_attempts SET state = 'running', attempts = attempts + 1, response_json = NULL, next_attempt_at = NULL, last_error = NULL, updated_at = ? WHERE user_id = ? AND command_id = ? AND provider = ? AND provider_idempotency_key = ? AND request_hash = ? AND state IN ('running', 'retrying', 'unknown') AND EXISTS (SELECT 1 FROM commands AS active_command JOIN outbox_events AS active_claim ON active_claim.user_id = active_command.user_id AND active_claim.aggregate_id = active_command.id WHERE active_command.id = ? AND active_command.user_id = ? AND active_command.state = 'running' AND active_command.version = ? AND active_claim.id = ? AND active_claim.user_id = ? AND active_claim.topic = ? AND active_claim.topic = 'command.execute' AND active_claim.aggregate_id = ? AND active_claim.idempotency_key = ? AND active_claim.state = 'running' AND active_claim.lease_token = ? AND active_claim.lease_expires_at IS NOT NULL AND active_claim.lease_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now'))"
}

async fn begin_effect_attempt(
    db: &D1Database,
    user_id: &str,
    command: &CommandRow,
    claim: &OutboxEventRow,
) -> ApiResult<Option<Value>> {
    let Some(lease_token) = claim.lease_token.as_deref() else {
        return Err(ApiError::conflict(
            "The durable effect permit has no active outbox lease",
        ));
    };
    let provider =
        providers::action_attempt_provider(&command.intent).unwrap_or(command.intent.as_str());
    let provider_idempotency_key = providers::scoped_action_idempotency_key(
        user_id,
        &command.intent,
        &command.idempotency_key,
    );
    let result = db::run(
        db,
        begin_effect_attempt_sql(),
        vec![
            db::text(&db::now_iso()),
            db::text(user_id),
            db::text(&command.id),
            db::text(provider),
            db::text(&provider_idempotency_key),
            db::text(&command.command_hash),
            db::text(&command.id),
            db::text(user_id),
            db::number(command.version),
            db::text(&claim.id),
            db::text(user_id),
            db::text(&claim.topic),
            db::text(&claim.aggregate_id),
            db::text(&claim.idempotency_key),
            db::text(lease_token),
        ],
    )
    .await?;
    if db::changes(&result) == 1 {
        return Ok(None);
    }
    if let Some(current) = previous_attempt(db, user_id, command).await? {
        if let Some(response) = reusable_succeeded_response(&current)? {
            return Ok(Some(response));
        }
    }
    Err(ApiError::conflict(
        "The durable effect permit is no longer active",
    ))
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
) -> ApiResult<CancelAttemptClaim> {
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
        return Ok(CancelAttemptClaim::CallProvider);
    }

    let existing: CancelAttemptRow = db::first(
        db,
        "SELECT state, response_json, updated_at FROM action_attempts WHERE provider = ? AND provider_idempotency_key = ?",
        vec![db::text(provider), db::text(provider_idempotency_key)],
    )
    .await?
    .ok_or_else(|| ApiError::new(500, "provider_cancel_error", "Cancellation fence disappeared"))?;

    if let Some(claim) = succeeded_cancel_claim(&existing) {
        return Ok(claim);
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
            return Ok(CancelAttemptClaim::CallProvider);
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
            return Ok(CancelAttemptClaim::CallProvider);
        }
    }

    Err(ApiError::new(
        503,
        "provider_cancel_in_progress",
        "Another request is already cancelling this reminder",
    ))
}

fn succeeded_cancel_claim(existing: &CancelAttemptRow) -> Option<CancelAttemptClaim> {
    if existing.state != "succeeded" {
        return None;
    }
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
    Some(CancelAttemptClaim::ReuseSucceeded(
        providers::ProviderResponse {
            provider_id,
            state: providers::ProviderDeliveryState::Succeeded,
        },
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

    fn succeeded_reminder_command(version: i64) -> CommandRow {
        CommandRow {
            id: "cmd_post_crash".to_string(),
            user_id: "usr_test".to_string(),
            device_id: None,
            session_id: Some("ses_test".to_string()),
            schema_version: 1,
            intent: "create_reminder".to_string(),
            args_json: json!({"title": "Call John", "due_at": "2026-08-12T13:00:00Z"}).to_string(),
            risk_level: "low".to_string(),
            needs_confirmation: 0,
            idempotency_key: "idem_post_crash".to_string(),
            confidence: None,
            locale: "en".to_string(),
            timezone: "Asia/Hong_Kong".to_string(),
            state: "succeeded".to_string(),
            command_hash: "hash_post_crash".to_string(),
            result_json: Some(
                json!({
                    "kind": "reminder",
                    "provider": "external.reminder",
                    "provider_id": "provider-reminder-1",
                    "status": "scheduled",
                })
                .to_string(),
            ),
            error_code: None,
            expires_at: None,
            model_version: None,
            version,
            created_at: "2026-08-11T23:00:00.000Z".to_string(),
            updated_at: "2026-08-11T23:00:00.000Z".to_string(),
        }
    }

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
    fn succeeded_attempt_reuses_exact_result_after_reminder_deadline_elapsed() {
        let args = serde_json::from_value::<Map<String, Value>>(json!({
            "title": "Call John",
            "due_at": "2020-01-01T00:00:00Z",
        }))
        .unwrap();
        assert!(commands::validate_action_args_shape("create_reminder", &args).is_ok());
        assert!(commands::validate_action_args("create_reminder", &args).is_err());

        let expected = json!({
            "kind": "reminder",
            "provider": "external.reminder",
            "provider_id": "provider-reminder-1",
            "status": "scheduled",
        });
        let attempt = EffectAttemptRow {
            provider: "action.reminder".to_string(),
            provider_idempotency_key: "scoped-idem".to_string(),
            state: "succeeded".to_string(),
            response_json: Some(expected.to_string()),
            attempts: 1,
        };

        let mut provider_calls = 0;
        let result = match reusable_succeeded_response(&attempt).unwrap() {
            Some(response) => response,
            None => {
                provider_calls += 1;
                json!(null)
            }
        };
        assert_eq!(provider_calls, 0);
        assert_eq!(result, expected);
    }

    #[test]
    fn zero_attempt_running_row_is_a_permit_not_an_unknown_provider_call() {
        let mut attempt = EffectAttemptRow {
            provider: "action.reminder".to_string(),
            provider_idempotency_key: "scoped-idem".to_string(),
            state: "running".to_string(),
            response_json: None,
            attempts: 0,
        };
        assert!(is_execution_permit_only(&attempt));

        attempt.attempts = 1;
        assert!(!is_execution_permit_only(&attempt));
        attempt.attempts = 0;
        attempt.state = "retrying".to_string();
        assert!(!is_execution_permit_only(&attempt));
    }

    #[test]
    fn beginning_an_effect_requires_exact_active_claim_and_command_generation() {
        let sql = begin_effect_attempt_sql();
        assert!(sql.contains("active_command.state = 'running'"));
        assert!(sql.contains("active_command.version = ?"));
        assert!(sql.contains("active_claim.topic = 'command.execute'"));
        assert!(sql.contains("active_claim.aggregate_id = active_command.id"));
        assert!(sql.contains("active_claim.lease_token = ?"));
        assert!(
            sql.contains("active_claim.lease_expires_at > strftime('%Y-%m-%dT%H:%M:%fZ','now')")
        );
        assert_eq!(sql.bytes().filter(|byte| *byte == b'?').count(), 15);
    }

    #[test]
    fn reconciled_provider_success_bypasses_only_elapsed_deadline() {
        let past_due = serde_json::from_value::<Map<String, Value>>(json!({
            "title": "Call John",
            "due_at": "2020-01-01T00:00:00Z",
        }))
        .unwrap();
        assert!(validate_args_for_effect("create_reminder", &past_due, true).is_ok());
        assert!(validate_args_for_effect("create_reminder", &past_due, false).is_err());

        let malformed = serde_json::from_value::<Map<String, Value>>(json!({
            "title": "Call John",
            "due_at": "not-a-timestamp",
        }))
        .unwrap();
        assert!(validate_args_for_effect("create_reminder", &malformed, true).is_err());
    }

    #[test]
    fn materialized_reminder_recovery_preserves_elapsed_effect_without_redelivery() {
        let row = MaterializedReminderRow {
            id: "rem_local".to_string(),
            status: "scheduled".to_string(),
            title: "Call John".to_string(),
            due_at: "2020-01-01T00:00:00Z".to_string(),
            timezone: "UTC".to_string(),
            provider: "local.reminder".to_string(),
            provider_reminder_id: None,
        };
        let response = materialized_reminder_response(&row);

        assert_eq!(response["reminder_id"], json!("rem_local"));
        assert_eq!(response["due_at"], json!("2020-01-01T00:00:00Z"));
        assert_eq!(response["provider"], json!("local.reminder"));
        assert_eq!(response["external_delivery"], Value::Null);
        assert_eq!(response["provider_id"], Value::Null);
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

    #[test]
    fn succeeded_cancel_attempt_reuses_response_without_another_provider_call() {
        // This is the durable state left if the Worker stops after persisting
        // provider success but before the local finalization batch.
        let attempt = CancelAttemptRow {
            state: "succeeded".to_string(),
            response_json: Some(
                json!({
                    "provider_id": "provider-reminder-1",
                    "state": "succeeded",
                })
                .to_string(),
            ),
            updated_at: "2026-08-11T23:00:00.000Z".to_string(),
        };

        let claim = succeeded_cancel_claim(&attempt).expect("success must be reusable");
        let (provider_calls, response) = match claim {
            CancelAttemptClaim::CallProvider => (1, None),
            CancelAttemptClaim::ReuseSucceeded(response) => (0, Some(response)),
        };

        assert_eq!(provider_calls, 0);
        let response = response.expect("the persisted response must be returned");
        assert_eq!(response.provider_id.as_deref(), Some("provider-reminder-1"));
        assert_eq!(response.state, providers::ProviderDeliveryState::Succeeded);
    }

    #[test]
    fn succeeded_cancel_reconciliation_is_provider_and_age_independent() {
        let query = succeeded_cancel_reconciliation_sql();
        assert!(query.contains("reminder.status = 'scheduled'"));
        assert!(query.contains("command.state = 'succeeded'"));
        assert!(query.contains("attempt.state = 'succeeded'"));
        assert!(!query.contains("attempt.updated_at <="));
        assert!(!query.contains('?'));

        let mut row = CancelReconciliationRow {
            user_id: "usr_test".to_string(),
            command_id: "cmd_post_crash".to_string(),
            attempt_state: "succeeded".to_string(),
        };
        assert!(is_succeeded_cancel_reconciliation_row(&row));
        row.attempt_state = "unknown".to_string();
        assert!(!is_succeeded_cancel_reconciliation_row(&row));

        let command = succeeded_reminder_command(7);
        let finalization = build_undo_finalization(&command, "reminder-1", "external.reminder");

        assert_eq!(
            finalization.result,
            json!({
                "kind": "undo",
                "provider": "external.reminder",
                "effect_id": "reminder-1",
                "status": "cancelled",
                "already_cancelled": false,
            })
        );
        assert_eq!(finalization.command_result["undo"], finalization.result);
        assert_eq!(finalization.next_version, 8);

        let effect_update = undo_effect_update_sql("reminders");
        let transaction_sql = [
            effect_update.as_str(),
            undo_command_update_sql(),
            undo_audit_insert_sql(),
            undo_phone_change_insert_sql(),
        ];
        assert!(effect_update.starts_with("UPDATE reminders SET status = 'cancelled'"));
        assert!(effect_update.contains("status IN ('scheduled', 'draft')"));
        assert!(effect_update.contains("state = 'succeeded' AND version = ?"));
        assert!(undo_command_update_sql().contains("version = ? AND changes() = 1"));
        assert_eq!(
            transaction_sql
                .iter()
                .filter(|sql| sql.contains("INSERT INTO phone_changes"))
                .count(),
            1
        );
        assert!(undo_phone_change_insert_sql().ends_with("WHERE changes() = 1"));
    }

    #[test]
    fn pending_cancel_reconciliation_requires_external_mode_and_running_lease_age() {
        let query = pending_cancel_reconciliation_sql();
        assert!(query.contains("attempt.state IN ('unknown', 'retrying')"));
        assert!(query.contains("attempt.state = 'running' AND attempt.updated_at <= ?"));
        assert!(!query.contains("attempt.state = 'succeeded'"));
        assert_eq!(query.matches('?').count(), 1);

        assert!(pending_cancel_reconciliation_enabled(
            providers::ActionProviderMode::External
        ));
        assert!(!pending_cancel_reconciliation_enabled(
            providers::ActionProviderMode::Internal
        ));
        assert!(!pending_cancel_reconciliation_enabled(
            providers::ActionProviderMode::Disabled
        ));

        let mut row = CancelReconciliationRow {
            user_id: "usr_test".to_string(),
            command_id: "cmd_pending".to_string(),
            attempt_state: "running".to_string(),
        };
        assert!(is_pending_cancel_reconciliation_row(&row));
        row.attempt_state = "succeeded".to_string();
        assert!(!is_pending_cancel_reconciliation_row(&row));
    }
}
